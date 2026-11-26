//! Same-origin HTTP API and server-sent events for the TV UI.
//!
//! Writes are protected by a random four-digit boot PIN. The PIN is persisted outside the
//! configuration and is only returned to loopback HTTP clients, because the TV browser loads
//! this UI from `127.0.0.1:3927`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use syncspin_client::ImageFormat;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::config::Config;
use crate::foreground::{leave_verdict, ForegroundCache, LeaveVerdict};
use crate::state::{AppState, SharedAppState};
use crate::visual::{AudioVisual, SharedAudioVisual};

pub const HTTP_BIND_ADDR: &str = "0.0.0.0:3927";
pub const PIN_FILE_PATH: &str = "/tmp/takt.pin";

const MAX_PIN_ATTEMPTS: usize = 5;
const PIN_WINDOW: Duration = Duration::from_secs(60);
const AUDIO_INTERVAL: Duration = Duration::from_millis(50);
const STATUS_EVERY: u32 = 20;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const INDEX_HTML: &str = include_str!("../../../web/index.html");
const APP_JS: &str = include_str!("../../../web/app.js");
const PROGRESS_JS: &str = include_str!("../../../web/progress.js");
const VISUAL_JS: &str = include_str!("../../../web/visual.js");
const STYLE_CSS: &str = include_str!("../../../web/style.css");
const ANTON_TTF: &[u8] = include_bytes!("../../../web/fonts/anton.ttf");

/// A boot-scoped PIN. It deliberately has no `Debug` implementation to reduce accidental leaks.
#[derive(Clone)]
pub struct PinStore(Arc<str>);

impl PinStore {
    /// Creates a store around a known PIN, primarily for in-process tests.
    pub fn new(pin: impl Into<String>) -> Self {
        Self(Arc::from(pin.into()))
    }

    /// Generates four random digits from `/dev/urandom` and atomically persists them with mode
    /// 0600. The file is replaced on every boot; no PIN is ever added to the daemon config.
    pub fn generate(path: impl AsRef<Path>) -> Result<Self> {
        let mut random = [0_u8; 12];
        File::open("/dev/urandom")
            .context("failed to open /dev/urandom")?
            .read_exact(&mut random)
            .context("failed to read PIN randomness from /dev/urandom")?;
        let number = u32::from_le_bytes(random[0..4].try_into().expect("four random bytes"));
        let nonce = u64::from_le_bytes(random[4..12].try_into().expect("eight random bytes"));
        let pin = format!("{:04}", number % 10_000);
        persist_pin(path.as_ref(), &pin, nonce)?;
        Ok(Self::new(pin))
    }

    fn matches(&self, candidate: Option<&str>) -> bool {
        candidate == Some(self.0.as_ref())
    }
}

/// Shared dependencies for all HTTP handlers.
#[derive(Clone)]
pub struct HttpState {
    app_state: SharedAppState,
    visual: SharedAudioVisual,
    config_path: PathBuf,
    pin: PinStore,
    commands: mpsc::UnboundedSender<String>,
    rate_limiter: Arc<Mutex<PinRateLimiter>>,
    /// The live post-port delay shared with the sink; a config write updates this at once.
    output_delay: Arc<AtomicU64>,
    /// The TV's foreground app, when a Luna worker is available. `None` makes the endpoint
    /// answer `503`, the same as a failed query.
    foreground: Option<Arc<ForegroundCache>>,
}

impl HttpState {
    pub fn new(
        app_state: SharedAppState,
        config_path: impl AsRef<Path>,
        pin: PinStore,
        commands: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            app_state,
            visual: Arc::new(RwLock::new(AudioVisual::default())),
            config_path: config_path.as_ref().to_path_buf(),
            pin,
            commands,
            rate_limiter: Arc::new(Mutex::new(PinRateLimiter::default())),
            output_delay: Arc::new(AtomicU64::new(0)),
            foreground: None,
        }
    }

    /// Shares the daemon's analyzed PCM levels with the SSE audio stream.
    pub fn with_visual(mut self, visual: SharedAudioVisual) -> Self {
        self.visual = visual;
        self
    }

    /// Shares the live post-port delay with the sink so a config write applies without a restart.
    pub fn with_output_delay(mut self, output_delay: Arc<AtomicU64>) -> Self {
        self.output_delay = output_delay;
        self
    }

    /// Wires the Luna-backed foreground query behind `GET /api/foreground`.
    pub fn with_foreground(mut self, foreground: ForegroundCache) -> Self {
        self.foreground = Some(Arc::new(foreground));
        self
    }
}

