//! Foreground watchdog: pause when another app takes the screen.
//!
//! The TV pushes every foreground change on a held
//! `com.webos.applicationManager/getForegroundAppInfo` subscription (`{"subscribe":true}`), so a
//! departure is seen as soon as webOS reports it — before the suspended page could run any timer
//! of its own. The pushed app id is parsed by [`crate::luna::autoopen::parse_foreground_app`] and
//! forwarded over a Tokio channel; this module applies [`crate::foreground::leave_verdict`] and
//! sends the `pause` a remote would through the transport command channel.
//!
//! A five-second fallback query covers a subscription that failed to arm or a TV that stopped
//! pushing: the tick only runs when no event arrived in the window, and a failed query is ignored.
//! The fallback reads the same [`ForegroundCache`] as `GET /api/foreground`.
//!
//! Only one pause goes out per departure: a foreign app that stays in front is not paused again on
//! the next event, and a playing player whose foreground is our app, a launcher id, or unnamed
//! re-arms the rule for the next departure. The stay/leave rule itself lives only in
//! [`crate::foreground`]; this module adds the playback gate and the latch.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::foreground::{in_grace, leave_verdict, ForegroundCache, GraceUntil, LeaveVerdict};
use crate::state::{PlaybackState, SharedAppState};

/// How long the watchdog waits for a pushed foreground answer before it asks the TV itself.
///
/// A live subscription answers long before this, so the tick is a safety net for a subscription
/// that failed to arm or a TV that stopped pushing. Every pushed answer restarts the window.
pub const WATCHDOG_FALLBACK_INTERVAL: Duration = Duration::from_secs(5);

/// Whether one foreground observation is a departure that should pause playback now.
///
/// Only a `Playing` player pauses, only on [`LeaveVerdict::Leave`], and only once per departure.
pub fn watchdog_should_pause(
    state: PlaybackState,
    verdict: LeaveVerdict,
    already_left: bool,
    in_auto_open_grace: bool,
) -> bool {
    state == PlaybackState::Playing
        && verdict == LeaveVerdict::Leave
        && !already_left
        && !in_auto_open_grace
}

/// What one observation of the foreground means for the watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogDecision {
    /// Nothing to do.
    Nothing,
    /// A new departure: pause once and remember it.
    Pause {
        /// Foreground app that took the screen, for the log line.
        app: String,
    },
    /// Playback is playing in our app (or no app) again: re-arm for the next departure.
    Back,
}

/// The once-per-departure latch, independent of timers and I/O.
#[derive(Debug, Default)]
pub struct DepartureLatch {
    left: bool,
}

impl DepartureLatch {
    /// Applies one foreground answer to the latch and reports what should happen.
    ///
    /// A player that is not `Playing` is not observed at all: the latch keeps its state, so a
    /// resume in front of the same foreign app still counts as that app's departure and is not
    /// paused again. The latch is released only by a playing player whose foreground is our app,
    /// a launcher id, or unnamed — the [`LeaveVerdict::Stay`] rule from [`leave_verdict`].
    /// As [`DepartureLatch::observe`], but a departure inside the auto-open grace window does not
    /// pause: the launch that is about to make our app the foreground is still in flight.
    pub fn observe_with_grace(
        &mut self,
        state: PlaybackState,
        app: Option<&str>,
        in_auto_open_grace: bool,
    ) -> WatchdogDecision {
        let verdict = leave_verdict(app);
        if watchdog_should_pause(state, verdict, self.left, in_auto_open_grace) {
            self.left = true;
            return WatchdogDecision::Pause {
                app: app.unwrap_or("unknown").to_owned(),
            };
        }
        if state == PlaybackState::Playing && verdict == LeaveVerdict::Stay && self.left {
            self.left = false;
            return WatchdogDecision::Back;
        }
        WatchdogDecision::Nothing
    }
}

/// Consumes pushed foreground answers and pauses on a departure.
pub struct Watchdog {
    state: SharedAppState,
    /// Luna-backed one-shot foreground source for the fallback tick. `None` when this host has no
    /// Luna backend; the watchdog then never queries and cannot recover a departure on its own.
    foreground: Option<ForegroundCache>,
    /// Pushed answers from the held Luna subscription. `None` once the worker dropped it; the
    /// fallback tick then owns every wake-up.
    events: Option<mpsc::UnboundedReceiver<Option<String>>>,
    /// The transport command channel a remote also uses; a departure sends an ordinary `pause`.
    commands: mpsc::UnboundedSender<String>,
    latch: DepartureLatch,
    /// The wait before the fallback query; the run loop restarts it on every pushed answer.
    fallback: Duration,
    /// While an auto-open is raising the UI, a foreign app in front is expected, not a departure.
    grace: Option<GraceUntil>,
}

