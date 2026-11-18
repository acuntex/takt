//! Playback state-machine tests.
//!
//! Every event fed to `App::handle_event` is a [`PlaybackEvent`], the daemon's view of a
//! `syncspin_client` event, so no WebSocket, decoder, or player process is started. TV side
//! effects use a fake `TvControl`, while sink effects are asserted as observable `Action`s (the
//! real sink is not installed in these fixtures).

#![allow(dead_code)]

#[path = "../src/app.rs"]
mod app;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/discovery.rs"]
mod discovery;
#[path = "../src/foreground.rs"]
mod foreground;
#[path = "../src/sink/mod.rs"]
mod sink;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/visual.rs"]
mod visual;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use app::{Action, App, PlaybackEvent, TvControl};
use config::Config;
use state::PlaybackState;
use syncspin_client::StreamEndReason;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TvCall {
    Muted(bool),
    Volume(u8),
}

#[derive(Clone, Default)]
struct FakeTvControl {
    calls: Arc<Mutex<Vec<TvCall>>>,
}

impl FakeTvControl {
    fn calls(&self) -> Vec<TvCall> {
        self.calls.lock().expect("fake TV lock").clone()
    }
}

impl TvControl for FakeTvControl {
    fn set_muted(&self, muted: bool) -> Result<()> {
        self.calls
            .lock()
            .expect("fake TV lock")
            .push(TvCall::Muted(muted));
        Ok(())
    }

