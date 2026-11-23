//! In-process HTTP API and SSE tests (Task 8).
//!
//! No test binds a socket or starts the daemon, audio processes, or a real session.

// Production-only listener constants/functions are unused when this module is compiled in-process.
#![allow(dead_code)]

#[path = "../src/config.rs"]
mod config;
#[path = "../src/foreground.rs"]
mod foreground;
#[path = "../src/http/mod.rs"]
mod http;
#[path = "../src/state.rs"]
mod state;

#[path = "../src/visual.rs"]
mod visual;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use config::Config;
use foreground::ForegroundCache;
use futures_util::StreamExt;
use http::{router, HttpState, PinStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use state::{AppState, PlaybackState, SharedAppState};
use tokio::sync::mpsc::UnboundedReceiver;
use tower::ServiceExt;
use visual::{AudioVisual, SharedAudioVisual, VisualLevels};

const TEST_PIN: &str = "1234";

/// Two tests can start within the same nanosecond, and a shared directory would let one test's
/// cleanup delete the other's files mid-run ("failed to persist default config").
static TEMP_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "takt-http-{label}-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    router: Router,
    shared_state: SharedAppState,
    visual: SharedAudioVisual,
    output_delay: Arc<AtomicU64>,
    commands: UnboundedReceiver<String>,
    config_path: PathBuf,
    _temp_dir: TempDir,
}

fn fixture() -> Fixture {
    fixture_with_foreground(None)
}

fn fixture_with_foreground(foreground: Option<ForegroundCache>) -> Fixture {
    let temp_dir = TempDir::new("routes");
    let config_path = temp_dir.path().join("config.json");
    let config = Config::load(&config_path).expect("load test config");
    let shared_state = Arc::new(RwLock::new(AppState::new(&config)));
    {
        let mut state = shared_state.write().expect("state lock");
        state.state = PlaybackState::Playing;
        state.title = Some("Test Track".to_owned());
        state.artist = Some("Test Artist".to_owned());
        state.album = Some("Test Album".to_owned());
        state.progress_ms = Some(12_000);
        state.duration_ms = Some(300_000);
        state.playback_speed = Some(1000);
        state.metadata_revision = 3;
        state.volume = Some(42);
        state.muted = true;
    }
    let (command_sender, commands) = tokio::sync::mpsc::unbounded_channel();
    let visual: SharedAudioVisual = Arc::new(RwLock::new(AudioVisual::default()));
    let output_delay = Arc::new(AtomicU64::new(u64::from(config.output_delay_ms)));
    let mut http_state = HttpState::new(
        Arc::clone(&shared_state),
        &config_path,
        PinStore::new(TEST_PIN),
        command_sender,
    )
    .with_visual(Arc::clone(&visual))
    .with_output_delay(Arc::clone(&output_delay));
    if let Some(foreground) = foreground {
        http_state = http_state.with_foreground(foreground);
    }

    Fixture {
        router: router(http_state),
        shared_state,
        visual,
        output_delay,
        commands,
        config_path,
        _temp_dir: temp_dir,
    }
}

fn request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("build request")
}

async fn send(router: &Router, request: Request<Body>) -> Response {
    router
        .clone()
        .oneshot(request)
        .await
        .expect("in-process HTTP response")
}

async fn json_body(response: Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect response body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("JSON response body")
}

#[tokio::test]
async fn ping_answers_with_a_png_for_the_channel_wrapper() {
    let fixture = fixture();

    let response = send(&fixture.router, request("GET", "/ping.png", None)).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect ping body")
        .to_bytes();
    assert!(
        body.starts_with(&[0x89, b'P', b'N', b'G']),
        "the probe must be a real PNG"
    );
}

