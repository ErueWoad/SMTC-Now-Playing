#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::env;
use std::ffi::c_void;
use std::fs;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use image::{
    imageops::FilterType, DynamicImage, GenericImageView, ImageFormat, ImageReader, Limits,
};
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use windows::{
    core::{w, PCWSTR},
    Foundation::{EventRegistrationToken, TypedEventHandler},
    Media::Control::{
        GlobalSystemMediaTransportControlsSession as MediaSession,
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

// Native Win32 constants for Registry and ShellExecuteW
const HKEY_CURRENT_USER: isize = 0x80000001u32 as i32 as isize;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const REG_SZ: u32 = 1;
const SW_SHOWNORMAL: i32 = 1;
const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VAL_NAME: PCWSTR = w!("SMTC-Now-Playing");

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        hkey: isize,
        lpsubkey: *const u16,
        lpvalue: *const u16,
        dwflags: u32,
        pdwtype: *mut u32,
        pvdata: *mut c_void,
        pcbdata: *mut u32,
    ) -> i32;

    fn RegSetKeyValueW(
        hkey: isize,
        lpsubkey: *const u16,
        lpvaluename: *const u16,
        dwtype: u32,
        lpdata: *const c_void,
        cbdata: u32,
    ) -> i32;

    fn RegDeleteKeyValueW(
        hkey: isize,
        lpsubkey: *const u16,
        lpvaluename: *const u16,
    ) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        lpoperation: *const u16,
        lpfile: *const u16,
        lpparameters: *const u16,
        lpdirectory: *const u16,
        nshowcmd: i32,
    ) -> isize;
}

// ---------------------------------------------------------------------------
// Configuration & State Models
// ---------------------------------------------------------------------------
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
struct Config {
    width: u32,
    height: u32,
    radius: u32,
    opacity: f32,
    always_show: bool,
    display_mode: String, // "default" | "always" | "toast"
    toast_seconds: u32,
    theme: String,        // "default" | "vinyl" | "glass" | "compact"
    clean_titles: bool,
    filter_mode: String,  // "all" | "allow" | "block"
    app_list: String,     // comma-separated app IDs / keywords
    text_format: String,  // e.g. "{artist} — {title}"
    text_idle: String,    // e.g. "No track playing"
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 510,
            height: 122,
            radius: 10,
            opacity: 1.0,
            always_show: false,
            display_mode: "default".to_string(),
            toast_seconds: 8,
            theme: "default".to_string(),
            clean_titles: true,
            filter_mode: "all".to_string(),
            app_list: String::new(),
            text_format: "{artist} — {title}".to_string(),
            text_idle: "No track playing".to_string(),
        }
    }
}

impl Config {
    fn clamped(mut self) -> Self {
        self.width = self.width.clamp(260, 1920);
        self.height = self.height.clamp(80, 600);
        self.radius = self.radius.clamp(0, 60);
        self.opacity = (self.opacity.clamp(0.0, 1.0) * 100.0).round() / 100.0;
        self.toast_seconds = self.toast_seconds.clamp(3, 60);

        match self.display_mode.as_str() {
            "always" | "toast" | "default" => {}
            _ => {
                self.display_mode = if self.always_show {
                    "always"
                } else {
                    "default"
                }
                .to_string()
            }
        }
        self.always_show = self.display_mode == "always";

        match self.theme.as_str() {
            "default" | "vinyl" | "glass" | "compact" => {}
            _ => self.theme = "default".to_string(),
        }

        match self.filter_mode.as_str() {
            "all" | "allow" | "block" => {}
            _ => self.filter_mode = "all".to_string(),
        }

        if self.text_format.trim().is_empty() {
            self.text_format = "{artist} — {title}".to_string();
        }
        if self.text_idle.len() > 512 {
            self.text_idle.truncate(512);
        }
        if self.text_format.len() > 512 {
            self.text_format.truncate(512);
        }
        if self.app_list.len() > 1024 {
            self.app_list.truncate(1024);
        }
        self
    }
}

#[derive(Clone, Serialize, Debug)]
struct MediaState {
    is_playing: bool,
    title: String,
    artist: String,
    album: String,
    source_app: String,
    active_sessions: Vec<String>,
    position: f64,
    duration: f64,
    art_hash: u64,
    accent: [u8; 3],
    accent2: [u8; 3],
}

impl Default for MediaState {
    fn default() -> Self {
        Self {
            is_playing: false,
            title: "Waiting for Media...".to_string(),
            artist: "Play a track to display".to_string(),
            album: String::new(),
            source_app: String::new(),
            active_sessions: Vec::new(),
            position: 0.0,
            duration: 0.0,
            art_hash: 0,
            accent: [255, 15, 135],
            accent2: [0, 210, 255],
        }
    }
}

#[derive(Serialize)]
struct StatePayload<'a> {
    #[serde(flatten)]
    state: &'a MediaState,
    config: &'a Config,
    run_on_startup: bool,
}

struct SharedApp {
    state: MediaState,
    state_updated_at: Instant,
    config: Config,
    config_path: PathBuf,
    art_png: Option<Arc<[u8]>>,
    run_on_startup: bool,
}

impl SharedApp {
    fn new() -> Self {
        let config_path = get_config_path();
        let mut app = Self {
            state: MediaState::default(),
            state_updated_at: Instant::now(),
            config: Config::default(),
            config_path,
            art_png: None,
            run_on_startup: is_startup_enabled(),
        };
        app.load_config();
        app
    }

    fn live_state(&self) -> MediaState {
        let mut s = self.state.clone();
        if s.is_playing {
            s.position += self.state_updated_at.elapsed().as_secs_f64();
            if s.duration > 0.0 {
                s.position = s.position.clamp(0.0, s.duration);
            }
        }
        s
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
// Native Windows Startup Registry & Shell Helpers
// ---------------------------------------------------------------------------
fn is_startup_enabled() -> bool {
    let mut size: u32 = 0;
    unsafe {
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY.as_ptr(),
            RUN_VAL_NAME.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        status == 0 && size > 2
    }
}

fn set_startup_enabled(enable: bool) -> bool {
    if enable {
        let Ok(exe) = env::current_exe() else {
            return false;
        };
        let quoted = format!("\"{}\"", exe.display());
        let wide: Vec<u16> = quoted.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let status = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY.as_ptr(),
                RUN_VAL_NAME.as_ptr(),
                REG_SZ,
                wide.as_ptr().cast(),
                (wide.len() * std::mem::size_of::<u16>()) as u32,
            );
            status == 0
        }
    } else {
        unsafe {
            let _ = RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY.as_ptr(),
                RUN_VAL_NAME.as_ptr(),
            );
        }
        false
    }
}