    fn set_volume(&self, volume: u8) -> Result<()> {
        self.calls
            .lock()
            .expect("fake TV lock")
            .push(TvCall::Volume(volume));
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FailingMuteTvControl {
    calls: Arc<Mutex<Vec<TvCall>>>,
}

impl TvControl for FailingMuteTvControl {
    fn set_muted(&self, muted: bool) -> Result<()> {
        self.calls
            .lock()
            .expect("failing fake TV lock")
            .push(TvCall::Muted(muted));
        Err(anyhow!("injected mute failure"))
    }

    fn set_volume(&self, volume: u8) -> Result<()> {
        self.calls
            .lock()
            .expect("failing fake TV lock")
            .push(TvCall::Volume(volume));
        Ok(())
    }
}

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

fn test_app() -> (App, FakeTvControl) {
    let tv = FakeTvControl::default();
    let app = App::new(test_config(), Box::new(tv.clone()));
    (app, tv)
}

/// A stream/start: the client has already opened and primed the sink.
fn stream_start() -> PlaybackEvent {
    PlaybackEvent::StreamStart
}

/// A stream/end as Music Assistant sends it on pause.
fn stream_end() -> PlaybackEvent {
    PlaybackEvent::StreamEnd {
        reason: StreamEndReason::End,
    }
}

/// A stream/clear as Music Assistant sends it on seek.
fn stream_clear() -> PlaybackEvent {
    PlaybackEvent::StreamClear {
        from_timestamp: None,
    }
}

/// A volume command already limited by the client's volume policy.
fn volume_command(applied: u8) -> PlaybackEvent {
    PlaybackEvent::Volume {
        requested: applied,
        applied,
        muted: None,
    }
}

/// A mute command from Music Assistant.
fn mute_command(volume: u8, muted: bool) -> PlaybackEvent {
    PlaybackEvent::Volume {
        requested: volume,
        applied: volume,
        muted: Some(muted),
    }
}

fn track(title: &str, artist: &str, progress_ms: Option<u64>) -> PlaybackEvent {
    PlaybackEvent::Track {
        title: title.to_owned(),
        artist: artist.to_owned(),
        album: "Test Album".to_owned(),
        artwork_url: "http://198.51.100.20:9000/artwork/abc123".to_owned(),
        progress_ms,
        duration_ms: Some(355_000),
        playback_speed: 1_000,
    }
}

fn start_playback(app: &mut App) {
    app.handle_event(stream_start());
}

#[test]
fn stream_start_resumes_playing_and_reaffirms_the_tv_volume() {
    let (mut app, _tv) = test_app();
    // Luna (the idle TV) establishes the authoritative volume before MA starts playback.
    assert!(!app.handle_external_volume(9, false));

    let actions = app.handle_event(stream_start());

    // The client restarts and flushes the sink itself, and the sink wrote the stream's codec
    // header before the first frame, so only the TV and MA view remain here. A fresh start has
    // no daemon auto-mute to lift, so it must not send a mute command either.
    assert_eq!(
        actions,
        vec![
            Action::UpdateState(PlaybackState::Playing),
            Action::EnsureUiForeground,
            Action::ReportPlayerState(9, false),
        ]
    );
    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.state, PlaybackState::Playing);
    assert!(!state.muted);
}

#[tokio::test]
async fn a_server_delay_is_applied_live_and_stored() {
    let shared = Arc::new(AtomicU64::new(0));
    let (app, _tv) = test_app();
    let mut app = app.with_output_delay(Arc::clone(&shared));

    let actions = app.handle_event(PlaybackEvent::OutputDelay {
        requested: 120,
        applied: 120,
    });

    assert_eq!(
        actions,
        vec![Action::StoreOutputDelay(120)],
        "the action persists the value and tells the session the sink adopted it"
    );
    assert_eq!(
        shared.load(Ordering::Relaxed),
        120,
        "the sink-side value is in force before anything is stored"
    );
    assert_eq!(
        app.shared_state()
            .read()
            .expect("state lock")
            .output_delay_ms,
        120,
        "the UI reads the value in force from the shared state"
    );

    // A repeated command is how a reconnect re-establishes the server's choice: accepted again,
    // and the same value simply stays.
    assert_eq!(
        app.handle_event(PlaybackEvent::OutputDelay {
            requested: 120,
            applied: 120,
        }),
        vec![Action::StoreOutputDelay(120)]
    );
    assert_eq!(shared.load(Ordering::Relaxed), 120);
}

#[tokio::test]
async fn a_server_delay_beyond_the_protocol_range_is_clamped() {
    let shared = Arc::new(AtomicU64::new(0));
    let (app, _tv) = test_app();
    let mut app = app.with_output_delay(Arc::clone(&shared));

    // The client already caps at 5000; the daemon applies the same bound so the stored value and
    // the reported one cannot disagree.
    let actions = app.handle_event(PlaybackEvent::OutputDelay {
        requested: 7_000,
        applied: 7_000,
    });

    assert_eq!(actions, vec![Action::StoreOutputDelay(5_000)]);
    assert_eq!(shared.load(Ordering::Relaxed), 5_000);
    assert_eq!(
        app.shared_state()
            .read()
            .expect("state lock")
            .output_delay_ms,
        5_000
    );
}

#[tokio::test]
async fn stream_end_stops_the_sink_without_touching_the_tv_mute() {
    let (mut app, tv) = test_app();
    start_playback(&mut app);

    let actions = app.handle_event(stream_end());

    assert_eq!(
        actions,
        vec![
            Action::FlushSink,
            Action::StopSink,
            Action::UpdateState(PlaybackState::Paused),
        ],
        "a pause discards buffered audio by stopping the player, not by muting the TV"
    );
    // There is deliberately no real sink in this test: `FlushSink` is a no-op and `StopSink`
    // has no handle. The fake TV records anything that would have reached the device.
    app.execute_actions(actions).await.expect("actions execute");
    assert!(
        tv.calls().is_empty(),
        "a pause must never touch the TV mute"
    );
    assert_eq!(
        app.shared_state().read().expect("state lock").state,
        PlaybackState::Paused
    );
}

#[tokio::test]
async fn stream_end_while_idle_stops_only_the_sink() {
    let (mut app, tv) = test_app();

    let actions = app.handle_event(stream_end());

    assert_eq!(
        actions,
        vec![
            Action::FlushSink,
            Action::StopSink,
            Action::UpdateState(PlaybackState::Paused),
        ],
        "an idle player has no children to stop, so the same sequence is a no-op"
    );
    app.execute_actions(actions).await.expect("actions execute");
    assert!(tv.calls().is_empty(), "an idle pause must not touch the TV");
}

#[test]
fn a_removed_player_role_is_handled_like_an_ordinary_stream_end() {
    let (mut app, _tv) = test_app();
    start_playback(&mut app);

    let actions = app.handle_event(PlaybackEvent::StreamEnd {
        reason: StreamEndReason::RoleRemoved,
    });

    assert_eq!(
        actions,
        vec![
            Action::FlushSink,
            Action::StopSink,
            Action::UpdateState(PlaybackState::Paused),
        ]
    );
    assert!(!actions.contains(&Action::SetMuted(true)));
    assert_eq!(
        app.shared_state().read().expect("state lock").state,
        PlaybackState::Paused
    );
}

#[tokio::test]
async fn resume_after_pause_reopens_the_pipeline_without_muting_the_tv() {
    let (mut app, tv) = test_app();
    start_playback(&mut app);

    let pause = app.handle_event(stream_end());
    assert_eq!(
        pause,
        vec![
            Action::FlushSink,
            Action::StopSink,
            Action::UpdateState(PlaybackState::Paused),
        ]
    );
    app.execute_actions(pause).await.expect("actions execute");

    // The client's `Sink::open` restarts both children on the next stream start; the daemon only
    // reflects the state and leaves the TV mute alone.
    let resume = app.handle_event(stream_start());
    assert_eq!(
        resume,
        vec![
            Action::UpdateState(PlaybackState::Playing),
            Action::EnsureUiForeground,
        ]
    );
    assert!(
        !resume.contains(&Action::SetMuted(false)),
        "an idle pause leaves nothing muted, so a resume has nothing to lift"
    );
    app.execute_actions(resume).await.expect("actions execute");

    assert!(
        tv.calls().is_empty(),
        "neither the pause nor the resume may touch the TV mute"
    );
    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.state, PlaybackState::Playing);
    assert!(!state.muted);
}

