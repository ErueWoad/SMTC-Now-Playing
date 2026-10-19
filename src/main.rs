#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat};
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Response, Server};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use windows::{
    core::w,
    Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager as MediaManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    },
    Storage::Streams::{DataReader, IRandomAccessStreamReference},
    Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MessageBoxW, PeekMessageW, TranslateMessage, MB_ICONINFORMATION, MB_OK,
        MSG, PM_REMOVE,
    },
};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 8765;
const MAX_THUMBNAIL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_THUMBNAIL_RETRIES: u32 = 10;
const WINDOWS_TO_UNIX_EPOCH_SECS: f64 = 11_644_473_600.0;

#[derive(Clone, Serialize, Deserialize, Debug)]
struct Config {
    width: u32,
    height: u32,
    radius: u32,
    opacity: f32,
    always_show: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 510,
            height: 122,
            radius: 10,
            opacity: 1.0,
            always_show: false,
        }
    }
}

impl Config {
    fn clamped(mut self) -> Self {
        self.width = self.width.clamp(260, 1920);
        self.height = self.height.clamp(80, 600);
        self.radius = self.radius.clamp(0, 60);
        self.opacity = (self.opacity.clamp(0.0, 1.0) * 100.0).round() / 100.0;
        self
    }
}

#[derive(Clone, Serialize, Debug)]
struct MediaState {
    is_playing: bool,
    title: String,
    artist: String,
    position: f64,
    duration: f64,
    art: Option<String>,
    accent: [u8; 3],
}

impl Default for MediaState {
    fn default() -> Self {
        Self {
            is_playing: false,
            title: "Waiting for Media...".to_string(),
            artist: "Play a track to display".to_string(),
            position: 0.0,
            duration: 0.0,
            art: None,
            accent: [255, 15, 135],
        }
    }
}

#[derive(Serialize)]
struct StatePayload<'a> {
    #[serde(flatten)]
    state: &'a MediaState,
    config: &'a Config,
}

struct SharedApp {
    state: MediaState,
    config: Config,
    config_path: PathBuf,
}

impl SharedApp {
    fn new() -> Self {
        let config_path = get_config_path();
        let mut app = Self {
            state: MediaState::default(),
            config: Config::default(),
            config_path,
        };
        app.load_config();
        app
    }

    fn load_config(&mut self) {
        if self.config_path.exists() {
            if let Ok(raw) = fs::read_to_string(&self.config_path) {
                if let Ok(parsed) = serde_json::from_str::<Config>(&raw) {
                    self.config = parsed.clamped();
                    return;
                }
            }
        }
        self.save_config();
    }

    fn save_config(&self) {
        let tmp_path = self.config_path.with_extension("json.tmp");
        if let Ok(json_str) = serde_json::to_string_pretty(&self.config) {
            if fs::write(&tmp_path, json_str).is_ok() {
                let _ = fs::rename(&tmp_path, &self.config_path);
            }
        }
    }
}

fn get_config_path() -> PathBuf {
    if let Ok(appdata) = env::var("APPDATA") {
        let dir = PathBuf::from(appdata).join("SMTC-Now-Playing");
        let _ = fs::create_dir_all(&dir);
        return dir.join("config.json");
    }
    env::temp_dir().join("smtc-now-playing-config.json")
}

// ---------------------------------------------------------------------------
// Color, Image & Icon Utilities
// ---------------------------------------------------------------------------
fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / delta).rem_euclid(6.0) / 6.0
    } else if max == g {
        (((b - r) / delta) + 2.0) / 6.0
    } else {
        (((r - g) / delta) + 4.0) / 6.0
    };

    let s = if max == 0.0 { 0.0 } else { delta / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (v, v, v);
    }
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

