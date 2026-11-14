use std::io::Cursor;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use image::{DynamicImage, ImageReader, Limits};
use tokio::sync::broadcast;
use windows::{
    Foundation::{EventRegistrationToken, TypedEventHandler},
    Media::Control::{
        GlobalSystemMediaTransportControlsSession as MediaSession,
        GlobalSystemMediaTransportControlsSessionManager as MediaManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    },
    Storage::Streams::{DataReader, IRandomAccessStreamReference},
    Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
};

use crate::config::{lock_shared, MediaState, SharedApp};
use crate::metadata::{artists_match, find_ascii_case_insensitive, sanitize_metadata};
use crate::palette::process_artwork;
use crate::server::broadcast_state;

const MAX_THUMBNAIL_BYTES: u64 = 16 * 1024 * 1024;
// Seconds between Windows FILETIME epoch (Jan 1, 1601) and Unix epoch (Jan 1, 1970).
const WINDOWS_TO_UNIX_EPOCH_SECS: f64 = 11_644_473_600.0;
// Brief delay before hiding the widget when media pauses, preventing flicker between songs.
const PAUSE_HIDE_GRACE: Duration = Duration::from_millis(800);

/// Wraps the Windows MediaManager and automatically unregisters its event listeners
/// when dropped (RAII pattern) so we never leak Windows COM callbacks.
struct HookedManager {
    mgr: MediaManager,
    cur_token: Option<EventRegistrationToken>,
    ses_token: Option<EventRegistrationToken>,
}

impl HookedManager {
    fn new(mgr: MediaManager, poller_thread: &Thread, props_changed: &Arc<AtomicBool>) -> Self {
        // Each Windows event uses a distinct EventArgs type under the hood
        // (`CurrentSessionChangedEventArgs` vs `SessionsChangedEventArgs`).
        // Creating a separate `TypedEventHandler::new` for each allows Rust
        // to automatically infer the exact type each Windows API method expects.
        let (t_cur, p_cur) = (poller_thread.clone(), Arc::clone(props_changed));
        let cur_token = mgr
            .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                p_cur.store(true, Ordering::Release);
                t_cur.unpark(); // Wake up the sleeping poller thread immediately
                Ok(())
            }))
            .ok();

        let (t_ses, p_ses) = (poller_thread.clone(), Arc::clone(props_changed));
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

/// Wraps an individual app's MediaSession and unregisters its callbacks when dropped.
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

        // Only subscribe to track metadata & timeline changes on the currently selected app.
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

/// Tracks all hooked sessions so we only re-register Windows event handlers when the
/// list of open media apps or the active session actually changes.
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
                .all(|(h, (s, id))| h.session == *s && &h.app_id == id)
    }
}

/// Reads the album cover thumbnail stream from Windows into a decoded `DynamicImage`.
fn read_thumbnail(thumb_ref: &IRandomAccessStreamReference) -> Option<DynamicImage> {
    let stream = thumb_ref.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > MAX_THUMBNAIL_BYTES {
        return None;
    }

    let _ = stream.Seek(0);
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    let mut buf = Vec::with_capacity(size as usize);

    while (buf.len() as u64) < size {
        let to_request = (size - buf.len() as u64).min(64 * 1024) as u32;
        let Ok(loaded) = reader.LoadAsync(to_request).and_then(|op| op.get()) else {
            let _ = reader.DetachStream();
            return None;
        };
        if loaded == 0 {
            break;
        }
        let unconsumed = reader.UnconsumedBufferLength().unwrap_or(0) as usize;
        if unconsumed == 0 {
            break;
        }
        let prev_len = buf.len();
        buf.resize(prev_len + unconsumed, 0);
        if reader.ReadBytes(&mut buf[prev_len..]).is_err() {
            let _ = reader.DetachStream();
            return None;
        }
    }
    let _ = reader.DetachStream();

    if buf.is_empty() {
        return None;
    }

    let mut img_reader = ImageReader::new(Cursor::new(&buf))
        .with_guessed_format()
        .ok()?;
    // Enforce strict memory and dimension limits to guard against malformed images.
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    img_reader.limits(limits);
    img_reader.decode().ok()
}

/// Extracts just the executable or app name if Windows returns a full file path.
fn normalize_app_id(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .rsplit_once(['\\', '/'])
        .map_or(trimmed, |(_, tail)| tail)
        .to_string()
}