#[test]
fn an_explicit_ma_mute_reaches_the_tv_and_survives_a_later_stream_start() {
    let (mut app, _tv) = test_app();
    assert!(!app.handle_external_volume(30, false));
    start_playback(&mut app);

    let mute = app.handle_event(mute_command(30, true));
    assert_eq!(
        mute,
        vec![Action::SetMuted(true)],
        "MA's mute reaches the TV"
    );

    // The pause path stops the sink; it must not lift the mute the user asked for.
    let pause = app.handle_event(stream_end());
    assert!(pause.contains(&Action::StopSink));
    assert!(!pause.contains(&Action::SetMuted(false)));

    // The resume re-affirms the user's mute instead of overriding it.
    let resume = app.handle_event(stream_start());
    assert_eq!(
        resume,
        vec![
            Action::UpdateState(PlaybackState::Playing),
            Action::EnsureUiForeground,
            Action::ReportPlayerState(30, true),
        ]
    );
    assert!(
        !resume.contains(&Action::SetMuted(false)),
        "a stream start must not lift a user mute"
    );

    // A new track may start while the player is already in the playing state.
    let track_change = app.handle_event(stream_start());
    assert_eq!(
        track_change,
        vec![
            Action::UpdateState(PlaybackState::Playing),
            Action::ReportPlayerState(30, true),
        ]
    );
    assert!(
        !track_change.contains(&Action::EnsureUiForeground),
        "a track change must not bring an already visible UI to the foreground again"
    );
    assert!(
        !track_change.contains(&Action::SetMuted(false)),
        "a track change must not override the user's mute"
    );
    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.state, PlaybackState::Playing);
    assert!(state.muted);
}

