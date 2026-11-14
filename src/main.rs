// Hides the black command-prompt console window in release builds while keeping
// it visible during `cargo run` debug builds so you can see error messages.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod metadata;
mod palette;
mod server;
mod smtc;
mod win32;

use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use windows::{
    core::w,
    Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
    Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MessageBoxW, PeekMessageW, TranslateMessage, MB_ICONINFORMATION, MB_OK,
        MSG, PM_REMOVE, WM_QUIT,
    },
};

use crate::config::{lock_shared, SharedApp, CONFIG_SAVE_DEBOUNCE, HOST, PORT};
use crate::palette::create_tray_icon;
use crate::server::{broadcast_state, spawn_http_server};
use crate::smtc::spawn_smtc_poller;
use crate::win32::{open_url, set_startup_enabled};

fn main() {
    // Initialize Windows COM (Component Object Model) in Single-Threaded Apartment
    // mode on the main thread. Required by Windows UI controls like the system tray.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // Bind to port 8765 immediately. This serves two purposes:
    // 1. It prepares the network socket for our local web server.
    // 2. It acts as a single-instance lock: if another copy of the app is already
    //    running, binding to port 8765 will fail and we can alert the user.
    let addr = format!("{HOST}:{PORT}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => {
            let _ = l.set_nonblocking(true);
            l
        }
        Err(_) => {
            unsafe {
                MessageBoxW(
                    None,
                    w!("SMTC Now Playing is already running on http://127.0.0.1:8765/\nCheck your Windows System Tray."),
                    w!("SMTC Now Playing"),
                    MB_OK | MB_ICONINFORMATION,
                );
                CoUninitialize();
            }
            return;
        }
    };

    // Create thread-safe containers (`Arc` = Atomic Reference Counted pointer)
    // so multiple background threads can share access to our app state.
    let shared = Arc::new(Mutex::new(SharedApp::new()));
    let running = Arc::new(AtomicBool::new(true));
    let ui_dirty = Arc::new(AtomicBool::new(false));

    // Channels for pushing live JSON updates to WebSocket clients and signaling shutdown.
    let (ws_tx, _) = broadcast::channel::<String>(32);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start the background thread that listens to Windows Media (SMTC) events.
    let (poller_thread, poller_handle) =
        spawn_smtc_poller(Arc::clone(&shared), Arc::clone(&running), ws_tx.clone());

    // Start the background thread that runs the Axum HTTP/WebSocket server.
    let http_handle = spawn_http_server(
        listener,
        Arc::clone(&shared),
        Arc::clone(&ui_dirty),
        poller_thread.clone(),
        ws_tx.clone(),
        shutdown_rx,
    );

    // Read initial checkbox states for the system tray menu.
    let (initial_always_show, initial_startup) = {
        let app = lock_shared(&shared);
        (app.config.always_show, app.run_on_startup)
    };

    // Build the right-click context menu for the system tray icon.
    let menu = Menu::new();
    let always_show_item = CheckMenuItem::new(
        "Always Show (Disable Auto-Hide)",
        true,
        initial_always_show,
        None,
    );
    let startup_item = CheckMenuItem::new("Launch at Windows Startup", true, initial_startup, None);
    let settings_item = MenuItem::new("Widget Size, Themes & Settings...", true, None);
    let copy_url_item = MenuItem::new("Copy OBS URL (http://127.0.0.1:8765/)", true, None);
    let preview_item = MenuItem::new("Open Preview in Browser", true, None);
    let quit_item = MenuItem::new("Exit", true, None);

    let _ = menu.append_items(&[
        &always_show_item,
        &startup_item,
        &settings_item,
        &PredefinedMenuItem::separator(),
        &copy_url_item,
        &preview_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);

    // Keep `_tray_icon` alive in `main`'s scope; dropping it removes the icon from the tray.
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("SMTC Browser Source (http://127.0.0.1:8765/)")
        .with_icon(create_tray_icon())
        .build()
        .expect("Failed to initialize system tray icon");

    let widget_url = format!("http://{HOST}:{PORT}/");
    let settings_url = format!("http://{HOST}:{PORT}/settings");

    // Main Windows UI event loop.
    let mut msg = MSG::default();
    while running.load(Ordering::Relaxed) {
        // Pump pending Windows messages so the system tray icon responds to mouse clicks.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    running.store(false, Ordering::Relaxed);
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Open the settings dashboard when the user left-clicks the tray icon.
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                open_url(&settings_url);
            }
        }

        // Handle clicks on items inside the right-click tray menu.
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == always_show_item.id() {
                let checked = always_show_item.is_checked();
                {
                    let mut app = lock_shared(&shared);
                    app.config.always_show = checked;
                    app.config.display_mode = if checked { "always" } else { "default" }.into();
                    app.save_config_now();
                }
                broadcast_state(&shared, &ws_tx);
            } else if event.id == startup_item.id() {
                let actual = set_startup_enabled(startup_item.is_checked());
                startup_item.set_checked(actual);
                lock_shared(&shared).run_on_startup = actual;
                broadcast_state(&shared, &ws_tx);
            } else if event.id == settings_item.id() {
                open_url(&settings_url);
            } else if event.id == copy_url_item.id() {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    let _ = clipboard.set_text(widget_url.clone());
                }
            } else if event.id == preview_item.id() {
                open_url(&widget_url);
            } else if event.id == quit_item.id() {
                running.store(false, Ordering::Relaxed);
                break;
            }
        }

        if !running.load(Ordering::Relaxed) {
            break;
        }

        // If settings were changed via the web dashboard, sync the tray menu checkboxes.
        if ui_dirty.swap(false, Ordering::Acquire) {
            let app = lock_shared(&shared);
            if always_show_item.is_checked() != app.config.always_show {
                always_show_item.set_checked(app.config.always_show);
            }
            if startup_item.is_checked() != app.run_on_startup {
                startup_item.set_checked(app.run_on_startup);
            }
        }

        // Flush pending configuration changes to disk once the debounce timer expires.
        {
            let mut app = lock_shared(&shared);
            if let Some(dirty_at) = app.config_dirty_at {
                if dirty_at.elapsed() >= CONFIG_SAVE_DEBOUNCE {
                    app.save_config_now();
                }
            }
        }

        // Sleep briefly (~60Hz) so the main loop uses ~0% CPU while idle.
        thread::sleep(Duration::from_millis(16));
    }

    // Graceful shutdown: wake up the poller thread and stop the web server.
    poller_thread.unpark();
    let _ = shutdown_tx.send(true);

    // Ensure any unsaved settings are written before exiting.
    {
        let mut app = lock_shared(&shared);
        if app.config_dirty_at.is_some() {
            app.save_config_now();
        }
    }

    let _ = poller_handle.join();
    let _ = http_handle.join();

    unsafe {
        CoUninitialize();
    }
}