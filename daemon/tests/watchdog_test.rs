//! Hermetic tests for the daemon's foreground watchdog.
//!
//! A fake foreground source feeds the real [`ForegroundCache`] with a zero TTL, so every fallback
//! tick reaches it. A pushed answer arrives through a Tokio channel, exactly as the Luna worker
//! delivers it. The five-second fallback is replaced by a short window where a test drives the
//! run loop; the decision itself is the pure [`watchdog_should_pause`]. No test binds the TV,
//! starts audio processes, or needs the network.

#![allow(dead_code)]

#[path = "../src/config.rs"]
mod config;
#[path = "../src/foreground.rs"]
mod foreground;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/watchdog.rs"]
mod watchdog;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use config::Config;
use foreground::{ForegroundCache, LeaveVerdict, APP_ID, LAUNCHER_APP_IDS};
use state::{AppState, PlaybackState, SharedAppState};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use watchdog::{watchdog_should_pause, DepartureLatch, Watchdog, WatchdogDecision};

fn test_config() -> Config {
    serde_json::from_str(
        r#"{
            "server": "ma.test:8927",
            "player_name": "Test TV",
            "client_id": "0123456789abcdef0123456789abcdef"
        }"#,
    )
    .expect("test config parses")
}

fn test_state(playback: PlaybackState) -> SharedAppState {
    let mut state = AppState::new(&test_config());
    state.state = playback;
    Arc::new(RwLock::new(state))
}

/// A watchdog driven by a settable fake foreground source and a pushed-answer channel, with the
/// transport channel drained by the test. Queries are counted, so a tick that must not reach Luna
/// is observable.
struct Fixture {
    state: SharedAppState,
    foreground: Arc<Mutex<Option<String>>>,
    queries: Arc<AtomicUsize>,
    events: UnboundedSender<Option<String>>,
    commands: UnboundedReceiver<String>,
    watchdog: Watchdog,
}

/// A fixture whose watchdog runs in its own task: the handles a test needs to push an answer and
/// to watch for a pause.
struct Running {
    state: SharedAppState,
    foreground: Arc<Mutex<Option<String>>>,
    queries: Arc<AtomicUsize>,
    /// `None` once a test has dropped it to simulate a dead subscription.
    events: Option<UnboundedSender<Option<String>>>,
    commands: UnboundedReceiver<String>,
}

impl Fixture {
    fn new(playback: PlaybackState, app: Option<&str>) -> Self {
        Self::with_backend(playback, app, true)
    }

    fn without_backend(playback: PlaybackState) -> Self {
        Self::with_backend(playback, None, false)
    }

    fn with_backend(playback: PlaybackState, app: Option<&str>, backend: bool) -> Self {
        let state = test_state(playback);
        let foreground = Arc::new(Mutex::new(app.map(str::to_owned)));
        let source = Arc::clone(&foreground);
        let queries = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&queries);
        // Zero TTL: every fallback tick must reach the fake source, so a cached answer never
        // hides a decision in a test.
        let cache = ForegroundCache::with_ttl(
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(source.lock().expect("foreground lock").clone())
            },
            Duration::ZERO,
        );
        let (events, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (sender, commands) = tokio::sync::mpsc::unbounded_channel();
        let watchdog = Watchdog::new(Arc::clone(&state), backend.then_some(cache), sender)
            .with_events(event_receiver);
        Self {
            state,
            foreground,
            queries,
            events,
            commands,
            watchdog,
        }
    }

    fn with_fallback(mut self, interval: Duration) -> Self {
        self.watchdog = self.watchdog.with_fallback_interval(interval);
        self
    }

    fn set_foreground(&self, app: Option<&str>) {
        *self.foreground.lock().expect("foreground lock") = app.map(str::to_owned);
    }

    fn set_playback(&self, playback: PlaybackState) {
        self.state.write().expect("state lock").state = playback;
    }

    fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }

    async fn tick(&mut self) {
        self.watchdog.tick().await;
    }

    fn spawn(self) -> Running {
        let Self {
            state,
            foreground,
            queries,
            events,
            commands,
            watchdog,
        } = self;
        tokio::spawn(watchdog.run());
        Running {
            state,
            foreground,
            queries,
            events: Some(events),
            commands,
        }
    }

    /// Drains the transport channel and returns how many pauses the watchdog sent.
    fn pauses(&mut self) -> usize {
        let mut count = 0;
        while let Ok(command) = self.commands.try_recv() {
            assert_eq!(command, "pause", "the watchdog only ever sends pause");
            count += 1;
        }
        count
    }
}