fn open_url(url: &str) {
    let wide_url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            0,
            w!("open").as_ptr(),
            wide_url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

// ---------------------------------------------------------------------------
// Metadata Sanitizer & Plaintext Status Formatter
// ---------------------------------------------------------------------------
fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn ends_with_ascii_case_insensitive(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack.as_bytes()[haystack.len() - suffix.len()..]
            .eq_ignore_ascii_case(suffix.as_bytes())
}

fn sanitize_metadata(raw_title: &str, raw_artist: &str) -> (String, String) {
    let mut title = raw_title.trim().to_string();
    let mut artist = raw_artist.trim().to_string();

    if let Some(stripped) = artist.strip_suffix(" - Topic") {
        artist = stripped.trim().to_string();
    }

    let platform_suffixes = [
        " - YouTube Music",
        " - YouTube",
        " - Twitch",
        " | Spotify",
    ];
    for suf in platform_suffixes {
        if title.len() > suf.len() && ends_with_ascii_case_insensitive(&title, suf) {
            title.truncate(title.len() - suf.len());
            title = title.trim().to_string();
        }
    }

    let noise_tags = [
        "(official music video)",
        "[official music video]",
        "(official video)",
        "[official video]",
        "(official audio)",
        "[official audio]",
        "(official lyric video)",
        "[official lyric video]",
        "(lyric video)",
        "[lyric video]",
        "(lyrics)",
        "[lyrics]",
        "(official visualizer)",
        "[official visualizer]",
        "(visualizer)",
        "[visualizer]",
        "(audio)",
        "[audio]",
        "(hd)",
        "[hd]",
        "(4k)",
        "[4k]",
        "[monstercat release]",
    ];

    loop {
        let mut changed = false;
        for tag in noise_tags {
            if let Some(idx) = find_ascii_case_insensitive(&title, tag) {
                let before = title[..idx].trim_end();
                let after = title[idx + tag.len()..].trim_start();
                title = if before.is_empty() {
                    after.to_string()
                } else if after.is_empty() {
                    before.to_string()
                } else {
                    format!("{before} {after}")
                };
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }

    if title.is_empty() {
        title = raw_title.trim().to_string();
    }
    (title, artist)
}

fn format_clock(sec: f64) -> String {
    let total = sec.max(0.0).floor() as u64;
    let hrs = total / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    if hrs > 0 {
        format!("{hrs}:{mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

fn format_status_text(state: &MediaState, cfg: &Config, custom_fmt: Option<&str>) -> String {
    if !state.is_playing && !cfg.always_show {
        return cfg.text_idle.clone();
    }
    let template = custom_fmt.unwrap_or(&cfg.text_format);
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find('}') {
            let key = &after_open[..close];
            match key {
                "title" => out.push_str(&state.title),
                "artist" => out.push_str(&state.artist),
                "album" => out.push_str(&state.album),
                "app" => out.push_str(&state.source_app),
                "pos" => out.push_str(&format_clock(state.position)),
                "dur" => out.push_str(&format_clock(state.duration)),
                "status" => out.push_str(if state.is_playing { "Playing" } else { "Paused" }),
                _ => {
                    out.push('{');
                    out.push_str(key);
                    out.push('}');
                }
            }
            rest = &after_open[close + 1..];
        } else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
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

fn extract_vibrant_palette(img: &DynamicImage) -> ([u8; 3], [u8; 3]) {
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

        if v < 0.20 || s < 0.25 || (v > 0.95 && s < 0.40) {
            continue;
        }

        let weight = s.powf(2.2) * v.powf(1.2);
        let idx = ((h * 36.0) as usize) % 36;
        bins[idx].weight += weight;
        bins[idx].r += rf * weight;
        bins[idx].g += gf * weight;
        bins[idx].b += bf * weight;
    }

    let (best_idx, best) = bins
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.weight.partial_cmp(&b.1.weight).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0, Bin::default()));

    if best.weight <= 0.0 {
        return ([235, 235, 245], [160, 170, 255]);
    }

    let bin_to_rgb = |b: Bin| -> ([u8; 3], f32, f32, f32) {
        let avg_r = b.r / b.weight;
        let avg_g = b.g / b.weight;
        let avg_b = b.b / b.weight;
        let (h, s, v) = rgb_to_hsv(avg_r, avg_g, avg_b);
        let (ro, go, bo) = hsv_to_rgb(h, s.max(0.80), v.max(0.92));
        (
            [(ro * 255.0) as u8, (go * 255.0) as u8, (bo * 255.0) as u8],
            h,
            s,
            v,
        )
    };

    let (primary_rgb, h1, s1, v1) = bin_to_rgb(best);

    // Find secondary hue bin at least 4 bins (40 deg) away on the hue wheel
    let second = bins
        .iter()
        .copied()
        .enumerate()
        .filter(|(idx, b)| {
            let diff = (*idx as i32 - best_idx as i32).abs();
            let circular_dist = diff.min(36 - diff);
            circular_dist >= 4 && b.weight >= best.weight * 0.08
        })
        .max_by(|a, b| a.1.weight.partial_cmp(&b.1.weight).unwrap_or(std::cmp::Ordering::Equal));

    let secondary_rgb = if let Some((_, sec_bin)) = second {
        bin_to_rgb(sec_bin).0
    } else {
        let h2 = (h1 + 0.11).fract();
        let (r2, g2, b2) = hsv_to_rgb(h2, s1.max(0.78), v1.max(0.92));
        [(r2 * 255.0) as u8, (g2 * 255.0) as u8, (b2 * 255.0) as u8]
    };

    (primary_rgb, secondary_rgb)
}

fn process_artwork(img: &DynamicImage) -> ([u8; 3], [u8; 3], Option<(Arc<[u8]>, u64)>) {
    let (w, h) = img.dimensions();
    let resized: Cow<DynamicImage> = if w > 256 || h > 256 {
        Cow::Owned(img.resize(256, 256, FilterType::Lanczos3))
    } else {
        Cow::Borrowed(img)
    };

    // Extract vibrant palette from the already-downsampled <= 256x256 image
    let (a1, a2) = extract_vibrant_palette(&resized);

    let mut buf = Cursor::new(Vec::with_capacity(32 * 1024));
    if resized.write_to(&mut buf, ImageFormat::Png).is_err() {
        return (a1, a2, None);
    }
    let bytes = buf.into_inner();

    // 64-bit FNV-1a hash for fast cache-busting URL token
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in &bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    if hash == 0 {
        hash = 1;
    }

    (a1, a2, Some((Arc::from(bytes), hash)))
}

fn create_tray_icon() -> Icon {
    let size = 64u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;

            let in_rect = if (6.0..=58.0).contains(&fx) && (6.0..=58.0).contains(&fy) {
                let cx = fx.clamp(20.0, 44.0);
                let cy = fy.clamp(20.0, 44.0);
                ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= 14.0
            } else {
                false
            };

            if !in_rect {
                continue;
            }

            let in_triangle = (24.0..=46.0).contains(&fx)
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
// SMTC Session Filtering & Event-Driven Poller Thread
// ---------------------------------------------------------------------------
struct HookedManager {
    mgr: MediaManager,
    cur_token: Option<EventRegistrationToken>,
    ses_token: Option<EventRegistrationToken>,
}

impl HookedManager {
    fn new(mgr: MediaManager, poller_thread: &Thread) -> Self {
        let t_cur = poller_thread.clone();
        let cur_token = mgr
            .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                t_cur.unpark();
                Ok(())
            }))
            .ok();

        let t_ses = poller_thread.clone();
        let ses_token = mgr
            .SessionsChanged(&TypedEventHandler::new(move |_, _| {
                t_ses.unpark();
                Ok(())
            }))
            .ok();

        Self {
            mgr,
            cur_token,
            ses_token,
        }
    }
}

impl Drop for HookedManager {
    fn drop(&mut self) {
        if let Some(tok) = self.cur_token {
            let _ = self.mgr.RemoveCurrentSessionChanged(tok);
        }
        if let Some(tok) = self.ses_token {
            let _ = self.mgr.RemoveSessionsChanged(tok);
        }
    }
}

struct HookedSession {
    session: MediaSession,
    prop_token: Option<EventRegistrationToken>,
    play_token: Option<EventRegistrationToken>,
    time_token: Option<EventRegistrationToken>,
}

impl HookedSession {
    fn new(session: MediaSession, poller_thread: &Thread) -> Self {
        let t1 = poller_thread.clone();
        let prop_token = session
            .MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                t1.unpark();
                Ok(())
            }))
            .ok();

        let t2 = poller_thread.clone();
        let play_token = session
            .PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                t2.unpark();
                Ok(())
            }))
            .ok();

        let t3 = poller_thread.clone();
        let time_token = session
            .TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                t3.unpark();
                Ok(())
            }))
            .ok();

        Self {
            session,
            prop_token,
            play_token,
            time_token,
        }
    }
}