#[test]
fn a_user_mute_survives_a_pause_stop() {
    let (mut app, _tv) = test_app();
    assert!(!app.handle_external_volume(30, false));
    start_playback(&mut app);

    app.handle_event(mute_command(30, true));

    let pause = app.handle_event(stream_end());
    assert_eq!(
        pause,
        vec![
            Action::FlushSink,
            Action::StopSink,
            Action::UpdateState(PlaybackState::Paused),
        ],
        "the pause path is the same whether or not the TV is muted"
    );
    assert!(app.shared_state().read().expect("state lock").muted);

    let resume = app.handle_event(stream_start());
    assert_eq!(
        resume,
        vec![
            Action::UpdateState(PlaybackState::Playing),
            Action::EnsureUiForeground,
            Action::ReportPlayerState(30, true),
        ]
    );
    assert!(
        !resume.contains(&Action::SetMuted(false)),
        "the user's mute must outlive the pause"
    );
    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert!(state.muted);
}

#[test]
fn disconnect_while_idle_does_not_mute_the_tv() {
    let (mut app, tv) = test_app();

    let actions = app.handle_event(PlaybackEvent::Disconnected {
        reason: "test disconnected".to_owned(),
        retry_in: Duration::from_secs(1),
    });

    assert!(!actions.contains(&Action::SetMuted(true)));
    assert!(
        actions.contains(&Action::StopSink),
        "the stop is unconditional; with no children it is a no-op"
    );
    assert!(
        tv.calls().is_empty(),
        "an idle disconnect must not touch TV mute"
    );
    assert!(!app.shared_state().read().expect("state lock").muted);
}

#[tokio::test]
async fn disconnect_while_paused_does_not_mute_the_tv() {
    let (mut app, tv) = test_app();
    start_playback(&mut app);
    let pause = app.handle_event(stream_end());
    app.execute_actions(pause).await.expect("actions execute");
    assert!(tv.calls().is_empty(), "the pause did not touch the TV");

    let disconnect = app.handle_event(PlaybackEvent::Disconnected {
        reason: "test disconnected".to_owned(),
        retry_in: Duration::from_secs(1),
    });
    app.execute_actions(disconnect)
        .await
        .expect("actions execute");

    assert!(
        tv.calls().is_empty(),
        "a Music Assistant restart while paused must not mute the TV"
    );
    assert!(!app.shared_state().read().expect("state lock").muted);
}

#[test]
fn disconnect_while_playing_stops_the_sink_without_muting_the_tv() {
    let (mut app, _tv) = test_app();
    start_playback(&mut app);

    let disconnect = app.handle_event(PlaybackEvent::Disconnected {
        reason: "test disconnected".to_owned(),
        retry_in: Duration::from_secs(1),
    });
    assert_eq!(
        disconnect,
        vec![
            Action::FlushSink,
            Action::StopSink,
            Action::UpdateState(PlaybackState::Connecting),
            Action::Reconnect {
                delay: Duration::from_secs(1)
            },
        ],
        "the stop discards the tail; no mute command is involved"
    );
    assert!(
        !app.shared_state().read().expect("state lock").muted,
        "a Music Assistant restart must not leave the TV muted"
    );
    assert_eq!(
        app.shared_state().read().expect("state lock").state,
        PlaybackState::Connecting,
        "the client retries on its own, so the UI shows 'connecting' first"
    );
}

#[test]
fn a_fatal_disconnect_reports_the_clients_own_delay() {
    let (mut app, _tv) = test_app();
    start_playback(&mut app);

    // A sink failure ends the connection for good; the client reports no retry delay.
    let disconnect = app.handle_event(PlaybackEvent::Disconnected {
        reason: "the audio sink failed".to_owned(),
        retry_in: Duration::ZERO,
    });

    assert_eq!(
        disconnect.last(),
        Some(&Action::Reconnect {
            delay: Duration::ZERO
        })
    );
}

