use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::{lock_shared, truncate_utf8_str, Config, SharedApp, StatePayload, HOST, PORT};
use crate::metadata::format_status_text;
use crate::win32::{post_sync_message, set_startup_enabled};

const WIDGET_HTML: &str = include_str!("assets/widget.html");
const SETTINGS_HTML: &str = include_str!("assets/settings.html");

const MAX_HEADER_BYTES: usize = 8192;
const MAX_CONFIG_BODY: usize = 8192;
const MAX_STARTUP_BODY: usize = 1024;
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

struct HubInner {
    version: u64,
    payload: Arc<str>,
    subscribers: usize,
    shutdown: bool,
}

/// Lightweight zero-dependency pub/sub hub for pushing live JSON snapshots over SSE.
pub struct StateHub {
    inner: Mutex<HubInner>,
    cvar: Condvar,
}

enum WaitOutcome {
    Payload(u64, Arc<str>),
    Heartbeat,
    Shutdown,
}

struct SubscriberGuard<'a>(&'a StateHub);

impl Drop for SubscriberGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.subscribers = inner.subscribers.saturating_sub(1);
    }
}

impl StateHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HubInner {
                version: 0,
                payload: Arc::from(""),
                subscribers: 0,
                shutdown: false,
            }),
            cvar: Condvar::new(),
        }
    }

    pub fn has_subscribers(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            > 0
    }

    pub fn publish(&self, json: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.version = inner.version.wrapping_add(1);
        inner.payload = Arc::from(json);
        self.cvar.notify_all();
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.shutdown = true;
        self.cvar.notify_all();
    }

    pub fn is_shutdown(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown
    }

    fn subscribe(&self) -> (SubscriberGuard<'_>, u64, bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.subscribers += 1;
        let ver = inner.version;
        let shutdown = inner.shutdown;
        (SubscriberGuard(self), ver, shutdown)
    }

    fn wait_next(&self, last_version: u64) -> WaitOutcome {
        let deadline = Instant::now() + SSE_HEARTBEAT_INTERVAL;
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        loop {
            if inner.shutdown {
                return WaitOutcome::Shutdown;
            }
            if inner.version != last_version {
                return WaitOutcome::Payload(inner.version, Arc::clone(&inner.payload));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return WaitOutcome::Heartbeat;
            };
            let (guard, timeout) = self
                .cvar
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|p| p.into_inner());
            inner = guard;
            if timeout.timed_out() && inner.version == last_version && !inner.shutdown {
                return WaitOutcome::Heartbeat;
            }
        }
    }
}

#[derive(Clone)]
struct ServerContext {
    shared: Arc<Mutex<SharedApp>>,
    ui_dirty: Arc<AtomicBool>,
    main_thread_id: u32,
    poller_thread: Thread,
    hub: Arc<StateHub>,
}

/// Serializes live state to JSON directly under the lock without mutating `SharedApp`.
pub fn build_state_json(shared: &Mutex<SharedApp>) -> String {
    let app = lock_shared(shared);
    let position = app.live_position();
    serde_json::to_string(&StatePayload {
        state: &app.state,
        position,
        config: &app.config,
        run_on_startup: app.run_on_startup,
    })
    .unwrap_or_default()
}

/// Publishes the latest state snapshot to all connected SSE clients with zero per-client copy.
pub fn broadcast_state(shared: &Mutex<SharedApp>, hub: &StateHub) {
    if hub.has_subscribers() {
        hub.publish(build_state_json(shared));
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    host: Option<String>,
    origin: Option<String>,
    content_type: Option<String>,
    content_length: usize,
    leftover: Vec<u8>,
}

fn is_allowed_host(host: &str) -> bool {
    let Some((name, port)) = host.rsplit_once(':') else {
        return false;
    };
    port.parse::<u16>().ok() == Some(PORT)
        && (name.eq_ignore_ascii_case(HOST) || name.eq_ignore_ascii_case("localhost"))
}

fn is_allowed_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(is_allowed_host)
}

fn is_valid_local_origin_and_host(req: &HttpRequest) -> bool {
    let Some(host) = req.host.as_deref() else {
        return false;
    };
    if !is_allowed_host(host) {
        return false;
    }
    if let Some(origin) = req.origin.as_deref() {
        if !is_allowed_origin(origin) {
            return false;
        }
    }
    true
}

