use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle, Thread};

use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use tokio::sync::{broadcast, watch};

use crate::config::{lock_shared, Config, SharedApp, StatePayload};
use crate::metadata::format_status_text;
use crate::win32::set_startup_enabled;

// Embeds the HTML files directly inside the compiled .exe binary at compile time.
const WIDGET_HTML: &str = include_str!("assets/widget.html");
const SETTINGS_HTML: &str = include_str!("assets/settings.html");

/// Shared resources passed to Axum route handlers.
#[derive(Clone)]
struct AppState {
    shared: Arc<Mutex<SharedApp>>,
    ui_dirty: Arc<AtomicBool>,
    poller_thread: Thread,
    ws_tx: broadcast::Sender<String>,
    shutdown_rx: watch::Receiver<bool>,
}

/// Updates the live track position and serializes the state to JSON directly inside
/// the lock, avoiding cloning any `MediaState` or `Config` strings.
pub fn build_state_json(shared: &Mutex<SharedApp>) -> String {
    let mut app = lock_shared(shared);
    app.sync_live_position();
    serde_json::to_string(&StatePayload {
        state: &app.state,
        config: &app.config,
        run_on_startup: app.run_on_startup,
    })
    .unwrap_or_default()
}

/// Pushes the latest JSON state to all connected WebSocket clients (if any).
pub fn broadcast_state(shared: &Mutex<SharedApp>, ws_tx: &broadcast::Sender<String>) {
    if ws_tx.receiver_count() > 0 {
        let _ = ws_tx.send(build_state_json(shared));
    }
}

/// Security check: ensures POST requests came from our own local settings page
/// and not a malicious external website visited in the user's browser (CSRF protection).
fn is_valid_local_post(headers: &HeaderMap) -> bool {
    let has_json_ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            s.get(..16)
                .is_some_and(|p| p.eq_ignore_ascii_case("application/json"))
        });
    if !has_json_ct {
        return false;
    }

    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if origin != "http://127.0.0.1:8765" && origin != "http://localhost:8765" {
            return false;
        }
    }

    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if host != "127.0.0.1:8765" && host != "localhost:8765" {
            return false;
        }
    }

    true
}

/// Upgrades an HTTP request at `/ws` into a persistent real-time WebSocket connection.
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_ws_client(socket, state))
}

/// Manages a single connected WebSocket client (OBS Browser Source or Settings tab).
async fn handle_ws_client(mut socket: WebSocket, state: AppState) {
    let mut rx = state.ws_tx.subscribe();
    let mut shutdown_rx = state.shutdown_rx.clone();

    // Immediately send the current track state upon connection.
    let initial_json = build_state_json(&state.shared);
    if socket
        .send(Message::Text(initial_json.into()))
        .await
        .is_err()
    {
        return;
    }

    // `tokio::select!` waits on multiple async events simultaneously and runs
    // whichever branch completes first.
    loop {
        tokio::select! {
            // Branch 1: A new state broadcast arrived from the SMTC poller or settings UI.
            res = rx.recv() => {
                let payload = match res {
                    Ok(msg) => msg,
                    Err(broadcast::error::RecvError::Lagged(_)) => build_state_json(&state.shared),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            // Branch 2: The browser sent a message or closed the WebSocket.
            msg_opt = socket.recv() => {
                if matches!(msg_opt, Some(Ok(Message::Close(_))) | None | Some(Err(_))) {
                    break;
                }
            }
            // Branch 3: The application is exiting.
            _ = shutdown_rx.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    }
}

/// `GET /state` — Returns current track metadata and configuration as JSON.
async fn get_state(State(state): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        build_state_json(&state.shared),
    )
        .into_response()
}

/// `GET /art` — Serves the current album cover PNG with zero-copy `Bytes`.
async fn get_art(State(state): State<AppState>) -> Response {
    // Cloning `Bytes` only increments an atomic reference counter, without copying the image!
    let png_opt = lock_shared(&state.shared).art_png.clone();
    if let Some(png_bytes) = png_opt {
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            ],
            png_bytes,
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            ],
            "No art",
        )
            .into_response()
    }
}

/// `GET /text` — Returns a formatted plain-text string for chat bots (Nightbot, Streamer.bot, etc.).
async fn get_text(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let custom_fmt = params.get("format").map(String::as_str);
    let text_out = {
        let mut app = lock_shared(&state.shared);
        app.sync_live_position();
        format_status_text(&app.state, &app.config, custom_fmt)
    };
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        text_out,
    )
        .into_response()
}

/// `POST /startup` — Enables or disables launching the app at Windows login.
async fn post_startup(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_valid_local_post(&headers) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    if body.len() > 1024 {
        return (StatusCode::BAD_REQUEST, "Bad Request").into_response();
    }
    #[derive(Deserialize)]
    struct StartupPayload {
        enabled: bool,
    }
    let Ok(payload) = serde_json::from_slice::<StartupPayload>(&body) else {
        return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
    };

    let actual = set_startup_enabled(payload.enabled);
    lock_shared(&state.shared).run_on_startup = actual;
    state.ui_dirty.store(true, Ordering::Release);
    broadcast_state(&state.shared, &state.ws_tx);

    ([(header::CONTENT_TYPE, "application/json")], "{\"ok\":true}").into_response()
}

/// `POST /config` — Updates widget settings and triggers a debounced save to disk.
async fn post_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_valid_local_post(&headers) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    if body.len() > 8192 {
        return (StatusCode::BAD_REQUEST, "Bad Request").into_response();
    }
    let Ok(new_cfg) = serde_json::from_slice::<Config>(&body) else {
        return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
    };

    {
        let mut app = lock_shared(&state.shared);
        app.config = new_cfg.clamped();
        app.mark_config_dirty();
    }
    state.ui_dirty.store(true, Ordering::Release);
    state.poller_thread.unpark();
    broadcast_state(&state.shared, &state.ws_tx);

    ([(header::CONTENT_TYPE, "application/json")], "{\"ok\":true}").into_response()
}

/// Spawns a lightweight single-threaded Tokio runtime on a dedicated background thread
/// to serve HTTP and WebSocket traffic.
pub fn spawn_http_server(
    std_listener: TcpListener,
    shared: Arc<Mutex<SharedApp>>,
    ui_dirty: Arc<AtomicBool>,
    poller_thread: Thread,
    ws_tx: broadcast::Sender<String>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime");

        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener)
                .expect("Failed to bind async TCP listener");

            let app_state = AppState {
                shared,
                ui_dirty,
                poller_thread,
                ws_tx,
                shutdown_rx: shutdown_rx.clone(),
            };

            let app = Router::new()
                .route("/ws", get(ws_handler))
                .route("/state", get(get_state))
                .route("/art", get(get_art))
                .route("/text", get(get_text))
                .route("/startup", post(post_startup))
                .route("/config", post(post_config))
                .route("/settings", get(|| async { Html(SETTINGS_HTML) }))
                .route("/", get(|| async { Html(WIDGET_HTML) }))
                .route("/index.html", get(|| async { Html(WIDGET_HTML) }))
                .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
                .with_state(app_state);

            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await;
        });
    })
}