fn extract_vibrant_accent(img: &DynamicImage) -> [u8; 3] {
    let small = img.resize_exact(64, 64, FilterType::Triangle).to_rgb8();

    #[derive(Clone, Copy, Default)]
    struct Bin {
        weight: f32,
        r: f32,
        g: f32,
        b: f32,
    }

    let mut bins = [Bin::default(); 36];

    for pixel in small.pixels() {
        let rf = pixel[0] as f32 / 255.0;
        let gf = pixel[1] as f32 / 255.0;
        let bf = pixel[2] as f32 / 255.0;
        let (h, s, v) = rgb_to_hsv(rf, gf, bf);

        if v < 0.22 || s < 0.28 || (v > 0.94 && s < 0.45) {
            continue;
        }

        let weight = s.powf(2.2) * v.powf(1.2);
        let idx = ((h * 36.0) as usize) % 36;
        bins[idx].weight += weight;
        bins[idx].r += rf * weight;
        bins[idx].g += gf * weight;
        bins[idx].b += bf * weight;
    }

    let best = bins
        .iter()
        .copied()
        .max_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or_default();

    if best.weight <= 0.0 {
        return [235, 235, 245];
    }

    let avg_r = best.r / best.weight;
    let avg_g = best.g / best.weight;
    let avg_b = best.b / best.weight;

    let (h, s, v) = rgb_to_hsv(avg_r, avg_g, avg_b);
    let (r_out, g_out, b_out) = hsv_to_rgb(h, s.max(0.80), v.max(0.92));

    [
        (r_out * 255.0) as u8,
        (g_out * 255.0) as u8,
        (b_out * 255.0) as u8,
    ]
}

fn image_to_data_uri(img: &DynamicImage) -> Option<String> {
    let (w, h) = img.dimensions();
    let resized = if w > 256 || h > 256 {
        img.resize(256, 256, FilterType::Lanczos3)
    } else {
        img.clone()
    };

    let mut buf = Cursor::new(Vec::new());
    resized.write_to(&mut buf, ImageFormat::Png).ok()?;
    let b64 = BASE64.encode(buf.into_inner());
    Some(format!("data:image/png;base64,{b64}"))
}

fn create_tray_icon() -> Icon {
    let size = 64u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;

            // Rounded rectangle: x in [6, 58], y in [6, 58], radius 14
            let in_rect = if fx >= 6.0 && fx <= 58.0 && fy >= 6.0 && fy <= 58.0 {
                let cx = fx.clamp(20.0, 44.0);
                let cy = fy.clamp(20.0, 44.0);
                ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= 14.0
            } else {
                false
            };

            if !in_rect {
                continue;
            }

            // White play triangle: (24, 18) -> (46, 32) -> (24, 46)
            let in_triangle = fx >= 24.0
                && fx <= 46.0
                && (fy - 32.0).abs() <= 14.0 * (46.0 - fx) / 22.0;

            let idx = ((y * size + x) * 4) as usize;
            if in_triangle {
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            } else {
                rgba[idx] = 255;
                rgba[idx + 1] = 15;
                rgba[idx + 2] = 135;
                rgba[idx + 3] = 255;
            }
        }
    }

    Icon::from_rgba(rgba, size, size).expect("Failed to create tray icon")
}

// ---------------------------------------------------------------------------
// Background SMTC Polling Thread
// ---------------------------------------------------------------------------
fn read_thumbnail(thumb_ref: &IRandomAccessStreamReference) -> Option<DynamicImage> {
    let stream = thumb_ref.OpenReadAsync().ok()?.get().ok()?;
    let result = (|| -> Option<DynamicImage> {
        let size = stream.Size().ok()?;
        if size == 0 || size > MAX_THUMBNAIL_BYTES {
            return None;
        }
        let input_stream = stream.GetInputStreamAt(0).ok()?;
        let reader = DataReader::CreateDataReader(&input_stream).ok()?;
        let load_ok = reader.LoadAsync(size as u32).ok()?.get().is_ok();
        let mut buf = vec![0u8; size as usize];
        let read_ok = load_ok && reader.ReadBytes(&mut buf).is_ok();
        let _ = reader.Close();
        if !read_ok {
            return None;
        }
        image::load_from_memory(&buf).ok()
    })();
    let _ = stream.Close();
    result
}

