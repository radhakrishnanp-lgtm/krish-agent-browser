//! Voice agent server: serves the UI and the /ws voice WebSocket.
//!
//! Run from the voice-agent folder:  cargo run --release
mod cli;
mod config;
mod pricing;
mod providers;
mod session;
mod store;
mod vad;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use session::{Out, Session, Shared};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

const AUTH_COOKIE: &str = "va_auth";
const MAX_WS_MESSAGE: usize = 2 * 1024 * 1024; // far above any audio frame the UI sends

pub fn log(level: &str, message: &str) {
    let label = match level {
        "warn" => "WARNING",
        "error" => "ERROR",
        _ => "INFO",
    };
    eprintln!("{} {:<7} {}", chrono::Local::now().format("%H:%M:%S"), label, message);
}

struct App {
    sh: Arc<Shared>,
    ui_dir: PathBuf,
    sessions_dir: PathBuf,
    access_token: String,
    /// The cookie holds a value derived from the token, so the token itself never appears in
    /// a URL, a log line or page-readable storage.
    auth_value: String,
    /// Extra host names the UI may be served from. Needed only when a proxy in front of the
    /// app replaces the Host header with something else.
    allowed_origins: HashSet<String>,
    max_connections: usize,
    connections: AtomicUsize,
}

fn same(a: &str, b: &str) -> bool {
    // constant-time comparison
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

impl App {
    /// True if no token is configured, or the request carries the login cookie or a Bearer token.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        if self.access_token.is_empty() {
            return true;
        }
        let cookie = header(headers, "cookie")
            .split(';')
            .filter_map(|c| c.trim().split_once('='))
            .find(|(k, _)| *k == AUTH_COOKIE)
            .map(|(_, v)| v)
            .unwrap_or("");
        if same(cookie, &self.auth_value) {
            return true;
        }
        header(headers, "authorization").strip_prefix("Bearer ").map(|t| same(t, &self.access_token)).unwrap_or(false)
    }

    /// Blocks other websites from opening the socket from a visitor's browser (cross-site
    /// WebSocket hijacking). Non-browser clients send no Origin and are allowed.
    fn same_origin(&self, headers: &HeaderMap) -> bool {
        let origin = header(headers, "origin");
        if origin.is_empty() {
            return true;
        }
        let host = origin.split_once("://").map(|(_, h)| h).unwrap_or(origin).to_lowercase();
        host == header(headers, "host").to_lowercase()
            || (!header(headers, "x-forwarded-host").is_empty() && host == header(headers, "x-forwarded-host").to_lowercase())
            || self.allowed_origins.contains(&host)
    }
}

fn file_response(path: &Path, content_type: &str, download_as: Option<&str>) -> Response {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut resp = Response::new(Body::from(bytes));
            resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
            if let Some(name) = download_as {
                if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", name)) {
                    resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
                }
            }
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

fn json_error(status: StatusCode, detail: &str) -> Response {
    (status, axum::Json(json!({"detail": detail}))).into_response()
}

async fn security_headers(req: Request, next: Next) -> Response {
    let api = req.uri().path().starts_with("/api/");
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    // Only this site's own scripts, styles and audio; no inline script, no framing.
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self' ws: wss:; media-src 'self' blob:; img-src 'self' data:; \
             object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("permissions-policy", HeaderValue::from_static("microphone=(self), camera=(), geolocation=()"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    if api {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    }
    resp
}

async fn healthz(State(app): State<Arc<App>>) -> Response {
    let cfg = app.sh.config();
    axum::Json(json!({"ok": true, "preset": cfg.active_preset, "missing_keys": cfg.missing_keys(&cfg.active_preset)})).into_response()
}

async fn login(State(app): State<Arc<App>>, headers: HeaderMap, body: String) -> Response {
    let token = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("token").and_then(|t| t.as_str()).map(String::from)).unwrap_or_default();
    if app.access_token.is_empty() || !same(&token, &app.access_token) {
        tokio::time::sleep(Duration::from_secs(1)).await; // slows down token guessing
        return json_error(StatusCode::UNAUTHORIZED, "Wrong access token");
    }
    let secure = header(&headers, "x-forwarded-proto") == "https";
    let cookie = format!(
        "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Strict{}",
        AUTH_COOKIE,
        app.auth_value,
        60 * 60 * 24 * 90,
        if secure { "; Secure" } else { "" }
    );
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    resp
}

