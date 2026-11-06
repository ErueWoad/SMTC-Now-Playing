use std::io::Cursor;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use image::{DynamicImage, ImageReader, Limits};
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
use crate::metadata::{artists_match, sanitize_metadata};
use crate::palette::process_artwork;

const MAX_THUMBNAIL_BYTES: u64 = 16 * 1024 * 1024;
const WINDOWS_TO_UNIX_EPOCH_SECS: f64 = 11_644_473_600.0;

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
                .all(|(h, (s, id))| h.session == *s && &h.app_id == id)
    }
}

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
        let loaded = match reader.LoadAsync(to_request).and_then(|op| op.get()) {
            Ok(n) => n,
            Err(_) => {
                let _ = reader.DetachStream();
                let _ = stream.Seek(0);
                return None;
            }
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
            let _ = stream.Seek(0);
            return None;
        }
    }
    let _ = reader.DetachStream();
    let _ = stream.Seek(0);

    if buf.is_empty() {
        return None;
    }

    let mut img_reader = ImageReader::new(Cursor::new(&buf))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    img_reader.limits(limits);
    img_reader.decode().ok()
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
    last_source_app: &str,
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

    if !last_source_app.is_empty() {
        for (s, id) in &allowed_sessions {
            if id == last_source_app {
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
        }
    }

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

    if !last_source_app.is_empty() {
        if let Some(found) = allowed_sessions.iter().find(|(_, id)| id == last_source_app) {
            return (Some(found.clone()), allowed_sessions, discovered_apps);
        }
    }

    if let Some(cur) = current_allowed {
        return (Some(cur), allowed_sessions, discovered_apps);
    }

    let first = allowed_sessions.first().cloned();
    (first, allowed_sessions, discovered_apps)
}

pub fn spawn_smtc_poller(
    shared: Arc<Mutex<SharedApp>>,
    running: Arc<AtomicBool>,
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
        let mut cached_art_bytes: Option<Arc<[u8]>> = None;
        let mut cached_accent: [u8; 3] = [255, 15, 135];
        let mut cached_accent2: [u8; 3] = [0, 210, 255];
        let mut local_anchor_time = Instant::now();
        let mut local_anchor_pos: f64 = 0.0;
        let mut was_playing = false;

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

            let step = (|| -> windows::core::Result<bool> {
                if hooked_mgr.is_none() {
                    let mgr = MediaManager::RequestAsync()?.get()?;
                    hooked_mgr = Some(HookedManager::new(mgr, &my_thread, &props_changed));
                }
                let mgr = &hooked_mgr.as_ref().unwrap().mgr;

                let (session_opt, allowed_sessions, active_sessions) =
                    select_session(mgr, &filter_mode, &app_list, &last_source_app);
                let active_id = session_opt.as_ref().map(|(_, id)| id.as_str());

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
                    let mut app = lock_shared(&shared);
                    app.state.is_playing = false;
                    app.state.active_sessions = active_sessions;
                    app.state_updated_at = Instant::now();
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
                    let mut app = lock_shared(&shared);
                    app.state.is_playing = false;
                    app.state.active_sessions = active_sessions;
                    app.state_updated_at = Instant::now();
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

                let retry_due = !track_art_loaded
                    && last_art_attempt
                        .map(|t| t.elapsed() >= Duration::from_millis(180))
                        .unwrap_or(true);

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

                let mut app = lock_shared(&shared);
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

                Ok(true)
            })();

            match step {
                Ok(has_active_track) => {
                    let waiting_for_art = has_active_track
                        && !track_art_loaded
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
                    {
                        let mut app = lock_shared(&shared);
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

    let poller_thread = thread_rx.recv().expect("Failed to start SMTC poller thread");
    (poller_thread, handle)
}