/// Checks if `app_id` passes the user's allow/block filter without allocating strings.
fn app_matches_filter(app_id: &str, filter_mode: &str, app_list: &str) -> bool {
    if filter_mode == "all" || app_list.trim().is_empty() {
        return true;
    }
    let matched = app_list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|token| find_ascii_case_insensitive(app_id, token).is_some());

    match filter_mode {
        "allow" => matched,
        "block" => !matched,
        _ => true,
    }
}

fn is_session_playing(s: &MediaSession) -> bool {
    s.GetPlaybackInfo()
        .and_then(|i| i.PlaybackStatus())
        .map(|st| st == PlaybackStatus::Playing)
        .unwrap_or(false)
}

/// Enumerates all active Windows media sessions, applies the user's app filter,
/// and picks the best session to display (preferring the currently playing app).
fn select_session(
    mgr: &MediaManager,
    filter_mode: &str,
    app_list: &str,
    last_source_app: &str,
) -> (
    Option<(MediaSession, String)>,
    Vec<(MediaSession, String)>,
    Vec<String>,
) {
    let mut discovered_apps = Vec::new();
    let mut allowed_sessions: Vec<(MediaSession, String)> = Vec::new();

    let mut register_session = |s: MediaSession| -> Option<(MediaSession, String)> {
        let app_id = s
            .SourceAppUserModelId()
            .map(|h| normalize_app_id(&h.to_string()))
            .unwrap_or_else(|_| "Unknown".into());
        if !discovered_apps.contains(&app_id) {
            discovered_apps.push(app_id.clone());
        }
        if app_matches_filter(&app_id, filter_mode, app_list) {
            if !allowed_sessions.iter().any(|(_, id)| id == &app_id) {
                allowed_sessions.push((s.clone(), app_id.clone()));
            }
            Some((s, app_id))
        } else {
            None
        }
    };

    if let Ok(list) = mgr.GetSessions() {
        let count = list.Size().unwrap_or(0);
        for i in 0..count {
            if let Ok(s) = list.GetAt(i) {
                let _ = register_session(s);
            }
        }
    }

    let current_allowed = mgr.GetCurrentSession().ok().and_then(register_session);

    // Priority 1: Keep the previously active app if it is still actively playing.
    if !last_source_app.is_empty() {
        if let Some(pair) = allowed_sessions
            .iter()
            .find(|(s, id)| id == last_source_app && is_session_playing(s))
        {
            return (Some(pair.clone()), allowed_sessions, discovered_apps);
        }
    }

    // Priority 2: Switch to any allowed app that is currently playing.
    if let Some(pair) = allowed_sessions.iter().find(|(s, _)| is_session_playing(s)) {
        return (Some(pair.clone()), allowed_sessions, discovered_apps);
    }

    // Priority 3: Stick with the last active app even if paused.
    if !last_source_app.is_empty() {
        if let Some(pair) = allowed_sessions.iter().find(|(_, id)| id == last_source_app) {
            return (Some(pair.clone()), allowed_sessions, discovered_apps);
        }
    }

    // Priority 4: Windows' default current session, or the first allowed session.
    let chosen = current_allowed.or_else(|| allowed_sessions.first().cloned());
    (chosen, allowed_sessions, discovered_apps)
}

struct StepOutcome {
    has_active_track: bool,
    in_pause_grace: bool,
}