fn is_valid_local_post(req: &HttpRequest) -> bool {
    let has_json_ct = req.content_type.as_deref().is_some_and(|s| {
        s.get(..16)
            .is_some_and(|p| p.eq_ignore_ascii_case("application/json"))
    });
    has_json_ct && is_valid_local_origin_and_host(req)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, target_key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == target_key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn read_request_head(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];

    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return None;
        }
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let req_line = lines.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let _version = parts.next()?;

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut host = None;
    let mut origin = None;
    let mut content_type = None;
    let mut content_length = 0usize;

    for line in lines {
        if let Some((name, val)) = line.split_once(':') {
            let val = val.trim();
            if name.eq_ignore_ascii_case("host") {
                host = Some(val.to_string());
            } else if name.eq_ignore_ascii_case("origin") {
                origin = Some(val.to_string());
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(val.to_string());
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = val.parse().ok()?;
            }
        }
    }

    let leftover = buf[header_end + 4..].to_vec();
    Some(HttpRequest {
        method,
        path,
        query,
        host,
        origin,
        content_type,
        content_length,
        leftover,
    })
}

fn read_request_body(
    stream: &mut TcpStream,
    req: &HttpRequest,
    max_bytes: usize,
) -> Result<Vec<u8>, &'static str> {
    if req.content_length > max_bytes {
        return Err("413 Payload Too Large");
    }
    if req.leftover.len() >= req.content_length {
        return Ok(req.leftover[..req.content_length].to_vec());
    }
    let mut body = vec![0u8; req.content_length];
    let already = req.leftover.len();
    body[..already].copy_from_slice(&req.leftover);
    stream
        .read_exact(&mut body[already..])
        .map_err(|_| "400 Bad Request")?;
    Ok(body)
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra_headers: &str,
    body: &[u8],
) {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).is_ok() {
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }
}