fn spawn_smtc_poller(shared: Arc<Mutex<SharedApp>>, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let mut manager: Option<MediaManager> = None;
        let mut last_track_key = String::new();
        let mut thumb_retries = 0u32;
        let mut cached_art_uri: Option<String> = None;
        let mut cached_accent: [u8; 3] = [255, 15, 135];
        let mut last_raw_pos: f64 = -1.0;
        let mut local_anchor_time = Instant::now();
        let mut local_anchor_pos: f64 = 0.0;

        while running.load(Ordering::Relaxed) {
            let step = (|| -> windows::core::Result<bool> {
                if manager.is_none() {
                    manager = Some(MediaManager::RequestAsync()?.get()?);
                }
                let mgr = manager.as_ref().unwrap();

                let session = match mgr.GetCurrentSession() {
                    Ok(s) => s,
                    Err(_) => {
                        if let Ok(mut app) = shared.lock() {
                            app.state.is_playing = false;
                        }
                        return Ok(false);
                    }
                };

                let is_playing = session
                    .GetPlaybackInfo()
                    .and_then(|info| info.PlaybackStatus())
                    .map(|status| status == PlaybackStatus::Playing)
                    .unwrap_or(false);

                let props = session.TryGetMediaPropertiesAsync()?.get()?;
                let title = props.Title().map(|s| s.to_string()).unwrap_or_default().trim().to_string();
                let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default().trim().to_string();

                if title.is_empty() {
                    if let Ok(mut app) = shared.lock() {
                        app.state.is_playing = false;
                    }
                    return Ok(false);
                }

                let track_key = format!("{title}||{artist}");
                let is_new_track = track_key != last_track_key;
                let thumb_ref = props.Thumbnail().ok();
                let should_retry_art = !is_new_track
                    && cached_art_uri.is_none()
                    && thumb_retries < MAX_THUMBNAIL_RETRIES
                    && thumb_ref.is_some();

                if is_new_track {
                    last_track_key = track_key;
                    last_raw_pos = -1.0;
                    thumb_retries = 0;
                    cached_accent = [255, 15, 135];
                    cached_art_uri = None;
                }

                if is_new_track || should_retry_art {
                    if let Some(ref tref) = thumb_ref {
                        if let Some(img) = read_thumbnail(tref) {
                            cached_accent = extract_vibrant_accent(&img);
                            cached_art_uri = image_to_data_uri(&img);
                        } else {
                            thumb_retries += 1;
                        }
                    }
                }

                let mut position = 0.0;
                let mut duration = 0.0;

                if let Ok(timeline) = session.GetTimelineProperties() {
                    let raw_pos = timeline
                        .Position()
                        .map(|t| t.Duration as f64 / 10_000_000.0)
                        .unwrap_or(0.0);
                    duration = timeline
                        .EndTime()
                        .map(|t| (t.Duration as f64 / 10_000_000.0).max(0.0))
                        .unwrap_or(0.0);

                    let last_updated_secs = timeline
                        .LastUpdatedTime()
                        .map(|dt| (dt.UniversalTime as f64 / 10_000_000.0) - WINDOWS_TO_UNIX_EPOCH_SECS)
                        .unwrap_or(0.0);

                    let now_secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);

                    let age = now_secs - last_updated_secs;

                    if (0.0..36000.0).contains(&age) {
                        position = raw_pos + if is_playing { age } else { 0.0 };
                    } else {
                        if (raw_pos - last_raw_pos).abs() > 0.25 {
                            last_raw_pos = raw_pos;
                            local_anchor_pos = raw_pos;
                            local_anchor_time = Instant::now();
                        }
                        if is_playing {
                            position = local_anchor_pos + local_anchor_time.elapsed().as_secs_f64();
                        } else {
                            position = raw_pos;
                        }
                    }

                    if duration > 0.0 {
                        position = position.clamp(0.0, duration);
                    }
                }

                if let Ok(mut app) = shared.lock() {
                    app.state = MediaState {
                        is_playing,
                        title,
                        artist,
                        position,
                        duration,
                        art: cached_art_uri.clone(),
                        accent: cached_accent,
                    };
                }

                Ok(true)
            })();

            match step {
                Ok(true) => thread::sleep(Duration::from_millis(250)),
                Ok(false) => thread::sleep(Duration::from_millis(500)),
                Err(_) => {
                    manager = None;
                    if let Ok(mut app) = shared.lock() {
                        app.state.is_playing = false;
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }

        unsafe {
            CoUninitialize();
        }
    });
}