#[tokio::test]
async fn status_returns_playback_fields_from_shared_state() {
    let fixture = fixture();
    fixture
        .shared_state
        .write()
        .expect("state lock")
        .output_delay_ms = 130;

    let response = send(&fixture.router, request("GET", "/api/status", None)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["state"], "playing");
    assert_eq!(body["title"], "Test Track");
    assert_eq!(body["artist"], "Test Artist");
    assert_eq!(body["album"], "Test Album");
    assert_eq!(body["progress_ms"], 12_000);
    assert_eq!(body["duration_ms"], 300_000);
    assert_eq!(body["playback_speed"], 1000);
    assert_eq!(body["metadata_revision"], 3);
    assert_eq!(body["volume"], 42);
    assert_eq!(body["muted"], true);
    assert_eq!(body["output_delay_ms"], 130);
    assert!(body.get("server").is_some());
    assert!(body.get("player_name").is_some());
    assert!(body.get("pin").is_none());
    assert!(body.get("client_id").is_none());
}

#[tokio::test]
async fn transport_without_pin_is_forbidden_and_dispatches_nothing() {
    let mut fixture = fixture();

    let response = send(
        &fixture.router,
        request("POST", "/api/transport", Some(json!({"cmd": "play"}))),
    )
    .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(fixture.commands.try_recv().is_err());
}