#[test]
fn resume_after_a_disconnect_reopens_the_pipeline_without_a_mute_command() {
    let (mut app, _tv) = test_app();
    start_playback(&mut app);
    app.handle_event(PlaybackEvent::Disconnected {
        reason: "test disconnected".to_owned(),
        retry_in: Duration::from_secs(1),
    });

    let resume = app.handle_event(stream_start());
    assert!(
        !resume.contains(&Action::SetMuted(false)) && !resume.contains(&Action::SetMuted(true)),
        "a resume after a disconnect must not send any mute command"
    );
}

#[test]
fn remote_tv_mute_is_recorded_reported_and_survives_a_stream_start() {
    let (mut app, _tv) = test_app();
    start_playback(&mut app);

    // The viewer pressed mute on the remote: recorded, and reported to MA while playing.
    assert!(
        app.handle_external_volume(30, true),
        "a playing player reports the external mute to MA"
    );
    assert!(app.shared_state().read().expect("state lock").muted);

    let start = app.handle_event(stream_start());
    assert!(
        !start.contains(&Action::SetMuted(false)),
        "an external mute is user intent and must survive stream starts"
    );
    assert_eq!(
        start,
        vec![
            Action::UpdateState(PlaybackState::Playing),
            Action::ReportPlayerState(30, true),
        ],
        "the stream start re-affirms the TV's real mute state to MA"
    );
    assert!(app.shared_state().read().expect("state lock").muted);
}

#[test]
fn initial_tv_mute_survives_a_stream_start() {
    let (app, _tv) = test_app();
    let mut app = app.with_tv_volume(20, true);

    let start = app.handle_event(stream_start());
    assert!(
        !start.contains(&Action::SetMuted(false)),
        "a TV that was already muted must stay muted when playback starts"
    );
    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.volume, Some(20));
    assert!(state.muted);
}

#[test]
fn a_tv_level_raised_while_paused_is_reported_unchanged_after_stream_start() {
    // The client connected while the TV was at 20; the user then raised the TV to 70 while the
    // player was paused. The guard limits requests, never the level the TV is really at, so the
    // report after the resume must carry the raw 70 — not 40 (the rule's threshold) or 60 (a
    // +20 step from the level the session was told at connect time).
    let app = App::new(test_config(), Box::new(FakeTvControl::default()));
    let mut app = app.with_tv_volume(20, false);
    start_playback(&mut app);
    app.handle_event(stream_end());
    assert!(!app.handle_external_volume(70, false));

    assert_eq!(
        app.shared_state().read().expect("state lock").volume,
        Some(70),
        "the shared state keeps the TV's own level"
    );

    let resume = app.handle_event(stream_start());
    assert_eq!(
        resume,
        vec![
            Action::UpdateState(PlaybackState::Playing),
            Action::EnsureUiForeground,
            Action::ReportPlayerState(70, false),
        ],
        "the report names the raw TV level and never goes through the volume clamp"
    );
    assert!(
        !resume.contains(&Action::SetVolume(70)),
        "a TV-originated level is reported, not requested"
    );
}

#[test]
fn stream_clear_restarts_the_sink_and_keeps_playing() {
    let (mut app, _tv) = test_app();
    app.handle_event(stream_start());

    let actions = app.handle_event(stream_clear());

    // The restarted sink re-primes the decoder with the stream's cached FLAC header itself.
    assert_eq!(actions, vec![Action::RestartSink]);
    assert_eq!(
        app.shared_state().read().expect("state lock").state,
        PlaybackState::Playing
    );
}

