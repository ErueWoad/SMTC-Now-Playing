use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::{
    Foundation::TypedEventHandler,
    Media::Control::{
        GlobalSystemMediaTransportControlsSession as MediaSession,
        GlobalSystemMediaTransportControlsSessionManager as MediaManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    },
    Storage::Streams::{DataReader, IRandomAccessStreamReference},
    Win32::System::Com::COINIT_MULTITHREADED,
};

use crate::config::{lock_shared, FilterMode, MediaState, SharedApp};
use crate::metadata::{artists_match, find_ascii_case_insensitive, sanitize_metadata};
use crate::server::{broadcast_state, StateHub};
use crate::win32::ComGuard;

const MAX_THUMBNAIL_BYTES: u64 = 16 * 1024 * 1024;
const INITIAL_THUMB_CAP: u64 = 128 * 1024;
const WINDOWS_TO_UNIX_EPOCH_SECS: f64 = 11_644_473_600.0;
const PAUSE_HIDE_GRACE: Duration = Duration::from_millis(800);

struct HookedManager {
    mgr: MediaManager,
    cur_token: Option<i64>,
    ses_token: Option<i64>,
}

impl HookedManager {
    fn new(mgr: MediaManager, poller_thread: &Thread, props_changed: &Arc<AtomicBool>) -> Self {
        let (t_cur, p_cur) = (poller_thread.clone(), Arc::clone(props_changed));
        let cur_token = mgr
            .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                p_cur.store(true, Ordering::Release);
                t_cur.unpark();
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

struct HookedSession {
    session: MediaSession,
    prop_token: Option<i64>,
    play_token: Option<i64>,
    time_token: Option<i64>,
}

impl HookedSession {
    fn new(
        session: MediaSession,
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
    active_session: Option<MediaSession>,
    items: Vec<HookedSession>,
}

impl HookedSessionList {
    fn new(
        allowed: &[(MediaSession, String)],
        active_session: Option<&MediaSession>,
        poller_thread: &Thread,
        props_changed: &Arc<AtomicBool>,
    ) -> Self {
        let items = allowed
            .iter()
            .map(|(s, _)| {
                HookedSession::new(
                    s.clone(),
                    active_session.is_some_and(|act| act == s),
                    poller_thread,
                    props_changed,
                )
            })
            .collect();
        Self {
            active_session: active_session.cloned(),
            items,
        }
    }

    fn matches(
        &self,
        allowed: &[(MediaSession, String)],
        active_session: Option<&MediaSession>,
    ) -> bool {
        self.active_session.as_ref() == active_session
            && self.items.len() == allowed.len()
            && self
                .items
                .iter()
                .zip(allowed.iter())
                .all(|(h, (s, _))| h.session == *s)
    }
}

/// Sniffs magic bytes to determine image MIME type without decoding pixels.
fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

/// Reads raw album cover bytes from Windows SMTC, sniffs its MIME type, and computes a 32-bit FNV-1a hash.
fn read_thumbnail(thumb_ref: &IRandomAccessStreamReference) -> Option<(Arc<[u8]>, &'static str, u32)> {
    let stream = thumb_ref.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > MAX_THUMBNAIL_BYTES {
        return None;
    }

    let _ = stream.Seek(0);
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    let mut buf = Vec::with_capacity(size.min(INITIAL_THUMB_CAP) as usize);

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

    let mime = detect_image_mime(&buf)?;

    // FNV-1a 32-bit hash: lossless in JS `Number` and compact in `MediaState`.
    let mut hash: u32 = 0x811c_9dc5;
    for &b in &buf {
        hash = (hash ^ b as u32).wrapping_mul(0x0100_0193);
    }

    Some((Arc::from(buf), mime, hash.max(1)))
}

fn normalize_app_id(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .rsplit_once(['\\', '/'])
        .map_or(trimmed, |(_, tail)| tail)
        .to_string()
}

fn app_matches_filter(app_id: &str, filter_mode: FilterMode, app_list: &str) -> bool {
    if filter_mode == FilterMode::All || app_list.trim().is_empty() {
        return true;
    }
    let matched = app_list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|token| find_ascii_case_insensitive(app_id, token).is_some());

    match filter_mode {
        FilterMode::Allow => matched,
        FilterMode::Block => !matched,
        FilterMode::All | FilterMode::Unknown => true,
    }
}

fn is_session_playing(s: &MediaSession) -> bool {
    s.GetPlaybackInfo()
        .and_then(|i| i.PlaybackStatus())
        .map(|st| st == PlaybackStatus::Playing)
        .unwrap_or(false)
}

fn select_session(
    mgr: &MediaManager,
    filter_mode: FilterMode,
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
            if !allowed_sessions.iter().any(|(existing, _)| existing == &s) {
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

    if !last_source_app.is_empty() {
        if let Some(pair) = allowed_sessions
            .iter()
            .find(|(s, id)| id == last_source_app && is_session_playing(s))
        {
            return (Some(pair.clone()), allowed_sessions, discovered_apps);
        }
    }

    if let Some(pair) = allowed_sessions.iter().find(|(s, _)| is_session_playing(s)) {
        return (Some(pair.clone()), allowed_sessions, discovered_apps);
    }

    if !last_source_app.is_empty() {
        if let Some(pair) = allowed_sessions.iter().find(|(_, id)| id == last_source_app) {
            return (Some(pair.clone()), allowed_sessions, discovered_apps);
        }
    }

    let chosen = current_allowed.or_else(|| allowed_sessions.first().cloned());
    (chosen, allowed_sessions, discovered_apps)
}

struct StepOutcome {
    has_active_track: bool,
    in_pause_grace: bool,
}

pub fn spawn_smtc_poller(
    shared: Arc<Mutex<SharedApp>>,
    running: Arc<AtomicBool>,
    hub: Arc<StateHub>,
) -> (Thread, JoinHandle<()>) {
    let (thread_tx, thread_rx) = mpsc::channel::<Thread>();

    let handle = thread::spawn(move || {
        let my_thread = thread::current();
        let _ = thread_tx.send(my_thread.clone());

        let _com = ComGuard::init(COINIT_MULTITHREADED);

        let props_changed = Arc::new(AtomicBool::new(true));
        let mut hooked_mgr: Option<HookedManager> = None;
        let mut hooked_sessions: Option<HookedSessionList> = None;
        let mut app_list_buf = String::new();
        let mut last_source_app = String::new();
        let mut last_raw_title = String::new();
        let mut last_raw_artist = String::new();
        let mut last_good_artist = String::new();
        let mut last_good_album = String::new();
        let mut same_context_preserve_art = false;
        let mut track_art_loaded = false;
        let mut track_started_at = Instant::now();
        let mut last_art_attempt: Option<Instant> = None;
        let mut cached_art_hash: u32 = 0;
        let mut cached_art_bytes: Option<Arc<[u8]>> = None;
        let mut cached_art_mime: &'static str = "image/png";
        let mut local_anchor_time = Instant::now();
        let mut local_anchor_pos: f64 = 0.0;
        let mut was_playing = false;
        let mut pause_started_at: Option<Instant> = None;

        while running.load(Ordering::Relaxed) {
            let did_props_change = props_changed.swap(false, Ordering::AcqRel);

            let (filter_mode, clean_titles) = {
                let app = lock_shared(&shared);
                app_list_buf.clone_from(&app.config.app_list);
                (app.config.filter_mode, app.config.clean_titles)
            };

            let step = (|| -> windows::core::Result<StepOutcome> {
                if hooked_mgr.is_none() {
                    let mgr = MediaManager::RequestAsync()?.get()?;
                    hooked_mgr = Some(HookedManager::new(mgr, &my_thread, &props_changed));
                }
                let mgr = &hooked_mgr.as_ref().unwrap().mgr;

                let (session_opt, allowed_sessions, active_sessions) =
                    select_session(mgr, filter_mode, &app_list_buf, &last_source_app);
                let active_session = session_opt.as_ref().map(|(s, _)| s);

                if !hooked_sessions
                    .as_ref()
                    .is_some_and(|h| h.matches(&allowed_sessions, active_session))
                {
                    hooked_sessions = Some(HookedSessionList::new(
                        &allowed_sessions,
                        active_session,
                        &my_thread,
                        &props_changed,
                    ));
                }

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
                        broadcast_state(&shared, &hub);
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

                let is_new_track = source_app != last_source_app
                    || raw_title != last_raw_title
                    || raw_artist != last_raw_artist;

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

                    last_source_app.clone_from(&source_app);
                    last_raw_title = raw_title;
                    last_raw_artist = raw_artist;

                    if !artist.is_empty() {
                        if !artists_match(&artist, &last_good_artist) || !album.is_empty() {
                            last_good_album.clone_from(&album);
                        }
                        last_good_artist.clone_from(&artist);
                    }

                    track_started_at = Instant::now();
                    last_art_attempt = None;
                    track_art_loaded = false;
                    local_anchor_pos = 0.0;
                    local_anchor_time = Instant::now();

                    if !same_app {
                        cached_art_hash = 0;
                        cached_art_bytes = None;
                    }
                }

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

                let retry_due = !track_art_loaded
                    && last_art_attempt.is_none_or(|t| t.elapsed() >= Duration::from_millis(180));

                if is_new_track || did_props_change || retry_due {
                    last_art_attempt = Some(Instant::now());
                    if let Ok(thumb_ref) = props.Thumbnail() {
                        if let Some((art_bytes, mime, hash)) = read_thumbnail(&thumb_ref) {
                            cached_art_hash = hash;
                            cached_art_bytes = Some(art_bytes);
                            cached_art_mime = mime;
                            track_art_loaded = true;
                        }
                    }
                }

                if !track_art_loaded
                    && !same_context_preserve_art
                    && cached_art_hash != 0
                    && track_started_at.elapsed() >= Duration::from_millis(1500)
                {
                    cached_art_hash = 0;
                    cached_art_bytes = None;
                }

                let mut position = 0.0;
                let mut duration = 0.0;

                if let Ok(timeline) = session.GetTimelineProperties() {
                    let start_secs = timeline
                        .StartTime()
                        .map(|t| t.Duration as f64 / 10_000_000.0)
                        .unwrap_or(0.0);
                    let raw_pos = timeline
                        .Position()
                        .map(|t| (t.Duration as f64 / 10_000_000.0) - start_secs)
                        .unwrap_or(0.0)
                        .max(0.0);
                    duration = timeline
                        .EndTime()
                        .map(|t| ((t.Duration as f64 / 10_000_000.0) - start_secs).max(0.0))
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
                    let prev_live_pos = app.live_position();
                    let timeline_changed = (position - prev_live_pos).abs() > 0.5
                        || (duration - app.state.duration).abs() > 0.1;
                    let state_changed = app.state.is_playing != is_playing
                        || app.state.title != title
                        || app.state.artist != artist
                        || app.state.album != album
                        || app.state.source_app != source_app
                        || app.state.art_hash != cached_art_hash
                        || app.state.active_sessions != active_sessions;

                    app.art_data = cached_art_bytes.clone();
                    app.art_mime = cached_art_mime;
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
                    };

                    state_changed || timeline_changed
                };

                if should_broadcast {
                    broadcast_state(&shared, &hub);
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
                        broadcast_state(&shared, &hub);
                    }
                    thread::park_timeout(Duration::from_secs(2));
                }
            }
        }

        drop(hooked_sessions);
        drop(hooked_mgr);
    });

    let poller_thread = thread_rx
        .recv()
        .expect("Failed to start SMTC poller thread");
    (poller_thread, handle)
}