#[tokio::test]
async fn sixth_pin_attempt_is_rate_limited_even_when_pin_is_correct() {
    let mut fixture = fixture();

    for _ in 0..5 {
        let response = send(
            &fixture.router,
            request(
                "POST",
                "/api/transport",
                Some(json!({"pin": "0000", "cmd": "play"})),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    let response = send(
        &fixture.router,
        request(
            "POST",
            "/api/transport",
            Some(json!({"pin": TEST_PIN, "cmd": "play"})),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(fixture.commands.try_recv().is_err());
}

#[tokio::test]
async fn unknown_command_with_valid_pin_is_bad_request() {
    let mut fixture = fixture();

    let response = send(
        &fixture.router,
        request(
            "POST",
            "/api/transport",
            Some(json!({"pin": TEST_PIN, "cmd": "rewind"})),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(fixture.commands.try_recv().is_err());
}

#[tokio::test]
async fn repeated_valid_commands_never_exhaust_the_pin_limiter() {
    let mut fixture = fixture();

    // The remote sends the PIN with every command, and holding a seek key is a dozen calls in a
    // couple of seconds: successful calls must not count against the guessing limit.
    for _ in 0..20 {
        let response = send(
            &fixture.router,
            request(
                "POST",
                "/api/transport",
                Some(json!({"pin": TEST_PIN, "cmd": "next"})),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    for _ in 0..20 {
        assert_eq!(fixture.commands.recv().await.as_deref(), Some("next"));
    }
}

#[tokio::test]
async fn wrong_pins_still_hit_the_guessing_limit() {
    let fixture = fixture();

    for attempt in 0..5 {
        let response = send(
            &fixture.router,
            request(
                "POST",
                "/api/transport",
                Some(json!({"pin": "0000", "cmd": "next"})),
            ),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "attempt {attempt}"
        );
    }
    let blocked = send(
        &fixture.router,
        request(
            "POST",
            "/api/transport",
            Some(json!({"pin": "0000", "cmd": "next"})),
        ),
    )
    .await;
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn valid_play_and_prev_dispatch_play_and_previous() {
    let mut fixture = fixture();

    for command in ["play", "prev"] {
        let response = send(
            &fixture.router,
            request(
                "POST",
                "/api/transport",
                Some(json!({"pin": TEST_PIN, "cmd": command})),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    assert_eq!(fixture.commands.recv().await.as_deref(), Some("play"));
    assert_eq!(fixture.commands.recv().await.as_deref(), Some("previous"));
}

#[tokio::test]
async fn foreground_reports_the_app_and_whether_it_is_a_departure() {
    for (app, leaving) in [
        ("com.github.acuntex.takt", false),
        ("com.webos.app.home", false),
        ("com.webos.app.launcher", false),
        ("com.webos.app.livetv", true),
        ("com.netflix.nic90", true),
        ("", false),
    ] {
        let fixture = fixture_with_foreground(Some(ForegroundCache::with_ttl(
            move || Ok(Some(app.to_owned())),
            Duration::from_secs(60),
        )));

        let response = send(&fixture.router, request("GET", "/api/foreground", None)).await;

        assert_eq!(response.status(), StatusCode::OK, "app {app:?}");
        let body = json_body(response).await;
        assert_eq!(body["app_id"], app);
        assert_eq!(body["leaving"], leaving, "app {app:?}");
    }
}

#[tokio::test]
async fn foreground_query_failure_is_unavailable() {
    let fixture = fixture_with_foreground(Some(ForegroundCache::with_ttl(
        || -> anyhow::Result<Option<String>> { anyhow::bail!("luna is gone") },
        Duration::from_secs(60),
    )));

    let response = send(&fixture.router, request("GET", "/api/foreground", None)).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await;
    assert!(
        body.get("app_id").is_none(),
        "a failed query has no app id: {body}"
    );
}

#[tokio::test]
async fn foreground_without_luna_is_unavailable() {
    let fixture = fixture();

    let response = send(&fixture.router, request("GET", "/api/foreground", None)).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn foreground_answers_are_cached_within_the_ttl() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let fixture = fixture_with_foreground(Some(ForegroundCache::with_ttl(
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Some("com.webos.app.home".to_owned()))
        },
        Duration::from_secs(60),
    )));

    for _ in 0..2 {
        let response = send(&fixture.router, request("GET", "/api/foreground", None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["app_id"], "com.webos.app.home");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn malformed_transport_json_is_bad_request() {
    let fixture = fixture();
    let malformed = Request::builder()
        .method("POST")
        .uri("/api/transport")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"pin":"1234","cmd":"#))
        .expect("build malformed request");

    let response = send(&fixture.router, malformed).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn closed_command_receiver_returns_service_unavailable() {
    let fixture = fixture();
    drop(fixture.commands);

    let response = send(
        &fixture.router,
        request(
            "POST",
            "/api/transport",
            Some(json!({"pin": TEST_PIN, "cmd": "play"})),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn config_missing_pin_is_forbidden_before_other_field_validation() {
    let fixture = fixture();

    let response = send(
        &fixture.router,
        request("POST", "/api/config", Some(json!({}))),
    )
    .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn config_requires_pin_persists_updates_and_hides_secrets() {
    let fixture = fixture();
    let original = Config::load(&fixture.config_path).expect("load original config");
    let update = json!({
        "server": "music.example:8927",
        "player_name": "Living Room TV"
    });

    let forbidden = send(
        &fixture.router,
        request("POST", "/api/config", Some(update.clone())),
    )
    .await;
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let unchanged = Config::load(&fixture.config_path).expect("reload unchanged config");
    assert_eq!(unchanged, original);

    let response = send(
        &fixture.router,
        request(
            "POST",
            "/api/config",
            Some(json!({
                "pin": TEST_PIN,
                "server": "music.example:8927",
                "player_name": "Living Room TV"
            })),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["server"], "music.example:8927");
    assert_eq!(body["player_name"], "Living Room TV");
    assert_eq!(body["restart_required"], true);
    assert!(body.get("pin").is_none());
    assert!(body.get("client_id").is_none());
    assert_eq!(
        body.to_string().find(&original.client_id),
        None,
        "response leaked client_id"
    );

    let persisted = Config::load(&fixture.config_path).expect("reload updated config");
    assert_eq!(persisted.server, "music.example:8927");
    assert_eq!(persisted.player_name, "Living Room TV");
    assert_eq!(persisted.client_id, original.client_id);
    let state = fixture.shared_state.read().expect("state lock");
    assert_eq!(state.server, "music.example:8927");
    assert_eq!(state.player_name, "Living Room TV");
}

#[tokio::test]
async fn output_delay_update_is_pin_gated_persists_and_applies_live() {
    let fixture = fixture();

    let forbidden = send(
        &fixture.router,
        request(
            "POST",
            "/api/config",
            Some(json!({"pin": "0000", "output_delay_ms": 120})),
        ),
    )
    .await;
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert_eq!(fixture.output_delay.load(Ordering::Relaxed), 0);
    assert_eq!(
        Config::load(&fixture.config_path)
            .expect("load unchanged config")
            .output_delay_ms,
        0
    );

    let response = send(
        &fixture.router,
        request(
            "POST",
            "/api/config",
            Some(json!({"pin": TEST_PIN, "output_delay_ms": 120})),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["output_delay_ms"], 120);
    assert_eq!(
        body["restart_required"], false,
        "a delay change needs no restart"
    );

    // The write applies to the live value the sink polls and to the persisted config at once;
    // a delay-only payload must not disturb the player name.
    assert_eq!(fixture.output_delay.load(Ordering::Relaxed), 120);
    let persisted = Config::load(&fixture.config_path).expect("load updated config");
    assert_eq!(persisted.output_delay_ms, 120);
    assert_eq!(persisted.player_name, "Takt");
    assert_eq!(
        fixture
            .shared_state
            .read()
            .expect("state lock")
            .output_delay_ms,
        120
    );

    let response = send(&fixture.router, request("GET", "/api/status", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["output_delay_ms"], 120);

    // The API accepts the protocol's 0..=5000 and clamps beyond it; an empty payload is still a
    // bad request.
    for (requested, applied) in [(5_001_u64, 5_000_u64), (10_000, 5_000), (0, 0)] {
        let response = send(
            &fixture.router,
            request(
                "POST",
                "/api/config",
                Some(json!({"pin": TEST_PIN, "output_delay_ms": requested})),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "requested {requested}");
        let body = json_body(response).await;
        assert_eq!(body["output_delay_ms"], applied, "requested {requested}");
        assert_eq!(
            fixture.output_delay.load(Ordering::Relaxed),
            applied,
            "requested {requested}"
        );
        assert_eq!(
            u64::from(
                Config::load(&fixture.config_path)
                    .expect("load clamped config")
                    .output_delay_ms
            ),
            applied,
            "requested {requested}"
        );
    }

    let response = send(
        &fixture.router,
        request("POST", "/api/config", Some(json!({"pin": TEST_PIN}))),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pin_is_available_only_to_loopback_clients() {
    let fixture = fixture();
    let loopback = Request::builder()
        .method("GET")
        .uri("/api/pin")
        .extension(ConnectInfo(
            "127.0.0.1:3927"
                .parse::<SocketAddr>()
                .expect("loopback address"),
        ))
        .body(Body::empty())
        .expect("build loopback request");

    let response = send(&fixture.router, loopback).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({"pin": TEST_PIN}));

    let remote = Request::builder()
        .method("GET")
        .uri("/api/pin")
        .extension(ConnectInfo(
            "192.0.2.10:3927"
                .parse::<SocketAddr>()
                .expect("remote address"),
        ))
        .body(Body::empty())
        .expect("build remote request");
    let response = send(&fixture.router, remote).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn index_serves_embedded_html() {
    let fixture = fixture();

    let response = send(&fixture.router, request("GET", "/", None)).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()[header::CONTENT_TYPE]
        .to_str()
        .expect("HTML content type")
        .starts_with("text/html"));
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect HTML")
        .to_bytes();
    let html = String::from_utf8(bytes.to_vec()).expect("UTF-8 HTML");
    assert!(html.to_ascii_lowercase().contains("<!doctype html>"));
    assert!(html.contains("Takt"));
    assert!(html.contains("/app.js"));
    assert!(html.contains("/progress.js"));
    assert!(html.contains("/style.css"));
}

#[tokio::test]
async fn app_js_and_style_css_are_served_with_the_expected_content_types() {
    let fixture = fixture();

    let response = send(&fixture.router, request("GET", "/app.js", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/javascript; charset=utf-8"
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect app.js")
        .to_bytes();
    let app_js = String::from_utf8(body.to_vec()).expect("UTF-8 app.js");
    assert!(app_js.contains("XMLHttpRequest"));
    assert!(app_js.contains("SendspinProgress"));
    assert!(!app_js.contains("const "));
    assert!(!app_js.contains("=>"));

    let response = send(&fixture.router, request("GET", "/progress.js", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/javascript; charset=utf-8"
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect progress.js")
        .to_bytes();
    let progress_js = String::from_utf8(body.to_vec()).expect("UTF-8 progress.js");
    assert!(progress_js.contains("function update"));
    assert!(!progress_js.contains("const "));
    assert!(!progress_js.contains("=>"));

    let response = send(&fixture.router, request("GET", "/style.css", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/css; charset=utf-8"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect style.css")
        .to_bytes();
    let style_css = String::from_utf8(body.to_vec()).expect("UTF-8 style.css");
    assert!(style_css.contains("body"));

    let response = send(&fixture.router, request("GET", "/fonts/anton.ttf", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "font/ttf");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect the font")
        .to_bytes();
    assert_eq!(&body[0..4], b"\x00\x01\x00\x00");
    assert!(body.len() > 100_000);

    let response = send(&fixture.router, request("GET", "/visual.js", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/javascript; charset=utf-8"
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect visual.js")
        .to_bytes();
    let visual_js = String::from_utf8(body.to_vec()).expect("UTF-8 visual.js");
    assert!(visual_js.contains("SendspinVisual"));
}

#[tokio::test]
async fn events_is_sse_and_emits_a_status_event_immediately() {
    let fixture = fixture();

    let response = send(&fixture.router, request("GET", "/events", None)).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body().into_data_stream();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("first SSE frame timed out")
        .expect("SSE stream ended")
        .expect("SSE frame error");
    let event = String::from_utf8(frame.to_vec()).expect("UTF-8 SSE event");
    let data = event
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .expect("SSE status data");
    let status: Value = serde_json::from_str(data.trim()).expect("SSE status JSON");
    assert_eq!(status["state"], "playing");
    assert_eq!(status["title"], "Test Track");
    assert_eq!(status["volume"], 42);
}

#[tokio::test]
async fn events_emit_audio_levels_after_the_status_frame() {
    let fixture = fixture();
    fixture
        .visual
        .write()
        .expect("visual lock")
        .update(VisualLevels {
            level: 0.75,
            bass: 0.6,
            mid: 0.4,
            treble: 0.2,
        });

    let response = send(&fixture.router, request("GET", "/events", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();

    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("first SSE frame timed out")
        .expect("SSE stream ended")
        .expect("SSE frame error");
    let first = String::from_utf8(first.to_vec()).expect("UTF-8 first SSE frame");
    assert!(!first.contains("event: audio"), "{first}");

    let second = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("audio SSE frame timed out")
        .expect("SSE stream ended")
        .expect("SSE frame error");
    let second = String::from_utf8(second.to_vec()).expect("UTF-8 audio SSE frame");
    assert!(second.contains("event: audio"), "{second}");
    let data = second
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .expect("audio SSE data");
    let audio: Value = serde_json::from_str(data.trim()).expect("audio JSON");
    assert!((audio["level"].as_f64().expect("level") - 0.75).abs() < 0.01);
    assert!((audio["bass"].as_f64().expect("bass") - 0.6).abs() < 0.01);
    assert!((audio["mid"].as_f64().expect("mid") - 0.4).abs() < 0.01);
    assert!((audio["treble"].as_f64().expect("treble") - 0.2).abs() < 0.01);
}

#[cfg(unix)]
#[test]
fn generated_pin_is_four_digits_and_persisted_with_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = TempDir::new("pin");
    let path = temp_dir.path().join("takt.pin");

    let _pin = PinStore::generate(&path).expect("generate and persist PIN");

    let persisted = fs::read_to_string(&path).expect("read persisted PIN");
    assert_eq!(persisted.len(), 4);
    assert!(persisted.bytes().all(|byte| byte.is_ascii_digit()));
    assert_eq!(
        fs::metadata(&path)
            .expect("PIN metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