impl Running {
    fn push(&self, app: Option<&str>) {
        self.events
            .as_ref()
            .expect("the event channel is still open")
            .send(app.map(str::to_owned))
            .expect("watchdog event receiver is alive");
    }

    /// Waits for one pause command; a push must deliver well before this window.
    async fn pause(&mut self) -> bool {
        match tokio::time::timeout(Duration::from_secs(2), self.commands.recv()).await {
            Ok(Some(command)) => {
                assert_eq!(command, "pause", "the watchdog only ever sends pause");
                true
            }
            Ok(None) | Err(_) => false,
        }
    }

    fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }
}

#[test]
fn the_gate_pauses_only_a_first_departure_from_a_playing_player() {
    assert!(watchdog_should_pause(
        PlaybackState::Playing,
        LeaveVerdict::Leave,
        false,
        false
    ));
    assert!(
        !watchdog_should_pause(PlaybackState::Playing, LeaveVerdict::Leave, true, false),
        "the same departure must not pause twice"
    );
    assert!(!watchdog_should_pause(
        PlaybackState::Playing,
        LeaveVerdict::Stay,
        false,
        false
    ));
    for state in [
        PlaybackState::Idle,
        PlaybackState::Connecting,
        PlaybackState::Paused,
        PlaybackState::Error,
    ] {
        assert!(
            !watchdog_should_pause(state, LeaveVerdict::Leave, false, false),
            "{state:?} must not pause"
        );
    }
}

#[test]
fn the_latch_releases_only_when_a_playing_player_is_back_home() {
    let mut latch = DepartureLatch::default();

    assert_eq!(
        latch.observe_with_grace(PlaybackState::Playing, Some("com.netflix.nic90"), false),
        WatchdogDecision::Pause {
            app: "com.netflix.nic90".to_owned()
        }
    );
    assert_eq!(
        latch.observe_with_grace(PlaybackState::Playing, Some("com.netflix.nic90"), false),
        WatchdogDecision::Nothing
    );
    // A paused player is not observed, so a return to the app is not seen yet and the latch
    // survives; the resume in our app is what releases it.
    assert_eq!(
        latch.observe_with_grace(PlaybackState::Paused, Some(APP_ID), false),
        WatchdogDecision::Nothing
    );
    assert_eq!(
        latch.observe_with_grace(PlaybackState::Playing, Some(APP_ID), false),
        WatchdogDecision::Back
    );
    assert_eq!(
        latch.observe_with_grace(PlaybackState::Playing, Some("com.netflix.nic90"), false),
        WatchdogDecision::Pause {
            app: "com.netflix.nic90".to_owned()
        },
        "a later departure pauses again"
    );
}

#[tokio::test]
async fn a_pushed_foreign_app_pauses_at_once_without_a_luna_query() {
    // The fallback is far out of reach: only the pushed answer can pause here.
    let mut running = Fixture::new(PlaybackState::Playing, Some(APP_ID))
        .with_fallback(Duration::from_secs(30))
        .spawn();

    running.push(Some("com.netflix.nic90"));

    assert!(running.pause().await, "a pushed departure pauses at once");
    assert_eq!(
        running.queries(),
        0,
        "the pushed answer costs no fallback query"
    );
}

#[tokio::test]
async fn the_same_foreign_app_is_not_paused_again_on_the_next_push() {
    let mut running = Fixture::new(PlaybackState::Playing, Some(APP_ID))
        .with_fallback(Duration::from_secs(30))
        .spawn();
    running.push(Some("com.netflix.nic90"));
    assert!(running.pause().await);

    running.push(Some("com.netflix.nic90"));

    assert!(!running.pause().await, "one pause per departure");
}

#[tokio::test]
async fn a_paused_player_ignores_a_pushed_departure() {
    let mut running = Fixture::new(PlaybackState::Paused, Some(APP_ID))
        .with_fallback(Duration::from_secs(30))
        .spawn();

    running.push(Some("com.netflix.nic90"));

    assert!(
        !running.pause().await,
        "a player that is not playing must not pause"
    );
}

