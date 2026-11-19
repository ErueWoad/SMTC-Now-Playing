use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use crate::win32::is_startup_enabled;

pub const HOST: &str = "127.0.0.1";
pub const PORT: u16 = 8765;

pub const CONFIG_SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

pub fn lock_shared(shared: &Mutex<SharedApp>) -> MutexGuard<'_, SharedApp> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Shortens a UTF-8 string to at most `max_bytes` without splitting a multi-byte character.
pub fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut boundary = max_bytes;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        s.truncate(boundary);
    }
}

/// Returns a prefix of `s` at most `max_bytes` long on a valid UTF-8 boundary.
pub fn truncate_utf8_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    #[default]
    Default,
    Always,
    Toast,
    #[serde(other)]
    Unknown,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Default,
    Vinyl,
    Glass,
    Compact,
    #[serde(other)]
    Unknown,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterMode {
    #[default]
    All,
    Allow,
    Block,
    #[serde(other)]
    Unknown,
}

/// User settings saved to `%APPDATA%\SMTC-Now-Playing\config.json`.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub radius: u32,
    pub opacity: f32,
    pub always_show: bool,
    pub display_mode: DisplayMode,
    pub toast_seconds: u32,
    pub theme: Theme,
    pub font_family: String,
    pub font_size: u32,
    pub clean_titles: bool,
    pub filter_mode: FilterMode,
    pub app_list: String,
    pub text_format: String,
    pub text_idle: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 510,
            height: 122,
            radius: 10,
            opacity: 1.0,
            always_show: false,
            display_mode: DisplayMode::Default,
            toast_seconds: 8,
            theme: Theme::Default,
            font_family: "Segoe UI".into(),
            font_size: 22,
            clean_titles: true,
            filter_mode: FilterMode::All,
            app_list: String::new(),
            text_format: "{artist} — {title}".into(),
            text_idle: "No track playing".into(),
        }
    }
}

impl Config {
    /// Validates and restricts all configuration values to safe ranges.
    pub fn clamped(mut self) -> Self {
        self.width = self.width.clamp(260, 1920);
        self.height = self.height.clamp(80, 600);
        self.radius = self.radius.clamp(0, 60);
        if !self.opacity.is_finite() {
            self.opacity = 1.0;
        }
        self.opacity = (self.opacity.clamp(0.0, 1.0) * 100.0).round() / 100.0;
        self.toast_seconds = self.toast_seconds.clamp(3, 60);
        self.font_size = self.font_size.clamp(12, 48);

        self.font_family.retain(|c| {
            !c.is_control() && !matches!(c, '"' | '\'' | ';' | '{' | '}' | '<' | '>' | '\\')
        });
        let trimmed_font = self.font_family.trim();
        if trimmed_font.is_empty() {
            self.font_family = "Segoe UI".into();
        } else if trimmed_font.len() != self.font_family.len() {
            self.font_family = trimmed_font.to_string();
        }
        truncate_utf8(&mut self.font_family, 128);

        if self.display_mode == DisplayMode::Unknown {
            self.display_mode = if self.always_show {
                DisplayMode::Always
            } else {
                DisplayMode::Default
            };
        }
        self.always_show = self.display_mode == DisplayMode::Always;

        if self.theme == Theme::Unknown {
            self.theme = Theme::Default;
        }
        if self.filter_mode == FilterMode::Unknown {
            self.filter_mode = FilterMode::All;
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

/// Represents the currently playing track metadata.
#[derive(Clone, Debug)]
pub struct MediaState {
    pub is_playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source_app: String,
    pub active_sessions: Vec<String>,
    pub position: f64,
    pub duration: f64,
    pub art_hash: u32,
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
        }
    }
}

/// Serializes MediaState fields flat alongside `config` and `run_on_startup`
/// without `#[serde(flatten)]` monomorphization overhead or string cloning.
pub struct StatePayload<'a> {
    pub state: &'a MediaState,
    pub position: f64,
    pub config: &'a Config,
    pub run_on_startup: bool,
}

impl Serialize for StatePayload<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("StatePayload", 11)?;
        s.serialize_field("is_playing", &self.state.is_playing)?;
        s.serialize_field("title", &self.state.title)?;
        s.serialize_field("artist", &self.state.artist)?;
        s.serialize_field("album", &self.state.album)?;
        s.serialize_field("source_app", &self.state.source_app)?;
        s.serialize_field("active_sessions", &self.state.active_sessions)?;
        s.serialize_field("position", &self.position)?;
        s.serialize_field("duration", &self.state.duration)?;
        s.serialize_field("art_hash", &self.state.art_hash)?;
        s.serialize_field("config", self.config)?;
        s.serialize_field("run_on_startup", &self.run_on_startup)?;
        s.end()
    }
}

/// Central state shared safely across the tray thread, HTTP server, and SMTC poller.
pub struct SharedApp {
    pub state: MediaState,
    pub state_updated_at: Instant,
    pub config: Config,
    pub config_path: PathBuf,
    pub config_dirty_at: Option<Instant>,
    pub art_data: Option<Arc<[u8]>>,
    pub art_mime: &'static str,
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
            art_data: None,
            art_mime: "image/png",
            run_on_startup: is_startup_enabled(),
        };
        app.load_config();
        app
    }

    /// Computes live interpolated playback position without mutating stored state.
    pub fn live_position(&self) -> f64 {
        if !self.state.is_playing {
            return self.state.position;
        }
        let pos = self.state.position + self.state_updated_at.elapsed().as_secs_f64();
        if self.state.duration > 0.0 {
            pos.clamp(0.0, self.state.duration)
        } else {
            pos.max(0.0)
        }
    }

    /// Reads `config.json` from disk if present. Backs up corrupt JSON instead of
    /// silently overwriting it, and avoids clobbering on transient read errors.
    pub fn load_config(&mut self) {
        match fs::read_to_string(&self.config_path) {
            Ok(raw) => match serde_json::from_str::<Config>(&raw) {
                Ok(parsed) => {
                    self.config = parsed.clamped();
                }
                Err(_) => {
                    let bak_path = self.config_path.with_extension("json.bak");
                    let _ = fs::rename(&self.config_path, &bak_path);
                    if let Some((path, json)) = self.prepare_config_save() {
                        write_config_atomic(&path, &json);
                    }
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if let Some((path, json)) = self.prepare_config_save() {
                    write_config_atomic(&path, &json);
                }
            }
            Err(_) => {}
        }
    }

    pub fn mark_config_dirty(&mut self) {
        self.config_dirty_at = Some(Instant::now());
    }

    /// Clears the dirty flag and serializes config JSON while holding the lock so the
    /// actual disk write can happen after releasing the `MutexGuard`.
    pub fn prepare_config_save(&mut self) -> Option<(PathBuf, String)> {
        self.config_dirty_at = None;
        let json = serde_json::to_string_pretty(&self.config).ok()?;
        Some((self.config_path.clone(), json))
    }
}

/// Writes configuration to a temporary file and atomically renames it over `config.json`.
pub fn write_config_atomic(config_path: &Path, json_str: &str) {
    let tmp_path = config_path.with_extension("json.tmp");
    if fs::write(&tmp_path, json_str).is_ok() {
        let _ = fs::rename(&tmp_path, config_path);
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