async fn session_log(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, headers: HeaderMap) -> Response {
    if !app.authorized(&headers) {
        return json_error(StatusCode::UNAUTHORIZED, "Sign in with the access token first");
    }
    let ok = (20..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-');
    let path = app.sessions_dir.join(format!("{}.json", id));
    if !ok || !path.exists() {
        return json_error(StatusCode::NOT_FOUND, "Session log not found (it is written after the first turn)");
    }
    file_response(&path, "application/json", Some(&format!("{}.json", id)))
}

async fn message_audio(State(app): State<Arc<App>>, UrlPath((conv_id, name)): UrlPath<(String, String)>, headers: HeaderMap) -> Response {
    if !app.authorized(&headers) {
        return json_error(StatusCode::UNAUTHORIZED, "Sign in with the access token first");
    }
    match app.sh.store.audio_path(&conv_id, &name).filter(|p| p.exists()) {
        Some(path) => file_response(&path, "audio/wav", None),
        None => json_error(StatusCode::NOT_FOUND, "Recording not found"),
    }
}

async fn conversation_recording(State(app): State<Arc<App>>, UrlPath(conv_id): UrlPath<String>, headers: HeaderMap) -> Response {
    if !app.authorized(&headers) {
        return json_error(StatusCode::UNAUTHORIZED, "Sign in with the access token first");
    }
    let sh = app.sh.clone();
    let id = conv_id.clone();
    let data = tokio::task::spawn_blocking(move || sh.store.load(&id).and_then(|c| sh.store.full_recording(&c))).await.ok().flatten();
    match data {
        Some(bytes) => {
            let mut resp = Response::new(Body::from(bytes));
            resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
            if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"conversation-{}.wav\"", conv_id)) {
                resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
            }
            resp
        }
        None => json_error(StatusCode::NOT_FOUND, "This conversation has no recorded audio"),
    }
}

/// The UI: a handful of static files, served by name from ui/ only.
async fn ui_file(State(app): State<Arc<App>>, req: Request) -> Response {
    let name = req.uri().path().trim_start_matches('/');
    let name = if name.is_empty() { "index.html" } else { name };
    let safe = !name.contains("..") && !name.contains('/') && !name.contains('\\') && !name.starts_with('.');
    let content_type = match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" | "webmanifest" => "application/json",
        _ => "",
    };
    if !safe || content_type.is_empty() {
        return json_error(StatusCode::NOT_FOUND, "Not Found");
    }
    let mut resp = file_response(&app.ui_dir.join(name), content_type, None);
    if resp.status() == StatusCode::OK {
        resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    resp
}