// ---------------------------------------------------------------------------
// HTTP Server Thread (Serves Widget at '/', State at '/state', Settings at '/settings')
// ---------------------------------------------------------------------------
fn spawn_http_server(shared: Arc<Mutex<SharedApp>>, running: Arc<AtomicBool>, server: Arc<Server>) {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let Ok(Some(mut request)) = server.try_recv() else {
                thread::sleep(Duration::from_millis(15));
                continue;
            };

            let url_path = request
                .url()
                .split('?')
                .next()
                .unwrap_or("/")
                .to_string();

            match (request.method(), url_path.as_str()) {
                (&Method::Get, "/state") => {
                    let body = if let Ok(app) = shared.lock() {
                        serde_json::to_vec(&StatePayload {
                            state: &app.state,
                            config: &app.config,
                        })
                        .unwrap_or_default()
                    } else {
                        Vec::new()
                    };

                    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                    let cache_ctrl = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_data(body)
                            .with_header(content_type)
                            .with_header(cache_ctrl),
                    );
                }
                (&Method::Post, "/config") => {
                    let mut body = String::new();
                    let _ = request.as_reader().read_to_string(&mut body);
                    if let Ok(new_cfg) = serde_json::from_str::<Config>(&body) {
                        if let Ok(mut app) = shared.lock() {
                            app.config = new_cfg.clamped();
                            app.save_config();
                        }
                    }
                    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_string("{\"ok\":true}").with_header(content_type),
                    );
                }
                (&Method::Get, "/settings") => {
                    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_string(SETTINGS_HTML).with_header(content_type),
                    );
                }
                _ => {
                    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_string(WIDGET_HTML).with_header(content_type),
                    );
                }
            }
        }
    });
}

fn open_url(url: &str) {
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

// ---------------------------------------------------------------------------
// Application Entry Point
// ---------------------------------------------------------------------------
fn main() {
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
            }
            return;
        }
    };

    let shared = Arc::new(Mutex::new(SharedApp::new()));
    let running = Arc::new(AtomicBool::new(true));

    spawn_smtc_poller(Arc::clone(&shared), Arc::clone(&running));
    spawn_http_server(Arc::clone(&shared), Arc::clone(&running), Arc::clone(&server));

    let initial_always_show = shared
        .lock()
        .map(|a| a.config.always_show)
        .unwrap_or(false);

    let menu = Menu::new();
    let always_show_item = CheckMenuItem::new(
        "Always Show (Disable Auto-Hide)",
        true,
        initial_always_show,
        None,
    );
    let settings_item = MenuItem::new("Widget Size & Style...", true, None);
    let copy_url_item = MenuItem::new("Copy OBS URL (http://127.0.0.1:8765/)", true, None);
    let preview_item = MenuItem::new("Open Preview in Browser", true, None);
    let quit_item = MenuItem::new("Exit", true, None);

    let _ = menu.append(&always_show_item);
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
    while running.load(Ordering::Relaxed) {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                open_url(&settings_url);
            }
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == always_show_item.id() {
                let checked = always_show_item.is_checked();
                if let Ok(mut app) = shared.lock() {
                    app.config.always_show = checked;
                    app.save_config();
                }
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
                server.unblock();
                break;
            }
        }

        // Keep tray checkmark synced if toggled via the web settings page
        if let Ok(app) = shared.lock() {
            if always_show_item.is_checked() != app.config.always_show {
                always_show_item.set_checked(app.config.always_show);
            }
        }

        thread::sleep(Duration::from_millis(16));
    }
}

