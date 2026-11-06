#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod metadata;
mod palette;
mod server;
mod smtc;
mod win32;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use tiny_http::Server;
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
use crate::server::spawn_http_server;
use crate::smtc::spawn_smtc_poller;
use crate::win32::{open_url, set_startup_enabled};

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let addr = format!("{HOST}:{PORT}");
    let server = match Server::http(&addr) {
        Ok(s) => Arc::new(s),
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

    let shared = Arc::new(Mutex::new(SharedApp::new()));
    let running = Arc::new(AtomicBool::new(true));
    let ui_dirty = Arc::new(AtomicBool::new(false));

    let (poller_thread, poller_handle) =
        spawn_smtc_poller(Arc::clone(&shared), Arc::clone(&running));
    let http_handle = spawn_http_server(
        Arc::clone(&shared),
        Arc::clone(&running),
        Arc::clone(&ui_dirty),
        Arc::clone(&server),
        poller_thread.clone(),
    );

    let (initial_always_show, initial_startup) = {
        let app = lock_shared(&shared);
        (app.config.always_show, app.run_on_startup)
    };

    let menu = Menu::new();
    let always_show_item = CheckMenuItem::new(
        "Always Show (Disable Auto-Hide)",
        true,
        initial_always_show,
        None,
    );
    let startup_item = CheckMenuItem::new(
        "Launch at Windows Startup",
        true,
        initial_startup,
        None,
    );
    let settings_item = MenuItem::new("Widget Size, Themes & Settings...", true, None);
    let copy_url_item = MenuItem::new("Copy OBS URL (http://127.0.0.1:8765/)", true, None);
    let preview_item = MenuItem::new("Open Preview in Browser", true, None);
    let quit_item = MenuItem::new("Exit", true, None);

    let _ = menu.append(&always_show_item);
    let _ = menu.append(&startup_item);
    let _ = menu.append(&settings_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&copy_url_item);
    let _ = menu.append(&preview_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_item);

    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("SMTC Browser Source (http://127.0.0.1:8765/)")
        .with_icon(create_tray_icon())
        .build()
        .expect("Failed to initialize system tray icon");

    let widget_url = format!("http://{HOST}:{PORT}/");
    let settings_url = format!("http://{HOST}:{PORT}/settings");

    let mut msg = MSG::default();
    'event_loop: while running.load(Ordering::Relaxed) {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    running.store(false, Ordering::Relaxed);
                    poller_thread.unpark();
                    server.unblock();
                    break 'event_loop;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

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

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == always_show_item.id() {
                let checked = always_show_item.is_checked();
                let mut app = lock_shared(&shared);
                app.config.always_show = checked;
                app.config.display_mode =
                    if checked { "always" } else { "default" }.to_string();
                app.save_config_now();
            } else if event.id == startup_item.id() {
                let checked = startup_item.is_checked();
                let actual = set_startup_enabled(checked);
                startup_item.set_checked(actual);
                lock_shared(&shared).run_on_startup = actual;
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
                poller_thread.unpark();
                server.unblock();
                break 'event_loop;
            }
        }

        if ui_dirty.swap(false, Ordering::Acquire) {
            let app = lock_shared(&shared);
            if always_show_item.is_checked() != app.config.always_show {
                always_show_item.set_checked(app.config.always_show);
            }
            if startup_item.is_checked() != app.run_on_startup {
                startup_item.set_checked(app.run_on_startup);
            }
        }

        {
            let mut app = lock_shared(&shared);
            if let Some(dirty_at) = app.config_dirty_at {
                if dirty_at.elapsed() >= CONFIG_SAVE_DEBOUNCE {
                    app.save_config_now();
                }
            }
        }

        thread::sleep(Duration::from_millis(16));
    }

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