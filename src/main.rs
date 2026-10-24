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
// SMTC Session Filtering & Pure Event-Driven Thread
// ---------------------------------------------------------------------------
struct HookedManager {
    mgr: MediaManager,
    cur_token: Option<EventRegistrationToken>,
    ses_token: Option<EventRegistrationToken>,
}

impl HookedManager {
    fn new(mgr: MediaManager, poller_thread: &Thread, props_changed: &Arc<AtomicBool>) -> Self {
        let t_cur = poller_thread.clone();
        let p_cur = Arc::clone(props_changed);
        let cur_token = mgr
            .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                p_cur.store(true, Ordering::Release);
                t_cur.unpark();
                Ok(())
            }))
            .ok();

        let t_ses = poller_thread.clone();
        let p_ses = Arc::clone(props_changed);
        let ses_token = mgr
            .SessionsChanged(&TypedEventHandler::new(move |_, _| {
                p_ses.store(true, Ordering::Release);
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
    app_id: String,
    prop_token: Option<EventRegistrationToken>,
    play_token: Option<EventRegistrationToken>,
    time_token: Option<EventRegistrationToken>,
}

impl HookedSession {
    fn new(
        session: MediaSession,
        app_id: String,
        is_active: bool,
        poller_thread: &Thread,
        props_changed: &Arc<AtomicBool>,
    ) -> Self {
        let t_play = poller_thread.clone();
        let play_token = session
            .PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                t_play.unpark();
                Ok(())
            }))
            .ok();

        let (prop_token, time_token) = if is_active {
            let t_prop = poller_thread.clone();
            let p_prop = Arc::clone(props_changed);
            let prop = session
                .MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    p_prop.store(true, Ordering::Release);
                    t_prop.unpark();
                    Ok(())
                }))
                .ok();

            let t_time = poller_thread.clone();
            let time = session
                .TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    t_time.unpark();
                    Ok(())
                }))
                .ok();

            (prop, time)
        } else {
            (None, None)
        };

        Self {
            session,
            app_id,
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

struct HookedSessionList {
    active_id: Option<String>,
    items: Vec<HookedSession>,
}

impl HookedSessionList {
    fn new(
        allowed: &[(MediaSession, String)],
        active_id: Option<&str>,
        poller_thread: &Thread,
        props_changed: &Arc<AtomicBool>,
    ) -> Self {
        let items = allowed
            .iter()
            .map(|(s, id)| {
                HookedSession::new(
                    s.clone(),
                    id.clone(),
                    active_id == Some(id.as_str()),
                    poller_thread,
                    props_changed,
                )
            })
            .collect();
        Self {
            active_id: active_id.map(str::to_owned),
            items,
        }
    }

    fn matches(&self, allowed: &[(MediaSession, String)], active_id: Option<&str>) -> bool {
        self.active_id.as_deref() == active_id
            && self.items.len() == allowed.len()
            && self
                .items
                .iter()
                .zip(allowed.iter())
                .all(|(h, (_, id))| &h.app_id == id)
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
        let mut buf = Vec::with_capacity(size as usize);

        while (buf.len() as u64) < size {
            let to_request = (size - buf.len() as u64).min(64 * 1024) as u32;
            let loaded = reader.LoadAsync(to_request).ok()?.get().ok()?;
            if loaded == 0 {
                break;
            }
            let unconsumed = reader.UnconsumedBufferLength().ok()? as usize;
            if unconsumed == 0 {
                break;
            }
            let prev_len = buf.len();
            buf.resize(prev_len + unconsumed, 0);
            if reader.ReadBytes(&mut buf[prev_len..]).is_err() {
                let _ = reader.Close();
                return None;
            }
        }
        let _ = reader.Close();
        if buf.is_empty() {
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
) -> (
    Option<(MediaSession, String)>,
    Vec<(MediaSession, String)>,
    Vec<String>,
) {
    let mut discovered_apps = Vec::new();
    let mut allowed_sessions: Vec<(MediaSession, String)> = Vec::new();

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
                    allowed_sessions.push((s, app_id));
                }
            }
        }
    }

    let current_allowed = mgr.GetCurrentSession().ok().and_then(|cur| {
        let app_id = cur
            .SourceAppUserModelId()
            .map(|h| normalize_app_id(&h.to_string()))
            .unwrap_or_else(|_| "Unknown".to_string());
        if !discovered_apps.contains(&app_id) {
            discovered_apps.push(app_id.clone());
        }
        if app_matches_filter(&app_id, filter_mode, app_list) {
            if !allowed_sessions.iter().any(|(_, id)| id == &app_id) {
                allowed_sessions.push((cur.clone(), app_id.clone()));
            }
            Some((cur, app_id))
        } else {
            None
        }
    });

    // 1. Prefer a filtered session that is actively Playing
    for (s, id) in &allowed_sessions {
        let playing = s
            .GetPlaybackInfo()
            .and_then(|i| i.PlaybackStatus())
            .map(|st| st == PlaybackStatus::Playing)
            .unwrap_or(false);
        if playing {
            return (
                Some((s.clone(), id.clone())),
                allowed_sessions,
                discovered_apps,
            );
        }
    }

    // 2. Otherwise check Windows default current session if it passes the filter
    if let Some(cur) = current_allowed {
        return (Some(cur), allowed_sessions, discovered_apps);
    }

    // 3. Fallback to first allowed session
    let first = allowed_sessions.first().cloned();
    (first, allowed_sessions, discovered_apps)
}