#[tokio::test]
async fn our_app_and_the_launcher_ids_never_pause_a_push() {
    for app in std::iter::once(APP_ID).chain(LAUNCHER_APP_IDS.iter().copied()) {
        let mut running = Fixture::new(PlaybackState::Playing, Some(APP_ID))
            .with_fallback(Duration::from_secs(30))
            .spawn();

        running.push(Some(app));
        running.push(None);

        assert!(!running.pause().await, "foreground {app} must not pause");
    }
}

#[tokio::test]
async fn returning_to_our_app_re_arms_the_rule() {
    let mut running = Fixture::new(PlaybackState::Playing, Some(APP_ID))
        .with_fallback(Duration::from_secs(30))
        .spawn();
    running.push(Some("com.netflix.nic90"));
    assert!(running.pause().await);

    // The pause stopped playback; the viewer brings the UI back and resumes there.
    running.state.write().expect("state lock").state = PlaybackState::Playing;
    running.push(Some(APP_ID));
    // The return is not a pause; give the loop its turn before the next departure.
    running.push(Some("com.netflix.nic90"));

    assert!(
        running.pause().await,
        "a later departure pauses again after the viewer returned"
    );
}

#[tokio::test]
async fn a_fallback_tick_asks_the_tv_when_no_event_arrives() {
    let mut running = Fixture::new(PlaybackState::Playing, Some("com.netflix.nic90"))
        .with_fallback(Duration::from_millis(50))
        .spawn();

    assert!(
        running.pause().await,
        "without a push the fallback query pauses"
    );
    assert_eq!(
        running.queries(),
        1,
        "the tick queried the fake source once"
    );
}

#[tokio::test]
async fn a_closed_event_channel_leaves_the_fallback_tick_running() {
    let mut running = Fixture::new(PlaybackState::Playing, Some("com.netflix.nic90"))
        .with_fallback(Duration::from_millis(50))
        .spawn();
    drop(running.events.take());

    assert!(
        running.pause().await,
        "a dead subscription must not disable the pause"
    );
    assert_eq!(running.queries(), 1);
}

#[tokio::test]
async fn pushed_answers_keep_the_fallback_tick_quiet() {
    let running = Fixture::new(PlaybackState::Playing, Some(APP_ID))
        .with_fallback(Duration::from_millis(500))
        .spawn();

    for _ in 0..8 {
        running.push(Some(APP_ID));
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    assert_eq!(
        running.queries(),
        0,
        "an event restarts the fallback window, so no query is due"
    );
}

#[tokio::test]
async fn a_failed_fallback_query_is_ignored_and_the_next_tick_asks_again() {
    let state = test_state(PlaybackState::Playing);
    let queries = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&queries);
    let cache = ForegroundCache::with_ttl(
        move || -> Result<Option<String>> {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("luna is gone"))
        },
        Duration::ZERO,
    );
    let (sender, mut commands) = tokio::sync::mpsc::unbounded_channel();
    let (_events, event_receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut watchdog =
        Watchdog::new(Arc::clone(&state), Some(cache), sender).with_events(event_receiver);

    watchdog.tick().await;
    watchdog.tick().await;

    assert!(
        commands.try_recv().is_err(),
        "a failed query is unknown, never a departure"
    );
    assert_eq!(
        queries.load(Ordering::SeqCst),
        2,
        "the next tick tries the query again"
    );
    assert_eq!(
        state.read().expect("state lock").state,
        PlaybackState::Playing,
        "a failed query leaves playback alone"
    );
}

#[tokio::test]
async fn without_a_luna_backend_the_watchdog_never_pauses() {
    let mut fixture = Fixture::without_backend(PlaybackState::Playing);

    fixture.tick().await;

    assert_eq!(fixture.pauses(), 0, "no backend means no foreground query");
}

#[tokio::test]
async fn a_paused_player_costs_no_fallback_query() {
    let mut fixture = Fixture::new(PlaybackState::Paused, Some("com.netflix.nic90"));

    fixture.tick().await;

    assert_eq!(fixture.pauses(), 0);
    assert_eq!(
        fixture.queries(),
        0,
        "a player that is not playing must not query Luna"
    );
}
