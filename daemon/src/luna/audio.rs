//! TV-master audio state and backend-independent control.
//!
//! [`AudioState`] contains the echo/mute policy and takes time as an argument, making its behavior
//! deterministic in tests. [`Audio`] adds a small backend trait; the productive backend is the
//! dedicated Luna worker, while tests use an in-memory fake and never load LS2.
//!
//! Subscription callbacks deliver external TV volume/mute changes regardless of playback state, so
//! the initial idle snapshot can seed the safety baseline. Reporting those changes to Music
//! Assistant is gated by the App, which only forwards them while `Playing`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::Deserialize;

/// Parsed LS2 `getVolume` reply, independent of the FFI message lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VolumeReply {
    pub accepted: bool,
    pub volume: Option<u8>,
    pub muted: Option<bool>,
}

#[derive(Deserialize)]
struct WireVolumeReply {
    #[serde(rename = "returnValue", default)]
    accepted: bool,
    volume: Option<i64>,
    #[serde(default, alias = "muteStatus")]
    muted: Option<bool>,
}

/// Parses both root/master subscription replies without touching LS2.
pub(crate) fn parse_volume_reply(payload: &str) -> Result<VolumeReply> {
    let wire: WireVolumeReply = serde_json::from_str(payload)?;
    let volume = match wire.volume {
        Some(volume) if !(0..=100).contains(&volume) => {
            bail!("TV volume {volume} out of range (0-100)")
        }
        Some(volume) => Some(volume as u8),
        None => None,
    };
    Ok(VolumeReply {
        accepted: wire.accepted,
        volume,
        muted: wire.muted,
    })
}

/// Last raw TV snapshot used by the webOS 4 polling fallback to suppress duplicate polls.
/// The initial snapshot is a change; afterwards only volume/mute transitions reach `AudioState`.
#[derive(Debug, Default)]
pub struct VolumeObservation {
    volume: Option<u8>,
    muted: bool,
}

impl VolumeObservation {
    pub fn changed(&mut self, volume: u8, muted: bool) -> bool {
        let changed = self.volume != Some(volume) || self.muted != muted;
        self.volume = Some(volume);
        self.muted = muted;
        changed
    }
}

/// Whether this accepted reply should start the volume polling chain. Exactly one chain may exist:
/// the first accepted reply starts a single repeating `g_timeout_add` source, and that source owns
/// every later poll. This TV acknowledges subscriptions but never pushes changes, so a later
/// one-shot reply must not schedule another source (it would multiply the poll rate). The OSE
/// master fallback uses its native subscription instead.
pub(crate) fn needs_volume_poll(polling_started: bool, master_fallback_attempted: bool) -> bool {
    !master_fallback_attempted && !polling_started
}

/// Own writes are echoes only strictly inside this 500 ms window.
pub const ECHO_WINDOW: Duration = Duration::from_millis(500);

/// Boxed subscription callback, invoked with `(volume, muted)` on the backend's callback thread.
pub type VolumeCallback = Box<dyn Fn(u8, bool) + Send + 'static>;

/// Small transport seam implemented by the Luna worker and by hermetic test fakes.
pub trait AudioBackend: Send + Sync {
    fn set_volume(&self, volume: u8) -> Result<()>;
    fn set_muted(&self, muted: bool) -> Result<()>;
    fn subscribe(&self, callback: VolumeCallback) -> Result<()>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Echo<T> {
    pub(crate) value: T,
    pub(crate) written_at: Duration,
}

impl<T: Copy + PartialEq> Echo<T> {
    pub(crate) fn matches(&self, value: T, now: Duration) -> bool {
        self.value == value && now.saturating_sub(self.written_at) < ECHO_WINDOW
    }
}

fn record_echo<T>(history: &mut Vec<Echo<T>>, value: T, now: Duration) {
    history.retain(|echo| now.saturating_sub(echo.written_at) < ECHO_WINDOW);
    history.push(Echo {
        value,
        written_at: now,
    });
}

fn prune_echoes<T>(history: &mut Vec<Echo<T>>, now: Duration) {
    history.retain(|echo| now.saturating_sub(echo.written_at) < ECHO_WINDOW);
}

/// Initial retry delay after a failed volume subscription setup.
pub const SUBSCRIPTION_RETRY_INITIAL: Duration = Duration::from_secs(1);
/// Upper bound for the volume-subscription setup retry backoff.
pub const SUBSCRIPTION_RETRY_MAX: Duration = Duration::from_secs(30);
/// Number of failed poll submissions between rate-limited poll-failure logs (10 s at 250 ms).
pub const POLL_FAILURE_LOG_INTERVAL: u32 = 40;

/// Bounded exponential backoff for the initial volume subscription: 1 → 2 → 4 … → 30 seconds.
pub fn next_subscription_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(SUBSCRIPTION_RETRY_MAX)
}

/// Keeps a broken poll chain from flooding the log while still surfacing the first failure.
pub fn should_log_poll_failure(failures: u32) -> bool {
    failures == 1 || (failures > 1 && failures.is_multiple_of(POLL_FAILURE_LOG_INTERVAL))
}