/// Constructs the in-process-testable router. Production adds peer addresses through axum's
/// `ConnectInfo<SocketAddr>` make-service; tests can inject the same request extension.
pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/progress.js", get(progress_js))
        .route("/visual.js", get(visual_js))
        .route("/style.css", get(style_css))
        .route("/fonts/anton.ttf", get(anton_ttf))
        .route("/ping.png", get(ping_png))
        .route("/api/status", get(status))
        .route("/api/artwork", get(artwork))
        .route("/api/foreground", get(foreground_app))
        .route("/api/pin", get(pin))
        .route("/api/transport", post(transport))
        .route("/api/config", post(update_config))
        .route("/events", get(events))
        .with_state(state)
}

/// Serves an already-bound listener. Binding is kept in `main` so bind failures are logged
/// without terminating audio playback.
pub async fn serve(listener: TcpListener, state: HttpState) -> Result<()> {
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("HTTP server stopped")
}

async fn index() -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(INDEX_HTML)).into_response()
}

async fn app_js() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        APP_JS,
    )
        .into_response()
}

async fn progress_js() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PROGRESS_JS,
    )
        .into_response()
}

async fn visual_js() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        VISUAL_JS,
    )
        .into_response()
}

async fn style_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        STYLE_CSS,
    )
        .into_response()
}

/// The display face of the TV stage (Anton, SIL OFL 1.1). Embedded like every other asset: the
/// TV must not need the network for its own type, and the IPK carries the binary unchanged.
async fn anton_ttf() -> Response {
    (
        [
            (header::CONTENT_TYPE, "font/ttf"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        ANTON_TTF,
    )
        .into_response()
}

async fn status(State(state): State<HttpState>) -> Json<AppState> {
    Json(snapshot(&state.app_state))
}

/// The latest artwork image exactly as the server delivered it.
///
/// The bytes are kept out of the status JSON; the UI reloads this endpoint when
/// `artwork_revision` changes and uses the ETag to tell one image from the next. The `Arc` is
/// cloned out of the shared state under a short read lock, and the bytes are copied only after
/// that lock is dropped, so serving an image never holds the state lock.
async fn artwork(State(state): State<HttpState>) -> Response {
    let (image, revision) = {
        let app_state = state.app_state.read().expect("app state lock poisoned");
        (app_state.artwork.clone(), app_state.artwork_revision)
    };
    let Some(image) = image else {
        return error_response(StatusCode::NOT_FOUND, "no artwork");
    };
    let content_type = match image.format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
    };
    let etag = format!("\"{revision}\"")
        .parse()
        .expect("an artwork revision is a valid ETag");
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from(image.data.clone()),
    )
        .into_response();
    response.headers_mut().insert(header::ETAG, etag);
    response
}

async fn pin(State(state): State<HttpState>, request: Request) -> Response {
    let loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(address)| address.ip().is_loopback());
    if !loopback {
        return error_response(StatusCode::FORBIDDEN, "PIN is only available on loopback");
    }
    Json(json!({ "pin": state.pin.0.as_ref() })).into_response()
}

#[derive(Deserialize)]
struct TransportRequest {
    pin: Option<String>,
    cmd: Option<String>,
    /// Absolute target for `seek`, in milliseconds.
    position_ms: Option<u64>,
    /// Offset for `seek_relative`, in milliseconds; negative rewinds.
    offset_ms: Option<i64>,
}