fn handle_sse(mut stream: TcpStream, req: &HttpRequest, ctx: &ServerContext) {
    if !is_valid_local_origin_and_host(req) {
        write_response(&mut stream, "403 Forbidden", "text/plain", "", b"Forbidden");
        return;
    }

    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let (_guard, mut last_version, already_shutdown) = ctx.hub.subscribe();
    if already_shutdown {
        return;
    }

    let headers = "HTTP/1.1 200 OK\r\n\
                   Content-Type: text/event-stream; charset=utf-8\r\n\
                   Cache-Control: no-cache, no-store\r\n\
                   Connection: close\r\n\
                   X-Accel-Buffering: no\r\n\r\n\
                   retry: 1000\n\n";
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }

    let initial_json = build_state_json(&ctx.shared);
    if write!(stream, "data: {initial_json}\n\n").is_err() || stream.flush().is_err() {
        return;
    }

    loop {
        match ctx.hub.wait_next(last_version) {
            WaitOutcome::Payload(ver, payload) => {
                last_version = ver;
                if write!(stream, "data: {payload}\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
            WaitOutcome::Heartbeat => {
                if stream.write_all(b": keepalive\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
            WaitOutcome::Shutdown => break,
        }
    }
}

fn handle_connection(mut stream: TcpStream, ctx: ServerContext) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let Some(req) = read_request_head(&mut stream) else {
        return;
    };

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/events") => handle_sse(stream, &req, &ctx),

        ("GET", "/" | "/index.html") => {
            write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                "Cache-Control: no-store\r\n",
                WIDGET_HTML.as_bytes(),
            );
        }

        ("GET", "/settings") => {
            write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                "Cache-Control: no-store\r\n",
                SETTINGS_HTML.as_bytes(),
            );
        }

        ("GET", "/favicon.ico") => {
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
        }

        ("GET", "/state") => {
            let json = build_state_json(&ctx.shared);
            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                "Cache-Control: no-store\r\n",
                json.as_bytes(),
            );
        }

        ("GET", "/art") => {
            let (art_opt, mime) = {
                let app = lock_shared(&ctx.shared);
                (app.art_data.clone(), app.art_mime)
            };
            if let Some(bytes) = art_opt {
                let has_version = query_param(&req.query, "v")
                    .as_deref()
                    .is_some_and(|v| !v.is_empty());
                let extra = if has_version {
                    "Cache-Control: public, max-age=86400, immutable\r\nAccess-Control-Allow-Origin: *\r\n"
                } else {
                    "Cache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\n"
                };
                write_response(&mut stream, "200 OK", mime, extra, &bytes);
            } else {
                write_response(
                    &mut stream,
                    "404 Not Found",
                    "text/plain",
                    "Cache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\n",
                    b"No art",
                );
            }
        }

        ("GET", "/text") => {
            let fmt_owned = query_param(&req.query, "format");
            let custom_fmt = fmt_owned.as_deref().map(|s| truncate_utf8_str(s, 512));
            let text_out = {
                let app = lock_shared(&ctx.shared);
                format_status_text(&app.state, app.live_position(), &app.config, custom_fmt)
            };
            write_response(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                "Cache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\n",
                text_out.as_bytes(),
            );
        }

        ("POST", "/startup") => {
            if !is_valid_local_post(&req) {
                write_response(&mut stream, "403 Forbidden", "text/plain", "", b"Forbidden");
                return;
            }
            let body = match read_request_body(&mut stream, &req, MAX_STARTUP_BODY) {
                Ok(b) => b,
                Err(status) => {
                    write_response(&mut stream, status, "text/plain", "", status.as_bytes());
                    return;
                }
            };
            #[derive(Deserialize)]
            struct StartupPayload {
                enabled: bool,
            }
            let Ok(payload) = serde_json::from_slice::<StartupPayload>(&body) else {
                write_response(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    "",
                    b"Invalid JSON",
                );
                return;
            };

            let actual = set_startup_enabled(payload.enabled);
            lock_shared(&ctx.shared).run_on_startup = actual;
            ctx.ui_dirty.store(true, Ordering::Release);
            post_sync_message(ctx.main_thread_id);
            broadcast_state(&ctx.shared, &ctx.hub);

            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                "Cache-Control: no-store\r\n",
                b"{\"ok\":true}",
            );
        }

        ("POST", "/config") => {
            if !is_valid_local_post(&req) {
                write_response(&mut stream, "403 Forbidden", "text/plain", "", b"Forbidden");
                return;
            }
            let body = match read_request_body(&mut stream, &req, MAX_CONFIG_BODY) {
                Ok(b) => b,
                Err(status) => {
                    write_response(&mut stream, status, "text/plain", "", status.as_bytes());
                    return;
                }
            };
            let Ok(new_cfg) = serde_json::from_slice::<Config>(&body) else {
                write_response(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    "",
                    b"Invalid JSON",
                );
                return;
            };

            {
                let mut app = lock_shared(&ctx.shared);
                app.config = new_cfg.clamped();
                app.mark_config_dirty();
            }
            ctx.ui_dirty.store(true, Ordering::Release);
            post_sync_message(ctx.main_thread_id);
            ctx.poller_thread.unpark();
            broadcast_state(&ctx.shared, &ctx.hub);

            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                "Cache-Control: no-store\r\n",
                b"{\"ok\":true}",
            );
        }

        _ => {
            write_response(&mut stream, "404 Not Found", "text/plain", "", b"Not Found");
        }
    }
}

/// Spawns a lightweight synchronous HTTP/1.1 + SSE server on a dedicated background thread.
pub fn spawn_http_server(
    listener: TcpListener,
    shared: Arc<Mutex<SharedApp>>,
    ui_dirty: Arc<AtomicBool>,
    main_thread_id: u32,
    poller_thread: Thread,
    hub: Arc<StateHub>,
) -> JoinHandle<()> {
    let ctx = ServerContext {
        shared,
        ui_dirty,
        main_thread_id,
        poller_thread,
        hub,
    };

    thread::spawn(move || {
        for incoming in listener.incoming() {
            if ctx.hub.is_shutdown() {
                break;
            }
            let Ok(stream) = incoming else {
                continue;
            };
            let conn_ctx = ctx.clone();
            let _ = thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || handle_connection(stream, conn_ctx));
        }
    })
}