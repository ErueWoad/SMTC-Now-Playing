use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use serde::{Deserialize, Serialize};

use crate::win32::is_startup_enabled;

// The local loopback address and port the embedded web server listens on.
// 127.0.0.1 means only programs on your own computer (like OBS) can connect.
pub const HOST: &str = "127.0.0.1";
pub const PORT: u16 = 8765;

// Wait 300ms after the user stops dragging a slider before writing to disk,
// avoiding hundreds of unnecessary hard drive writes per second.
pub const CONFIG_SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Safely locks the shared application state.
/// In Rust, a Mutex becomes "poisoned" if a thread crashes while holding the lock.
/// `into_inner()` recovers the data inside so the app can keep running smoothly.
pub fn lock_shared(shared: &Mutex<SharedApp>) -> MutexGuard<'_, SharedApp> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Shortens a UTF-8 string to at most `max_bytes` without splitting a multi-byte
/// character (like an emoji or accented letter) in half, which would crash Rust.
fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut boundary = max_bytes;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        s.truncate(boundary);
    }
}

/// User settings saved to `%APPDATA%\SMTC-Now-Playing\config.json`.
/// `#[serde(default)]` ensures that if a user upgrades from an older version
/// missing a new field, Serde fills in the default value instead of failing.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub radius: u32,
    pub opacity: f32,
    pub always_show: bool,
    pub display_mode: String, // "default" | "always" | "toast"
    pub toast_seconds: u32,
    pub theme: String,        // "default" | "vinyl" | "glass" | "compact"
    pub font_family: String,  // e.g. "Segoe UI", "Inter"
    pub font_size: u32,       // Base title font size in px (12..=48)
    pub clean_titles: bool,
    pub filter_mode: String,  // "all" | "allow" | "block"
    pub app_list: String,     // comma-separated app IDs / keywords
    pub text_format: String,  // e.g. "{artist} — {title}"
    pub text_idle: String,    // e.g. "No track playing"
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 510,
            height: 122,
            radius: 10,
            opacity: 1.0,
            always_show: false,
            display_mode: "default".into(),
            toast_seconds: 8,
            theme: "default".into(),
            font_family: "Segoe UI".into(),
            font_size: 22,
            clean_titles: true,
            filter_mode: "all".into(),
            app_list: String::new(),
            text_format: "{artist} — {title}".into(),
            text_idle: "No track playing".into(),
        }
    }
}

impl Config {
    /// Validates and restricts all configuration values to safe ranges so malformed
    /// JSON or invalid user inputs cannot break the UI layout or inject CSS/HTML.
    pub fn clamped(mut self) -> Self {
        self.width = self.width.clamp(260, 1920);
        self.height = self.height.clamp(80, 600);
        self.radius = self.radius.clamp(0, 60);
        self.opacity = (self.opacity.clamp(0.0, 1.0) * 100.0).round() / 100.0;
        self.toast_seconds = self.toast_seconds.clamp(3, 60);
        self.font_size = self.font_size.clamp(12, 48);

        // Strip characters that could break out of a CSS font-family string in-place
        self.font_family
            .retain(|c| !matches!(c, '"' | '\'' | ';' | '{' | '}' | '<' | '>' | '\\'));
        let trimmed_font = self.font_family.trim();
        if trimmed_font.is_empty() {
            self.font_family = "Segoe UI".into();
        } else if trimmed_font.len() != self.font_family.len() {
            self.font_family = trimmed_font.to_string();
        }
        truncate_utf8(&mut self.font_family, 128);

        // Normalize display_mode and keep the legacy `always_show` boolean in sync
        if !matches!(self.display_mode.as_str(), "always" | "toast" | "default") {
            self.display_mode = if self.always_show { "always" } else { "default" }.into();
        }
        self.always_show = self.display_mode == "always";

        if !matches!(self.theme.as_str(), "default" | "vinyl" | "glass" | "compact") {
            self.theme = "default".into();
        }

        if !matches!(self.filter_mode.as_str(), "all" | "allow" | "block") {
            self.filter_mode = "all".into();
        }

        if self.text_format.trim().is_empty() {
            self.text_format = "{artist} — {title}".into();
        }

        truncate_utf8(&mut self.text_idle, 512);
        truncate_utf8(&mut self.text_format, 512);
        truncate_utf8(&mut self.app_list, 1024);
        self
    }
}