#[test]
fn metadata_and_controller_state_update_the_shared_app_state() {
    let (mut app, _tv) = test_app();

    assert!(app
        .handle_event(track("Bohemian Rhapsody", "Queen", Some(120_000)))
        .is_empty());
    assert!(app.handle_event(PlaybackEvent::ControllerState).is_empty());

    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.title.as_deref(), Some("Bohemian Rhapsody"));
    assert_eq!(state.artist.as_deref(), Some("Queen"));
    assert_eq!(state.album.as_deref(), Some("Test Album"));
    assert_eq!(
        state.artwork_url.as_deref(),
        Some("http://198.51.100.20:9000/artwork/abc123")
    );
    assert_eq!(state.progress_ms, Some(120_000));
    assert_eq!(state.duration_ms, Some(355_000));
    assert_eq!(state.playback_speed, Some(1000));
    assert_eq!(state.metadata_revision, 1);
    // MA's controller state is not authoritative for the TV volume; only Luna reports it.
    assert_eq!(state.volume, None);
    assert!(!state.muted);
    assert_eq!(state.server, "ma.test:8927");
    assert_eq!(state.player_name, "Test TV");
}

#[test]
fn empty_metadata_text_is_stored_as_absent() {
    let (mut app, _tv) = test_app();
    app.handle_event(track("Song", "Artist", Some(0)));

    app.handle_event(PlaybackEvent::Track {
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        artwork_url: String::new(),
        progress_ms: None,
        duration_ms: None,
        playback_speed: 1_000,
    });

    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.title, None);
    assert_eq!(state.artist, None);
    assert_eq!(state.album, None);
    assert_eq!(state.artwork_url, None);
    assert_eq!(state.progress_ms, None);
    assert_eq!(state.duration_ms, None);
}

#[test]
fn the_track_event_forwards_the_reported_playback_speed() {
    let (mut app, _tv) = test_app();

    app.handle_event(PlaybackEvent::Track {
        title: "Half speed".to_owned(),
        artist: "Artist".to_owned(),
        album: String::new(),
        artwork_url: String::new(),
        progress_ms: Some(1_000),
        duration_ms: Some(300_000),
        playback_speed: 500,
    });

    assert_eq!(
        app.shared_state()
            .read()
            .expect("state lock")
            .playback_speed,
        Some(500)
    );
}

#[test]
fn metadata_event_increments_the_revision_even_when_progress_is_unchanged() {
    let (mut app, _tv) = test_app();

    app.handle_event(track("Repeat", "Artist", Some(0)));
    let first = app.shared_state().read().expect("state lock").clone();
    assert_eq!(first.metadata_revision, 1);
    assert_eq!(first.progress_ms, Some(0));

    app.handle_event(track("Repeat", "Artist", Some(0)));
    let second = app.shared_state().read().expect("state lock").clone();
    assert_eq!(second.metadata_revision, 2);
    assert_eq!(second.progress_ms, Some(0));
}

#[tokio::test]
async fn volume_and_mute_commands_reach_the_fake_tv_and_update_state() {
    let (mut app, tv) = test_app();
    // Luna establishes the baseline while the TV is idle.
    assert!(!app.handle_external_volume(9, false));

    let volume_actions = app.handle_event(volume_command(0));
    let mute_actions = app.handle_event(mute_command(0, true));

    // The client applied the volume policy and reported the level itself; the daemon only
    // has to move the TV.
    assert_eq!(volume_actions, vec![Action::SetVolume(0)]);
    assert_eq!(mute_actions, vec![Action::SetMuted(true)]);
    app.execute_actions(volume_actions)
        .await
        .expect("volume action executes");
    app.execute_actions(mute_actions)
        .await
        .expect("mute action executes");
    assert_eq!(tv.calls(), vec![TvCall::Volume(0), TvCall::Muted(true)]);
    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.volume, Some(0));
    assert!(state.muted);
}

#[test]
fn ma_volume_is_ignored_until_a_tv_baseline_exists() {
    let (mut app, _tv) = test_app();

    // No Luna snapshot yet: the TV level is unknown, and applying a command from an invented 0
    // would drop the TV to whatever the client clamped it to.
    let ignored = app.handle_event(volume_command(40));

    assert!(
        ignored.is_empty(),
        "an unknown baseline must not be assumed to be 0"
    );
    assert_eq!(
        app.shared_state().read().expect("state lock").volume,
        None,
        "ignoring the command must not change the unknown baseline"
    );

    // The first TV snapshot establishes the baseline; subsequent commands clamp against it.
    assert!(!app.handle_external_volume(40, false));
    let actions = app.handle_event(volume_command(60));

    assert_eq!(actions, vec![Action::SetVolume(60)]);
    assert_eq!(
        app.shared_state().read().expect("state lock").volume,
        Some(60)
    );
}