async fn ws_handler(State(app): State<Arc<App>>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    if !app.same_origin(&headers) {
        log(
            "warn",
            &format!(
                "Refused WebSocket from origin {} (Host is {}). If this is your own site behind a proxy, set ALLOWED_ORIGINS to its host name.",
                header(&headers, "origin"),
                header(&headers, "host")
            ),
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    let authorized = app.authorized(&headers);
    ws.max_message_size(MAX_WS_MESSAGE).max_frame_size(MAX_WS_MESSAGE).on_upgrade(move |socket| serve(app, socket, authorized))
}

async fn close(mut socket: WebSocket, code: u16, reason: &'static str) {
    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
}

async fn serve(app: Arc<App>, socket: WebSocket, authorized: bool) {
    if !authorized {
        return close(socket, 4401, "Sign in first").await;
    }
    if app.connections.fetch_add(1, Ordering::SeqCst) >= app.max_connections {
        app.connections.fetch_sub(1, Ordering::SeqCst);
        return close(socket, 1013, "Too many connections").await;
    }
    connection(&app, socket).await;
    app.connections.fetch_sub(1, Ordering::SeqCst);
}

async fn connection(app: &Arc<App>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
    let writer = tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            let msg = match item {
                Out::Json(v) => Message::Text(v.to_string().into()),
                Out::Audio(bytes) => Message::Binary(bytes.into()),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let session = match Session::new(app.sh.clone(), tx.clone()) {
        Ok(s) => s,
        Err(e) => {
            let text = format!("Config error: {}", e);
            log("error", &text);
            let _ = tx.send(Out::Json(json!({"type": "log", "level": "error", "message": text})));
            let _ = tx.send(Out::Json(json!({"type": "error", "message": text})));
            drop(tx);
            let _ = writer.await;
            return;
        }
    };
    session.send_hello();
    session.log("info", &format!("Session {} started with preset '{}'", session.id, session.preset()), None);
    for key in session.missing_keys() {
        session.log("error", &format!("{} is not set on the server. Add it to voice-agent/.env and restart.", key), None);
    }
    session.send_conversations();
    session.emit_cost();
    session.send(json!({"type": "status", "state": "listening"}));
    session.warm();

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Binary(bytes) => session.on_audio(&bytes).await,
            Message::Text(text) => {
                let Ok(data) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if data.is_object() {
                    handle(&session, &data).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    let (turns, total) = session.close().await;
    if turns > 0 {
        log("info", &format!("Session {} ended: {} turn(s), total ${:.6}, log {}", session.id, turns, total, session.log_path.display()));
    }
    drop(session);
    drop(tx);
    writer.abort();
}

async fn handle(session: &Arc<Session>, data: &Value) {
    let s = |key: &str| -> String {
        match data.get(key) {
            Some(Value::String(v)) => v.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    };
    match data.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "text" => {
            let text: String = s("text").trim().chars().take(2000).collect();
            if !text.is_empty() {
                session.text_turn(text);
            }
        }
        "new" => {
            let greet = data.get("greet").and_then(|g| g.as_bool()).unwrap_or(true);
            session.new_conversation(data.get("mode").and_then(|m| m.as_str()), greet).await
        }
        "close" => session.close_conversation().await,
        "open" => session.open_conversation(&s("id")).await,
        "delete" => session.delete_conversation(&s("id")).await,
        "rename" => session.rename_conversation(&s("id"), &s("title")),
        "folder_create" | "folder_rename" | "folder_delete" | "folder_order" | "move" => session.folder_action(data),
        "settings" => session.apply_settings(data),
        "set_instructions" => session.set_instructions(data),
        "review" => session.start_review(),
        "save_mode" => session.save_custom_mode(data),
        "delete_mode" => session.delete_custom_mode(&s("id")),
        "set_models" => {
            session.cancel_turn().await;
            session.set_models(data, false);
        }
        "add_model" => session.add_custom_model(data),
        "delete_model" => {
            session.cancel_turn().await;
            session.delete_custom_model(&s("id"));
        }
        "test_model" => {
            let kind = s("kind");
            if config::KINDS.contains(&kind.as_str()) {
                session.spawn_test(kind, s("id"));
            }
        }
        "set_preset" => {
            session.cancel_turn().await;
            session.set_preset(&s("name"));
        }
        "raw" => session.set_show_raw(data.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false)),
        "stop" => session.stop().await,
        _ => {}
    }
}

fn find_runtime(root: &Path) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let lib = if cfg!(target_os = "macos") { "libonnxruntime.dylib" } else if cfg!(windows) { "onnxruntime.dll" } else { "libonnxruntime.so" };
    let candidates = [root.join("vendor/onnxruntime/lib").join(lib), PathBuf::from("/usr/local/lib").join(lib), PathBuf::from("/usr/lib").join(lib)];
    candidates.iter().find(|p| p.exists()).cloned().ok_or_else(|| {
        format!(
            "ONNX Runtime ({}) was not found. Run ./scripts/get-onnxruntime.sh, or set ORT_DYLIB_PATH to the library file.",
            lib
        )
    })
}

fn fatal(message: &str) -> ! {
    log("error", message);
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let root = std::env::var("VOICE_AGENT_ROOT").map(PathBuf::from).unwrap_or_else(|_| std::env::current_dir().unwrap());
    let _ = dotenvy::from_path(root.join(".env"));

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--check") => std::process::exit(cli::check(&root)),
        Some("--smoke") => std::process::exit(cli::smoke(&root, args.get(1).cloned()).await),
        Some("--help") | Some("-h") => {
            println!("voice-agent            start the server\nvoice-agent --check    validate the config (free, no network)\nvoice-agent --smoke [preset]   one real call to TTS, STT and LLM");
            return;
        }
        _ => {}
    }

    let cfg = config::Config::load(&root).unwrap_or_else(|e| fatal(&format!("Config error: {}", e)));
    let stages = cfg.resolve(&cfg.active_preset).unwrap_or_else(|e| fatal(&format!("Config error: {}", e)));
    let summary: Vec<String> = config::KINDS.iter().map(|k| format!("{}: {}/{}", k, stages[*k].provider, stages[*k].model)).collect();
    log("info", &format!("Active preset '{}': {}", cfg.active_preset, summary.join(", ")));
    for key in cfg.missing_keys(&cfg.active_preset) {
        log("warn", &format!("{} is not set. Add it to voice-agent/.env; calls that need it will fail.", key));
    }

    let access_token = std::env::var("ACCESS_TOKEN").unwrap_or_default().trim().to_string();
    if access_token.is_empty() {
        log("warn", "ACCESS_TOKEN is not set: anyone who can reach this server can use your API credits.");
    }
    let auth_value = Sha256::digest(format!("voice-agent:{}", access_token).as_bytes()).iter().map(|b| format!("{:02x}", b)).collect::<String>();
    let allowed_origins: HashSet<String> =
        std::env::var("ALLOWED_ORIGINS").unwrap_or_default().split(',').map(|h| h.trim().to_lowercase()).filter(|h| !h.is_empty()).collect();

    let runtime = find_runtime(&root).unwrap_or_else(|e| fatal(&e));
    let vad = vad::Vad::load(&root.join("assets/silero_vad.onnx"), &runtime).unwrap_or_else(|e| fatal(&e));
    let store = store::Store::open(
        root.join(cfg.str(&["storage", "conversations_dir"], "data/conversations")),
        root.join(cfg.str(&["storage", "audio_dir"], "data/audio")),
    )
    .unwrap_or_else(|e| fatal(&format!("Could not open the data folder: {}", e)));
    // Long keep-alive: a reused TLS connection saves roughly 200-300 ms per call.
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|e| fatal(&format!("Could not start the HTTP client: {}", e)));

    let host = std::env::var("HOST").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| cfg.str(&["server", "host"], "0.0.0.0"));
    let port = std::env::var("PORT").ok().and_then(|p| p.parse::<u16>().ok()).unwrap_or(cfg.f64(&["server", "port"], 8080.0) as u16);
    let app = Arc::new(App {
        ui_dir: root.join("ui"),
        sessions_dir: root.join(cfg.str(&["logging", "sessions_dir"], "logs/sessions")),
        max_connections: cfg.f64(&["server", "max_connections"], 20.0) as usize,
        connections: AtomicUsize::new(0),
        access_token,
        auth_value,
        allowed_origins,
        sh: Arc::new(Shared { root, cfg: RwLock::new(Arc::new(cfg)), store, http, vad, warmed: Mutex::new(HashMap::new()) }),
    });

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/login", post(login))
        .route("/api/sessions/{id}", get(session_log))
        .route("/api/audio/{conv_id}/{name}", get(message_audio))
        .route("/api/conversations/{conv_id}/recording.wav", get(conversation_recording))
        .route("/ws", get(ws_handler))
        .fallback(get(ui_file))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await.unwrap_or_else(|e| fatal(&format!("Could not listen on {}:{}: {}", host, port, e)));
    log("info", &format!("Voice agent running on http://{}:{}", host, port));
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = axum::serve(listener, router).with_graceful_shutdown(shutdown).await {
        fatal(&format!("Server error: {}", e));
    }
}
