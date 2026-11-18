//! Hermetic tests for Task 9's Luna state machines.
//!
//! Only the pure backend-trait modules are included: these tests never load LS2, start glib, or
//! start the production Luna worker.

#![allow(dead_code)]

#[path = "../src/luna/audio.rs"]
mod audio;
#[path = "../src/luna/autoopen.rs"]
mod autoopen;
#[path = "../src/foreground.rs"]
mod foreground;
#[path = "../src/luna/tvpower.rs"]
mod tvpower;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use audio::{
    needs_volume_poll, next_subscription_retry_delay, parse_volume_reply, should_log_poll_failure,
    Audio, AudioBackend, Echo, VolumeCallback, VolumeObservation, ECHO_WINDOW,
    POLL_FAILURE_LOG_INTERVAL, SUBSCRIPTION_RETRY_INITIAL, SUBSCRIPTION_RETRY_MAX,
};
use autoopen::{
    foreground_decision, launch_accepted, launch_payload, parse_foreground_app,
    retry_launch_payload, ForegroundDecision, APP_ID, REQUEST_DEADLINE_MS, VERIFY_DELAY_MS,
};
use tvpower::{
    response_screen_saver_payload, veto_reply_accepted, veto_request_timestamp, TvPower,
    TvPowerBackend,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum AudioCall {
    Subscribe,
    Volume(u8),
    Muted(bool),
}

#[derive(Default)]
struct FakeAudioShared {
    calls: Mutex<Vec<AudioCall>>,
    callback: Mutex<Option<VolumeCallback>>,
    notify_during_writes: AtomicBool,
    last_volume: AtomicU64,
}

#[derive(Clone, Default)]
struct FakeAudioBackend {
    shared: Arc<FakeAudioShared>,
}

impl FakeAudioBackend {
    fn emit(&self, volume: u8, muted: bool) {
        let callback = self.shared.callback.lock().expect("callback lock");
        callback.as_ref().expect("subscription installed")(volume, muted);
    }
}

impl AudioBackend for FakeAudioBackend {
    fn subscribe(&self, callback: VolumeCallback) -> Result<()> {
        self.shared
            .calls
            .lock()
            .expect("calls lock")
            .push(AudioCall::Subscribe);
        *self.shared.callback.lock().expect("callback lock") = Some(callback);
        Ok(())
    }

    fn set_volume(&self, volume: u8) -> Result<()> {
        self.shared
            .calls
            .lock()
            .expect("calls lock")
            .push(AudioCall::Volume(volume));
        self.shared
            .last_volume
            .store(volume.into(), Ordering::SeqCst);
        if self.shared.notify_during_writes.load(Ordering::SeqCst) {
            self.emit(volume, false);
        }
        Ok(())
    }

    fn set_muted(&self, muted: bool) -> Result<()> {
        self.shared
            .calls
            .lock()
            .expect("calls lock")
            .push(AudioCall::Muted(muted));
        if self.shared.notify_during_writes.load(Ordering::SeqCst) {
            self.emit(self.shared.last_volume.load(Ordering::SeqCst) as u8, muted);
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeClock {
    milliseconds: Arc<AtomicU64>,
}

impl FakeClock {
    fn set_ms(&self, milliseconds: u64) {
        self.milliseconds.store(milliseconds, Ordering::SeqCst);
    }

    fn duration(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.milliseconds.load(Ordering::SeqCst))
    }
}

type RecordedEvents = Arc<Mutex<Vec<(u8, bool)>>>;
type PlayingAudio = (
    Audio<FakeAudioBackend>,
    FakeAudioBackend,
    FakeClock,
    RecordedEvents,
);

fn playing_audio() -> PlayingAudio {
    let backend = FakeAudioBackend::default();
    let clock = FakeClock::default();
    let callback_clock = clock.clone();
    let audio = Audio::with_clock(backend.clone(), move || callback_clock.duration());
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback_events = Arc::clone(&events);
    audio
        .subscribe(move |volume, muted| {
            callback_events
                .lock()
                .expect("events lock")
                .push((volume, muted));
        })
        .expect("subscription succeeds");
    audio.set_playing(true);
    (audio, backend, clock, events)
}

fn recorded_events(events: &Arc<Mutex<Vec<(u8, bool)>>>) -> Vec<(u8, bool)> {
    events.lock().expect("events lock").clone()
}

fn calls(backend: &FakeAudioBackend) -> Vec<AudioCall> {
    backend.shared.calls.lock().expect("calls lock").clone()
}

#[test]
fn own_volume_echo_is_suppressed_and_unchanged_snapshots_are_not_repeated() {
    let (audio, backend, clock, events) = playing_audio();
    audio.set_volume(31).expect("local volume succeeds");

    clock.set_ms(499);
    backend.emit(31, false);
    clock.set_ms(500);
    backend.emit(31, false);
    clock.set_ms(600);
    backend.emit(32, false);

    assert_eq!(recorded_events(&events), vec![(32, false)]);
}

#[test]
fn echo_window_is_strictly_less_than_500_ms() {
    let echo = Echo {
        value: 31u8,
        written_at: std::time::Duration::ZERO,
    };
    assert_eq!(ECHO_WINDOW, std::time::Duration::from_millis(500));
    assert!(echo.matches(31, std::time::Duration::from_millis(499)));
    assert!(!echo.matches(31, std::time::Duration::from_millis(500)));
}

#[test]
fn initial_snapshot_is_reported_and_unchanged_duplicates_are_not() {
    let (_audio, backend, clock, events) = playing_audio();

    backend.emit(23, false);
    clock.set_ms(100);
    backend.emit(23, false);
    clock.set_ms(200);
    backend.emit(24, false);

    assert_eq!(recorded_events(&events), vec![(23, false), (24, false)]);
}

#[test]
fn an_earlier_local_write_is_still_suppressed_after_a_newer_one() {
    let (audio, backend, clock, events) = playing_audio();
    audio.set_volume(20).expect("first local volume succeeds");
    clock.set_ms(100);
    audio.set_volume(30).expect("second local volume succeeds");

    // A poll that read 20 is processed after the write of 30 was recorded. It is still our echo.
    clock.set_ms(200);
    backend.emit(20, false);
    clock.set_ms(300);
    backend.emit(30, false);

    assert_eq!(recorded_events(&events), Vec::<(u8, bool)>::new());

    // A value that was never written is external, even in the same window.
    clock.set_ms(400);
    backend.emit(25, false);
    assert_eq!(recorded_events(&events), vec![(25, false)]);
}

#[test]
fn local_write_echoes_expire_after_the_window() {
    let (audio, backend, clock, events) = playing_audio();
    audio.set_volume(20).expect("local volume succeeds");

    clock.set_ms(100);
    backend.emit(30, false);
    assert_eq!(recorded_events(&events), vec![(30, false)]);

    clock.set_ms(600);
    backend.emit(20, false);

    assert_eq!(recorded_events(&events), vec![(30, false), (20, false)]);
}

#[test]
fn a_different_volume_inside_the_echo_window_is_external() {
    let (audio, backend, clock, events) = playing_audio();
    audio.set_volume(31).expect("local volume succeeds");

    clock.set_ms(100);
    backend.emit(32, false);

    assert_eq!(recorded_events(&events), vec![(32, false)]);
}

#[test]
fn own_writes_record_their_echo_before_reentrant_backend_notifications() {
    let (audio, backend, _clock, events) = playing_audio();
    backend
        .shared
        .notify_during_writes
        .store(true, Ordering::SeqCst);

    audio.set_volume(31).expect("local volume succeeds");
    audio.set_muted(true).expect("local mute succeeds");

    assert_eq!(recorded_events(&events), Vec::<(u8, bool)>::new());
}

#[test]
fn external_mute_and_unmute_are_reported_separately_with_the_tv_level() {
    let (audio, backend, clock, events) = playing_audio();
    audio.set_volume(44).expect("local volume succeeds");

    clock.set_ms(600);
    backend.emit(44, true);
    clock.set_ms(700);
    backend.emit(44, false);

    assert_eq!(recorded_events(&events), vec![(44, true), (44, false)]);
}

#[test]
fn external_events_are_delivered_while_paused_and_idle() {
    let (audio, backend, clock, events) = playing_audio();
    audio.set_playing(false);
    clock.set_ms(1_000);

    // The backend still delivers the initial/idle snapshot so the App can learn the TV volume.
    backend.emit(72, true);
    assert_eq!(recorded_events(&events), vec![(72, true)]);

    clock.set_ms(1_100);
    backend.emit(73, false);
    assert_eq!(recorded_events(&events), vec![(72, true), (73, false)]);
}

#[test]
fn idle_external_volume_event_is_delivered_for_the_safety_baseline() {
    let (audio, backend, clock, events) = playing_audio();
    audio.set_playing(false);
    clock.set_ms(1_000);

    backend.emit(42, false);

    assert_eq!(recorded_events(&events), vec![(42, false)]);
}

#[test]
fn local_unmute_restores_the_last_known_level_after_sending_unmute() {
    let (audio, backend, _clock, _events) = playing_audio();
    audio.set_volume(37).expect("local volume succeeds");
    audio.set_muted(true).expect("local mute succeeds");

    audio.set_muted(false).expect("local unmute succeeds");

    assert_eq!(
        calls(&backend),
        vec![
            AudioCall::Subscribe,
            AudioCall::Volume(37),
            AudioCall::Muted(true),
            AudioCall::Muted(false),
            AudioCall::Volume(37),
        ]
    );
}

#[test]
fn local_mute_does_not_rewrite_the_volume() {
    let (audio, backend, _clock, _events) = playing_audio();

    audio.set_muted(true).expect("local mute succeeds");

    assert_eq!(
        calls(&backend),
        vec![AudioCall::Subscribe, AudioCall::Muted(true)]
    );
}

#[test]
fn out_of_range_volume_is_rejected_without_touching_the_backend() {
    let (audio, backend, _clock, _events) = playing_audio();

    assert!(audio.set_volume(101).is_err());

    assert_eq!(calls(&backend), vec![AudioCall::Subscribe]);
}

#[derive(Clone, Default)]
struct FakeTvPowerBackend {
    registrations: Arc<Mutex<Vec<u64>>>,
}

impl TvPowerBackend for FakeTvPowerBackend {
    fn register_screen_saver_veto(&self, timestamp_secs: u64) -> Result<()> {
        self.registrations
            .lock()
            .expect("registrations lock")
            .push(timestamp_secs);
        Ok(())
    }
}

#[test]
fn keep_awake_registers_and_idempotent_renewal_registers_again() {
    let backend = FakeTvPowerBackend::default();
    let registrations = Arc::clone(&backend.registrations);
    let power = TvPower::with_clock(backend, || 1_234_567);

    power.keep_awake().expect("initial veto succeeds");
    power.keep_awake().expect("60-second renewal succeeds");

    assert_eq!(
        registrations.lock().expect("registrations lock").as_slice(),
        &[1_234_567, 1_234_567]
    );
}

#[test]
fn volume_reply_parses_snapshots_and_exposes_root_rejection() {
    let snapshot =
        parse_volume_reply(r#"{"returnValue":true,"subscribed":true,"volume":42,"muted":true}"#)
            .expect("valid volume reply");
    assert!(snapshot.accepted);
    assert_eq!(snapshot.volume, Some(42));
    assert_eq!(snapshot.muted, Some(true));

    let rejected = parse_volume_reply(r#"{"returnValue":false,"errorText":"denied"}"#)
        .expect("valid rejection reply");
    assert!(!rejected.accepted);
    assert_eq!(rejected.volume, None);
}

#[test]
fn veto_reply_requires_both_return_value_and_active_subscription() {
    assert!(
        veto_reply_accepted(r#"{"returnValue":true,"subscribed":true}"#)
            .expect("valid veto acknowledgement")
    );
    assert!(
        !veto_reply_accepted(r#"{"returnValue":true,"subscribed":false}"#)
            .expect("unsubscribed veto acknowledgement")
    );
    assert!(
        !veto_reply_accepted(r#"{"returnValue":false}"#).expect("rejected veto acknowledgement")
    );
}

#[test]
fn volume_observation_emits_only_initial_and_changed_snapshots() {
    let mut observation = VolumeObservation::default();
    assert!(observation.changed(23, false), "initial snapshot");
    assert!(!observation.changed(23, false), "duplicate poll");
    assert!(observation.changed(24, false), "external volume");
    assert!(!observation.changed(24, false), "duplicate poll");
    assert!(observation.changed(24, true), "external mute");
    assert!(observation.changed(24, false), "external unmute");
}

#[test]
fn poll_scheduling_allows_only_one_outstanding_request_chain() {
    assert!(
        needs_volume_poll(false, false),
        "the first accepted reply starts exactly one polling chain"
    );
    // The chain's repeating g_timeout_add source issues every later poll, so neither a
    // subscription push nor a one-shot getVolume reply may start another chain.
    assert!(
        !needs_volume_poll(true, false),
        "a later reply (push or one-shot) starts no second chain"
    );
    assert!(
        !needs_volume_poll(false, true),
        "master fallback uses its native subscription"
    );
    assert!(
        !needs_volume_poll(true, true),
        "master fallback never polls"
    );
}

#[test]
fn subscription_retry_backoff_starts_at_one_second_and_caps_at_thirty() {
    let mut delay = SUBSCRIPTION_RETRY_INITIAL;
    let mut delays = Vec::new();
    for _ in 0..6 {
        delays.push(delay);
        delay = next_subscription_retry_delay(delay);
    }

    assert_eq!(
        delays,
        vec![1, 2, 4, 8, 16, 30]
            .into_iter()
            .map(std::time::Duration::from_secs)
            .collect::<Vec<_>>()
    );
    assert_eq!(delay, SUBSCRIPTION_RETRY_MAX);
    assert_eq!(
        next_subscription_retry_delay(SUBSCRIPTION_RETRY_MAX),
        SUBSCRIPTION_RETRY_MAX
    );
}

#[test]
fn poll_failure_logging_is_rate_limited_after_the_first() {
    assert!(should_log_poll_failure(1));
    for failures in 2..POLL_FAILURE_LOG_INTERVAL {
        assert!(!should_log_poll_failure(failures), "failure #{failures}");
    }
    assert!(should_log_poll_failure(POLL_FAILURE_LOG_INTERVAL));
    assert!(!should_log_poll_failure(POLL_FAILURE_LOG_INTERVAL + 1));
    assert!(should_log_poll_failure(POLL_FAILURE_LOG_INTERVAL * 2));
}

#[test]
fn veto_already_registered_confirms_the_existing_subscription_idempotently() {
    assert!(veto_reply_accepted(
        r#"{"returnValue":false,"errorCode":"-3","errorText":"The client is already registered","subscribed":false}"#
    )
    .expect("idempotent already-registered acknowledgement"));
}

#[test]
fn screensaver_request_with_active_state_yields_its_timestamp() {
    assert_eq!(
        veto_request_timestamp(
            r#"{"returnValue":true,"timestamp":"1699999999","state":"Active","instantBoot":"on"}"#
        ),
        Some("1699999999".to_string())
    );
}

#[test]
fn screensaver_request_without_subscribed_flag_yields_its_timestamp() {
    assert_eq!(
        veto_request_timestamp(r#"{"returnValue":true,"timestamp":"1700000042"}"#),
        Some("1700000042".to_string())
    );
}

#[test]
fn subscription_ack_is_not_a_screensaver_request() {
    assert_eq!(
        veto_request_timestamp(r#"{"returnValue":true,"subscribed":true}"#),
        None
    );
}

#[test]
fn already_registered_error_is_not_a_screensaver_request() {
    let payload = r#"{"returnValue":false,"errorCode":"-3","errorText":"The client is already registered","subscribed":false}"#;
    assert_eq!(veto_request_timestamp(payload), None);
    assert!(veto_reply_accepted(payload).expect("idempotent acknowledgement"));
}

#[test]
fn screensaver_response_payload_is_exact() {
    assert_eq!(
        response_screen_saver_payload("com.github.acuntex.takt", "1699999999"),
        r#"{"clientName":"com.github.acuntex.takt","ack":false,"timestamp":"1699999999"}"#
    );
}

#[test]
fn screensaver_requests_precede_the_subscription_classifier() {
    let requests = [
        (
            r#"{"returnValue":true,"timestamp":"1699999999","state":"Active","instantBoot":"on"}"#,
            "1699999999",
        ),
        (
            r#"{"returnValue":true,"timestamp":"1700000042"}"#,
            "1700000042",
        ),
    ];
    for (payload, timestamp) in requests {
        assert_eq!(
            veto_request_timestamp(payload),
            Some(timestamp.to_string()),
            "request shape must be recognized: {payload}"
        );
        // `veto_reply_accepted` alone reports an unsubscribed failure for a request. `veto_cb`
        // must therefore dispatch on `veto_request_timestamp` first; this pins the mutual
        // exclusion between the two classifiers.
        assert!(
            !veto_reply_accepted(payload).expect("request payload is valid JSON"),
            "request must not look like a held subscription: {payload}"
        );
    }
}

#[test]
fn incomplete_payloads_are_not_screensaver_requests() {
    assert_eq!(veto_request_timestamp(r#"{"state":"Active"}"#), None);
    assert_eq!(
        veto_request_timestamp(r#"{"timestamp":"1699999999"}"#),
        None
    );
    assert_eq!(
        veto_request_timestamp(
            r#"{"returnValue":true,"timestamp":"1699999999","subscribed":true}"#
        ),
        None
    );
}

#[test]
fn foreground_reply_accepts_app_id_and_foreground_app_id() {
    assert_eq!(
        parse_foreground_app(r#"{"returnValue":true,"appId":"com.webos.app.livetv"}"#)
            .expect("appId reply"),
        Some("com.webos.app.livetv".to_owned())
    );
    assert_eq!(
        parse_foreground_app(r#"{"returnValue":true,"foregroundAppId":"com.github.acuntex.takt"}"#)
            .expect("foregroundAppId reply"),
        Some(APP_ID.to_owned())
    );
    assert_eq!(
        parse_foreground_app(r#"{"returnValue":true}"#).expect("launcher reply"),
        None,
        "no foreground app still needs a launch"
    );
}

#[test]
fn a_pushed_foreground_subscription_reply_parses_like_a_query() {
    // The first answer to `{"subscribe":true}` is the acknowledgement and may already name the
    // current app; every later answer is one foreground change.
    assert_eq!(
        parse_foreground_app(
            r#"{"appId":"com.github.acuntex.takt","subscribed":true,"returnValue":true}"#
        )
        .expect("subscription acknowledgement"),
        Some(APP_ID.to_owned())
    );
    assert_eq!(
        parse_foreground_app(
            r#"{"appId":"com.netflix.nic90","subscribed":true,"returnValue":true}"#
        )
        .expect("pushed foreground change"),
        Some("com.netflix.nic90".to_owned())
    );
    assert_eq!(
        parse_foreground_app(r#"{"subscribed":true,"returnValue":true}"#)
            .expect("acknowledgement without an app id"),
        None,
        "an acknowledgement without an app id is not an observation"
    );
}

#[test]
fn rejected_foreground_query_is_an_error_not_a_launch_decision() {
    let error = parse_foreground_app(r#"{"returnValue":false,"errorText":"denied"}"#)
        .expect_err("a rejected query must not be mistaken for an empty foreground");
    assert!(error.to_string().contains("denied"));
}

#[test]
fn only_our_own_app_id_suppresses_the_launch() {
    assert_eq!(
        foreground_decision(Some(APP_ID)),
        ForegroundDecision::AlreadyForeground
    );
    assert_eq!(
        foreground_decision(Some("com.webos.app.livetv")),
        ForegroundDecision::Launch
    );
    assert_eq!(
        foreground_decision(None),
        ForegroundDecision::Launch,
        "the launcher has no foreground app"
    );
}

#[test]
fn launch_payload_and_reply_matching_are_exact() {
    assert_eq!(launch_payload(), r#"{"id":"com.github.acuntex.takt"}"#);
    launch_accepted(r#"{"returnValue":true,"appId":"com.github.acuntex.takt"}"#)
        .expect("accepted launch reply");
    let error = launch_accepted(r#"{"returnValue":false,"errorText":"not installed"}"#)
        .expect_err("a rejected launch is a failure");
    assert!(error.to_string().contains("not installed"));
}

#[test]
fn first_launch_payload_parses_without_a_retry_token() {
    // The exact bytes are pinned by `launch_payload_and_reply_matching_are_exact`; this adds the
    // shape the application manager must see: the app id and no retry member.
    let payload = launch_payload();
    let value: serde_json::Value = serde_json::from_str(&payload).expect("valid launch JSON");
    assert_eq!(value["id"], APP_ID);
    assert!(
        value.get("params").is_none(),
        "the first attempt must stay the plain launch"
    );
}

#[test]
fn retry_payload_is_distinct_per_token_and_keeps_the_app_id() {
    assert_eq!(
        retry_launch_payload(7),
        r#"{"id":"com.github.acuntex.takt","params":{"relaunch":7}}"#
    );
    let first = retry_launch_payload(1);
    let second = retry_launch_payload(2);
    assert_ne!(first, second, "the retry must differ from a previous retry");
    assert_ne!(
        first,
        launch_payload(),
        "the retry must not repeat the launch that was swallowed"
    );

    for (payload, token) in [(&first, 1), (&second, 2)] {
        let value: serde_json::Value =
            serde_json::from_str(payload).expect("valid retry launch JSON");
        assert_eq!(value["id"], APP_ID);
        assert_eq!(value["params"]["relaunch"], token);
    }
}

#[test]
fn auto_open_timeline_fits_inside_the_command_acknowledgement_timeout() {
    // The worker's `COMMAND_TIMEOUT` is 5 s. Verification must happen well before the deadline,
    // and the deadline before the acknowledgement timeout, so the caller always gets an answer.
    assert_eq!(VERIFY_DELAY_MS, 1_500);
    assert_eq!(REQUEST_DEADLINE_MS, 4_000);
    const { assert!(VERIFY_DELAY_MS < REQUEST_DEADLINE_MS) };
    const { assert!(REQUEST_DEADLINE_MS < 5_000) };
}
