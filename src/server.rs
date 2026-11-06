use std::io::{Cursor, Read};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle, Thread};

use serde::Deserialize;
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::config::{lock_shared, Config, SharedApp, StatePayload};
use crate::metadata::format_status_text;
use crate::win32::set_startup_enabled;

const WIDGET_HTML: &str = include_str!("assets/widget.html");
const SETTINGS_HTML: &str = include_str!("assets/settings.html");

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
        .take(max_bytes + 1)
        .read_to_string(&mut body)
        .ok()?;
    if body.len() as u64 > max_bytes {
        return None;
    }
    Some(body)
}

pub fn spawn_http_server(
    shared: Arc<Mutex<SharedApp>>,
    running: Arc<AtomicBool>,
    ui_dirty: Arc<AtomicBool>,
    server: Arc<Server>,
    poller_thread: Thread,
) -> JoinHandle<()> {
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

            let cors_wildcard =
                Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap();

            match (request.method(), url_path) {
                (&Method::Get, "/state") => {
                    let (live, cfg, run_on_startup) = {
                        let app = lock_shared(&shared);
                        (app.live_state(), app.config.clone(), app.run_on_startup)
                    };
                    let body = serde_json::to_vec(&StatePayload {
                        state: &live,
                        config: &cfg,
                        run_on_startup,
                    })
                    .unwrap_or_default();

                    let content_type =
                        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                    let cache_ctrl =
                        Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
                    let _ = request.respond(
                        Response::from_data(body)
                            .with_header(content_type)
                            .with_header(cache_ctrl),
                    );
                }
                (&Method::Get, "/art") => {
                    let png_opt = lock_shared(&shared).art_png.clone();
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
                            vec![content_type, cache_ctrl, cors_wildcard],
                            Cursor::new(png_bytes),
                            Some(len),
                            None,
                        ));
                    } else {
                        let cache_ctrl =
                            Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
                        let _ = request.respond(
                            Response::from_string("No art")
                                .with_header(cache_ctrl)
                                .with_header(cors_wildcard)
                                .with_status_code(404),
                        );
                    }
                }
                (&Method::Get, "/text") => {
                    let custom_fmt = parse_query_param(query_str, "format");
                    let text_out = {
                        let app = lock_shared(&shared);
                        let live = app.live_state();
                        format_status_text(&live, &app.config, custom_fmt.as_deref())
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
                            .with_header(cors_wildcard),
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
                    let Some(body) = read_bounded_body(&mut request, 1024) else {
                        let _ = request
                            .respond(Response::from_string("Bad Request").with_status_code(400));
                        continue;
                    };
                    let Ok(payload) = serde_json::from_str::<StartupPayload>(&body) else {
                        let _ = request
                            .respond(Response::from_string("Invalid JSON").with_status_code(400));
                        continue;
                    };
                    let actual = set_startup_enabled(payload.enabled);
                    lock_shared(&shared).run_on_startup = actual;
                    ui_dirty.store(true, Ordering::Release);

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
                    let Some(body) = read_bounded_body(&mut request, 8192) else {
                        let _ = request
                            .respond(Response::from_string("Bad Request").with_status_code(400));
                        continue;
                    };
                    let Ok(new_cfg) = serde_json::from_str::<Config>(&body) else {
                        let _ = request
                            .respond(Response::from_string("Invalid JSON").with_status_code(400));
                        continue;
                    };
                    {
                        let mut app = lock_shared(&shared);
                        app.config = new_cfg.clamped();
                        app.mark_config_dirty();
                    }
                    ui_dirty.store(true, Ordering::Release);
                    poller_thread.unpark();

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
                (&Method::Get, "/") | (&Method::Get, "/index.html") => {
                    let content_type =
                        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                            .unwrap();
                    let _ = request
                        .respond(Response::from_string(WIDGET_HTML).with_header(content_type));
                }
                (&Method::Get, "/favicon.ico") => {
                    let _ = request.respond(Response::empty(204));
                }
                _ => {
                    let _ = request
                        .respond(Response::from_string("Not Found").with_status_code(404));
                }
            }
        }
    })
}