#[test]
fn ma_volume_zero_is_preserved() {
    let (mut app, _tv) = test_app();
    assert!(!app.handle_external_volume(9, false));

    let actions = app.handle_event(volume_command(0));

    assert_eq!(actions, vec![Action::SetVolume(0)]);
    assert_eq!(
        app.shared_state().read().expect("state lock").volume,
        Some(0)
    );
}

#[test]
fn ma_mute_is_applied_and_updates_the_mute_state() {
    let (mut app, _tv) = test_app();
    assert!(!app.handle_external_volume(30, false));

    let actions = app.handle_event(mute_command(30, true));

    assert_eq!(actions, vec![Action::SetMuted(true)]);
    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.volume, Some(30));
    assert!(state.muted);
}

#[tokio::test]
async fn a_player_state_report_does_not_touch_the_tv() {
    let (mut app, tv) = test_app();
    start_playback(&mut app);

    // The report is a statement about the output, so dispatching it must not re-set the TV
    // (which would flash the volume OSD on every stream start).
    app.execute_actions(vec![Action::ReportPlayerState(70, false)])
        .await
        .expect("report dispatch is a no-op without a connection");

    assert!(tv.calls().is_empty(), "a report must not re-set the TV");
}

#[tokio::test]
async fn a_failing_tv_mute_does_not_abort_the_disconnect_cleanup() {
    let tv = FailingMuteTvControl::default();
    let calls = Arc::clone(&tv.calls);
    let mut app = App::new(test_config(), Box::new(tv));
    start_playback(&mut app);
    let mut actions = app.handle_event(PlaybackEvent::Disconnected {
        reason: "test disconnected".to_owned(),
        retry_in: Duration::from_secs(1),
    });
    // A failing TV call is injected in front of the sink cleanup; this sentinel after it makes
    // continued dispatch observable without starting real processes. The old short-circuit
    // returned before cleanup and never reached it.
    actions.insert(0, Action::SetMuted(true));
    actions.push(Action::SetVolume(19));

    let error = app
        .execute_actions(actions)
        .await
        .expect_err("the TV error is returned after cleanup");

    assert!(error.to_string().contains("injected mute failure"));
    assert_eq!(
        calls.lock().expect("failing fake TV lock").as_slice(),
        &[TvCall::Muted(true), TvCall::Volume(19)]
    );
    assert_eq!(
        app.shared_state().read().expect("state lock").state,
        PlaybackState::Connecting
    );
}

#[test]
fn external_volume_updates_state_and_requests_ma_report_while_playing() {
    let (mut app, _tv) = test_app();
    start_playback(&mut app);

    assert!(app.handle_external_volume(63, true));

    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.volume, Some(63));
    assert!(state.muted);
}

#[test]
fn external_volume_while_idle_updates_state_without_ma_report() {
    let (mut app, _tv) = test_app();

    assert!(!app.handle_external_volume(9, false));

    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.volume, Some(9));
    assert!(!state.muted);
}

#[test]
fn external_volume_while_paused_updates_state_without_ma_report() {
    let (mut app, _tv) = test_app();
    start_playback(&mut app);
    assert!(app.handle_external_volume(63, false));
    app.handle_event(stream_end());

    assert!(!app.handle_external_volume(12, true));

    let state = app.shared_state();
    let state = state.read().expect("state lock");
    assert_eq!(state.volume, Some(12));
    assert!(state.muted, "the TV's latest mute state is still recorded");
}

#[derive(Clone, Default)]
struct PlaybackGateTvControl {
    playing: Arc<Mutex<Vec<bool>>>,
}