// ---------------------------------------------------------------------------
// HTML Templates
// ---------------------------------------------------------------------------
const WIDGET_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>SMTC Now Playing Browser Source</title>
<style>
  :root {
    --w: 510px;
    --h: 122px;
    --radius: 10px;
    --accent-r: 255;
    --accent-g: 15;
    --accent-b: 135;
    --accent: rgb(var(--accent-r), var(--accent-g), var(--accent-b));
    --bg-alpha: 1.0;
  }

  * {
    box-sizing: border-box;
    margin: 0;
    padding: 0;
    user-select: none;
  }

  body {
    background: transparent;
    overflow: hidden;
    font-family: "Segoe UI", -apple-system, BlinkMacSystemFont, sans-serif;
  }

  #widget {
    position: relative;
    width: var(--w);
    height: var(--h);
    border-radius: var(--radius);
    background-color: rgba(18, 18, 21, var(--bg-alpha));
    border: 1px solid rgba(255, 255, 255, 0.07);
    display: flex;
    align-items: center;
    padding: 10px;
    gap: 18px;
    opacity: 0;
    transition: opacity 0.35s ease, width 0.2s ease, height 0.2s ease, border-radius 0.2s ease;
    overflow: hidden;
  }

  #widget.visible {
    opacity: 1;
  }

  #widget::before {
    content: "";
    position: absolute;
    top: 0; left: 0; right: 0;
    height: 40px;
    background: linear-gradient(
      to bottom,
      rgba(var(--accent-r), var(--accent-g), var(--accent-b), 0.16),
      rgba(0, 0, 0, 0)
    );
    pointer-events: none;
    transition: background 0.3s ease;
  }

  #art-box {
    height: calc(var(--h) - 20px);
    aspect-ratio: 1 / 1;
    border-radius: 6px;
    background-color: rgb(38, 38, 45);
    background-size: cover;
    background-position: center;
    flex-shrink: 0;
    z-index: 1;
  }

  .right-col {
    flex: 1;
    min-width: 0;
    height: 100%;
    padding-right: 8px;
    display: flex;
    flex-direction: column;
    justify-content: center;
    z-index: 1;
  }

  .scroll-mask {
    width: 100%;
    overflow: hidden;
    white-space: nowrap;
    position: relative;
  }

  #title {
    display: inline-block;
    color: #ffffff;
    font-size: 22px;
    font-weight: 600;
    white-space: nowrap;
    line-height: 1.25;
    will-change: transform;
  }

  #artist {
    display: inline-block;
    color: rgb(225, 225, 230);
    font-size: 16px;
    font-weight: 500;
    white-space: nowrap;
    margin-top: 2px;
    line-height: 1.25;
    will-change: transform;
  }

  .scrolling {
    animation: text-scroll-return var(--scroll-duration, 8s) ease-in-out infinite;
  }

  @keyframes text-scroll-return {
    0%, 20% { transform: translateX(0px); }
    70%, 85% { transform: translateX(var(--scroll-dist, 0px)); }
    100% { transform: translateX(0px); }
  }

  .time-row {
    display: flex;
    justify-content: space-between;
    color: rgb(220, 220, 225);
    font-size: 13px;
    font-weight: 600;
    margin-top: auto;
    margin-bottom: 5px;
  }

  .bar-track {
    width: 100%;
    height: 5.5px;
    background-color: rgb(65, 65, 72);
    border-radius: 999px;
    position: relative;
    margin-bottom: 4px;
  }

  #bar-fill {
    height: 100%;
    width: 0%;
    border-radius: 999px;
    background-color: var(--accent);
    box-shadow:
      0 0 5px rgba(var(--accent-r), var(--accent-g), var(--accent-b), 0.55),
      0 0 12px rgba(var(--accent-r), var(--accent-g), var(--accent-b), 0.30),
      0 0 18px rgba(var(--accent-r), var(--accent-g), var(--accent-b), 0.15);
    transition: background-color 0.25s ease, box-shadow 0.25s ease;
  }