/// Pure TV-audio state machine.
///
/// Local writes are kept in a small per-dimension history so several own writes (and a late poll
/// that read an earlier value) are all treated as echoes inside the window. Subscription events
/// update the last known TV state and are delivered even while paused or idle; the App decides
/// whether to report them to Music Assistant. Volume and mute echoes are filtered independently so
/// an external mute/unmute is never swallowed by a matching volume echo. An unmute event carries
/// the volume restored by the TV in the same callback.
#[derive(Debug, Default)]
pub struct AudioState {
    last_volume: Option<u8>,
    muted: bool,
    volume_echoes: Vec<Echo<u8>>,
    mute_echoes: Vec<Echo<bool>>,
}

impl AudioState {
    /// Compatibility no-op: external events are no longer gated here. The App suppresses MA
    /// reporting while not playing.
    pub fn set_playing(&mut self, _playing: bool) {}

    pub fn local_volume(&mut self, volume: u8, now: Duration) {
        self.last_volume = Some(volume);
        record_echo(&mut self.volume_echoes, volume, now);
    }

    pub fn local_muted(&mut self, muted: bool, now: Duration) {
        self.muted = muted;
        record_echo(&mut self.mute_echoes, muted, now);
    }

    /// Returns the external event to deliver, or `None` for an echo or an unchanged snapshot.
    /// Delivery is independent of playback state; the App gates MA reporting.
    pub fn external_event(&mut self, volume: u8, muted: bool, now: Duration) -> Option<(u8, bool)> {
        prune_echoes(&mut self.volume_echoes, now);
        prune_echoes(&mut self.mute_echoes, now);

        let volume_echo = self
            .volume_echoes
            .iter()
            .any(|echo| echo.matches(volume, now));
        let mute_echo = self.mute_echoes.iter().any(|echo| echo.matches(muted, now));

        // A mute subscription snapshot also carries the current volume. If that snapshot merely
        // acknowledges our mute, its unchanged (or initially unknown) level is not a new external
        // volume event. A genuinely different level is still reported.
        let volume_is_mute_snapshot = mute_echo
            && self
                .last_volume
                .is_none_or(|last_volume| last_volume == volume);
        let volume_changed = self.last_volume != Some(volume);
        let external_volume = volume_changed && !volume_echo && !volume_is_mute_snapshot;
        let external_mute = !mute_echo && muted != self.muted;
        let external = external_volume || external_mute;

        self.last_volume = Some(volume);
        self.muted = muted;

        external.then_some((volume, muted))
    }
}

/// Backend-independent productive audio API.
///
/// `set_muted(false)` sends unmute first and then restores the last known level with `set_volume`.
/// The clock is injectable; production uses a monotonic [`Instant`] and tests use explicit time.
pub struct Audio<B: AudioBackend> {
    backend: B,
    state: Arc<Mutex<AudioState>>,
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
}

impl<B: AudioBackend> Audio<B> {
    pub fn new(backend: B) -> Self {
        let started = Instant::now();
        Self::with_clock(backend, move || started.elapsed())
    }

    pub fn with_clock(backend: B, clock: impl Fn() -> Duration + Send + Sync + 'static) -> Self {
        Self {
            backend,
            state: Arc::new(Mutex::new(AudioState::default())),
            clock: Arc::new(clock),
        }
    }

    /// Compatibility no-op retained for the `TvControl` seam. External events are always delivered
    /// by [`Audio`]; the App gates MA reporting on playback state.
    pub fn set_playing(&self, playing: bool) {
        self.state
            .lock()
            .expect("audio state lock poisoned")
            .set_playing(playing);
    }

    /// Subscribes to TV volume/mute snapshots. External changes reach `callback` in every playback
    /// state so the App can keep its safety baseline current; the App gates MA reporting.
    pub fn subscribe(&self, callback: impl Fn(u8, bool) + Send + 'static) -> Result<()> {
        let state = Arc::clone(&self.state);
        let clock = Arc::clone(&self.clock);
        self.backend.subscribe(Box::new(move |volume, muted| {
            let event = {
                let mut state = state.lock().expect("audio state lock poisoned");
                state.external_event(volume, muted, clock())
            };
            if let Some((volume, muted)) = event {
                callback(volume, muted);
            }
        }))
    }

    /// Writes TV-master volume (0–100) and records the value used for echo suppression.
    pub fn set_volume(&self, volume: u8) -> Result<()> {
        if volume > 100 {
            bail!("volume {volume} out of range (0-100)");
        }
        // Record intent before the backend can synchronously produce a subscription/poll echo.
        // A later observed snapshot repairs state if the backend write fails.
        self.state
            .lock()
            .expect("audio state lock poisoned")
            .local_volume(volume, (self.clock)());
        self.backend.set_volume(volume)
    }

    /// Writes TV mute. Unmute restores the last known level after the unmute call succeeds.
    pub fn set_muted(&self, muted: bool) -> Result<()> {
        let restore = {
            let mut state = self.state.lock().expect("audio state lock poisoned");
            state.local_muted(muted, (self.clock)());
            (!muted).then_some(state.last_volume).flatten()
        };
        self.backend.set_muted(muted)?;

        if let Some(volume) = restore {
            self.state
                .lock()
                .expect("audio state lock poisoned")
                .local_volume(volume, (self.clock)());
            self.backend.set_volume(volume)?;
        }
        Ok(())
    }
}