/// Spawns the event-driven SMTC background thread. Instead of constantly polling in a
/// tight loop, this thread sleeps (`thread::park()`) and wakes up only when Windows
/// fires a media event or when artwork/grace timers need checking.
pub fn spawn_smtc_poller(
    shared: Arc<Mutex<SharedApp>>,
    running: Arc<AtomicBool>,
    ws_tx: broadcast::Sender<String>,
) -> (Thread, JoinHandle<()>) {
    let (thread_tx, thread_rx) = mpsc::channel::<Thread>();

    let handle = thread::spawn(move || {
        let my_thread = thread::current();
        let _ = thread_tx.send(my_thread.clone());

        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let props_changed = Arc::new(AtomicBool::new(true));
        let mut hooked_mgr: Option<HookedManager> = None;
        let mut hooked_sessions: Option<HookedSessionList> = None;
        let mut last_raw_track_key = String::new();
        let mut last_source_app = String::new();
        let mut last_good_artist = String::new();
        let mut last_good_album = String::new();
        let mut same_context_preserve_art = false;
        let mut track_art_loaded = false;
        let mut track_started_at = Instant::now();
        let mut last_art_attempt: Option<Instant> = None;
        let mut cached_art_hash: u64 = 0;
        let mut cached_art_bytes: Option<Bytes> = None;
        let mut cached_accent: [u8; 3] = [255, 15, 135];
        let mut cached_accent2: [u8; 3] = [0, 210, 255];
        let mut local_anchor_time = Instant::now();
        let mut local_anchor_pos: f64 = 0.0;
        let mut was_playing = false;
        let mut pause_started_at: Option<Instant> = None;

        while running.load(Ordering::Relaxed) {
            let did_props_change = props_changed.swap(false, Ordering::AcqRel);

            let (filter_mode, app_list, clean_titles) = {
                let app = lock_shared(&shared);
                (
                    app.config.filter_mode.clone(),
                    app.config.app_list.clone(),
                    app.config.clean_titles,
                )
            };

            let step = (|| -> windows::core::Result<StepOutcome> {
                if hooked_mgr.is_none() {
                    let mgr = MediaManager::RequestAsync()?.get()?;
                    hooked_mgr = Some(HookedManager::new(mgr, &my_thread, &props_changed));
                }
                let mgr = &hooked_mgr.as_ref().unwrap().mgr;

                let (session_opt, allowed_sessions, active_sessions) =
                    select_session(mgr, &filter_mode, &app_list, &last_source_app);
                let active_id = session_opt.as_ref().map(|(_, id)| id.as_str());

                if !hooked_sessions
                    .as_ref()
                    .is_some_and(|h| h.matches(&allowed_sessions, active_id))
                {
                    hooked_sessions = Some(HookedSessionList::new(
                        &allowed_sessions,
                        active_id,
                        &my_thread,
                        &props_changed,
                    ));
                }

                // Reusable helper for transitioning to idle when no session or title is active.
                let mut handle_no_track = |active_sessions: Vec<String>| -> StepOutcome {
                    if was_playing {
                        let paused_at = *pause_started_at.get_or_insert_with(Instant::now);
                        if paused_at.elapsed() < PAUSE_HIDE_GRACE {
                            return StepOutcome {
                                has_active_track: true,
                                in_pause_grace: true,
                            };
                        }
                    }
                    was_playing = false;
                    pause_started_at = None;
                    let changed = {
                        let mut app = lock_shared(&shared);
                        let ch =
                            app.state.is_playing || app.state.active_sessions != active_sessions;
                        app.state.is_playing = false;
                        app.state.active_sessions = active_sessions;
                        app.state_updated_at = Instant::now();
                        ch
                    };
                    if changed || did_props_change {
                        broadcast_state(&shared, &ws_tx);
                    }
                    StepOutcome {
                        has_active_track: false,
                        in_pause_grace: false,
                    }
                };

                let Some((session, source_app)) = session_opt else {
                    return Ok(handle_no_track(active_sessions));
                };

                let raw_is_playing = is_session_playing(&session);
                let props = session.TryGetMediaPropertiesAsync()?.get()?;

                let raw_title = props
                    .Title()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if raw_title.is_empty() {
                    return Ok(handle_no_track(active_sessions));
                }

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

                let (title, artist) = if clean_titles {
                    sanitize_metadata(&raw_title, &raw_artist)
                } else {
                    (raw_title.clone(), raw_artist.clone())
                };

                let raw_track_key = format!("{source_app}||{raw_title}||{raw_artist}");
                let is_new_track = raw_track_key != last_raw_track_key;

                if is_new_track {
                    let same_app = !last_source_app.is_empty() && source_app == last_source_app;
                    let same_album = same_app
                        && !album.is_empty()
                        && !last_good_album.is_empty()
                        && album.eq_ignore_ascii_case(&last_good_album);
                    let same_artist = same_app
                        && artists_match(&artist, &last_good_artist)
                        && (album.is_empty()
                            || last_good_album.is_empty()
                            || album.eq_ignore_ascii_case(&last_good_album));
                    let is_transitional = same_app && artist.is_empty();

                    same_context_preserve_art = same_album || same_artist || is_transitional;

                    last_raw_track_key = raw_track_key;
                    last_source_app = source_app.clone();
                    if !artist.is_empty() {
                        if !artists_match(&artist, &last_good_artist) || !album.is_empty() {
                            last_good_album = album.clone();
                        }
                        last_good_artist = artist.clone();
                    }

                    track_started_at = Instant::now();
                    last_art_attempt = None;
                    track_art_loaded = false;
                    local_anchor_pos = 0.0;
                    local_anchor_time = Instant::now();

                    if !same_app {
                        cached_accent = [255, 15, 135];
                        cached_accent2 = [0, 210, 255];
                        cached_art_hash = 0;
                        cached_art_bytes = None;
                    }
                }

                // Apply pause grace period so brief gaps between tracks don't hide the widget.
                let is_playing = if raw_is_playing {
                    pause_started_at = None;
                    true
                } else if was_playing {
                    if is_new_track {
                        pause_started_at = Some(Instant::now());
                    }
                    let paused_at = *pause_started_at.get_or_insert_with(Instant::now);
                    if paused_at.elapsed() < PAUSE_HIDE_GRACE {
                        true
                    } else {
                        pause_started_at = None;
                        false
                    }
                } else {
                    pause_started_at = None;
                    false
                };

                // Browsers/players often supply album art a few hundred milliseconds after
                // the track title changes, so retry every 180ms until loaded.
                let retry_due = !track_art_loaded
                    && last_art_attempt.is_none_or(|t| t.elapsed() >= Duration::from_millis(180));

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
                                track_art_loaded = true;
                            }
                        }
                    }
                }

                // Clear stale artwork if the new track still has no art after 1.5s.
                if !track_art_loaded
                    && !same_context_preserve_art
                    && cached_art_hash != 0
                    && track_started_at.elapsed() >= Duration::from_millis(1500)
                {
                    cached_accent = [255, 15, 135];
                    cached_accent2 = [0, 210, 255];
                    cached_art_hash = 0;
                    cached_art_bytes = None;
                }

                let mut position = 0.0;
                let mut duration = 0.0;

                // Calculate smooth playback position by combining Windows' last reported
                // timeline position with monotonic local elapsed time.
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
                    let smtc_pos = if (0.0..36000.0).contains(&age) && raw_is_playing {
                        raw_pos + age
                    } else {
                        raw_pos
                    };

                    let predicted_pos = if raw_is_playing && was_playing {
                        local_anchor_pos + local_anchor_time.elapsed().as_secs_f64()
                    } else {
                        local_anchor_pos
                    };

                    if is_new_track
                        || (raw_is_playing != was_playing)
                        || (smtc_pos - predicted_pos).abs() > 1.5
                    {
                        position = smtc_pos;
                    } else if raw_is_playing {
                        position = (predicted_pos + (smtc_pos - predicted_pos) * 0.10)
                            .max(predicted_pos);
                    } else {
                        position = smtc_pos;
                    }

                    if duration > 0.0 {
                        position = position.clamp(0.0, duration);
                    }
                    local_anchor_pos = position;
                    local_anchor_time = Instant::now();
                }
                was_playing = is_playing;

                let should_broadcast = {
                    let mut app = lock_shared(&shared);
                    let state_changed = app.state.is_playing != is_playing
                        || app.state.title != title
                        || app.state.artist != artist
                        || app.state.album != album
                        || app.state.source_app != source_app
                        || app.state.art_hash != cached_art_hash
                        || app.state.active_sessions != active_sessions;

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

                    state_changed || pause_started_at.is_none()
                };

                if should_broadcast {
                    broadcast_state(&shared, &ws_tx);
                }

                Ok(StepOutcome {
                    has_active_track: true,
                    in_pause_grace: pause_started_at.is_some(),
                })
            })();

            match step {
                Ok(outcome) => {
                    let waiting_for_art = outcome.has_active_track
                        && !track_art_loaded
                        && track_started_at.elapsed() < Duration::from_secs(15);

                    if outcome.in_pause_grace {
                        thread::park_timeout(Duration::from_millis(80));
                    } else if waiting_for_art {
                        thread::park_timeout(Duration::from_millis(250));
                    } else {
                        // Everything is loaded and synced: sleep until an event wakes us up!
                        thread::park();
                    }
                }
                Err(_) => {
                    hooked_sessions = None;
                    hooked_mgr = None;
                    if was_playing {
                        let paused_at = *pause_started_at.get_or_insert_with(Instant::now);
                        if paused_at.elapsed() < PAUSE_HIDE_GRACE {
                            thread::park_timeout(Duration::from_millis(100));
                            continue;
                        }
                    }
                    was_playing = false;
                    pause_started_at = None;
                    let was_active = {
                        let mut app = lock_shared(&shared);
                        let prev = app.state.is_playing;
                        app.state.is_playing = false;
                        app.state_updated_at = Instant::now();
                        prev
                    };
                    if was_active {
                        broadcast_state(&shared, &ws_tx);
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

    let poller_thread = thread_rx
        .recv()
        .expect("Failed to start SMTC poller thread");
    (poller_thread, handle)
}