</style>
</head>
<body>
  <div id="widget">
    <div id="art-box"></div>
    <div class="right-col">
      <div class="scroll-mask" id="title-mask">
        <div id="title">Waiting for Media...</div>
      </div>
      <div class="scroll-mask" id="artist-mask">
        <div id="artist">Play a track to display</div>
      </div>
      <div class="time-row">
        <span id="pos">00:00</span>
        <span id="dur">00:00</span>
      </div>
      <div class="bar-track">
        <div id="bar-fill"></div>
      </div>
    </div>
  </div>

<script>
  const params = new URLSearchParams(window.location.search);
  const root = document.documentElement;

  const widgetEl = document.getElementById("widget");
  const artBoxEl = document.getElementById("art-box");
  const titleEl = document.getElementById("title");
  const artistEl = document.getElementById("artist");
  const posEl = document.getElementById("pos");
  const durEl = document.getElementById("dur");
  const barFillEl = document.getElementById("bar-fill");

  function parseNumericParam(value, min, max) {
    if (value === null || value.trim() === "") return null;
    const num = Number.parseFloat(value);
    if (!Number.isFinite(num)) return null;
    return Math.min(max, Math.max(min, num));
  }

  const urlW = parseNumericParam(params.get("w") ?? params.get("width"), 260, 1920);
  const urlH = parseNumericParam(params.get("h") ?? params.get("height") ?? params.get("length"), 80, 600);
  const urlRadius = parseNumericParam(params.get("radius"), 0, 60);
  const urlOpacity = parseNumericParam(params.get("opacity") ?? params.get("bg_alpha"), 0.0, 1.0);
  const urlAlwaysShow = params.has("always_show")
    ? ["1", "true", "yes"].includes((params.get("always_show") || "").toLowerCase())
    : null;

  let state = {
    is_playing: false,
    position: 0,
    duration: 0,
    syncMono: performance.now()
  };

  let lastTitle = null;
  let lastArtist = null;
  let lastArt = null;
  let lastW = null;
  let lastH = null;
  let lastPosText = "";
  let lastDurText = "";

  function formatTime(sec) {
    sec = Math.max(0, Math.floor(sec));
    const hrs = Math.floor(sec / 3600);
    const mins = Math.floor((sec % 3600) / 60);
    const secs = sec % 60;
    if (hrs > 0) {
      return `${hrs}:${String(mins).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
    }
    return `${String(mins).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
  }

  function updateScrollAnimation(maskId, textEl) {
    const maskEl = document.getElementById(maskId);
    if (!maskEl || !textEl) return;

    textEl.classList.remove("scrolling");
    textEl.style.removeProperty("--scroll-dist");
    textEl.style.removeProperty("--scroll-duration");

    void textEl.offsetWidth;

    const overflowPx = textEl.scrollWidth - maskEl.clientWidth;
    if (overflowPx > 2) {
      const durationSec = Math.max(6.0, (overflowPx / 40.0) + 4.5);
      textEl.style.setProperty("--scroll-dist", `-${overflowPx}px`);
      textEl.style.setProperty("--scroll-duration", `${durationSec.toFixed(2)}s`);
      textEl.classList.add("scrolling");
    }
  }

  async function pollServer() {
    try {
      const res = await fetch("/state", { cache: "no-store" });
      const data = await res.json();
      const cfg = data.config || {};

      const finalW = urlW !== null ? urlW : (cfg.width || 510);
      const finalH = urlH !== null ? urlH : (cfg.height || 122);
      const finalRadius = urlRadius !== null ? urlRadius : (cfg.radius ?? 10);
      const finalOpacity = urlOpacity !== null ? urlOpacity : (cfg.opacity ?? 1.0);
      const finalAlwaysShow = urlAlwaysShow !== null ? urlAlwaysShow : Boolean(cfg.always_show);

      root.style.setProperty("--w", `${finalW}px`);
      root.style.setProperty("--h", `${finalH}px`);
      root.style.setProperty("--radius", `${finalRadius}px`);
      root.style.setProperty("--bg-alpha", finalOpacity);

      state.is_playing = Boolean(data.is_playing);
      state.position = data.position || 0;
      state.duration = data.duration || 0;
      state.syncMono = performance.now();

      const newTitle = data.title || "Waiting for Media...";
      const newArtist = data.artist || "";
      const sizeChanged = (finalW !== lastW) || (finalH !== lastH);

      if (newTitle !== lastTitle) {
        titleEl.textContent = newTitle;
        lastTitle = newTitle;
        updateScrollAnimation("title-mask", titleEl);
      } else if (sizeChanged) {
        setTimeout(() => updateScrollAnimation("title-mask", titleEl), 220);
      }

      if (newArtist !== lastArtist) {
        artistEl.textContent = newArtist;
        lastArtist = newArtist;
        updateScrollAnimation("artist-mask", artistEl);
      } else if (sizeChanged) {
        setTimeout(() => updateScrollAnimation("artist-mask", artistEl), 220);
      }

      lastW = finalW;
      lastH = finalH;

      const newArt = data.art || null;
      if (newArt !== lastArt) {
        artBoxEl.style.backgroundImage = newArt ? `url('${newArt}')` : "none";
        lastArt = newArt;
      }

      if (data.accent && data.accent.length === 3) {
        root.style.setProperty("--accent-r", data.accent[0]);
        root.style.setProperty("--accent-g", data.accent[1]);
        root.style.setProperty("--accent-b", data.accent[2]);
      }

      if (state.is_playing || finalAlwaysShow) {
        widgetEl.classList.add("visible");
      } else {
        widgetEl.classList.remove("visible");
      }
    } catch (e) {
      widgetEl.classList.remove("visible");
    }
  }

  function animateTimeline() {
    let curPos = state.position;
    if (state.is_playing) {
      const elapsed = (performance.now() - state.syncMono) / 1000.0;
      curPos += elapsed;
      if (state.duration > 0) curPos = Math.min(curPos, state.duration);
    }

    const posText = formatTime(curPos);
    if (posText !== lastPosText) {
      posEl.textContent = posText;
      lastPosText = posText;
    }

    const durText = formatTime(state.duration);
    if (durText !== lastDurText) {
      durEl.textContent = durText;
      lastDurText = durText;
    }

    const pct = state.duration > 0 ? Math.min(100, Math.max(0, (curPos / state.duration) * 100)) : 0;
    barFillEl.style.width = `${pct}%`;

    requestAnimationFrame(animateTimeline);
  }

  setInterval(pollServer, 250);
  pollServer();
  requestAnimationFrame(animateTimeline);
</script>
</body>
</html>
"#;

const SETTINGS_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>SMTC Now Playing — Settings</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    background: #121216;
    color: #f0f0f5;
    font-family: "Segoe UI", -apple-system, sans-serif;
    padding: 28px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 24px;
  }
  .card {
    background: #1b1b22;
    border: 1px solid rgba(255,255,255,0.08);
    border-radius: 12px;
    padding: 22px;
    width: 100%;
    max-width: 560px;
    box-shadow: 0 10px 30px rgba(0,0,0,0.35);
  }
  h2 { font-size: 18px; margin-bottom: 16px; color: #ff0f87; }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 14px;
    gap: 12px;
  }
  label { font-size: 14px; font-weight: 500; }
  input[type="number"] {
    background: #121216;
    color: #fff;
    border: 1px solid rgba(255,255,255,0.15);
    border-radius: 6px;
    padding: 6px 10px;
    width: 100px;
    font-size: 14px;
  }
  .btn-row { display: flex; gap: 10px; margin-top: 18px; }
  button {
    flex: 1;
    background: #2b2b36;
    color: #fff;
    border: 1px solid rgba(255,255,255,0.1);
    padding: 9px 14px;
    border-radius: 6px;
    cursor: pointer;
    font-weight: 600;
  }
  button.primary { background: #ff0f87; border-color: #ff0f87; }
  button:hover { filter: brightness(1.1); }
  iframe {
    border: 1px dashed rgba(255,255,255,0.15);
    border-radius: 12px;
    background: repeating-conic-gradient(#18181f 0% 25%, #202029 0% 50%) 50% / 20px 20px;
  }
</style>
</head>
<body>
  <div class="card">
    <h2>Browser Source Settings (Auto-Saved)</h2>
    <div class="row">
      <label for="w">Width (260 – 1920 px)</label>
      <input id="w" type="number" min="260" max="1920" step="10" value="510">
    </div>
    <div class="row">
      <label for="h">Height / Length (80 – 600 px)</label>
      <input id="h" type="number" min="80" max="600" step="4" value="122">
    </div>
    <div class="row">
      <label for="r">Corner Radius (0 – 60 px)</label>
      <input id="r" type="number" min="0" max="60" step="1" value="10">
    </div>
    <div class="row">
      <label for="o">Card Opacity (0.00 – 1.00)</label>
      <input id="o" type="number" min="0" max="1" step="0.05" value="1.0">
    </div>
    <div class="row">
      <label for="a">Always Show (Disable Auto-Hide)</label>
      <input id="a" type="checkbox">
    </div>
    <div class="btn-row">
      <button id="reset">Reset Defaults</button>
      <button id="copy" class="primary">Copy OBS URL</button>
    </div>
  </div>

  <iframe id="preview" src="/?always_show=1" width="600" height="180"></iframe>

<script>
  const wEl = document.getElementById("w");
  const hEl = document.getElementById("h");
  const rEl = document.getElementById("r");
  const oEl = document.getElementById("o");
  const aEl = document.getElementById("a");
  const preview = document.getElementById("preview");

  async function loadInitial() {
    const res = await fetch("/state", { cache: "no-store" });
    const data = await res.json();
    const c = data.config || {};
    wEl.value = c.width ?? 510;
    hEl.value = c.height ?? 122;
    rEl.value = c.radius ?? 10;
    oEl.value = c.opacity ?? 1.0;
    aEl.checked = Boolean(c.always_show);
    resizePreview();
  }

  function resizePreview() {
    preview.width = Math.max(540, Number(wEl.value) + 40);
    preview.height = Math.max(160, Number(hEl.value) + 40);
  }

  async function saveLive() {
    resizePreview();
    await fetch("/config", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        width: Number(wEl.value),
        height: Number(hEl.value),
        radius: Number(rEl.value),
        opacity: Number(oEl.value),
        always_show: aEl.checked
      })
    });
  }

  [wEl, hEl, rEl, oEl, aEl].forEach(el => el.addEventListener("input", saveLive));

  document.getElementById("reset").addEventListener("click", () => {
    wEl.value = 510;
    hEl.value = 122;
    rEl.value = 10;
    oEl.value = 1.0;
    saveLive();
  });

  document.getElementById("copy").addEventListener("click", async (e) => {
    await navigator.clipboard.writeText("http://127.0.0.1:8765/");
    e.target.textContent = "Copied!";
    setTimeout(() => e.target.textContent = "Copy OBS URL", 1500);
  });

  loadInitial();
</script>
</body>
</html>
"#;