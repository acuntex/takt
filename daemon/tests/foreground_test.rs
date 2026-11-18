//! Hermetic tests for the foreground-app policy behind `GET /api/foreground`.
//!
//! Only the pure module is included: nothing here loads LS2, starts glib, or reads a TV.

#![allow(dead_code)]

#[path = "../src/foreground.rs"]
mod foreground;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use foreground::{leave_verdict, ForegroundCache, LeaveVerdict, APP_ID, LAUNCHER_APP_IDS};

#[test]
fn our_own_app_is_not_leaving() {
    assert_eq!(leave_verdict(Some(APP_ID)), LeaveVerdict::Stay);
}

#[test]
fn the_home_and_old_launcher_ids_are_not_leaving() {
    for id in LAUNCHER_APP_IDS {
        assert_eq!(
            leave_verdict(Some(id)),
            LeaveVerdict::Stay,
            "launcher id {id} must not pause playback"
        );
    }
}

#[test]
fn another_app_or_input_is_leaving() {
    for id in [
        "com.webos.app.livetv",
        "com.netflix.nic90",
        "com.webos.app.hdmi1",
        "com.webos.app.browser",
    ] {
        assert_eq!(
            leave_verdict(Some(id)),
            LeaveVerdict::Leave,
            "foreground app {id} must pause playback"
        );
    }
}

#[test]
fn an_empty_or_missing_foreground_is_not_leaving() {
    assert_eq!(leave_verdict(Some("")), LeaveVerdict::Stay);
    assert_eq!(leave_verdict(None), LeaveVerdict::Stay);
}

#[test]
fn a_failed_query_is_reported_and_not_repeated_within_the_ttl() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let cache = ForegroundCache::with_ttl(
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("luna is gone")
        },
        Duration::from_secs(60),
    );

    assert!(cache.read().is_err(), "a failed query must be an error");
    assert!(
        cache.read().is_err(),
        "the cached failure must not turn into an answer"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn an_answer_within_the_ttl_does_not_reach_luna_twice() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let cache = ForegroundCache::with_ttl(
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Some("com.webos.app.home".to_owned()))
        },
        Duration::from_secs(60),
    );

    assert_eq!(
        cache.read().expect("first read"),
        Some("com.webos.app.home".to_owned())
    );
    assert_eq!(
        cache.read().expect("cached read"),
        Some("com.webos.app.home".to_owned())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn the_cache_lets_a_new_query_through_after_its_ttl() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let cache = ForegroundCache::with_ttl(
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        },
        Duration::ZERO,
    );

    assert_eq!(cache.read().expect("first read"), None);
    assert_eq!(cache.read().expect("second read"), None);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_cache_clone_shares_the_query_and_the_cached_answer() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let cache = ForegroundCache::with_ttl(
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Some(APP_ID.to_owned()))
        },
        Duration::from_secs(60),
    );
    let watchdog_side = cache.clone();

    assert_eq!(cache.read().expect("UI-side read"), Some(APP_ID.to_owned()));
    assert_eq!(
        watchdog_side.read().expect("watchdog-side read"),
        Some(APP_ID.to_owned())
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the clone must share the window, not cost a second Luna query"
    );
}