/// Reports the TV's current foreground app, the same answer the departure watchdog sees.
///
/// `leaving` is true only when another app or TV input is in front. The webOS launcher overlay
/// that Back and Home raise is not a departure, and neither is an empty answer. A failed query is
/// `503`, which keeps a transport error from being mistaken for our own app. The cache in
/// [`ForegroundCache`] bounds the Luna load and is the same one the watchdog's fallback reads.
async fn foreground_app(State(state): State<HttpState>) -> Response {
    let Some(cache) = state.foreground.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "foreground app is unavailable",
        );
    };
    match tokio::task::spawn_blocking(move || cache.read()).await {
        Ok(Ok(app)) => {
            let leaving = leave_verdict(app.as_deref()) == LeaveVerdict::Leave;
            Json(json!({
                "app_id": app.unwrap_or_default(),
                "leaving": leaving,
            }))
            .into_response()
        }
        // The cache logs the failure transition itself; do not log once per UI poll.
        Ok(Err(_)) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "foreground app is unavailable",
        ),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "foreground query task failed",
        ),
    }
}

async fn transport(
    State(state): State<HttpState>,
    payload: Result<Json<TransportRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "malformed JSON"),
    };
    if let Err(response) = authorize(&state, payload.pin.as_deref()) {
        return *response;
    }

    let command = match payload.cmd.as_deref() {
        Some("play") => "play".to_owned(),
        Some("pause") => "pause".to_owned(),
        Some("next") => "next".to_owned(),
        Some("prev") => "previous".to_owned(),
        // A seek carries its target with the command: `seek:<ms>` or `seek_rel:<ms>`.
        Some("seek") => match payload.position_ms {
            Some(position) => format!("seek:{position}"),
            None => return error_response(StatusCode::BAD_REQUEST, "seek needs position_ms"),
        },
        Some("seek_relative") => match payload.offset_ms {
            Some(offset) => format!("seek_rel:{offset}"),
            None => {
                return error_response(StatusCode::BAD_REQUEST, "seek_relative needs offset_ms")
            }
        },
        _ => return error_response(StatusCode::BAD_REQUEST, "unknown transport command"),
    };
    if state.commands.send(command).is_err() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "playback command receiver is unavailable",
        );
    }
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct ConfigRequest {
    pin: Option<String>,
    server: Option<String>,
    player_name: Option<String>,
    /// Requested post-port delay in milliseconds; clamped to the config's range.
    output_delay_ms: Option<u64>,
}

/// Writes any subset of the persisted settings and applies the delay to the live sink.
///
/// Absent fields keep their stored value, so the TV can change the delay without resending the
/// player name (and without overwriting a name another client just saved). The server and player
/// name still need a restart to take effect; the post-port delay is live because the client polls
/// the sink after every state report and clock burst.
async fn update_config(
    State(state): State<HttpState>,
    payload: Result<Json<ConfigRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "malformed JSON"),
    };
    if let Err(response) = authorize(&state, payload.pin.as_deref()) {
        return *response;
    }
    let restart_required = payload.server.is_some() || payload.player_name.is_some();
    if !restart_required && payload.output_delay_ms.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "no writable fields");
    }

    let result = (|| -> Result<Config> {
        let mut config = Config::load(&state.config_path)?;
        if let Some(server) = payload.server {
            config.server = server;
        }
        if let Some(player_name) = payload.player_name {
            config.player_name = player_name;
        }
        if let Some(requested) = payload.output_delay_ms {
            config.output_delay_ms = Config::clamp_output_delay_ms(requested);
        }
        config.save()?;
        Ok(config)
    })();
    let config = match result {
        Ok(config) => config,
        Err(error) => {
            eprintln!("[takt] HTTP config update failed: {error:#}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to persist configuration",
            );
        }
    };

    let applied_delay = u64::from(config.output_delay_ms);
    let previous_delay = state.output_delay.swap(applied_delay, Ordering::Relaxed);
    {
        let mut app_state = state.app_state.write().expect("app state lock poisoned");
        app_state.server = config.server.clone();
        app_state.player_name = config.player_name.clone();
        app_state.output_delay_ms = config.output_delay_ms;
    }
    if previous_delay != applied_delay {
        println!("[takt] output delay {} ms", config.output_delay_ms);
    }

    // The running App and Session intentionally keep their existing Config. A daemon restart is
    // required to apply a changed server/player identity; no live reconnect is claimed here.
    Json(json!({
        "server": config.server,
        "player_name": config.player_name,
        "output_delay_ms": config.output_delay_ms,
        "restart_required": restart_required
    }))
    .into_response()
}