impl Drop for HookedSession {
    fn drop(&mut self) {
        if let Some(tok) = self.prop_token {
            let _ = self.session.RemoveMediaPropertiesChanged(tok);
        }
        if let Some(tok) = self.play_token {
            let _ = self.session.RemovePlaybackInfoChanged(tok);
        }
        if let Some(tok) = self.time_token {
            let _ = self.session.RemoveTimelinePropertiesChanged(tok);
        }
    }
}

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

        let mut img_reader = ImageReader::new(Cursor::new(&buf))
            .with_guessed_format()
            .ok()?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024); // Cap decoded pixel allocation at 64 MB
        img_reader.limits(limits);
        img_reader.decode().ok()
    })();
    let _ = stream.Close();
    result
}

fn normalize_app_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((_, tail)) = trimmed.rsplit_once(['\\', '/']) {
        tail.to_string()
    } else {
        trimmed.to_string()
    }
}

fn app_matches_filter(app_id: &str, filter_mode: &str, app_list: &str) -> bool {
    if filter_mode == "all" || app_list.trim().is_empty() {
        return true;
    }
    let lower_app = app_id.to_lowercase();
    let matched = app_list
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .any(|token| lower_app.contains(&token));

    match filter_mode {
        "allow" => matched,
        "block" => !matched,
        _ => true,
    }
}

fn select_session(
    mgr: &MediaManager,
    filter_mode: &str,
    app_list: &str,
) -> (Option<MediaSession>, Vec<String>) {
    let mut discovered_apps = Vec::new();
    let mut allowed_sessions = Vec::new();

    if let Ok(list) = mgr.GetSessions() {
        let count = list.Size().unwrap_or(0);
        for i in 0..count {
            if let Ok(s) = list.GetAt(i) {
                let app_id = s
                    .SourceAppUserModelId()
                    .map(|h| normalize_app_id(&h.to_string()))
                    .unwrap_or_else(|_| "Unknown".to_string());
                if !discovered_apps.contains(&app_id) {
                    discovered_apps.push(app_id.clone());
                }
                if app_matches_filter(&app_id, filter_mode, app_list) {
                    allowed_sessions.push(s);
                }
            }
        }
    }

    // Prefer a filtered session that is actively Playing
    for s in &allowed_sessions {
        let playing = s
            .GetPlaybackInfo()
            .and_then(|i| i.PlaybackStatus())
            .map(|st| st == PlaybackStatus::Playing)
            .unwrap_or(false);
        if playing {
            return (Some(s.clone()), discovered_apps);
        }
    }

    // Otherwise check Windows default current session if it passes the filter
    if let Ok(cur) = mgr.GetCurrentSession() {
        let app_id = cur
            .SourceAppUserModelId()
            .map(|h| normalize_app_id(&h.to_string()))
            .unwrap_or_else(|_| "Unknown".to_string());
        if !discovered_apps.contains(&app_id) {
            discovered_apps.push(app_id.clone());
        }
        if app_matches_filter(&app_id, filter_mode, app_list) {
            return (Some(cur), discovered_apps);
        }
    }

    (allowed_sessions.into_iter().next(), discovered_apps)
}