/// Represents the currently playing track and extracted theme colors.
#[derive(Clone, Serialize, Debug)]
pub struct MediaState {
    pub is_playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source_app: String,
    pub active_sessions: Vec<String>,
    pub position: f64,
    pub duration: f64,
    pub art_hash: u64,
    pub accent: [u8; 3],
    pub accent2: [u8; 3],
}

impl Default for MediaState {
    fn default() -> Self {
        Self {
            is_playing: false,
            title: "Waiting for Media...".into(),
            artist: "Play a track to display".into(),
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

/// Combines MediaState and Config into a single JSON object without copying strings.
/// `#[serde(flatten)]` merges all fields of `MediaState` into the top-level JSON.
#[derive(Serialize)]
pub struct StatePayload<'a> {
    #[serde(flatten)]
    pub state: &'a MediaState,
    pub config: &'a Config,
    pub run_on_startup: bool,
}

/// Central state shared safely across the tray thread, HTTP server, and SMTC poller.
pub struct SharedApp {
    pub state: MediaState,
    pub state_updated_at: Instant,
    pub config: Config,
    pub config_path: PathBuf,
    pub config_dirty_at: Option<Instant>,
    // `Bytes` is a reference-counted byte slice that Axum can send over HTTP
    // with zero memory copying.
    pub art_png: Option<Bytes>,
    pub run_on_startup: bool,
}

impl SharedApp {
    pub fn new() -> Self {
        let mut app = Self {
            state: MediaState::default(),
            state_updated_at: Instant::now(),
            config: Config::default(),
            config_path: get_config_path(),
            config_dirty_at: None,
            art_png: None,
            run_on_startup: is_startup_enabled(),
        };
        app.load_config();
        app
    }

    /// Advances the playback position in-place based on how much real time has passed
    /// since the last update. Doing this in-place avoids cloning Strings/Vecs.
    pub fn sync_live_position(&mut self) {
        let now = Instant::now();
        if self.state.is_playing {
            self.state.position += now.duration_since(self.state_updated_at).as_secs_f64();
            if self.state.duration > 0.0 {
                self.state.position = self.state.position.clamp(0.0, self.state.duration);
            }
        }
        self.state_updated_at = now;
    }

    /// Reads `config.json` from disk if it exists, otherwise writes default settings.
    pub fn load_config(&mut self) {
        if let Ok(raw) = fs::read_to_string(&self.config_path) {
            if let Ok(parsed) = serde_json::from_str::<Config>(&raw) {
                self.config = parsed.clamped();
                return;
            }
        }
        self.save_config_now();
    }

    /// Schedules a config save to happen after `CONFIG_SAVE_DEBOUNCE` (300ms).
    pub fn mark_config_dirty(&mut self) {
        self.config_dirty_at = Some(Instant::now());
    }

    /// Writes the configuration to a temporary file first, then atomically renames it
    /// over `config.json` so a power loss or crash can never corrupt the file.
    pub fn save_config_now(&mut self) {
        self.config_dirty_at = None;
        let tmp_path = self.config_path.with_extension("json.tmp");
        if let Ok(json_str) = serde_json::to_string_pretty(&self.config) {
            if fs::write(&tmp_path, json_str).is_ok() {
                let _ = fs::rename(&tmp_path, &self.config_path);
            }
        }
    }
}

/// Resolves `%APPDATA%\SMTC-Now-Playing\config.json`, falling back to `%TEMP%`.
fn get_config_path() -> PathBuf {
    if let Ok(appdata) = env::var("APPDATA") {
        let dir = PathBuf::from(appdata).join("SMTC-Now-Playing");
        let _ = fs::create_dir_all(&dir);
        return dir.join("config.json");
    }
    env::temp_dir().join("smtc-now-playing-config.json")
}