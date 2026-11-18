//! Foreground-app policy for the leave rule.
//!
//! Back and Home raise webOS's launcher overlay, which hides the page without leaving the app.
//! Hiding alone therefore must not pause playback: the daemon asks what the TV says is in front,
//! and this module is the single place that decides whether that answer means a departure. The
//! TV pushes every foreground change on a held Luna subscription; the watchdog applies this
//! verdict to each push, and its five-second fallback queries the same [`ForegroundCache`]
//! `GET /api/foreground` reads. `None` and the empty string are "the TV named no app" (the
//! launcher with no card selected, or a transition), which is not a departure either.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

/// Application id of the bundled TV UI; also the LS2 service name.
pub const APP_ID: &str = "com.github.acuntex.takt";

/// Foreground ids that are the TV's own launcher, not a departure from Takt.
///
/// * `com.webos.app.home` — webOS 3.0+/4.x Home; Back out of an app also opens it.
/// * `com.webos.app.launcher` — the older launcher id still present on some builds.
pub const LAUNCHER_APP_IDS: &[&str] = &["com.webos.app.home", "com.webos.app.launcher"];

/// While the daemon is bringing the UI to the front, a foreign app in front is expected.
///
/// An auto-open takes a launch plus its verification (up to ~2 s); until it lands the foreground
/// is still Netflix or Live TV, and pausing there would kill playback in the very moment the
/// viewer asked for it.
pub const AUTO_OPEN_GRACE: Duration = Duration::from_secs(4);

/// A deadline before which the watchdog must not act on a departure.
pub type GraceUntil = std::sync::Arc<std::sync::Mutex<Option<Instant>>>;

/// Whether the auto-open grace window still holds at `now`.
pub fn in_grace(grace: &GraceUntil, now: Instant) -> bool {
    grace
        .lock()
        .expect("grace lock poisoned")
        .is_some_and(|until| now < until)
}

/// Arms the auto-open grace window from `now`.
pub fn arm_grace(grace: &GraceUntil, now: Instant) {
    *grace.lock().expect("grace lock poisoned") = Some(now + AUTO_OPEN_GRACE);
}

/// The shortest interval between two Luna foreground queries. A UI read and the watchdog's
/// fallback share this cache, so neither can cost more than one query per second.
pub const FOREGROUND_CACHE_TTL: Duration = Duration::from_secs(1);

/// What a foreground answer means for playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveVerdict {
    /// Our app, the launcher overlay, or an unnamed foreground: the user has not left.
    Stay,
    /// A different app or TV input is in front: pause.
    Leave,
}

/// Decides whether a `getForegroundAppInfo` answer means that another app is in front.
pub fn leave_verdict(app_id: Option<&str>) -> LeaveVerdict {
    match app_id {
        None | Some("") => LeaveVerdict::Stay,
        Some(id) if id == APP_ID || LAUNCHER_APP_IDS.contains(&id) => LeaveVerdict::Stay,
        Some(_) => LeaveVerdict::Leave,
    }
}

/// Supplies one `getForegroundAppInfo` answer; `Ok(None)` means the TV named no app.
type ForegroundQuery = Arc<dyn Fn() -> Result<Option<String>> + Send + Sync>;

struct Entry {
    at: Instant,
    app: Option<String>,
    failed: bool,
}

/// Caches the foreground answer for [`FOREGROUND_CACHE_TTL`], failures included, so a UI read
/// cannot hammer the Luna worker. A failure is remembered for the same window and then retried;
/// only the transition into and out of failure is logged, never a per-query result.
///
/// Clones share the query and the cached window: the HTTP endpoint and the watchdog's fallback
/// must cost one Luna query between them, not one each.
#[derive(Clone)]
pub struct ForegroundCache {
    query: ForegroundQuery,
    ttl: Duration,
    entry: Arc<Mutex<Option<Entry>>>,
}

impl ForegroundCache {
    pub fn new(query: impl Fn() -> Result<Option<String>> + Send + Sync + 'static) -> Self {
        Self::with_ttl(query, FOREGROUND_CACHE_TTL)
    }

    /// Builds a cache with an explicit window; the foreground tests use zero to observe every
    /// query and a long window to prove that a second read is served from the cache.
    pub fn with_ttl(
        query: impl Fn() -> Result<Option<String>> + Send + Sync + 'static,
        ttl: Duration,
    ) -> Self {
        Self {
            query: Arc::new(query),
            ttl,
            entry: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns the foreground app, querying Luna at most once per cached window.
    pub fn read(&self) -> Result<Option<String>> {
        let mut entry = self.entry.lock().expect("foreground cache lock poisoned");
        if let Some(cached) = entry.as_ref() {
            if cached.at.elapsed() < self.ttl {
                return if cached.failed {
                    Err(anyhow!("foreground query failed"))
                } else {
                    Ok(cached.app.clone())
                };
            }
        }
        match (self.query)() {
            Ok(app) => {
                if entry.as_ref().is_some_and(|cached| cached.failed) {
                    println!("[takt] foreground query recovered");
                }
                *entry = Some(Entry {
                    at: Instant::now(),
                    app: app.clone(),
                    failed: false,
                });
                Ok(app)
            }
            Err(error) => {
                if !entry.as_ref().is_some_and(|cached| cached.failed) {
                    eprintln!("[takt] foreground query failed: {error:#}");
                }
                *entry = Some(Entry {
                    at: Instant::now(),
                    app: None,
                    failed: true,
                });
                Err(error)
            }
        }
    }
}