/// A transparent 1x1 PNG. The Channel wrapper probes it to tell "the daemon answers" from "not yet".
async fn ping_png() -> Response {
    const PIXEL: [u8; 70] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 96, 96, 96, 96, 0, 0,
        0, 5, 0, 1, 165, 246, 69, 64, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    ([(axum::http::header::CONTENT_TYPE, "image/png")], PIXEL).into_response()
}

async fn events(
    State(state): State<HttpState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let app_state = state.app_state;
    let visual = state.visual;
    let mut interval = tokio::time::interval(AUDIO_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let stream = futures_util::stream::unfold(
        (app_state, visual, interval, 0u32),
        |(app_state, visual, mut interval, tick)| async move {
            interval.tick().await;
            let tick = tick + 1;
            let event = if tick == 1 || tick % STATUS_EVERY == 0 {
                let data = serde_json::to_string(&snapshot(&app_state))
                    .expect("AppState always serializes");
                Event::default().data(data)
            } else {
                let levels = visual
                    .read()
                    .expect("visual state lock poisoned")
                    .snapshot();
                let data = serde_json::to_string(&levels).expect("VisualLevels always serializes");
                Event::default().event("audio").data(data)
            };
            Some((Ok(event), (app_state, visual, interval, tick)))
        },
    );
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(KEEPALIVE_INTERVAL)
            .text("keepalive"),
    )
}

fn snapshot(state: &SharedAppState) -> AppState {
    state.read().expect("app state lock poisoned").clone()
}

/// Counts every syntactically valid PIN-gated request before checking its PIN. The limit is
/// global, which is intentional: one LAN client cannot reset another client's attempt budget.
fn authorize(state: &HttpState, candidate: Option<&str>) -> Result<(), Box<Response>> {
    // The limiter guards against guessing, so only failures count. The UI sends the PIN with
    // every transport command, and holding a seek key for two seconds is a dozen calls —
    // counting those locked the remote out of its own player. A window already full of failures
    // still blocks everyone, a correct PIN included.
    let permitted = state
        .rate_limiter
        .lock()
        .expect("PIN rate limiter lock poisoned")
        .permitted();
    if !permitted {
        return Err(Box::new(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "too many PIN attempts",
        )));
    }
    if state.pin.matches(candidate) {
        return Ok(());
    }
    state
        .rate_limiter
        .lock()
        .expect("PIN rate limiter lock poisoned")
        .record_attempt();
    Err(Box::new(error_response(
        StatusCode::FORBIDDEN,
        "missing or invalid PIN",
    )))
}

struct PinRateLimiter {
    attempts: VecDeque<Instant>,
}

impl Default for PinRateLimiter {
    fn default() -> Self {
        Self {
            attempts: VecDeque::with_capacity(MAX_PIN_ATTEMPTS),
        }
    }
}

impl PinRateLimiter {
    /// Whether another attempt may be judged at all, without counting this one.
    fn permitted(&mut self) -> bool {
        self.prune();
        self.attempts.len() < MAX_PIN_ATTEMPTS
    }

    /// Records one failed attempt.
    fn record_attempt(&mut self) {
        self.prune();
        self.attempts.push_back(Instant::now());
    }

    fn prune(&mut self) {
        let now = Instant::now();
        while self
            .attempts
            .front()
            .is_some_and(|attempt| now.duration_since(*attempt) >= PIN_WINDOW)
        {
            self.attempts.pop_front();
        }
    }
}

fn error_response(status: StatusCode, message: &'static str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn persist_pin(path: &Path, pin: &str, nonce: u64) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("PIN path {} has no UTF-8 file name", path.display()))?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let temp_path = directory.join(format!(
        ".{file_name}.{}.{nonce:016x}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)
        .with_context(|| format!("failed to create temporary PIN {}", temp_path.display()))?;

    let write_result = (|| -> Result<()> {
        file.write_all(pin.as_bytes())
            .context("failed to write temporary PIN")?;
        file.sync_all().context("failed to sync temporary PIN")?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error)
            .with_context(|| format!("failed to replace PIN file {}", path.display()));
    }
    Ok(())
}
