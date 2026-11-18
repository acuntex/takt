#![allow(dead_code)]

#[path = "../src/pulse.rs"]
mod pulse;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use pulse::{sink_percent, volume_percent, PactlRunner, PulseSink, PulseVolume};

fn recording_runner(
    calls: Arc<Mutex<Vec<(String, String)>>>,
    fail_on: Option<&'static str>,
) -> PactlRunner {
    Arc::new(move |sink: &str, percent: &str| {
        calls
            .lock()
            .expect("calls lock")
            .push((sink.to_owned(), percent.to_owned()));
        if fail_on == Some(percent) {
            Err(anyhow!("simulated pactl failure"))
        } else {
            Ok(())
        }
    })
}

#[test]
fn percent_argument_covers_the_full_range() {
    assert_eq!(volume_percent(0).unwrap(), "0%");
    assert_eq!(volume_percent(50).unwrap(), "71%");
    assert_eq!(volume_percent(100).unwrap(), "100%");
}

#[test]
fn sink_percent_lifts_low_logical_volumes_into_the_audible_range() {
    assert_eq!(sink_percent(0), 0);
    assert_eq!(sink_percent(1), 10);
    assert_eq!(sink_percent(4), 20);
    assert_eq!(sink_percent(20), 45);
    assert_eq!(sink_percent(25), 50);
    assert_eq!(sink_percent(50), 71);
    assert_eq!(sink_percent(100), 100);
}

#[test]
fn percent_argument_rejects_values_above_one_hundred() {
    assert!(volume_percent(101).is_err());
}

#[test]
fn sink_name_is_preserved() {
    assert_eq!(PulseSink::new("pcm_output").sink(), "pcm_output");
    assert_eq!(PulseSink::new("custom").sink(), "custom");
}

#[test]
fn set_volume_passes_sink_and_percent_to_the_runner() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let sink = PulseSink::with_runner("pcm_output", recording_runner(Arc::clone(&calls), None));

    sink.set_volume(0).expect("zero is valid");
    sink.set_volume(20).expect("twenty is valid");
    sink.set_volume(100).expect("one hundred is valid");

    assert_eq!(
        *calls.lock().expect("calls lock"),
        vec![
            ("pcm_output".to_owned(), "0%".to_owned()),
            ("pcm_output".to_owned(), "45%".to_owned()),
            ("pcm_output".to_owned(), "100%".to_owned()),
        ]
    );
}

#[test]
fn invalid_volume_does_not_reach_the_runner() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let sink = PulseSink::with_runner("pcm_output", recording_runner(Arc::clone(&calls), None));

    assert!(sink.set_volume(101).is_err());
    assert!(calls.lock().expect("calls lock").is_empty());
}

#[test]
fn set_volume_returns_runner_errors() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let sink = PulseSink::with_runner(
        "pcm_output",
        recording_runner(Arc::clone(&calls), Some("71%")),
    );

    assert!(sink.set_volume(50).is_err());
}

#[tokio::test]
async fn volume_queue_applies_in_order_and_continues_after_a_sink_error() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(PulseSink::with_runner(
        "pcm_output",
        recording_runner(Arc::clone(&calls), Some("32%")),
    ));
    let volume = PulseVolume::spawn(sink);

    assert!(volume.set_volume(10).is_ok());
    assert!(volume.set_volume(20).is_ok());

    for _ in 0..100 {
        if calls.lock().expect("calls lock").len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *calls.lock().expect("calls lock"),
        vec![
            ("pcm_output".to_owned(), "32%".to_owned()),
            ("pcm_output".to_owned(), "45%".to_owned()),
        ]
    );
}