fn spawn_smtc_poller(shared: Arc<Mutex<SharedApp>>, running: Arc<AtomicBool>) -> Thread {
    let (thread_tx, thread_rx) = mpsc::channel::<Thread>();

    thread::spawn(move || {
        let my_thread = thread::current();
        let _ = thread_tx.send(my_thread.clone());

        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let mut hooked_mgr: Option<HookedManager> = None;
        let mut hooked_ses: Option<HookedSession> = None;
        let mut last_raw_track_key = String::new();
        let mut thumb_retries = 0u32;
        let mut cached_art_hash: u64 = 0;
        let mut cached_art_bytes: Option<Arc<[u8]>> = None;
        let mut cached_accent: [u8; 3] = [255, 15, 135];
        let mut cached_accent2: [u8; 3] = [0, 210, 255];
        let mut local_anchor_time = Instant::now();
        let mut local_anchor_pos: f64 = 0.0;
        let mut was_playing = false;

        while running.load(Ordering::Relaxed) {
            let (filter_mode, app_list, clean_titles) = if let Ok(app) = shared.lock() {
                (
                    app.config.filter_mode.clone(),
                    app.config.app_list.clone(),
                    app.config.clean_titles,
                )
            } else {
                ("all".to_string(), String::new(), true)
            };

            let step = (|| -> windows::core::Result<bool> {
                if hooked_mgr.is_none() {
                    let mgr = MediaManager::RequestAsync()?.get()?;
                    hooked_mgr = Some(HookedManager::new(mgr, &my_thread));
                }
                let mgr = &hooked_mgr.as_ref().unwrap().mgr;

                let (session_opt, active_sessions) =
                    select_session(mgr, &filter_mode, &app_list);

                let Some(session) = session_opt else {
                    hooked_ses = None;
                    was_playing = false;
                    if let Ok(mut app) = shared.lock() {
                        app.state.is_playing = false;
                        app.state.active_sessions = active_sessions;
                        app.state_updated_at = Instant::now();
                    }
                    return Ok(false);
                };

                if hooked_ses.as_ref().map(|h| &h.session) != Some(&session) {
                    hooked_ses = Some(HookedSession::new(session.clone(), &my_thread));
                }

                let source_app = session
                    .SourceAppUserModelId()
                    .map(|h| normalize_app_id(&h.to_string()))
                    .unwrap_or_else(|_| "Unknown".to_string());

                let is_playing = session
                    .GetPlaybackInfo()
                    .and_then(|info| info.PlaybackStatus())
                    .map(|status| status == PlaybackStatus::Playing)
                    .unwrap_or(false);

                let props = session.TryGetMediaPropertiesAsync()?.get()?;
                let raw_title = props
                    .Title()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let raw_artist = props
                    .Artist()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let album = props
                    .AlbumTitle()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
                    .trim()
                    .to_string();

                if raw_title.is_empty() {
                    was_playing = false;
                    if let Ok(mut app) = shared.lock() {
                        app.state.is_playing = false;
                        app.state.active_sessions = active_sessions;
                        app.state_updated_at = Instant::now();
                    }
                    return Ok(false);
                }

                let (title, artist) = if clean_titles {
                    sanitize_metadata(&raw_title, &raw_artist)
                } else {
                    (raw_title.clone(), raw_artist.clone())
                };

                let raw_track_key = format!("{source_app}||{raw_title}||{raw_artist}");
                let is_new_track = raw_track_key != last_raw_track_key;
                let thumb_ref = props.Thumbnail().ok();
                let should_retry_art = !is_new_track
                    && cached_art_hash == 0
                    && thumb_retries < MAX_THUMBNAIL_RETRIES
                    && thumb_ref.is_some();

                if is_new_track {
                    last_raw_track_key = raw_track_key;
                    thumb_retries = 0;
                    cached_accent = [255, 15, 135];
                    cached_accent2 = [0, 210, 255];
                    cached_art_hash = 0;
                    cached_art_bytes = None;
                    local_anchor_pos = 0.0;
                    local_anchor_time = Instant::now();
                    if let Ok(mut app) = shared.lock() {
                        app.art_png = None;
                    }
                }

                if is_new_track || should_retry_art {
                    if let Some(ref tref) = thumb_ref {
                        if let Some(img) = read_thumbnail(tref) {
                            let (a1, a2, encoded) = process_artwork(&img);
                            cached_accent = a1;
                            cached_accent2 = a2;
                            if let Some((png_bytes, hash)) = encoded {
                                cached_art_hash = hash;
                                cached_art_bytes = Some(png_bytes);
                            }
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
                        .map(|dt| {
                            (dt.UniversalTime as f64 / 10_000_000.0) - WINDOWS_TO_UNIX_EPOCH_SECS
                        })
                        .unwrap_or(0.0);

                    let now_secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);

                    let age = now_secs - last_updated_secs;
                    let smtc_pos = if (0.0..36000.0).contains(&age) {
                        raw_pos + if is_playing { age } else { 0.0 }
                    } else {
                        raw_pos
                    };

                    let predicted_pos = if is_playing && was_playing {
                        local_anchor_pos + local_anchor_time.elapsed().as_secs_f64()
                    } else {
                        local_anchor_pos
                    };

                    // Only snap anchor on new track, play/pause toggle, or user seek (> 1.5s)
                    if is_new_track
                        || (is_playing != was_playing)
                        || (smtc_pos - predicted_pos).abs() > 1.5
                    {
                        local_anchor_pos = smtc_pos;
                        local_anchor_time = Instant::now();
                        position = smtc_pos;
                    } else if is_playing {
                        // Gentle forward-only drift correction so time never steps backward
                        let corrected = predicted_pos + (smtc_pos - predicted_pos) * 0.10;
                        position = corrected.max(predicted_pos);
                        local_anchor_pos = position;
                        local_anchor_time = Instant::now();
                    } else {
                        position = smtc_pos;
                        local_anchor_pos = position;
                        local_anchor_time = Instant::now();
                    }

                    if duration > 0.0 {
                        position = position.clamp(0.0, duration);
                    }
                }
                was_playing = is_playing;

                if let Ok(mut app) = shared.lock() {
                    app.art_png = cached_art_bytes.clone();
                    app.state_updated_at = Instant::now();
                    app.state = MediaState {
                        is_playing,
                        title,
                        artist,
                        album,
                        source_app,
                        active_sessions,
                        position,
                        duration,
                        art_hash: cached_art_hash,
                        accent: cached_accent,
                        accent2: cached_accent2,
                    };
                }

                Ok(true)
            })();

            match step {
                Ok(true) => {
                    // Fast retry if waiting on delayed thumbnail; otherwise 1s since events unpark immediately
                    let wait_ms = if cached_art_hash == 0 && thumb_retries < MAX_THUMBNAIL_RETRIES {
                        250
                    } else {
                        1000
                    };
                    thread::park_timeout(Duration::from_millis(wait_ms));
                }
                Ok(false) => thread::park_timeout(Duration::from_millis(1000)),
                Err(_) => {
                    hooked_ses = None;
                    hooked_mgr = None;
                    was_playing = false;
                    if let Ok(mut app) = shared.lock() {
                        app.state.is_playing = false;
                        app.state_updated_at = Instant::now();
                    }
                    thread::park_timeout(Duration::from_millis(1000));
                }
            }
        }

        drop(hooked_ses);
        drop(hooked_mgr);
        unsafe {
            CoUninitialize();
        }
    });

    thread_rx.recv().expect("Failed to start SMTC poller thread")
}

// ---------------------------------------------------------------------------
// HTTP Server Thread ('/', '/state', '/art', '/text', '/startup', '/settings')
// ---------------------------------------------------------------------------
fn url_decode_param(raw: &str) -> String {
    let mut bytes = Vec::with_capacity(raw.len());
    let src = raw.as_bytes();
    let mut i = 0;
    while i < src.len() {
        if src[i] == b'+' {
            bytes.push(b' ');
            i += 1;
        } else if src[i] == b'%' && i + 2 < src.len() {
            if let Ok(hex) = std::str::from_utf8(&src[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    bytes.push(val);
                    i += 3;
                    continue;
                }
            }
            bytes.push(src[i]);
            i += 1;
        } else {
            bytes.push(src[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn parse_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode_param(v));
            }
        }
    }
    None
}

fn is_valid_local_post(request: &tiny_http::Request) -> bool {
    let has_json_ct = request.headers().iter().any(|h| {
        h.field.equiv("Content-Type")
            && h.value
                .as_str()
                .to_ascii_lowercase()
                .starts_with("application/json")
    });
    if !has_json_ct {
        return false;
    }

    for h in request.headers() {
        if h.field.equiv("Origin") {
            let origin = h.value.as_str();
            if origin != "http://127.0.0.1:8765" && origin != "http://localhost:8765" {
                return false;
            }
        } else if h.field.equiv("Host") {
            let host = h.value.as_str();
            if host != "127.0.0.1:8765" && host != "localhost:8765" {
                return false;
            }
        }
    }
    true
}

fn read_bounded_body(request: &mut tiny_http::Request, max_bytes: u64) -> Option<String> {
    let mut body = String::new();
    request
        .as_reader()
        .take(max_bytes)
        .read_to_string(&mut body)
        .ok()?;
    Some(body)
}

fn spawn_http_server(
    shared: Arc<Mutex<SharedApp>>,
    running: Arc<AtomicBool>,
    ui_dirty: Arc<AtomicBool>,
    server: Arc<Server>,
    poller_thread: Thread,
) {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let Ok(mut request) = server.recv() else {
                break;
            };
            if !running.load(Ordering::Relaxed) {
                break;
            }

            let full_url = request.url().to_string();
            let (url_path, query_str) = full_url.split_once('?').unwrap_or((&full_url, ""));

            let cors_hdr =
                Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap();

            match (request.method(), url_path) {
                (&Method::Get, "/state") => {
                    let body = if let Ok(app) = shared.lock() {
                        let live = app.live_state();
                        serde_json::to_vec(&StatePayload {
                            state: &live,
                            config: &app.config,
                            run_on_startup: app.run_on_startup,
                        })
                        .unwrap_or_default()
                    } else {
                        Vec::new()
                    };

                    let content_type =
                        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                    let cache_ctrl =
                        Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_data(body)
                            .with_header(content_type)
                            .with_header(cache_ctrl)
                            .with_header(cors_hdr),
                    );
                }
                (&Method::Get, "/art") => {
                    let png_opt = shared.lock().ok().and_then(|app| app.art_png.clone());
                    if let Some(png_bytes) = png_opt {
                        let content_type =
                            Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).unwrap();
                        let cache_ctrl = Header::from_bytes(
                            &b"Cache-Control"[..],
                            &b"public, max-age=86400, immutable"[..],
                        )
                        .unwrap();
                        let len = png_bytes.len();
                        let _ = request.respond(Response::new(
                            StatusCode(200),
                            vec![content_type, cache_ctrl, cors_hdr],
                            Cursor::new(png_bytes),
                            Some(len),
                            None,
                        ));
                    } else {
                        let _ = request
                            .respond(Response::from_string("No art").with_status_code(404));
                    }
                }
                (&Method::Get, "/text") => {
                    let custom_fmt = parse_query_param(query_str, "format");
                    let text_out = if let Ok(app) = shared.lock() {
                        let live = app.live_state();
                        format_status_text(&live, &app.config, custom_fmt.as_deref())
                    } else {
                        String::new()
                    };

                    let content_type = Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/plain; charset=utf-8"[..],
                    )
                    .unwrap();
                    let cache_ctrl =
                        Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_string(text_out)
                            .with_header(content_type)
                            .with_header(cache_ctrl)
                            .with_header(cors_hdr),
                    );
                }
                (&Method::Post, "/startup") => {
                    if !is_valid_local_post(&request) {
                        let _ = request
                            .respond(Response::from_string("Forbidden").with_status_code(403));
                        continue;
                    }
                    #[derive(Deserialize)]
                    struct StartupPayload {
                        enabled: bool,
                    }
                    if let Some(body) = read_bounded_body(&mut request, 1024) {
                        if let Ok(payload) = serde_json::from_str::<StartupPayload>(&body) {
                            let actual = set_startup_enabled(payload.enabled);
                            if let Ok(mut app) = shared.lock() {
                                app.run_on_startup = actual;
                            }
                            ui_dirty.store(true, Ordering::Release);
                        }
                    }
                    let content_type =
                        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_string("{\"ok\":true}").with_header(content_type),
                    );
                }
                (&Method::Post, "/config") => {
                    if !is_valid_local_post(&request) {
                        let _ = request
                            .respond(Response::from_string("Forbidden").with_status_code(403));
                        continue;
                    }
                    if let Some(body) = read_bounded_body(&mut request, 8192) {
                        if let Ok(new_cfg) = serde_json::from_str::<Config>(&body) {
                            if let Ok(mut app) = shared.lock() {
                                app.config = new_cfg.clamped();
                                app.save_config();
                            }
                            ui_dirty.store(true, Ordering::Release);
                            poller_thread.unpark();
                        }
                    }
                    let content_type =
                        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_string("{\"ok\":true}").with_header(content_type),
                    );
                }
                (&Method::Get, "/settings") => {
                    let content_type =
                        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                            .unwrap();
                    let _ = request.respond(
                        Response::from_string(SETTINGS_HTML).with_header(content_type),
                    );
                }
                _ => {
                    let content_type =
                        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                            .unwrap();
                    let _ = request
                        .respond(Response::from_string(WIDGET_HTML).with_header(content_type));
                }
            }
        }
    });
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
    let ui_dirty = Arc::new(AtomicBool::new(false));

    let poller_thread = spawn_smtc_poller(Arc::clone(&shared), Arc::clone(&running));
    spawn_http_server(
        Arc::clone(&shared),
        Arc::clone(&running),
        Arc::clone(&ui_dirty),
        Arc::clone(&server),
        poller_thread.clone(),
    );

    let (initial_always_show, initial_startup) = shared
        .lock()
        .map(|a| (a.config.always_show, a.run_on_startup))
        .unwrap_or((false, false));

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
    while running.load(Ordering::Relaxed) {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
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
                if let Ok(mut app) = shared.lock() {
                    app.config.always_show = checked;
                    app.config.display_mode =
                        if checked { "always" } else { "default" }.to_string();
                    app.save_config();
                }
            } else if event.id == startup_item.id() {
                let checked = startup_item.is_checked();
                let actual = set_startup_enabled(checked);
                startup_item.set_checked(actual);
                if let Ok(mut app) = shared.lock() {
                    app.run_on_startup = actual;
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
                poller_thread.unpark();
                server.unblock();
                break;
            }
        }

        // Only lock SharedApp to sync tray checkmarks when modified via HTTP dashboard
        if ui_dirty.swap(false, Ordering::Acquire) {
            if let Ok(app) = shared.lock() {
                if always_show_item.is_checked() != app.config.always_show {
                    always_show_item.set_checked(app.config.always_show);
                }
                if startup_item.is_checked() != app.run_on_startup {
                    startup_item.set_checked(app.run_on_startup);
                }
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
    --accent2-r: 0;
    --accent2-g: 210;
    --accent2-b: 255;
    --accent: rgb(var(--accent-r), var(--accent-g), var(--accent-b));
    --accent2: rgb(var(--accent2-r), var(--accent2-g), var(--accent2-b));
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
    border: 1px solid rgba(255, 255, 255, 0.08);
    display: flex;
    align-items: center;
    padding: 10px;
    gap: 16px;
    opacity: 0;
    transform: translateY(6px);
    transition: opacity 0.38s cubic-bezier(0.22, 1, 0.36, 1),
                transform 0.38s cubic-bezier(0.22, 1, 0.36, 1),
                width 0.2s ease, height 0.2s ease, border-radius 0.2s ease;
    overflow: hidden;
  }

  #widget.visible {
    opacity: 1;
    transform: translateY(0px);
  }

  #glass-bg {
    position: absolute;
    inset: -24px;
    background-size: cover;
    background-position: center;
    filter: blur(26px) saturate(1.6) brightness(0.45);
    opacity: 0;
    transition: opacity 0.4s ease, background-image 0.4s ease;
    pointer-events: none;
    z-index: 0;
  }

  #widget.theme-glass #glass-bg {
    opacity: calc(var(--bg-alpha) * 0.92);
  }

  #widget::before {
    content: "";
    position: absolute;
    top: 0; left: 0; right: 0;
    height: 52px;
    background: linear-gradient(
      90deg,
      rgba(var(--accent-r), var(--accent-g), var(--accent-b), 0.22),
      rgba(var(--accent2-r), var(--accent2-g), var(--accent2-b), 0.16)
    );
    -webkit-mask-image: linear-gradient(to bottom, rgba(0, 0, 0, 1) 0%, rgba(0, 0, 0, 0) 100%);
    mask-image: linear-gradient(to bottom, rgba(0, 0, 0, 1) 0%, rgba(0, 0, 0, 0) 100%);
    pointer-events: none;
    transition: background 0.3s ease;
    z-index: 0;
  }

  #art-box {
    position: relative;
    height: calc(var(--h) - 20px);
    aspect-ratio: 1 / 1;
    border-radius: 6px;
    background-color: rgb(38, 38, 45);
    background-size: cover;
    background-position: center;
    flex-shrink: 0;
    z-index: 1;
    box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35);
    transition: border-radius 0.3s ease;
  }

  /* Vinyl Theme */
  @keyframes vinyl-spin {
    from { transform: rotate(0deg); }
    to { transform: rotate(360deg); }
  }

  #widget.theme-vinyl #art-box {
    border-radius: 50%;
    border: 3px solid rgba(20, 20, 24, 0.95);
    box-shadow: 0 0 0 2px rgba(255, 255, 255, 0.10), 0 6px 16px rgba(0, 0, 0, 0.45);
    animation: vinyl-spin 9s linear infinite;
    animation-play-state: paused;
  }

  #widget.theme-vinyl.is-playing #art-box {
    animation-play-state: running;
  }

  #widget.theme-vinyl #art-box::after {
    content: "";
    position: absolute;
    top: 50%; left: 50%;
    width: 22%; height: 22%;
    transform: translate(-50%, -50%);
    border-radius: 50%;
    background: #121215;
    border: 2px solid rgba(255, 255, 255, 0.25);
    box-shadow: inset 0 1px 3px rgba(0,0,0,0.8);
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
    background-color: rgba(255, 255, 255, 0.14);
    border-radius: 999px;
    position: relative;
    margin-bottom: 4px;
    overflow: hidden;
  }

  #bar-fill {
    height: 100%;
    width: 100%;
    border-radius: 999px;
    transform-origin: left center;
    transform: scaleX(0);
    will-change: transform;
    background: linear-gradient(90deg, var(--accent), var(--accent2));
    box-shadow:
      0 0 6px rgba(var(--accent-r), var(--accent-g), var(--accent-b), 0.55),
      0 0 14px rgba(var(--accent2-r), var(--accent2-g), var(--accent2-b), 0.35);
    transition: background 0.25s ease, box-shadow 0.25s ease;
  }

  /* Compact Theme */
  #widget.theme-compact {
    padding: 8px 14px 12px 10px;
    gap: 12px;
  }
  #widget.theme-compact #art-box {
    height: calc(var(--h) - 20px);
    border-radius: 6px;
  }
  #widget.theme-compact .time-row {
    display: none;
  }
  #widget.theme-compact .bar-track {
    position: absolute;
    left: 0; right: 0; bottom: 0;
    height: 4px;
    margin: 0;
    border-radius: 0;
  }