impl Watchdog {
    pub fn new(
        state: SharedAppState,
        foreground: Option<ForegroundCache>,
        commands: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            state,
            foreground,
            events: None,
            commands,
            latch: DepartureLatch::default(),
            fallback: WATCHDOG_FALLBACK_INTERVAL,
            grace: None,
        }
    }

    /// Shares the auto-open grace window with the daemon, so a launch in flight is not a departure.
    pub fn with_grace(mut self, grace: GraceUntil) -> Self {
        self.grace = Some(grace);
        self
    }

    /// Adds the channel that carries pushed `getForegroundAppInfo` answers.
    pub fn with_events(mut self, events: mpsc::UnboundedReceiver<Option<String>>) -> Self {
        self.events = Some(events);
        self
    }

    /// Replaces the five-second fallback window. Only the hermetic tests use this; the product
    /// keeps [`WATCHDOG_FALLBACK_INTERVAL`].
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_fallback_interval(mut self, fallback: Duration) -> Self {
        self.fallback = fallback;
        self
    }

    /// Runs one fallback query. Public so the hermetic tests can drive ticks without timers.
    pub async fn tick(&mut self) {
        // Idle, paused and connecting players cost no Luna query at all.
        if self.playback_state() != PlaybackState::Playing {
            return;
        }
        let Some(foreground) = self.foreground.clone() else {
            return;
        };
        // `getForegroundAppInfo` is answered on the dedicated glib worker; keep the async runtime
        // free while it waits. A failed query is not a departure: the cache already logged the
        // failure transition, and the next tick simply asks again.
        let app = match tokio::task::spawn_blocking(move || foreground.read()).await {
            Ok(Ok(app)) => app,
            Ok(Err(_)) | Err(_) => return,
        };
        // The answer may have taken a while; only a player that is still playing pauses.
        self.observe(app.as_deref());
    }

    /// Applies one pushed foreground answer and delivers the verdict.
    fn observe(&mut self, app: Option<&str>) {
        let state = self.playback_state();
        let in_grace_now = self
            .grace
            .as_ref()
            .is_some_and(|grace| in_grace(grace, std::time::Instant::now()));
        match self.latch.observe_with_grace(state, app, in_grace_now) {
            WatchdogDecision::Nothing => {}
            WatchdogDecision::Pause { app } => {
                println!("[takt] leaving: foreground is {app}; pausing");
                if self.commands.send("pause".to_owned()).is_err() {
                    eprintln!("[takt] leaving: playback command receiver is unavailable");
                }
            }
            WatchdogDecision::Back => println!("[takt] back in the app"),
        }
    }

    /// Watches the foreground for the daemon's lifetime.
    ///
    /// A pushed answer is evaluated immediately and restarts the fallback window; a tick fires
    /// only when nothing arrived for [`WATCHDOG_FALLBACK_INTERVAL`] (or `self.fallback`).
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.fallback);
        // A slow Luna answer must not queue a burst of catch-up ticks, and the first query is due
        // only after the window: a live subscription usually answers sooner.
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.reset();
        loop {
            tokio::select! {
                event = next_event(&mut self.events) => match event {
                    Some(app) => {
                        interval.reset();
                        self.observe(app.as_deref());
                    }
                    None => {
                        // The worker dropped the subscription. Its retry owns re-arming; this
                        // loop keeps the fallback query so a departure is still caught.
                        eprintln!(
                            "[takt] leaving: foreground subscription closed; falling back to polling"
                        );
                        self.events = None;
                    }
                },
                _ = interval.tick() => self.tick().await,
            }
        }
    }

    fn playback_state(&self) -> PlaybackState {
        self.state.read().expect("app state lock poisoned").state
    }
}

/// Waits for the next pushed answer. With no subscription the branch never completes, so the
/// fallback tick owns every wake-up.
async fn next_event(
    events: &mut Option<mpsc::UnboundedReceiver<Option<String>>>,
) -> Option<Option<String>> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}