fn spawn_smtc_poller(shared: Arc<Mutex<SharedApp>>, running: Arc<AtomicBool>) -> Thread {
    let (thread_tx, thread_rx) = mpsc::channel::<Thread>();

    thread::spawn(move || {
        let my_thread = thread::current();
        let _ = thread_tx.send(my_thread.clone());

        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let props_changed = Arc::new(AtomicBool::new(true));
        let mut hooked_mgr: Option<HookedManager> = None;
        let mut hooked_sessions: Option<HookedSessionList> = None;
        let mut last_raw_track_key = String::new();
        let mut track_started_at = Instant::now();
        let mut last_art_attempt: Option<Instant> = None;
        let mut cached_art_hash: u64 = 0;
        let mut cached_art_bytes: Option<Arc<[u8]>> = None;
        let mut cached_accent: [u8; 3] = [255, 15, 135];
        let mut cached_accent2: [u8; 3] = [0, 210, 255];
        let mut local_anchor_time = Instant::now();
        let mut local_anchor_pos: f64 = 0.0;
        let mut was_playing = false;

        while running.load(Ordering::Relaxed) {
            let did_props_change = props_changed.swap(false, Ordering::AcqRel);

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
                    hooked_mgr = Some(HookedManager::new(mgr, &my_thread, &props_changed));
                }
                let mgr = &hooked_mgr.as_ref().unwrap().mgr;

                let (session_opt, allowed_sessions, active_sessions) =
                    select_session(mgr, &filter_mode, &app_list);
                let active_id = session_opt.as_ref().map(|(_, id)| id.as_str());

                // Only re-register session event hooks when the session list or active app ID changes
                let hooks_match = hooked_sessions
                    .as_ref()
                    .map(|h| h.matches(&allowed_sessions, active_id))
                    .unwrap_or(false);
                if !hooks_match {
                    hooked_sessions = Some(HookedSessionList::new(
                        &allowed_sessions,
                        active_id,
                        &my_thread,
                        &props_changed,
                    ));
                }

                let Some((session, source_app)) = session_opt else {
                    was_playing = false;
                    if let Ok(mut app) = shared.lock() {
                        app.state.is_playing = false;
                        app.state.active_sessions = active_sessions;
                        app.state_updated_at = Instant::now();
                    }
                    return Ok(false);
                };

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

                if is_new_track {
                    last_raw_track_key = raw_track_key;
                    track_started_at = Instant::now();
                    last_art_attempt = None;
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

                let retry_due = cached_art_hash == 0
                    && last_art_attempt
                        .map(|t| t.elapsed() >= Duration::from_millis(180))
                        .unwrap_or(true);

                // Attempt thumbnail load on new track, whenever MediaPropertiesChanged fires, or when retry timer is due
                if is_new_track || did_props_change || retry_due {
                    last_art_attempt = Some(Instant::now());
                    if let Ok(thumb_ref) = props.Thumbnail() {
                        if let Some(img) = read_thumbnail(&thumb_ref) {
                            let (a1, a2, encoded) = process_artwork(&img);
                            cached_accent = a1;
                            cached_accent2 = a2;
                            if let Some((png_bytes, hash)) = encoded {
                                cached_art_hash = hash;
                                cached_art_bytes = Some(png_bytes);
                            }
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

                    if is_new_track
                        || (is_playing != was_playing)
                        || (smtc_pos - predicted_pos).abs() > 1.5
                    {
                        local_anchor_pos = smtc_pos;
                        local_anchor_time = Instant::now();
                        position = smtc_pos;
                    } else if is_playing {
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
                Ok(has_active_track) => {
                    // Actively poll every 250ms for up to 15s while waiting on missing artwork
                    let waiting_for_art = has_active_track
                        && cached_art_hash == 0
                        && track_started_at.elapsed() < Duration::from_secs(15);

                    if waiting_for_art {
                        thread::park_timeout(Duration::from_millis(250));
                    } else {
                        thread::park();
                    }
                }
                Err(_) => {
                    hooked_sessions = None;
                    hooked_mgr = None;
                    was_playing = false;
                    if let Ok(mut app) = shared.lock() {
                        app.state.is_playing = false;
                        app.state_updated_at = Instant::now();
                    }
                    thread::park_timeout(Duration::from_secs(2));
                }
            }
        }

        drop(hooked_sessions);
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
      if (urlAlwaysShow === true || (urlMode === null && urlAlwaysShow === null && cfg.always_show)) {
        finalMode = "always";
      } else if (urlAlwaysShow === false && finalMode === "always") {
        finalMode = "default";
      }

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

const SETTINGS_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>SMTC Now Playing — Settings</title>
<style>
  :root {
    --bg-sidebar: #141418;
    --bg-stage: #0d0d10;
    --bg-card: #1c1c22;
    --bg-input: #25252d;
    --bg-input-hover: #2d2d37;
    --border: rgba(255, 255, 255, 0.08);
    --border-focus: #ff0f87;
    --accent: #ff0f87;
    --accent-hover: #ff2a95;
    --accent-2: #00d2ff;
    --text-main: #f2f2f7;
    --text-muted: #9494a3;
    --text-dim: #686878;
    --radius-sm: 6px;
    --radius-md: 8px;
    --radius-lg: 12px;
  }

  * {
    box-sizing: border-box;
    margin: 0;
    padding: 0;
  }

  body {
    background: var(--bg-stage);
    color: var(--text-main);
    font-family: "Segoe UI", -apple-system, BlinkMacSystemFont, Roboto, sans-serif;
    height: 100vh;
    width: 100vw;
    overflow: hidden;
    display: flex;
  }

  /* -------------------------------------------------------------------------
     Left Sidebar
     ------------------------------------------------------------------------- */
  .sidebar {
    width: 460px;
    min-width: 400px;
    max-width: 100%;
    height: 100vh;
    background: var(--bg-sidebar);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    z-index: 10;
  }

  .sidebar-top {
    padding: 18px 20px 14px;
    border-bottom: 1px solid var(--border);
    background: rgba(20, 20, 24, 0.96);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .brand-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .brand-title {
    display: flex;
    align-items: center;
    gap: 9px;
    font-size: 16px;
    font-weight: 700;
    letter-spacing: 0.2px;
    color: #fff;
  }

  .brand-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 10px var(--accent);
  }

  .url-mode-tabs {
    display: flex;
    gap: 4px;
    background: var(--bg-card);
    padding: 3px;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
  }

  .url-tab {
    background: transparent;
    border: none;
    color: var(--text-muted);
    font-size: 11px;
    font-weight: 600;
    padding: 3px 8px;
    border-radius: 4px;
    cursor: pointer;
    transition: all 0.15s ease;
  }

  .url-tab.active {
    background: var(--bg-input-hover);
    color: #fff;
  }

  /* Click to Copy URL Box */
  .copy-url-box {
    position: relative;
    width: 100%;
    background: #0d0d10;
    border: 1px solid rgba(255, 255, 255, 0.13);
    border-radius: var(--radius-md);
    padding: 10px 12px;
    cursor: pointer;
    overflow: hidden;
    transition: border-color 0.18s ease, box-shadow 0.18s ease;
  }

  .copy-url-box:hover {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px rgba(255, 15, 135, 0.18);
  }

  .copy-url-text {
    font-family: Consolas, "SFMono-Regular", Menlo, monospace;
    font-size: 12px;
    color: var(--accent-2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    user-select: none;
  }

  .copy-url-overlay {
    position: absolute;
    inset: 0;
    background: rgba(255, 15, 135, 0.92);
    color: #fff;
    font-size: 12.5px;
    font-weight: 700;
    letter-spacing: 0.3px;
    display: flex;
    align-items: center;
    justify-content: center;
    opacity: 0;
    transition: opacity 0.16s ease;
  }

  .copy-url-box:hover .copy-url-overlay,
  .copy-url-box.copied .copy-url-overlay {
    opacity: 1;
  }

  .copy-url-box.copied .copy-url-overlay {
    background: #10b981;
  }

  .top-actions {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .btn {
    background: var(--bg-input);
    color: var(--text-main);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 8px 12px;
    font-size: 12.5px;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s ease, border-color 0.15s ease, transform 0.08s ease;
    text-align: center;
  }

  .btn:hover {
    background: var(--bg-input-hover);
    border-color: rgba(255, 255, 255, 0.18);
  }

  .btn:active {
    transform: translateY(1px);
  }

  .btn-primary {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }

  .btn-primary:hover {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }

  .btn-danger {
    background: #e11d48;
    border-color: #e11d48;
    color: #fff;
  }

  .btn-danger:hover {
    background: #f43f5e;
  }

  /* Scrollable Settings Body */
  .sidebar-content {
    flex: 1;
    overflow-y: auto;
    padding: 18px 20px 32px;
    display: flex;
    flex-direction: column;
    gap: 24px;
  }

  .sidebar-content::-webkit-scrollbar {
    width: 6px;
  }
  .sidebar-content::-webkit-scrollbar-track {
    background: transparent;
  }
  .sidebar-content::-webkit-scrollbar-thumb {
    background: rgba(255, 255, 255, 0.14);
    border-radius: 999px;
  }

  .section {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .section-header {
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.9px;
    color: var(--accent);
    padding-bottom: 6px;
    border-bottom: 1px solid var(--border);
  }

  .setting-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
  }

  .setting-item.column {
    flex-direction: column;
    align-items: stretch;
    gap: 8px;
  }

  .setting-item.hidden {
    display: none;
  }

  .setting-info {
    display: flex;
    flex-direction: column;
    gap: 3px;
    flex: 1;
    min-width: 0;
  }

  .setting-label {
    font-size: 13.5px;
    font-weight: 600;
    color: var(--text-main);
  }

  .setting-desc {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.35;
  }

  .setting-control {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: 8px;
  }

  select,
  input[type="text"],
  input[type="number"] {
    color-scheme: dark;
	background: var(--bg-input);
    color: #fff;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: var(--radius-sm);
    padding: 7px 10px;
    font-size: 13px;
    outline: none;
    transition: border-color 0.15s ease, background 0.15s ease;
  }

  select:focus,
  input[type="text"]:focus,
  input[type="number"]:focus {
    border-color: var(--accent);
    background: var(--bg-input-hover);
  }

  select {
    min-width: 185px;
    cursor: pointer;
  }

  input[type="number"] {
    width: 78px;
    text-align: left;
  }

  input[type="text"].full-input {
    width: 100%;
  }

  input[type="range"] {
    -webkit-appearance: none;
    appearance: none;
    width: 105px;
    height: 5px;
    background: var(--bg-input);
    border-radius: 999px;
    outline: none;
    cursor: pointer;
  }

  input[type="range"]::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--accent);
    cursor: pointer;
    border: 2px solid #fff;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.5);
  }

  /* Modern Pill Toggle Switch */
  .switch {
    position: relative;
    display: inline-block;
    width: 42px;
    height: 24px;
    flex-shrink: 0;
  }

  .switch input {
    opacity: 0;
    width: 0;
    height: 0;
  }

  .slider {
    position: absolute;
    cursor: pointer;
    inset: 0;
    background-color: var(--bg-input);
    border: 1px solid rgba(255, 255, 255, 0.14);
    transition: 0.2s cubic-bezier(0.22, 1, 0.36, 1);
    border-radius: 999px;
  }

  .slider::before {
    position: absolute;
    content: "";
    height: 16px;
    width: 16px;
    left: 3px;
    bottom: 3px;
    background-color: #c9c9d6;
    transition: 0.2s cubic-bezier(0.22, 1, 0.36, 1);
    border-radius: 50%;
  }

  .switch input:checked + .slider {
    background-color: var(--accent);
    border-color: var(--accent);
  }

  .switch input:checked + .slider::before {
    transform: translateX(18px);
    background-color: #fff;
  }

  /* Expandable Active Sources Drawer */
  .link-btn {
    background: none;
    border: none;
    color: var(--accent-2);
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    padding: 0;
    text-align: left;
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }

  .link-btn:hover {
    text-decoration: underline;
  }

  .sources-drawer {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 10px 12px;
    display: none;
    flex-direction: column;
    gap: 8px;
  }

  .sources-drawer.open {
    display: flex;
  }

  .pill-list {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .pill {
    font-size: 11.5px;
    background: var(--bg-input);
    border: 1px solid rgba(255, 255, 255, 0.12);
    padding: 4px 10px;
    border-radius: 999px;
    cursor: pointer;
    color: var(--text-main);
    transition: all 0.14s ease;
  }

  .pill:hover {
    border-color: var(--accent);
    color: #fff;
  }

  .pill.active {
    background: rgba(255, 15, 135, 0.2);
    border-color: var(--accent);
    color: #fff;
  }

  .token-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
  }

  .token-chip {
    font-family: Consolas, monospace;
    font-size: 11px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    color: var(--text-muted);
    padding: 2px 7px;
    border-radius: 4px;
    cursor: pointer;
  }

  .token-chip:hover {
    color: #fff;
    border-color: var(--accent-2);
  }

  .bridge-grid {
    display: grid;
    grid-template-columns: 1fr 110px;
    gap: 8px;
  }

  .readonly-box {
    background: #101014;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 7px 10px;
    font-family: Consolas, monospace;
    font-size: 12.5px;
    color: var(--text-muted);
  }

  /* -------------------------------------------------------------------------
     Right Preview Stage
     ------------------------------------------------------------------------- */
  .stage {
    flex: 1;
    height: 100vh;
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    overflow: hidden;
    transition: background 0.2s ease;
  }

  .stage.bg-checker {
    background-image: repeating-conic-gradient(#121217 0% 25%, #181820 0% 50%);
    background-size: 24px 24px;
  }

  .stage.bg-dark {
    background: #09090b;
  }

  .stage.bg-light {
    background: #e4e4e7;
  }

  .stage.bg-green {
    background: #00b140;
  }

  .stage-top-bar {
    position: absolute;
    top: 18px;
    right: 20px;
    display: flex;
    align-items: center;
    gap: 6px;
    background: rgba(20, 20, 24, 0.88);
    backdrop-filter: blur(10px);
    border: 1px solid var(--border);
    padding: 5px 8px;
    border-radius: var(--radius-md);
    z-index: 5;
  }

  .stage-top-label {
    font-size: 11px;
    color: var(--text-muted);
    margin-right: 4px;
    font-weight: 600;
  }

  .bg-swatch {
    border: 1px solid rgba(255, 255, 255, 0.2);
    background: var(--bg-input);
    color: var(--text-main);
    font-size: 11px;
    font-weight: 600;
    padding: 4px 8px;
    border-radius: 5px;
    cursor: pointer;
  }

  .bg-swatch.active {
    border-color: var(--accent);
    background: rgba(255, 15, 135, 0.22);
    color: #fff;
  }

  .preview-wrapper {
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 24px;
    max-width: 100%;
    max-height: calc(100vh - 120px);
    overflow: auto;
  }

  iframe#preview {
    border: none;
    background: transparent;
    overflow: hidden;
    transition: width 0.2s ease, height 0.2s ease;
  }

  .stage-hud {
    position: absolute;
    bottom: 18px;
    left: 50%;
    transform: translateX(-50%);
    background: rgba(20, 20, 24, 0.9);
    backdrop-filter: blur(12px);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 8px 18px;
    display: flex;
    align-items: center;
    gap: 12px;
    max-width: 90%;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
  }

  .status-indicator {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #71717a;
    flex-shrink: 0;
  }

  .status-indicator.playing {
    background: #10b981;
    box-shadow: 0 0 8px #10b981;
  }

  .hud-track {
    font-size: 12.5px;
    font-weight: 600;
    color: #fff;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 360px;
  }

  .hud-app-badge {
    font-size: 11px;
    font-weight: 600;
    color: var(--accent-2);
    background: rgba(0, 210, 255, 0.12);
    border: 1px solid rgba(0, 210, 255, 0.25);
    padding: 2px 8px;
    border-radius: 999px;
    white-space: nowrap;
  }

  /* -------------------------------------------------------------------------
     Modals (Load Settings & Load Defaults)
     ------------------------------------------------------------------------- */
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.72);
    backdrop-filter: blur(4px);
    display: none;
    align-items: center;
    justify-content: center;
    z-index: 100;
    padding: 20px;
  }

  .modal-backdrop.open {
    display: flex;
  }

  .modal {
    background: var(--bg-sidebar);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: var(--radius-lg);
    width: 100%;
    max-width: 440px;
    padding: 22px;
    box-shadow: 0 20px 50px rgba(0, 0, 0, 0.6);
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .modal-title {
    font-size: 16px;
    font-weight: 700;
    color: #fff;
  }

  .modal-desc {
    font-size: 13px;
    color: var(--text-muted);
    line-height: 1.4;
  }

  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 4px;
  }

  @media (max-width: 900px) {
    body {
      flex-direction: column;
      overflow-y: auto;
      height: auto;
    }
    .sidebar {
      width: 100%;
      height: auto;
      border-right: none;
      border-bottom: 1px solid var(--border);
    }
    .stage {
      min-height: 360px;
      height: auto;
      padding: 60px 16px;
    }
  }
</style>
</head>
<body>

  <!-- LEFT CONFIGURATOR SIDEBAR -->
  <aside class="sidebar">
    <div class="sidebar-top">
      <div class="brand-row">
        <div class="brand-title">
          <span class="brand-dot"></span>
          <span>SMTC Now Playing</span>
        </div>
        <div class="url-mode-tabs" title="Choose which URL to display and copy">
          <button type="button" class="url-tab active" data-url-mode="override">Widget URL</button>
          <button type="button" class="url-tab" data-url-mode="global">Global</button>
          <button type="button" class="url-tab" data-url-mode="text">/text</button>
        </div>
      </div>

      <div class="copy-url-box" id="copy-url-box" title="Click to copy URL">
        <div class="copy-url-text" id="top-url-display">http://127.0.0.1:8765/</div>
        <div class="copy-url-overlay" id="copy-url-overlay">Click to copy URL</div>
      </div>

      <div class="top-actions">
        <button type="button" class="btn" id="btn-open-defaults">Load Default Settings</button>
        <button type="button" class="btn" id="btn-open-load">Load Settings</button>
      </div>
    </div>

    <div class="sidebar-content">
      <!-- APPEARANCE -->
      <div class="section">
        <div class="section-header">Appearance</div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="theme">Theme</label>
            <div class="setting-desc">Visual layout preset for the browser source</div>
          </div>
          <div class="setting-control">
            <select id="theme">
              <option value="default">Standard (Dual-Tone Glow)</option>
              <option value="vinyl">Vinyl (Spinning Record)</option>
              <option value="glass">Glass (Frosted Art Blur)</option>
              <option value="compact">Compact (Minimal Bar)</option>
            </select>
          </div>
        </div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="w">Widget Width</label>
            <div class="setting-desc">Horizontal width in pixels (260 – 1920)</div>
          </div>
          <div class="setting-control">
            <input id="w-range" type="range" min="260" max="1200" step="10" value="510">
            <input id="w" type="number" min="260" max="1920" step="10" value="510">
          </div>
        </div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="h">Widget Height</label>
            <div class="setting-desc">Vertical height in pixels (80 – 600)</div>
          </div>
          <div class="setting-control">
            <input id="h-range" type="range" min="80" max="320" step="2" value="122">
            <input id="h" type="number" min="80" max="600" step="2" value="122">
          </div>
        </div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="r">Corner Radius</label>
            <div class="setting-desc">Card border roundness in pixels (0 – 60)</div>
          </div>
          <div class="setting-control">
            <input id="r-range" type="range" min="0" max="60" step="1" value="10">
            <input id="r" type="number" min="0" max="60" step="1" value="10">
          </div>
        </div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="o">Card Opacity</label>
            <div class="setting-desc">Background surface opacity (0.00 – 1.00)</div>
          </div>
          <div class="setting-control">
            <input id="o-range" type="range" min="0" max="1" step="0.05" value="1.0">
            <input id="o" type="number" min="0" max="1" step="0.05" value="1.0">
          </div>
        </div>
      </div>

      <!-- GENERAL -->
      <div class="section">
        <div class="section-header">General</div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="show-paused">Show While Paused</label>
            <div class="setting-desc">If enabled, the widget will be visible even when the track is paused.</div>
          </div>
          <div class="setting-control">
            <label class="switch">
              <input id="show-paused" type="checkbox">
              <span class="slider"></span>
            </label>
          </div>
        </div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="auto-hide-toast">Auto-Hide on Track Change</label>
            <div class="setting-desc">If enabled, the widget will show for a set duration whenever a new track starts.</div>
          </div>
          <div class="setting-control">
            <label class="switch">
              <input id="auto-hide-toast" type="checkbox">
              <span class="slider"></span>
            </label>
          </div>
        </div>

        <div class="setting-item hidden" id="toast-duration-row">
          <div class="setting-info">
            <label class="setting-label" for="toast">Display Duration (seconds)</label>
            <div class="setting-desc">How long the widget should remain visible.</div>
          </div>
          <div class="setting-control">
            <input id="toast" type="number" min="3" max="60" step="1" value="8">
          </div>
        </div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="clean">Clean Noisy Video Titles</label>
            <div class="setting-desc">Strips tags like (Official Music Video), [Lyrics], and platform suffixes.</div>
          </div>
          <div class="setting-control">
            <label class="switch">
              <input id="clean" type="checkbox" checked>
              <span class="slider"></span>
            </label>
          </div>
        </div>
      </div>

      <!-- SOURCE FILTERING -->
      <div class="section">
        <div class="section-header">App Source Filtering</div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="fmode">Filter Mode</label>
            <div class="setting-desc">Control which Windows media sessions are tracked</div>
          </div>
          <div class="setting-control">
            <select id="fmode">
              <option value="all">Track All Media Apps</option>
              <option value="allow">Included Apps Only</option>
              <option value="block">Excluded Apps</option>
            </select>
          </div>
        </div>

        <div class="setting-item column">
          <div class="setting-info">
            <label class="setting-label" id="flist-label" for="flist">App Filter List</label>
            <div class="setting-desc" id="flist-desc">Comma-separated app names or keywords (e.g. Spotify, chrome).</div>
          </div>
          <input id="flist" class="full-input" type="text" placeholder="e.g. Spotify.exe, chrome.exe">
          <button type="button" class="link-btn" id="toggle-sources-btn">View active sources ▾</button>
          <div class="sources-drawer" id="sources-drawer">
            <div class="setting-desc">Click an active media source to add or remove it:</div>
            <div class="pill-list" id="session-pills"></div>
          </div>
        </div>
      </div>

      <!-- CHAT BOT / TEXT ENDPOINT -->
      <div class="section">
        <div class="section-header">Chat Bot Plaintext (/text)</div>

        <div class="setting-item column">
          <div class="setting-info">
            <label class="setting-label" for="tfmt">Format Template</label>
            <div class="setting-desc">Plaintext response served at <code>http://127.0.0.1:8765/text</code></div>
          </div>
          <input id="tfmt" class="full-input" type="text" value="{artist} — {title}">
          <div class="token-chips">
            <span class="token-chip" data-token="{title}">{title}</span>
            <span class="token-chip" data-token="{artist}">{artist}</span>
            <span class="token-chip" data-token="{album}">{album}</span>
            <span class="token-chip" data-token="{pos}">{pos}</span>
            <span class="token-chip" data-token="{dur}">{dur}</span>
            <span class="token-chip" data-token="{app}">{app}</span>
            <span class="token-chip" data-token="{status}">{status}</span>
          </div>
        </div>

        <div class="setting-item column">
          <div class="setting-info">
            <label class="setting-label" for="tidle">Idle Fallback Text</label>
            <div class="setting-desc">Returned by <code>/text</code> when no media is playing</div>
          </div>
          <input id="tidle" class="full-input" type="text" value="No track playing">
        </div>
      </div>

      <!-- SMTC BRIDGE & SYSTEM -->
      <div class="section">
        <div class="section-header">SMTC Bridge &amp; System</div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="startup">Launch at Windows Startup</label>
            <div class="setting-desc">Automatically start in the system tray when you sign in.</div>
          </div>
          <div class="setting-control">
            <label class="switch">
              <input id="startup" type="checkbox">
              <span class="slider"></span>
            </label>
          </div>
        </div>

        <div class="setting-item">
          <div class="setting-info">
            <label class="setting-label" for="sync-global">Save Edits to Global Config</label>
            <div class="setting-desc">Uncheck to build a per-source URL without overwriting saved global defaults.</div>
          </div>
          <div class="setting-control">
            <label class="switch">
              <input id="sync-global" type="checkbox" checked>
              <span class="slider"></span>
            </label>
          </div>
        </div>

        <div class="setting-item column">
          <div class="setting-info">
            <div class="setting-label">Local Server Endpoint</div>
            <div class="setting-desc">Address and port serving OBS Browser Sources</div>
          </div>
          <div class="bridge-grid">
            <div class="readonly-box">127.0.0.1</div>
            <div class="readonly-box">8765</div>
          </div>
        </div>
      </div>
    </div>
  </aside>

  <!-- RIGHT LIVE PREVIEW STAGE -->
  <main class="stage bg-checker" id="stage">
    <div class="stage-top-bar">
      <span class="stage-top-label">Preview Backdrop:</span>
      <button type="button" class="bg-swatch active" data-bg="bg-checker">Grid</button>
      <button type="button" class="bg-swatch" data-bg="bg-dark">Dark</button>
      <button type="button" class="bg-swatch" data-bg="bg-light">Light</button>
      <button type="button" class="bg-swatch" data-bg="bg-green">Chroma</button>
    </div>

    <div class="preview-wrapper">
      <iframe id="preview" src="/?mode=always" width="550" height="162"></iframe>
    </div>

    <div class="stage-hud">
      <span class="status-indicator" id="hud-dot"></span>
      <span class="hud-track" id="live-title">Waiting for Media...</span>
      <span class="hud-app-badge" id="live-sub">Idle</span>
    </div>
  </main>

  <!-- MODAL: LOAD SETTINGS FROM URL -->
  <div class="modal-backdrop" id="modal-load">
    <div class="modal">
      <div class="modal-title">Load Settings</div>
      <div class="modal-desc">Paste in your existing widget URL to import its theme, dimensions, and visibility settings:</div>
      <input id="import-url-input" class="full-input" type="text" placeholder="http://127.0.0.1:8765/?theme=vinyl&w=510...">
      <div class="modal-actions">
        <button type="button" class="btn" id="btn-cancel-load">Cancel</button>
        <button type="button" class="btn btn-primary" id="btn-confirm-load">Load Settings</button>
      </div>
    </div>
  </div>

  <!-- MODAL: LOAD DEFAULTS CONFIRMATION -->
  <div class="modal-backdrop" id="modal-defaults">
    <div class="modal">
      <div class="modal-title">Load Defaults</div>
      <div class="modal-desc">Are you sure you want to reset all widget settings back to their defaults?</div>
      <div class="modal-actions">
        <button type="button" class="btn" id="btn-cancel-defaults">No</button>
        <button type="button" class="btn btn-danger" id="btn-confirm-defaults">Yes</button>
      </div>
    </div>
  </div>

<script>
  const BASE_URL = "http://127.0.0.1:8765";
  const el = id => document.getElementById(id);

  const wEl = el("w"), wRangeEl = el("w-range");
  const hEl = el("h"), hRangeEl = el("h-range");
  const rEl = el("r"), rRangeEl = el("r-range");
  const oEl = el("o"), oRangeEl = el("o-range");

  const themeEl = el("theme");
  const showPausedEl = el("show-paused");
  const autoHideToastEl = el("auto-hide-toast");
  const toastRowEl = el("toast-duration-row");
  const toastEl = el("toast");

  const fmodeEl = el("fmode"), flistEl = el("flist");
  const cleanEl = el("clean"), tfmtEl = el("tfmt"), tidleEl = el("tidle");
  const startupEl = el("startup"), syncGlobalEl = el("sync-global");
  const preview = el("preview");

  const topUrlDisplay = el("top-url-display");
  const copyUrlBox = el("copy-url-box");
  const copyUrlOverlay = el("copy-url-overlay");

  let activeUrlTab = "override"; // "override" | "global" | "text"
  let lastSessionsKey = null;

  function getDisplayMode() {
    if (autoHideToastEl.checked) return "toast";
    if (showPausedEl.checked) return "always";
    return "default";
  }

  function applyDisplayModeToToggles(mode) {
    if (mode === "always") {
      showPausedEl.checked = true;
      autoHideToastEl.checked = false;
    } else if (mode === "toast") {
      showPausedEl.checked = false;
      autoHideToastEl.checked = true;
    } else {
      showPausedEl.checked = false;
      autoHideToastEl.checked = false;
    }
    updateConditionalRows();
  }

  function updateConditionalRows() {
    toastRowEl.classList.toggle("hidden", !autoHideToastEl.checked);
    const fmode = fmodeEl.value;
    if (fmode === "allow") {
      el("flist-label").textContent = "Included Apps";
      el("flist-desc").textContent = "Only apps matching these keywords will be displayed.";
    } else if (fmode === "block") {
      el("flist-label").textContent = "Excluded Apps";
      el("flist-desc").textContent = "Apps matching these keywords will never be displayed.";
    } else {
      el("flist-label").textContent = "App Filter Keywords";
      el("flist-desc").textContent = "Select 'Included' or 'Excluded' above to filter by app keywords.";
    }
  }

  function syncRangeAndNumber(source, target) {
    target.value = source.value;
  }

  function buildWidgetOverrideUrl() {
    const mode = getDisplayMode();
    const params = new URLSearchParams();
    params.set("theme", themeEl.value);
    params.set("mode", mode);
    if (mode === "toast") {
      params.set("toast", toastEl.value);
    }
    params.set("w", wEl.value);
    params.set("h", hEl.value);
    params.set("radius", rEl.value);
    params.set("opacity", oEl.value);
    return `${BASE_URL}/?${params.toString()}`;
  }

  function buildTextOverrideUrl() {
    const fmt = tfmtEl.value.trim();
    if (!fmt) return `${BASE_URL}/text`;
    const params = new URLSearchParams({ format: fmt });
    return `${BASE_URL}/text?${params.toString()}`;
  }

  function getActiveUrlString() {
    if (activeUrlTab === "global") return `${BASE_URL}/`;
    if (activeUrlTab === "text") return buildTextOverrideUrl();
    return buildWidgetOverrideUrl();
  }

  function updateGeneratedUrls() {
    topUrlDisplay.textContent = getActiveUrlString();

    const previewParams = new URLSearchParams({
      mode: "always",
      theme: themeEl.value,
      w: wEl.value,
      h: hEl.value,
      radius: rEl.value,
      opacity: oEl.value
    });
    const nextSrc = `/?${previewParams.toString()}`;
    if (preview.getAttribute("src") !== nextSrc) {
      preview.setAttribute("src", nextSrc);
    }
  }

  function resizePreview() {
    preview.width = Math.max(300, Number(wEl.value) + 28);
    preview.height = Math.max(110, Number(hEl.value) + 28);
  }

  async function loadInitial() {
    const res = await fetch("/state", { cache: "no-store" });
    const data = await res.json();
    const c = data.config || {};

    wEl.value = wRangeEl.value = c.width ?? 510;
    hEl.value = hRangeEl.value = c.height ?? 122;
    rEl.value = rRangeEl.value = c.radius ?? 10;
    oEl.value = oRangeEl.value = c.opacity ?? 1.0;

    themeEl.value = c.theme || "default";
    toastEl.value = c.toast_seconds ?? 8;

    const mode = c.display_mode || (c.always_show ? "always" : "default");
    applyDisplayModeToToggles(mode);

    fmodeEl.value = c.filter_mode || "all";
    flistEl.value = c.app_list || "";
    cleanEl.checked = c.clean_titles !== false;
    tfmtEl.value = c.text_format || "{artist} — {title}";
    tidleEl.value = c.text_idle ?? "No track playing";
    startupEl.checked = Boolean(data.run_on_startup);

    updateConditionalRows();
    resizePreview();
    updateGeneratedUrls();
    refreshStatus(data);
  }

  function refreshStatus(data) {
    const isPlaying = Boolean(data.is_playing);
    el("hud-dot").classList.toggle("playing", isPlaying);

    const artist = data.artist ? `${data.artist} — ` : "";
    el("live-title").textContent = data.title ? `${artist}${data.title}` : "Waiting for Media...";
    el("live-sub").textContent = data.source_app || (isPlaying ? "Playing" : "Idle");

    const sessions = data.active_sessions || [];
    const currentTokens = flistEl.value.split(",").map(s => s.trim().toLowerCase()).filter(Boolean);
    const sessionsKey = JSON.stringify({ sessions, currentTokens });

    if (sessionsKey !== lastSessionsKey) {
      lastSessionsKey = sessionsKey;
      const pillBox = el("session-pills");
      pillBox.innerHTML = "";

      if (sessions.length === 0) {
        const empty = document.createElement("span");
        empty.className = "setting-desc";
        empty.textContent = "No active media apps detected right now.";
        pillBox.appendChild(empty);
        return;
      }

      sessions.forEach(app => {
        const span = document.createElement("span");
        const isListed = currentTokens.includes(app.toLowerCase());
        span.className = `pill${isListed ? " active" : ""}`;
        span.textContent = app;
        span.onclick = () => {
          const cur = flistEl.value.split(",").map(s => s.trim()).filter(Boolean);
          const idx = cur.findIndex(item => item.toLowerCase() === app.toLowerCase());
          if (idx >= 0) {
            cur.splice(idx, 1);
          } else {
            cur.push(app);
          }
          flistEl.value = cur.join(", ");
          lastSessionsKey = null;
          saveLive();
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

  async function saveLive() {
    updateConditionalRows();
    resizePreview();
    updateGeneratedUrls();
    if (!syncGlobalEl.checked) return;

    const mode = getDisplayMode();
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

  // Range <-> Number two-way bindings
  [
    [wRangeEl, wEl],
    [hRangeEl, hEl],
    [rRangeEl, rEl],
    [oRangeEl, oEl]
  ].forEach(([rangeInput, numInput]) => {
    rangeInput.addEventListener("input", () => {
      syncRangeAndNumber(rangeInput, numInput);
      saveLive();
    });
    numInput.addEventListener("input", () => {
      syncRangeAndNumber(numInput, rangeInput);
      saveLive();
    });
  });

  // Mutually exclusive visibility toggles (maps to "always", "toast", or "default")
  showPausedEl.addEventListener("change", () => {
    if (showPausedEl.checked) autoHideToastEl.checked = false;
    saveLive();
  });

  autoHideToastEl.addEventListener("change", () => {
    if (autoHideToastEl.checked) showPausedEl.checked = false;
    saveLive();
  });

  [themeEl, toastEl, fmodeEl, flistEl, cleanEl, tfmtEl, tidleEl].forEach(input => {
    input.addEventListener("input", saveLive);
  });

  syncGlobalEl.addEventListener("change", () => {
    if (syncGlobalEl.checked) saveLive();
  });

  startupEl.addEventListener("change", async () => {
    await fetch("/startup", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ enabled: startupEl.checked })
    });
  });

  // Top URL Mode Tabs & Click-to-Copy Box
  document.querySelectorAll(".url-tab").forEach(btn => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".url-tab").forEach(b => b.classList.remove("active"));
      btn.classList.add("active");
      activeUrlTab = btn.dataset.urlMode;
      updateGeneratedUrls();
    });
  });

  copyUrlBox.addEventListener("click", async () => {
    await navigator.clipboard.writeText(getActiveUrlString());
    copyUrlBox.classList.add("copied");
    copyUrlOverlay.textContent = "Copied to clipboard!";
    setTimeout(() => {
      copyUrlBox.classList.remove("copied");
      copyUrlOverlay.textContent = "Click to copy URL";
    }, 1400);
  });

  // Active Sources Drawer Toggle
  el("toggle-sources-btn").addEventListener("click", () => {
    const drawer = el("sources-drawer");
    const isOpen = drawer.classList.toggle("open");
    el("toggle-sources-btn").textContent = isOpen ? "Hide active sources ▴" : "View active sources ▾";
  });

  // Chat Bot Token Chips
  document.querySelectorAll(".token-chip").forEach(chip => {
    chip.addEventListener("click", () => {
      const token = chip.dataset.token;
      const start = tfmtEl.selectionStart ?? tfmtEl.value.length;
      const end = tfmtEl.selectionEnd ?? tfmtEl.value.length;
      tfmtEl.value = tfmtEl.value.slice(0, start) + token + tfmtEl.value.slice(end);
      tfmtEl.focus();
      saveLive();
    });
  });

  // Preview Stage Backdrop Switcher
  const stageEl = el("stage");
  document.querySelectorAll(".bg-swatch").forEach(btn => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".bg-swatch").forEach(b => b.classList.remove("active"));
      btn.classList.add("active");
      stageEl.className = `stage ${btn.dataset.bg}`;
    });
  });

  // Modal: Load Default Settings
  const modalDefaults = el("modal-defaults");
  el("btn-open-defaults").addEventListener("click", () => modalDefaults.classList.add("open"));
  el("btn-cancel-defaults").addEventListener("click", () => modalDefaults.classList.remove("open"));
  el("btn-confirm-defaults").addEventListener("click", () => {
    wEl.value = wRangeEl.value = 510;
    hEl.value = hRangeEl.value = 122;
    rEl.value = rRangeEl.value = 10;
    oEl.value = oRangeEl.value = 1.0;
    themeEl.value = "default";
    toastEl.value = 8;
    applyDisplayModeToToggles("default");
    fmodeEl.value = "all";
    flistEl.value = "";
    cleanEl.checked = true;
    tfmtEl.value = "{artist} — {title}";
    tidleEl.value = "No track playing";
    modalDefaults.classList.remove("open");
    saveLive();
  });

  // Modal: Load Settings from URL
  const modalLoad = el("modal-load");
  const importUrlInput = el("import-url-input");
  el("btn-open-load").addEventListener("click", () => {
    importUrlInput.value = "";
    modalLoad.classList.add("open");
    importUrlInput.focus();
  });
  el("btn-cancel-load").addEventListener("click", () => modalLoad.classList.remove("open"));
  el("btn-confirm-load").addEventListener("click", () => {
    const raw = importUrlInput.value.trim();
    if (!raw) return;
    try {
      const parsed = new URL(raw, BASE_URL);
      const p = parsed.searchParams;

      if (p.has("theme")) themeEl.value = p.get("theme");
      if (p.has("w") || p.has("width")) {
        wEl.value = wRangeEl.value = p.get("w") || p.get("width");
      }
      if (p.has("h") || p.has("height")) {
        hEl.value = hRangeEl.value = p.get("h") || p.get("height");
      }
      if (p.has("radius")) {
        rEl.value = rRangeEl.value = p.get("radius");
      }
      if (p.has("opacity") || p.has("bg_alpha")) {
        oEl.value = oRangeEl.value = p.get("opacity") || p.get("bg_alpha");
      }
      if (p.has("toast")) {
        toastEl.value = p.get("toast");
      }
      if (p.has("mode")) {
        applyDisplayModeToToggles(p.get("mode"));
      } else if (p.has("always_show")) {
        const always = ["1", "true", "yes"].includes((p.get("always_show") || "").toLowerCase());
        applyDisplayModeToToggles(always ? "always" : "default");
      }
      if (p.has("format")) {
        tfmtEl.value = p.get("format");
      }

      modalLoad.classList.remove("open");
      saveLive();
    } catch (err) {
      importUrlInput.style.borderColor = "#e11d48";
    }
  });

  // Close modals on backdrop click
  [modalDefaults, modalLoad].forEach(modal => {
    modal.addEventListener("click", e => {
      if (e.target === modal) modal.classList.remove("open");
    });
  });

  loadInitial().then(() => setTimeout(pollDashboard, 1000));
</script>
</body>
</html>
"##;