</style>
</head>
<body>
  <div id="widget">
    <div id="glass-bg"></div>
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
  const glassBgEl = document.getElementById("glass-bg");
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
  const urlTheme = params.get("theme");
  const urlMode = params.get("mode");
  const urlToastSec = parseNumericParam(params.get("toast"), 3, 60);
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
  let lastArtHash = -1;
  let lastW = null;
  let lastH = null;
  let lastRadius = null;
  let lastOpacity = null;
  let lastTheme = null;
  let lastAccentKey = "";
  let lastPosText = "";
  let lastDurText = "";
  let lastScaleStr = "";
  let toastUntilMono = 0;
  let wasPlaying = false;
  let isWidgetVisible = false;
  let rafId = null;

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

  function renderTimelineFrame() {
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

    const ratio = state.duration > 0 ? Math.min(1, Math.max(0, curPos / state.duration)) : 0;
    const scaleStr = ratio.toFixed(4);
    if (scaleStr !== lastScaleStr) {
      barFillEl.style.transform = `scaleX(${scaleStr})`;
      lastScaleStr = scaleStr;
    }
  }

  function animateTimeline() {
    rafId = null;
    renderTimelineFrame();
    if (state.is_playing && isWidgetVisible) {
      rafId = requestAnimationFrame(animateTimeline);
    }
  }

  function ensureAnimationLoop() {
    renderTimelineFrame();
    if (state.is_playing && isWidgetVisible && rafId === null) {
      rafId = requestAnimationFrame(animateTimeline);
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
      const finalTheme = urlTheme || cfg.theme || "default";
      const finalToastSec = urlToastSec !== null ? urlToastSec : (cfg.toast_seconds || 8);

      let finalMode = urlMode || cfg.display_mode || "default";
      if (urlAlwaysShow === true || cfg.always_show) finalMode = "always";
      else if (urlAlwaysShow === false && finalMode === "always") finalMode = "default";

      const sizeChanged = (finalW !== lastW) || (finalH !== lastH);
      if (finalW !== lastW) {
        root.style.setProperty("--w", `${finalW}px`);
        lastW = finalW;
      }
      if (finalH !== lastH) {
        root.style.setProperty("--h", `${finalH}px`);
        lastH = finalH;
      }
      if (finalRadius !== lastRadius) {
        root.style.setProperty("--radius", `${finalRadius}px`);
        lastRadius = finalRadius;
      }
      if (finalOpacity !== lastOpacity) {
        root.style.setProperty("--bg-alpha", finalOpacity);
        lastOpacity = finalOpacity;
      }
      if (finalTheme !== lastTheme) {
        widgetEl.classList.remove("theme-default", "theme-vinyl", "theme-glass", "theme-compact");
        widgetEl.classList.add(`theme-${finalTheme}`);
        lastTheme = finalTheme;
      }

      const nowMono = performance.now();
      const incomingPlaying = Boolean(data.is_playing);
      const incomingPos = data.position || 0;
      const incomingDur = data.duration || 0;

      const newTitle = data.title || "Waiting for Media...";
      const newArtist = data.artist || "";
      const trackChanged = (newTitle !== lastTitle) || (newArtist !== lastArtist);

      const curInterp = state.is_playing
        ? state.position + (nowMono - state.syncMono) / 1000.0
        : state.position;
      const drift = incomingPos - curInterp;

      if (
        trackChanged ||
        incomingPlaying !== state.is_playing ||
        Math.abs(drift) > 1.5 ||
        (incomingPos < 1.0 && curInterp > 2.0)
      ) {
        state.position = incomingPos;
        state.syncMono = nowMono;
      } else if (incomingPlaying) {
        state.position = Math.max(curInterp, curInterp + drift * 0.15);
        state.syncMono = nowMono;
      } else {
        state.position = incomingPos;
        state.syncMono = nowMono;
      }

      state.is_playing = incomingPlaying;
      state.duration = incomingDur;
      widgetEl.classList.toggle("is-playing", state.is_playing);

      if (trackChanged || (state.is_playing && !wasPlaying)) {
        toastUntilMono = nowMono + (finalToastSec * 1000);
      }
      wasPlaying = state.is_playing;

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

      const artHash = data.art_hash || 0;
      if (artHash !== lastArtHash) {
        const bgVal = artHash > 0 ? `url('/art?v=${artHash}')` : "none";
        artBoxEl.style.backgroundImage = bgVal;
        glassBgEl.style.backgroundImage = bgVal;
        lastArtHash = artHash;
      }

      if (data.accent && data.accent.length === 3 && data.accent2 && data.accent2.length === 3) {
        const accentKey = `${data.accent.join(",")}|${data.accent2.join(",")}`;
        if (accentKey !== lastAccentKey) {
          root.style.setProperty("--accent-r", data.accent[0]);
          root.style.setProperty("--accent-g", data.accent[1]);
          root.style.setProperty("--accent-b", data.accent[2]);
          root.style.setProperty("--accent2-r", data.accent2[0]);
          root.style.setProperty("--accent2-g", data.accent2[1]);
          root.style.setProperty("--accent2-b", data.accent2[2]);
          lastAccentKey = accentKey;
        }
      }

      let shouldShow = false;
      if (finalMode === "always") {
        shouldShow = true;
      } else if (finalMode === "toast") {
        shouldShow = state.is_playing && (nowMono < toastUntilMono);
      } else {
        shouldShow = state.is_playing;
      }

      isWidgetVisible = shouldShow;
      widgetEl.classList.toggle("visible", shouldShow);
      ensureAnimationLoop();
    } catch (e) {
      isWidgetVisible = false;
      widgetEl.classList.remove("visible");
    } finally {
      setTimeout(pollServer, 250);
    }
  }

  pollServer();
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
    padding: 24px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 20px;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(340px, 1fr));
    gap: 18px;
    width: 100%;
    max-width: 960px;
  }
  .card {
    background: #1b1b22;
    border: 1px solid rgba(255,255,255,0.08);
    border-radius: 12px;
    padding: 20px;
    box-shadow: 0 10px 30px rgba(0,0,0,0.35);
  }
  h2 { font-size: 16px; margin-bottom: 14px; color: #ff0f87; text-transform: uppercase; letter-spacing: 0.5px; }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 12px;
    gap: 12px;
  }
  label { font-size: 13.5px; font-weight: 500; color: #d8d8e2; }
  input[type="number"], input[type="text"], select {
    background: #121216;
    color: #fff;
    border: 1px solid rgba(255,255,255,0.15);
    border-radius: 6px;
    padding: 6px 10px;
    font-size: 13.5px;
  }
  input[type="number"] { width: 95px; }
  input[type="text"], select { width: 190px; }
  .btn-row { display: flex; gap: 10px; margin-top: 14px; }
  button {
    flex: 1;
    background: #2b2b36;
    color: #fff;
    border: 1px solid rgba(255,255,255,0.1);
    padding: 8px 12px;
    border-radius: 6px;
    cursor: pointer;
    font-weight: 600;
    font-size: 13px;
  }
  button.primary { background: #ff0f87; border-color: #ff0f87; }
  button:hover { filter: brightness(1.12); }
  .status-bar {
    background: #14141a;
    border: 1px solid rgba(255,255,255,0.07);
    border-radius: 8px;
    padding: 10px 14px;
    margin-bottom: 14px;
  }
  .now-title { font-size: 14px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .now-sub { font-size: 12px; color: #9a9aa6; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; margin-top: 2px; }
  .pill-list { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 6px; }
  .pill {
    font-size: 11.5px;
    background: #262631;
    border: 1px solid rgba(255,255,255,0.12);
    padding: 3px 8px;
    border-radius: 999px;
    cursor: pointer;
    color: #cfd0dc;
  }
  .pill:hover { border-color: #ff0f87; color: #fff; }
  iframe {
    border: 1px dashed rgba(255,255,255,0.15);
    border-radius: 12px;
    background: repeating-conic-gradient(#18181f 0% 25%, #202029 0% 50%) 50% / 20px 20px;
  }
</style>
</head>
<body>
  <div class="grid">
    <div class="card">
      <h2>Style, Theme & Dimensions</h2>
      <div class="row">
        <label for="theme">Visual Theme</label>
        <select id="theme">
          <option value="default">Default (Dual-Tone Glow)</option>
          <option value="vinyl">Vinyl (Spinning Record)</option>
          <option value="glass">Glass (Frosted Art Blur)</option>
          <option value="compact">Compact (Minimal Bar)</option>
        </select>
      </div>
      <div class="row">
        <label for="mode">Visibility Mode</label>
        <select id="mode">
          <option value="default">Show While Playing</option>
          <option value="always">Always Show</option>
          <option value="toast">Track Change Toast</option>
        </select>
      </div>
      <div class="row">
        <label for="toast">Toast Duration (3 – 60 s)</label>
        <input id="toast" type="number" min="3" max="60" step="1" value="8">
      </div>
      <div class="row">
        <label for="w">Width (260 – 1920 px)</label>
        <input id="w" type="number" min="260" max="1920" step="10" value="510">
      </div>
      <div class="row">
        <label for="h">Height (80 – 600 px)</label>
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
      <div class="btn-row">
        <button id="reset">Reset Defaults</button>
        <button id="copy" class="primary">Copy OBS URL</button>
      </div>
    </div>

    <div class="card">
      <h2>Session Filtering & Stream Tools</h2>
      <div class="status-bar">
        <div class="now-title" id="live-title">Waiting for Media...</div>
        <div class="now-sub" id="live-sub">No active session</div>
      </div>

      <div class="row">
        <label for="fmode">App Session Filter</label>
        <select id="fmode">
          <option value="all">All Media Apps</option>
          <option value="allow">Only Allow Listed</option>
          <option value="block">Block Listed Apps</option>
        </select>
      </div>
      <div class="row">
        <label for="flist">App Filter Keywords</label>
        <input id="flist" type="text" placeholder="e.g. Spotify, chrome">
      </div>
      <div style="margin-bottom: 12px;">
        <div style="font-size:11.5px; color:#8e8ea0;">Detected Apps (click to add to filter):</div>
        <div class="pill-list" id="session-pills"></div>
      </div>

      <div class="row">
        <label for="clean">Clean Noisy Video Titles</label>
        <input id="clean" type="checkbox" checked>
      </div>
      <div class="row">
        <label for="tfmt">Chat Bot <code>/text</code> Format</label>
        <input id="tfmt" type="text" value="{artist} — {title}">
      </div>
      <div class="row">
        <label for="tidle">Idle <code>/text</code> Fallback</label>
        <input id="tidle" type="text" value="No track playing">
      </div>
      <div class="row">
        <label for="startup">Launch at Windows Startup</label>
        <input id="startup" type="checkbox">
      </div>
      <div class="btn-row">
        <button id="copy-text">Copy /text Bot URL</button>
      </div>
    </div>
  </div>

  <iframe id="preview" src="/?mode=always" width="600" height="180"></iframe>

<script>
  const el = id => document.getElementById(id);
  const wEl = el("w"), hEl = el("h"), rEl = el("r"), oEl = el("o");
  const themeEl = el("theme"), modeEl = el("mode"), toastEl = el("toast");
  const fmodeEl = el("fmode"), flistEl = el("flist");
  const cleanEl = el("clean"), tfmtEl = el("tfmt"), tidleEl = el("tidle");
  const startupEl = el("startup"), preview = el("preview");
  let lastSessionsKey = null;

  async function loadInitial() {
    const res = await fetch("/state", { cache: "no-store" });
    const data = await res.json();
    const c = data.config || {};
    wEl.value = c.width ?? 510;
    hEl.value = c.height ?? 122;
    rEl.value = c.radius ?? 10;
    oEl.value = c.opacity ?? 1.0;
    themeEl.value = c.theme || "default";
    modeEl.value = c.display_mode || (c.always_show ? "always" : "default");
    toastEl.value = c.toast_seconds ?? 8;
    fmodeEl.value = c.filter_mode || "all";
    flistEl.value = c.app_list || "";
    cleanEl.checked = c.clean_titles !== false;
    tfmtEl.value = c.text_format || "{artist} — {title}";
    tidleEl.value = c.text_idle ?? "No track playing";
    startupEl.checked = Boolean(data.run_on_startup);
    resizePreview();
    refreshStatus(data);
  }

  function refreshStatus(data) {
    el("live-title").textContent = data.title || "Waiting for Media...";
    const appLabel = data.source_app ? ` [${data.source_app}]` : "";
    el("live-sub").textContent = (data.artist || "No artist") + appLabel;

    const sessions = data.active_sessions || [];
    const sessionsKey = JSON.stringify(sessions);
    if (sessionsKey !== lastSessionsKey) {
      lastSessionsKey = sessionsKey;
      const pillBox = el("session-pills");
      pillBox.innerHTML = "";
      sessions.forEach(app => {
        const span = document.createElement("span");
        span.className = "pill";
        span.textContent = app;
        span.onclick = () => {
          const cur = flistEl.value.split(",").map(s => s.trim()).filter(Boolean);
          if (!cur.includes(app)) {
            cur.push(app);
            flistEl.value = cur.join(", ");
            saveLive();
          }
        };
        pillBox.appendChild(span);
      });
    }
  }

  async function pollDashboard() {
    try {
      const res = await fetch("/state", { cache: "no-store" });
      const data = await res.json();
      refreshStatus(data);
    } catch (e) {}
    setTimeout(pollDashboard, 1000);
  }

  function resizePreview() {
    preview.width = Math.max(540, Number(wEl.value) + 40);
    preview.height = Math.max(160, Number(hEl.value) + 40);
  }

  async function saveLive() {
    resizePreview();
    const mode = modeEl.value;
    await fetch("/config", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        width: Number(wEl.value),
        height: Number(hEl.value),
        radius: Number(rEl.value),
        opacity: Number(oEl.value),
        always_show: mode === "always",
        display_mode: mode,
        toast_seconds: Number(toastEl.value),
        theme: themeEl.value,
        clean_titles: cleanEl.checked,
        filter_mode: fmodeEl.value,
        app_list: flistEl.value,
        text_format: tfmtEl.value,
        text_idle: tidleEl.value
      })
    });
  }

  [wEl, hEl, rEl, oEl, themeEl, modeEl, toastEl, fmodeEl, flistEl, cleanEl, tfmtEl, tidleEl]
    .forEach(input => input.addEventListener("input", saveLive));

  startupEl.addEventListener("change", async () => {
    await fetch("/startup", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ enabled: startupEl.checked })
    });
  });

  el("reset").addEventListener("click", () => {
    wEl.value = 510; hEl.value = 122; rEl.value = 10; oEl.value = 1.0;
    themeEl.value = "default"; modeEl.value = "default"; toastEl.value = 8;
    fmodeEl.value = "all"; flistEl.value = "";
    cleanEl.checked = true;
    tfmtEl.value = "{artist} — {title}";
    tidleEl.value = "No track playing";
    saveLive();
  });

  el("copy").addEventListener("click", async (e) => {
    await navigator.clipboard.writeText("http://127.0.0.1:8765/");
    e.target.textContent = "Copied!";
    setTimeout(() => e.target.textContent = "Copy OBS URL", 1500);
  });

  el("copy-text").addEventListener("click", async (e) => {
    await navigator.clipboard.writeText("http://127.0.0.1:8765/text");
    e.target.textContent = "Copied http://127.0.0.1:8765/text!";
    setTimeout(() => e.target.textContent = "Copy /text Bot URL", 1500);
  });

  loadInitial().then(() => setTimeout(pollDashboard, 1000));
</script>
</body>
</html>
"#;