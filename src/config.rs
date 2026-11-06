use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::win32::is_startup_enabled;

pub const HOST: &str = "127.0.0.1";
pub const PORT: u16 = 8765;
pub const CONFIG_SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

pub fn lock_shared(shared: &Mutex<SharedApp>) -> MutexGuard<'_, SharedApp> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut boundary = max_bytes;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        s.truncate(boundary);
    }
}

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
    pub fn clamped(mut self) -> Self {
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
        truncate_utf8(&mut self.text_idle, 512);
        truncate_utf8(&mut self.text_format, 512);
        truncate_utf8(&mut self.app_list, 1024);
        self
    }
}

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
pub struct StatePayload<'a> {
    #[serde(flatten)]
    pub state: &'a MediaState,
    pub config: &'a Config,
    pub run_on_startup: bool,
}

pub struct SharedApp {
    pub state: MediaState,
    pub state_updated_at: Instant,
    pub config: Config,
    pub config_path: PathBuf,
    pub config_dirty_at: Option<Instant>,
    pub art_png: Option<Arc<[u8]>>,
    pub run_on_startup: bool,
}

impl SharedApp {
    pub fn new() -> Self {
        let config_path = get_config_path();
        let mut app = Self {
            state: MediaState::default(),
            state_updated_at: Instant::now(),
            config: Config::default(),
            config_path,
            config_dirty_at: None,
            art_png: None,
            run_on_startup: is_startup_enabled(),
        };
        app.load_config();
        app
    }

    pub fn live_state(&self) -> MediaState {
        let mut s = self.state.clone();
        if s.is_playing {
            s.position += self.state_updated_at.elapsed().as_secs_f64();
            if s.duration > 0.0 {
                s.position = s.position.clamp(0.0, s.duration);
            }
        }
        s
    }

    pub fn load_config(&mut self) {
        if self.config_path.exists() {
            if let Ok(raw) = fs::read_to_string(&self.config_path) {
                if let Ok(parsed) = serde_json::from_str::<Config>(&raw) {
                    self.config = parsed.clamped();
                    return;
                }
            }
        }
        self.save_config_now();
    }

    pub fn mark_config_dirty(&mut self) {
        self.config_dirty_at = Some(Instant::now());
    }

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

fn get_config_path() -> PathBuf {
    if let Ok(appdata) = env::var("APPDATA") {
        let dir = PathBuf::from(appdata).join("SMTC-Now-Playing");
        let _ = fs::create_dir_all(&dir);
        return dir.join("config.json");
    }
    env::temp_dir().join("smtc-now-playing-config.json")
}