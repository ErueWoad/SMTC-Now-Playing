#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod metadata;
mod server;
mod smtc;
mod win32;

use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use windows::{
    core::{w, HSTRING},
    Win32::System::Com::COINIT_APARTMENTTHREADED,
    Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MessageBoxW, TranslateMessage, MB_ICONERROR,
        MB_ICONINFORMATION, MB_OK, MSG,
    },
};

use crate::config::{
    lock_shared, write_config_atomic, DisplayMode, SharedApp, CONFIG_SAVE_DEBOUNCE, HOST, PORT,
};
use crate::server::{broadcast_state, spawn_http_server, StateHub};
use crate::smtc::spawn_smtc_poller;
use crate::win32::{
    current_thread_id, kill_thread_timer, open_url, set_clipboard_text, set_startup_enabled,
    set_thread_timer, ComGuard, WM_APP_SYNC, WM_TIMER_MSG,
};

/// Procedurally draws a 64x64 pink rounded-square icon with a white play triangle in memory.
fn create_tray_icon() -> Icon {
    const SIZE: u32 = 64;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);

            let in_rect = (6.0..=58.0).contains(&fx) && (6.0..=58.0).contains(&fy) && {
                let dx = fx - fx.clamp(20.0, 44.0);
                let dy = fy - fy.clamp(20.0, 44.0);
                (dx * dx + dy * dy) <= 196.0
            };

            if !in_rect {
                continue;
            }

            let in_triangle =
                (24.0..=46.0).contains(&fx) && (fy - 32.0).abs() <= 14.0 * (46.0 - fx) / 22.0;

            let idx = ((y * SIZE + x) * 4) as usize;
            let color = if in_triangle {
                [255, 255, 255, 255]
            } else {
                [255, 15, 135, 255]
            };
            rgba[idx..idx + 4].copy_from_slice(&color);
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("Failed to create tray icon")
}

fn main() {
    let _com = ComGuard::init(COINIT_APARTMENTTHREADED);
    let main_thread_id = current_thread_id();

    let addr = format!("{HOST}:{PORT}");
    let widget_url = format!("http://{HOST}:{PORT}/");
    let preview_url = format!("http://{HOST}:{PORT}/?mode=always");
    let settings_url = format!("http://{HOST}:{PORT}/settings");

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(err) => {
            let (body, icon) = if err.kind() == ErrorKind::AddrInUse {
                (
                    format!(
                        "SMTC Now Playing is already running on {widget_url}\nCheck your Windows System Tray."
                    ),
                    MB_OK | MB_ICONINFORMATION,
                )
            } else {
                (
                    format!("Failed to bind local server on {addr}:\n{err}"),
                    MB_OK | MB_ICONERROR,
                )
            };
            let body_h = HSTRING::from(body);
            // SAFETY: Valid HSTRING and static title passed to Win32 MessageBoxW.
            unsafe {
                MessageBoxW(None, &body_h, w!("SMTC Now Playing"), icon);
            }
            return;
        }
    };

    let shared = Arc::new(Mutex::new(SharedApp::new()));
    let running = Arc::new(AtomicBool::new(true));
    let ui_dirty = Arc::new(AtomicBool::new(false));
    let hub = Arc::new(StateHub::new());

    let (poller_thread, poller_handle) =
        spawn_smtc_poller(Arc::clone(&shared), Arc::clone(&running), Arc::clone(&hub));

    let http_handle = spawn_http_server(
        listener,
        Arc::clone(&shared),
        Arc::clone(&ui_dirty),
        main_thread_id,
        poller_thread.clone(),
        Arc::clone(&hub),
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
    let startup_item = CheckMenuItem::new("Launch at Windows Startup", true, initial_startup, None);
    let settings_item = MenuItem::new("Widget Size, Themes & Settings...", true, None);
    let copy_url_item = MenuItem::new(format!("Copy OBS URL ({widget_url})"), true, None);
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

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("SMTC Browser Source ({widget_url})"))
        .with_icon(create_tray_icon())
        .build()
        .expect("Failed to initialize system tray icon");

    // Event-driven Win32 UI loop: blocks with 0% CPU and 0 wake-ups while idle.
    let mut msg = MSG::default();
    let mut debounce_timer_id: usize = 0;

    while running.load(Ordering::Relaxed) {
        // SAFETY: `GetMessageW` blocks until a Win32 message or timer arrives for this thread.
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if ret <= 0 {
            running.store(false, Ordering::Relaxed);
            break;
        }

        if msg.message == WM_TIMER_MSG
            && debounce_timer_id != 0
            && msg.wParam.0 == debounce_timer_id
        {
            kill_thread_timer(debounce_timer_id);
            debounce_timer_id = 0;
        } else if msg.message != WM_APP_SYNC {
            // SAFETY: Standard Win32 message translation and dispatch.
            unsafe {
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
                let to_save = {
                    let mut app = lock_shared(&shared);
                    app.config.always_show = checked;
                    app.config.display_mode = if checked {
                        DisplayMode::Always
                    } else {
                        DisplayMode::Default
                    };
                    app.prepare_config_save()
                };
                if let Some((path, json)) = to_save {
                    write_config_atomic(&path, &json);
                }
                broadcast_state(&shared, &hub);
            } else if event.id == startup_item.id() {
                let actual = set_startup_enabled(startup_item.is_checked());
                startup_item.set_checked(actual);
                lock_shared(&shared).run_on_startup = actual;
                broadcast_state(&shared, &hub);
            } else if event.id == settings_item.id() {
                open_url(&settings_url);
            } else if event.id == copy_url_item.id() {
                let _ = set_clipboard_text(&widget_url);
            } else if event.id == preview_item.id() {
                open_url(&preview_url);
            } else if event.id == quit_item.id() {
                running.store(false, Ordering::Relaxed);
                break;
            }
        }

        if !running.load(Ordering::Relaxed) {
            break;
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

        let (save_payload, remaining_ms) = {
            let mut app = lock_shared(&shared);
            match app.config_dirty_at {
                Some(dirty_at) => {
                    let elapsed = dirty_at.elapsed();
                    if elapsed >= CONFIG_SAVE_DEBOUNCE {
                        (app.prepare_config_save(), None)
                    } else {
                        let rem = (CONFIG_SAVE_DEBOUNCE - elapsed).as_millis().max(15) as u32;
                        (None, Some(rem))
                    }
                }
                None => (None, None),
            }
        };

        if let Some((path, json)) = save_payload {
            kill_thread_timer(debounce_timer_id);
            debounce_timer_id = 0;
            write_config_atomic(&path, &json);
        } else if let Some(ms) = remaining_ms {
            debounce_timer_id = set_thread_timer(debounce_timer_id, ms);
        }
    }

    kill_thread_timer(debounce_timer_id);
    drop(tray_icon);

    poller_thread.unpark();
    hub.shutdown();
    // Unblock `TcpListener::incoming()` so the HTTP server thread exits immediately.
    let _ = TcpStream::connect(&addr);

    let final_save = {
        let mut app = lock_shared(&shared);
        if app.config_dirty_at.is_some() {
            app.prepare_config_save()
        } else {
            None
        }
    };
    if let Some((path, json)) = final_save {
        write_config_atomic(&path, &json);
    }

    let _ = poller_handle.join();
    let _ = http_handle.join();
}