impl TvControl for PlaybackGateTvControl {
    fn set_muted(&self, _muted: bool) -> Result<()> {
        Ok(())
    }

    fn set_volume(&self, _volume: u8) -> Result<()> {
        Ok(())
    }

    fn set_playing(&self, playing: bool) -> Result<()> {
        self.playing
            .lock()
            .expect("playing gate lock")
            .push(playing);
        Ok(())
    }
}

#[tokio::test]
async fn playback_state_actions_drive_the_tv_external_reporting_gate() {
    let tv = PlaybackGateTvControl::default();
    let playing = Arc::clone(&tv.playing);
    let mut app = App::new(test_config(), Box::new(tv));

    app.execute_actions(vec![
        Action::UpdateState(PlaybackState::Playing),
        Action::UpdateState(PlaybackState::Paused),
    ])
    .await
    .expect("state actions execute");

    assert_eq!(
        playing.lock().expect("playing gate lock").as_slice(),
        &[true, false]
    );
}

#[derive(Clone, Default)]
struct AutoOpenTvControl {
    foreground_calls: Arc<Mutex<usize>>,
    fail: bool,
}

impl AutoOpenTvControl {
    fn calls(&self) -> usize {
        *self.foreground_calls.lock().expect("auto-open fake lock")
    }
}

impl TvControl for AutoOpenTvControl {
    fn set_muted(&self, _muted: bool) -> Result<()> {
        Ok(())
    }

    fn set_volume(&self, _volume: u8) -> Result<()> {
        Ok(())
    }

    fn ensure_ui_foreground(&self) -> Result<()> {
        *self.foreground_calls.lock().expect("auto-open fake lock") += 1;
        if self.fail {
            return Err(anyhow!("injected auto-open failure"));
        }
        Ok(())
    }
}

fn auto_open_app(config: Config) -> (App, AutoOpenTvControl) {
    let tv = AutoOpenTvControl::default();
    let app = App::new(config, Box::new(tv.clone()));
    (app, tv)
}

#[tokio::test]
async fn auto_open_runs_once_per_playback_start() {
    let (mut app, tv) = auto_open_app(test_config());
    // Paused is a non-playing state; the next stream start is a playback start.
    app.handle_event(stream_end());

    let start = app.handle_event(stream_start());
    assert_eq!(
        start
            .iter()
            .filter(|action| **action == Action::EnsureUiForeground)
            .count(),
        1
    );
    app.execute_actions(start).await.expect("actions execute");
    assert_eq!(tv.calls(), 1);

    // A second stream/start while already playing is a track change, not a playback start.
    let track_change = app.handle_event(stream_start());
    assert!(!track_change.contains(&Action::EnsureUiForeground));
    app.execute_actions(track_change)
        .await
        .expect("actions execute");
    assert_eq!(tv.calls(), 1, "a track change must not auto-open again");
}

#[tokio::test]
async fn auto_open_is_skipped_when_disabled() {
    let mut config = test_config();
    config.auto_open = false;
    let (mut app, tv) = auto_open_app(config);
    app.handle_event(stream_end());

    let start = app.handle_event(stream_start());

    assert!(!start.contains(&Action::EnsureUiForeground));
    app.execute_actions(start).await.expect("actions execute");
    assert_eq!(tv.calls(), 0, "auto_open: false must not touch the TV");
}

#[tokio::test]
async fn auto_open_failure_does_not_stop_playback() {
    let mut config = test_config();
    config.auto_open = true;
    let tv = AutoOpenTvControl {
        fail: true,
        ..AutoOpenTvControl::default()
    };
    let mut app = App::new(config, Box::new(tv.clone()));

    let start = app.handle_event(stream_start());
    app.execute_actions(start)
        .await
        .expect("an auto-open failure must never propagate");

    assert_eq!(tv.calls(), 1, "the failure must not be retried");
    assert_eq!(
        app.shared_state().read().expect("state lock").state,
        PlaybackState::Playing
    );
}
