//! Production daemon loop and pure playback event mapping.
//!
//! [`App::handle_event`] performs no network, process, or TV I/O. Apart from low-volume
//! stream-lifecycle log lines on stderr, it only updates the shared application state and returns
//! observable [`Action`]s, allowing the playback tests to run without external services.
//! [`App::run`] drives a `syncspin_client` connection and executes those actions with the real
//! `GstSink` and [`TvControl`] implementations.
//!
//! Audio itself never passes through the daemon loop: the client crate hands each encoded frame
//! straight to [`GstSink`](crate::sink::gst::GstSink), which owns the `gst-launch` decoder and
//! writes the stream's FLAC header before the first frame. The loop only reacts to the events the
//! client publishes: playback state, metadata, volume/mute requests, the stream lifecycle
//! (`start`/`end`/`clear`) and disconnects.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use syncspin_client::{
    next_backoff, ArtworkChannel, ArtworkImage, ArtworkSource, AudioMode, Client, ClientConfig,
    ColorScheme, ControlHandle, DeviceInfo, Event, EventStream, ImageFormat, StreamEndReason,
    VolumePolicy,
};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::discovery::ServerFinder;
use crate::foreground::{arm_grace, GraceUntil};
use crate::sink::gst::{GstSink, GstSinkHandle, SharedOutputDelay};
use crate::state::{AppState, PlaybackState, SharedAppState, ThemeColors};
use crate::visual::SharedAudioVisual;

/// Upper bound for the client's reconnect delay: the sequence is 1, 2, 4 … 16, 30 seconds.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long the UI keeps reporting `connecting` after a drop before it shows `error`.
///
/// The client retries on its own, so a short drop is not worth an error label; a player that
/// has been unreachable for longer than the reconnect ceiling is.
const DISCONNECT_ERROR_TIMEOUT: Duration = Duration::from_secs(30);

/// Startup lead this host needs from the server.
///
/// The client keeps the reported lead at least its own pre-port delay (200 ms) but does not add
/// the two, so this number has to cover both: spawning `gst-launch` and `pacat` and priming the
/// decoder (measured at 100-300 ms on the TV) plus the 200 ms the frame spends before the port.
/// A server that honours it waits this long before the first frame is due.
const REQUIRED_LEAD_TIME_MS: u32 = 900;

/// Edge length of the single artwork channel, in pixels.
///
/// The UI draws the album image in a 620 px square card, so 640 px covers the card at a
/// 1080p layout without asking the server for an image larger than it ever renders.
const ARTWORK_CHANNEL_PX: u32 = 640;

/// Largest artwork image the client keeps, in bytes.
///
/// The UI only needs the 640 px JPEG above; 1 MiB is roomy for it and bounds how much an image
/// can cost the client's memory on the TV.
const MAX_ARTWORK_BYTES: usize = 1_048_576;

/// The artwork channels this client asks for: one album image at the card size.
fn artwork_channels() -> Vec<Option<ArtworkChannel>> {
    vec![Some(ArtworkChannel {
        source: ArtworkSource::Album,
        format: ImageFormat::Jpeg,
        width: ARTWORK_CHANNEL_PX,
        height: ARTWORK_CHANNEL_PX,
    })]
}

/// Largest upward TV-volume jump Music Assistant may cause in a single command above the guard
/// threshold. Anything larger is treated as an unsafe event, clamped, and reported back so MA's
/// slider drops too.
pub const MAX_VOLUME_STEP: u8 = 20;

/// Volume up to which Music Assistant requests are applied verbatim. Above it, a single upward
/// command may only add [`MAX_VOLUME_STEP`] so a stale MA slider or a mis-click cannot blast the TV.
pub const VOLUME_GUARD_THRESHOLD: u8 = 40;

/// The volume rule the client applies to every request, including the daemon's own reports.
///
/// `threshold_floor` lets a low baseline jump to the threshold in one command, so TV 1 plus a
/// request for 100 becomes 40 and every later request adds at most 20 — the daemon's deployed
/// rule, now expressed once in the client instead of twice.
pub const VOLUME_POLICY: VolumePolicy = VolumePolicy {
    threshold: VOLUME_GUARD_THRESHOLD,
    max_step: MAX_VOLUME_STEP,
    threshold_floor: true,
};

/// TV-master audio control seam.
///
/// Tests use fakes; `main` provides the productive Luna/LS2 bridge over the backend-independent
/// `Audio` state machine.
pub trait TvControl: Send {
    fn set_muted(&self, muted: bool) -> Result<()>;
    fn set_volume(&self, volume: u8) -> Result<()>;

    /// Records the playback transition for controls that care about it. This no longer gates
    /// external-volume delivery: Luna always reports TV changes, and [`App`] decides whether to
    /// forward them to Music Assistant. Non-Luna controls need no playback state.
    fn set_playing(&self, _playing: bool) -> Result<()> {
        Ok(())
    }

    /// Brings the bundled UI to the foreground unless it already is.
    ///
    /// Called at most once per playback start (a stream start that moves the player out of a
    /// non-`Playing` state), never for a track change or a reconnect that was already playing,
    /// and never retried. Implementations must not steal focus when `com.github.acuntex.takt` is already
    /// the foreground app. A failure is logged by the action runner and never stops playback, so
    /// the default is a no-op for tests and non-Luna controls.
    fn ensure_ui_foreground(&self) -> Result<()> {
        Ok(())
    }
}

/// One externally observable effect of a client event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Restart both sink children and re-prime the decoder with the current FLAC header.
    RestartSink,
    /// The client's flush barrier. Dispatch is a no-op: the client flushed before the event
    /// reached the daemon, and this pipeline discards its buffer by stopping (see
    /// [`App::execute_actions`]).
    FlushSink,
    /// Stop/reap both sink children and discard the pipeline. This is what makes a stream end
    /// silent: the player's buffered audio dies with the process.
    StopSink,
    SetMuted(bool),
    SetVolume(u8),
    /// Correct Music Assistant's view of the TV volume/mute through the client's control handle.
    ReportPlayerState(u8, bool),
    /// Playback transition, included in the action log for observability. The shared state is
    /// updated synchronously by `handle_event`, so action dispatch does not apply it again.
    UpdateState(PlaybackState),
    /// Playback started from a non-playing state: bring the bundled UI to the foreground if it is
    /// not already visible. Dispatch logs a failure and continues; it never stops playback and is
    /// never retried.
    EnsureUiForeground,
    /// The client lost the connection and retries after this delay. Informational: the client
    /// owns the reconnect, the daemon only logs the delay.
    Reconnect {
        delay: Duration,
    },
    /// Store a server-chosen post-port delay and tell the session the sink adopted it, so the
    /// next `client/state` carries the value that is really in force.
    StoreOutputDelay(u16),
}

/// One client event as the playback mapping acts on it.
///
/// This is the daemon's view of [`Event`]: it carries only what the mapping needs, so the
/// stream format (which the sink owns) is dropped and future client events map to
/// [`PlaybackEvent::Other`]. Tests can build these values directly, which a
/// `syncspin_client::AudioFormat` (non-exhaustive, constructed only by the client) would not
/// allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackEvent {
    /// The Noise handshake finished and the session is ready.
    Connected {
        /// The server's static public key, for the log line.
        server_id: String,
        /// Whether the connection is encrypted; always true for this client.
        encrypted: bool,
    },
    /// A new stream was announced; the sink is already open and primed.
    StreamStart,
    /// The player stream ended.
    StreamEnd {
        /// Why it ended.
        reason: StreamEndReason,
    },
    /// The server asked to drop buffered audio without ending the stream (a seek).
    StreamClear {
        /// Server timestamp audio should resume from, when one was named.
        from_timestamp: Option<i64>,
    },
    /// Frames arrived too late to be played in time and were dropped.
    LateAudio {
        /// Worst lateness in the reporting window.
        late_by: std::time::Duration,
        /// Frames dropped in that window.
        dropped: u64,
    },
    /// Now-playing metadata changed.
    Track {
        /// Track title, empty when absent.
        title: String,
        /// Track artist, empty when absent.
        artist: String,
        /// Album title, empty when absent.
        album: String,
        /// Artwork URL, empty when absent.
        artwork_url: String,
        /// Position within the track in milliseconds.
        progress_ms: Option<u64>,
        /// Track length in milliseconds.
        duration_ms: Option<u64>,
        /// Playback speed per mille; `1000` is normal speed.
        playback_speed: u32,
    },
    /// The server cleared the now-playing metadata while playback continues.
    ///
    /// The metadata role owns these seven fields; artwork and color keep their own lifecycles.
    TrackCleared,
    /// An artwork channel shows a new image, or no image any more.
    Artwork {
        /// Channel number, 0-3. The daemon keeps only channel 0.
        channel: u8,
        /// The image, or `None` for a cleared channel.
        image: Option<ArtworkImage>,
    },
    /// The server's colors took effect.
    Color(ThemeColors),
    /// The color state was dropped because the color role was removed.
    ColorCleared,
    /// The server's controller view changed; not authoritative for the TV volume.
    ControllerState,
    /// The server corrected the delay after the audio port.
    OutputDelay {
        /// Value the server asked for, unchanged from the wire.
        requested: u32,
        /// Value this host can apply, already limited to the protocol range.
        applied: u32,
    },
    /// The server asked for a volume or mute change, already limited by the volume policy.
    Volume {
        /// Level the server asked for, unchanged from the wire.
        requested: u8,
        /// Level the policy allows, in the protocol's 0-100 range.
        applied: u8,
        /// Requested mute state, absent for a volume command.
        muted: Option<bool>,
    },
    /// The connection attempt ended; the client retries after `retry_in`.
    Disconnected {
        /// Why the attempt ended, for the log line. Never contains key material.
        reason: String,
        /// Delay the client waits before its next attempt.
        retry_in: Duration,
    },
    /// An event this daemon revision does not act on.
    Other,
}

impl From<Event> for PlaybackEvent {
    fn from(event: Event) -> Self {
        match event {
            Event::Connected {
                server_id,
                encrypted,
            } => Self::Connected {
                server_id,
                encrypted,
            },
            Event::StreamStart { .. } => Self::StreamStart,
            Event::StreamEnd { reason } => Self::StreamEnd { reason },
            Event::StreamClear { from_timestamp } => Self::StreamClear { from_timestamp },
            Event::LateAudio { late_by, dropped } => Self::LateAudio { late_by, dropped },
            Event::Track {
                title,
                artist,
                album,
                artwork_url,
                progress,
                ..
            } => Self::Track {
                title,
                artist,
                album,
                artwork_url,
                progress_ms: progress.position_ms,
                duration_ms: progress.duration_ms,
                playback_speed: progress.speed_permille,
            },
            Event::State(_) => Self::ControllerState,
            Event::TrackCleared => Self::TrackCleared,
            Event::Artwork { channel, image } => Self::Artwork { channel, image },
            Event::Color(scheme) => Self::Color(theme_colors(scheme)),
            Event::ColorCleared => Self::ColorCleared,
            Event::Volume {
                requested,
                applied,
                muted,
            } => Self::Volume {
                requested,
                applied,
                muted,
            },
            Event::OutputDelay { requested, applied } => Self::OutputDelay { requested, applied },
            Event::Disconnected { reason, retry_in } => Self::Disconnected { reason, retry_in },
            _ => Self::Other,
        }
    }
}

/// Owns the client connection, TV-control seam, sink handle and shared application state.
pub struct App {
    config: Config,
    state: SharedAppState,
    tv: Box<dyn TvControl>,
    /// Control channel of the stopped/running sink; `None` in hermetic fixtures.
    sink: Option<GstSinkHandle>,
    /// Playback controls of the live client connection; `None` while disconnected.
    control: Option<ControlHandle>,
    command_receiver: Option<mpsc::UnboundedReceiver<String>>,
    volume_receiver: Option<mpsc::UnboundedReceiver<(u8, bool)>>,
    /// Shared PCM levels for the reactive UI; `None` in hermetic fixtures.
    visual: Option<SharedAudioVisual>,
    /// Live post-port delay shared with the sink and the HTTP API. Zero in hermetic fixtures.
    output_delay: SharedOutputDelay,
    /// While an auto-open is raising the UI, the watchdog must not treat the foreign app that is
    /// still in front as a departure.
    grace: Option<GraceUntil>,
    /// When the current run of failed connection attempts started. Cleared on every connection,
    /// so the UI shows `connecting` for [`DISCONNECT_ERROR_TIMEOUT`] before it shows `error`.
    disconnected_since: Option<Instant>,
    /// May replace an unreachable configured address with an advertised one; `None` keeps the
    /// config alone, as the hermetic fixtures do.
    finder: Option<Arc<dyn ServerFinder>>,
}

impl App {
    pub fn new(config: Config, tv: Box<dyn TvControl>) -> Self {
        let state = Arc::new(RwLock::new(AppState::new(&config)));
        Self {
            config,
            state,
            tv,
            sink: None,
            control: None,
            command_receiver: None,
            volume_receiver: None,
            visual: None,
            output_delay: Arc::new(AtomicU64::new(0)),
            grace: None,
            disconnected_since: None,
            finder: None,
        }
    }

    /// Installs the finder that may replace an unreachable configured address with an advertised
    /// one.
    pub fn with_server_finder(mut self, finder: Arc<dyn ServerFinder>) -> Self {
        self.finder = Some(finder);
        self
    }

    /// Adds external TV volume/mute events reported to MA only while playback is `Playing`.
    pub fn with_volume_events(mut self, receiver: mpsc::UnboundedReceiver<(u8, bool)>) -> Self {
        self.volume_receiver = Some(receiver);
        self
    }

    /// Seeds the authoritative TV volume/mute from the initial Luna snapshot.
    ///
    /// This runs before the client connects, so the first `client/state` advertises the real TV
    /// level instead of the safe placeholder 0.
    pub fn with_tv_volume(self, volume: u8, muted: bool) -> Self {
        // The initial Luna snapshot is the TV's observed state: it is recorded, and nothing in
        // the daemon overrides it later.
        let mut state = self.state.write().expect("app state lock poisoned");
        state.volume = Some(volume);
        state.muted = muted;
        drop(state);
        self
    }

    /// Adds controller commands from the HTTP layer without changing the `run` interface.
    pub fn with_command_receiver(mut self, receiver: mpsc::UnboundedReceiver<String>) -> Self {
        self.command_receiver = Some(receiver);
        self
    }

    /// Shares the auto-open grace window with the watchdog.
    pub fn with_grace(mut self, grace: GraceUntil) -> Self {
        self.grace = Some(grace);
        self
    }

    /// Shares analyzed PCM levels with the HTTP/UI layer. Fixtures leave this unset.
    pub fn with_visual(mut self, visual: SharedAudioVisual) -> Self {
        self.visual = Some(visual);
        self
    }

    /// Shares the live post-port delay with the sink the client will drive. Fixtures leave this
    /// at zero.
    pub fn with_output_delay(mut self, output_delay: SharedOutputDelay) -> Self {
        self.output_delay = output_delay;
        self
    }

    /// Cloneable shared-state handle for the HTTP/SSE layer and fixture tests.
    pub fn shared_state(&self) -> SharedAppState {
        Arc::clone(&self.state)
    }

    /// Records a successful connection: playback is idle until the next `stream/start`. The
    /// `Config` (and therefore the client identity) is never changed.
    pub fn connection_established(&mut self) {
        self.disconnected_since = None;
        self.set_playback_state(PlaybackState::Idle);
    }

    /// Maps one client event to state changes and external actions without performing I/O.
    pub fn handle_event(&mut self, event: PlaybackEvent) -> Vec<Action> {
        match event {
            PlaybackEvent::Connected {
                server_id,
                encrypted,
            } => {
                eprintln!("[takt] connected server={server_id} encrypted={encrypted}");
                self.connection_established();
                Vec::new()
            }
            PlaybackEvent::StreamStart => {
                // Whatever the seek was waiting for has arrived.
                self.set_seeking(false);
                // The client already opened the sink, which restarted both children and wrote the
                // stream's codec header before the first frame; only the TV and MA view remain.
                self.log_stream_event("start");
                // Only a transition out of a non-playing state is a playback start worth
                // auto-opening for. A stream start while already playing is a track change or a
                // reconnect that kept the state, and must neither steal focus nor re-open the UI.
                let was_playing = self.playback_state() == PlaybackState::Playing;
                self.set_playback_state(PlaybackState::Playing);
                let mut actions = vec![Action::UpdateState(PlaybackState::Playing)];
                // The TV shows whatever the viewer last opened; a real playback start brings the
                // UI back. `auto_open: false` keeps the viewer's app untouched.
                if !was_playing && self.config.auto_open {
                    actions.push(Action::EnsureUiForeground);
                }
                // MA's initial volume default is 100. Re-affirm the TV's actual volume (the
                // authoritative Luna value) once playback starts, so a level the two sides
                // drifted apart on cannot survive a stream start.
                let state = self.state.read().expect("app state lock poisoned");
                if let Some(volume) = state.volume {
                    actions.push(Action::ReportPlayerState(volume, state.muted));
                }
                actions
            }
            PlaybackEvent::StreamEnd { reason } => {
                self.log_stream_event(&format!("end reason={}", end_reason_name(reason)));
                // Pause stops the sink. Killing the player child discards everything it still
                // buffers — its stdin pipe and PulseAudio's queue — which is this pipeline's
                // equivalent of `snd_pcm_drop`; the client's next `stream/start` opens fresh
                // children. Mute is user intent, never a pause mechanism.
                self.set_playback_state(PlaybackState::Paused);
                vec![
                    Action::FlushSink,
                    Action::StopSink,
                    Action::UpdateState(PlaybackState::Paused),
                ]
            }
            PlaybackEvent::StreamClear { from_timestamp } => {
                self.log_stream_event(&format!(
                    "clear from={}",
                    from_timestamp.map_or_else(|| "-".to_owned(), |value| value.to_string())
                ));
                // Seek: restarting both children discards decoder, bridge, and player buffers,
                // and re-primes the fresh decoder with the cached header before any later frame.
                vec![Action::RestartSink]
            }
            PlaybackEvent::Track {
                title,
                artist,
                album,
                artwork_url,
                progress_ms,
                duration_ms,
                playback_speed,
            } => {
                let mut state = self.state.write().expect("app state lock poisoned");
                state.title = none_if_empty(title);
                state.artist = none_if_empty(artist);
                state.album = none_if_empty(album);
                state.artwork_url = none_if_empty(artwork_url);
                state.progress_ms = progress_ms;
                state.duration_ms = duration_ms;
                // MA's metadata role reports the speed in per mille; the UI interpolates with it.
                state.playback_speed = Some(playback_speed);
                // Bump on every metadata event, even when the numeric position is unchanged, so
                // the UI can distinguish a seek to the same position (e.g. 0) from a periodic
                // snapshot.
                state.metadata_revision = state.metadata_revision.wrapping_add(1);
                Vec::new()
            }
            PlaybackEvent::TrackCleared => {
                // The metadata role dropped its state. Playback, the stream and the other roles
                // are untouched; only the revision tells the UI to re-base its now-playing view.
                let mut state = self.state.write().expect("app state lock poisoned");
                state.title = None;
                state.artist = None;
                state.album = None;
                state.artwork_url = None;
                state.progress_ms = None;
                state.duration_ms = None;
                state.playback_speed = None;
                state.metadata_revision = state.metadata_revision.wrapping_add(1);
                Vec::new()
            }
            PlaybackEvent::Artwork { channel, image } => {
                // The state keeps one artwork channel; another channel is not ours to show.
                if channel != 0 {
                    return Vec::new();
                }
                let mut state = self.state.write().expect("app state lock poisoned");
                state.artwork = image.map(Arc::new);
                // A clear is a change too: the UI must drop the image it is showing.
                state.artwork_revision = state.artwork_revision.wrapping_add(1);
                Vec::new()
            }
            PlaybackEvent::Color(colors) => {
                self.state.write().expect("app state lock poisoned").color = Some(colors);
                Vec::new()
            }
            PlaybackEvent::ColorCleared => {
                self.state.write().expect("app state lock poisoned").color = None;
                Vec::new()
            }
            // MA's controller view is not authoritative for the TV's actual volume; Luna is.
            PlaybackEvent::ControllerState => Vec::new(),
            // A server that knows the receiver's chain can correct the delay after our port. The
            // sink stays the authority: the value is applied live and stored, then the session
            // reads the sink back and reports what it adopted. A repeated command with the value
            // already in force is how a reconnect re-establishes the server's choice, so it is
            // accepted and stored again, but it does not log a change.
            PlaybackEvent::OutputDelay { requested, applied } => {
                let applied = Config::clamp_output_delay_ms(u64::from(applied));
                let previous = self
                    .output_delay
                    .swap(u64::from(applied), Ordering::Relaxed);
                self.state
                    .write()
                    .expect("app state lock poisoned")
                    .output_delay_ms = applied;
                if previous != u64::from(applied) {
                    println!("[takt] output delay {applied} ms (server requested {requested} ms)");
                }
                vec![Action::StoreOutputDelay(applied)]
            }
            PlaybackEvent::Volume {
                requested: _,
                applied,
                muted,
            } => match muted {
                // An explicit mute command defines the user's intent for all later stream starts;
                // it moves the TV and is recorded, and nothing in the daemon overrides it.
                Some(muted) => {
                    self.set_muted_state(muted);
                    vec![Action::SetMuted(muted)]
                }
                // The client already applied the volume policy and reported `applied` back to
                // MA, so the daemon only has to make the TV match it. Without a TV snapshot the
                // baseline is unknown: clamping from an assumed 0 would drop the TV to the guard
                // threshold, so the command is ignored until Luna reports a level (the client's
                // own view is corrected by the first TV report).
                None => {
                    if self
                        .state
                        .read()
                        .expect("app state lock poisoned")
                        .volume
                        .is_none()
                    {
                        return Vec::new();
                    }
                    self.set_volume_state(applied);
                    vec![Action::SetVolume(applied)]
                }
            },
            PlaybackEvent::Disconnected { reason, retry_in } => {
                eprintln!(
                    "[takt] disconnected: {reason} (retry in {}s)",
                    retry_in.as_secs()
                );
                // A seek cannot outlive the connection that carried it.
                self.set_seeking(false);
                // The client retries on its own, so an error label would flash on every brief
                // drop; the state stays `connecting` until a retry run has lasted long enough
                // to be called a failure.
                let now = Instant::now();
                let since = *self.disconnected_since.get_or_insert(now);
                let playback = if disconnect_is_error(since, now) {
                    PlaybackState::Error
                } else {
                    PlaybackState::Connecting
                };
                self.set_playback_state(playback);
                // Stopping the children discards whatever audio they still buffered. An idle or
                // paused player has none running, so the stop is a no-op there and the TV's mute
                // is never involved.
                vec![
                    Action::FlushSink,
                    Action::StopSink,
                    Action::UpdateState(playback),
                    Action::Reconnect { delay: retry_in },
                ]
            }
            // The client throttles this to one report per second, so one line is honest. Drops
            // mean the host or the network is too slow for the lead it was given.
            PlaybackEvent::LateAudio { late_by, dropped } => {
                eprintln!(
                    "[takt] dropped {dropped} late frame(s), worst {} ms",
                    late_by.as_millis()
                );
                Vec::new()
            }
            PlaybackEvent::Other => Vec::new(),
        }
    }

    /// Applies one external TV volume/mute event and returns whether MA should be notified.
    /// The TV is authoritative, so its state is always recorded (this is how the initial Luna
    /// volume reaches the safety clamp). Only `Playing` changes are reported to MA.
    pub fn handle_external_volume(&mut self, volume: u8, muted: bool) -> bool {
        // The TV is the source of truth, a mute the viewer pressed on the remote included: it is
        // recorded here and, while playing, reported to MA. The daemon adds nothing of its own.
        let mut state = self.state.write().expect("app state lock poisoned");
        state.volume = Some(volume);
        state.muted = muted;
        state.state == PlaybackState::Playing
    }

    /// Dispatches actions returned by [`App::handle_event`].
    ///
    /// Every action is attempted even if an earlier one fails, so a fallible TV mute can never
    /// skip sink cleanup. The first error is returned after the sequence. This is public so
    /// fixture tests can dispatch TV-only actions to a fake. Shared state was already applied by
    /// `handle_event`; `UpdateState` additionally drives the TV's external-volume reporting gate.
    pub async fn execute_actions(&mut self, actions: Vec<Action>) -> Result<()> {
        let mut first_error = None;
        for action in actions {
            let result: Result<()> = async {
                match action {
                    Action::RestartSink => {
                        if let Some(sink) = &self.sink {
                            sink.restart().await?;
                        }
                    }
                    // The client flushes the sink itself on stream start/end/clear and after a
                    // failed attempt, so this is deliberately a no-op here. A flush only empties
                    // the daemon's bridge: `pacat` has no flush API, so the bytes already in its
                    // stdin pipe and PulseAudio's buffer die with the process, and a stream end
                    // stops it. That is why `StopSink`, not this barrier, is what makes a pause
                    // silent.
                    Action::FlushSink => {}
                    Action::StopSink => {
                        if let Some(sink) = &self.sink {
                            sink.stop().await?;
                        }
                    }
                    Action::SetMuted(muted) => self.tv.set_muted(muted)?,
                    Action::SetVolume(volume) => self.tv.set_volume(volume)?,
                    // Best effort, one attempt: a missing or refusing application manager must
                    // never stop playback, so the failure is logged and deliberately excluded
                    // from `first_error` (and therefore from the caller's error path).
                    Action::EnsureUiForeground => {
                        if let Some(grace) = &self.grace {
                            arm_grace(grace, Instant::now());
                        }
                        if let Err(error) = self.tv.ensure_ui_foreground() {
                            eprintln!("[takt] auto-open failed: {error:#}");
                        }
                    }
                    // Sent by the run loop through the control handle *after* this dispatch, so
                    // a failure to move the TV is never hidden by a report that claims it moved.
                    Action::ReportPlayerState(_, _) => {}
                    Action::UpdateState(playback) => {
                        self.tv.set_playing(playback == PlaybackState::Playing)?
                    }
                    // Persisting is best effort: a read-only config file must not stop playback,
                    // and the value is already in force in the sink and the shared state.
                    Action::StoreOutputDelay(millis) => {
                        if let Err(error) = self.config.store_output_delay(u64::from(millis)) {
                            eprintln!(
                                "[takt] output delay {millis} ms could not be persisted: {error:#}"
                            );
                        }
                        if let Some(control) = self.control.clone() {
                            if let Err(error) = control.output_delay_applied().await {
                                eprintln!("[takt] output delay report failed: {error:#}");
                            }
                        }
                    }
                    Action::Reconnect { .. } => {}
                }
                Ok(())
            }
            .await;

            if let Err(error) = result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Runs the daemon forever.
    ///
    /// The client owns the connection and reconnects after a drop, so the outer loop only runs
    /// again when a connection ended for good: an invalid configuration, or a sink failure the
    /// host has to repair by building a new sink. A rebuilt connection uses the same persisted
    /// identity and seeds the first `client/state` with the current TV volume/mute.
    pub async fn run(mut self) -> Result<()> {
        let mut rebuild_delay = Duration::ZERO;
        loop {
            self.set_playback_state(PlaybackState::Connecting);
            // Resolution comes before the audio children: a dead address must not spawn a
            // pipeline that then waits a second and a half for a server. The Arc is cloned so the
            // field is not borrowed across the await; `self.config.server` is a separate borrow
            // that ends before the sink is set.
            let finder = self.finder.clone();
            let server = match finder {
                Some(finder) => {
                    crate::discovery::resolve(&self.config.server, finder.as_ref())
                        .await
                        .server
                }
                None => self.config.server.clone(),
            };
            let (volume, muted) = {
                let state = self.state.read().expect("app state lock poisoned");
                (state.volume, state.muted)
            };
            let identity = self
                .config
                .identity()
                .ok_or_else(|| anyhow!("the config holds no valid client identity"))?;
            let (sink, handle) =
                GstSink::start(self.visual.clone(), Arc::clone(&self.output_delay))?;
            self.sink = Some(handle);
            let mut client_config =
                ClientConfig::new(server, self.config.player_name.clone(), identity);
            client_config.device_info = Some(device_info());
            // The decoder pipeline forwards encoded FLAC frames unchanged.
            client_config.audio_mode = AudioMode::Passthrough;
            // Without a Luna snapshot the level is unknown; 0 is the safe, silent
            // placeholder the daemon has always reported. MA volume commands are ignored
            // until Luna reports a level (see `PlaybackEvent::Volume`).
            client_config.volume = volume.unwrap_or(0);
            client_config.muted = muted;
            // gst-launch and pacat have to spawn before the first sample can play. Measured on
            // the TV at 100-300 ms; the extra margin is what removes the two underruns per
            // stream the fixed 250 ms default produced.
            client_config.required_lead_time_ms = REQUIRED_LEAD_TIME_MS;
            client_config.volume_policy = VOLUME_POLICY;
            client_config.max_backoff = MAX_BACKOFF;
            // This host can change the delay after its audio port and stores it, so it advertises
            // `set_output_delay`: a server that knows the receiver's chain can correct the group's
            // alignment without anyone walking to the TV. The sink still owns the value.
            client_config.output_delay_control = true;
            // These two lines advertise the artwork role: a non-empty channel list, one album
            // image at the card size. The enabled `color` feature advertises the colour role, and
            // that role has no configuration.
            client_config.artwork_channels = artwork_channels();
            client_config.max_artwork_bytes = MAX_ARTWORK_BYTES;

            let (client, mut events) = match Client::connect(client_config, Box::new(sink)).await {
                Ok(connection) => connection,
                Err(error) => {
                    // Only an invalid configuration or an address this build cannot use gets
                    // here; retrying would fail identically on every attempt.
                    self.sink = None;
                    return Err(error);
                }
            };
            self.control = Some(client.control());
            let played = self.run_connection(&mut events).await;
            self.control = None;
            // The event stream ended, so the connection task is done: report why it stopped.
            // A sink failure ends a connection for good, which is why the outer loop rebuilds
            // both the sink and the client here.
            match client.finished().await {
                Ok(()) => eprintln!("[takt] connection closed; rebuilding the audio sink"),
                Err(error) => {
                    eprintln!("[takt] connection failed: {error:#}; rebuilding the audio sink")
                }
            }
            self.sink = None;
            // A permanently broken pipeline must not churn MA at a fixed rate: a connection
            // that came up and played retries in one second, one that cannot even open its
            // sink backs off 1, 2, 4 … 30 seconds.
            rebuild_delay = next_backoff(rebuild_delay, played, MAX_BACKOFF);
            eprintln!(
                "[takt] rebuilding the audio sink in {}s",
                rebuild_delay.as_secs()
            );
            tokio::time::sleep(rebuild_delay).await;
        }
    }

    /// Drives one client connection until it ends.
    ///
    /// Returns whether a stream started in this connection, which tells the rebuild backoff a
    /// pipeline that worked apart from a pipeline that never opened.
    async fn run_connection(&mut self, events: &mut EventStream) -> bool {
        let mut played = false;
        loop {
            let input = tokio::select! {
                event = events.next() => match event {
                    Some(event) => SessionInput::Event(event),
                    // The client ended the connection: a fatal sink failure or a dropped stream.
                    None => return played,
                },
                command = next_command(&mut self.command_receiver) => {
                    SessionInput::Command(command)
                }
                volume = next_volume_event(&mut self.volume_receiver) => {
                    SessionInput::Volume(volume)
                }
            };

            match input {
                SessionInput::Event(event) => {
                    let event: PlaybackEvent = event.into();
                    if matches!(event, PlaybackEvent::StreamStart) {
                        played = true;
                    }
                    let actions = self.handle_event(event);
                    let report = report_from_actions(&actions);
                    // The report describes the output, so it goes out only after every action of
                    // this event (TV volume/mute, sink restart/flush/stop) succeeded.
                    if let Err(error) = self.execute_actions(actions).await {
                        eprintln!(
                            "[takt] action dispatch failed: {error:#}; not reporting the new \
                             player state"
                        );
                        continue;
                    }
                    if let Some((volume, muted)) = report {
                        self.report_player_state(volume, muted).await;
                    }
                }
                SessionInput::Command(Some(command)) => {
                    if let Err(error) = self.send_transport(&command).await {
                        // A refused transport command (no controller role, command not
                        // advertised) is not a connection failure; the client keeps playing.
                        eprintln!("[takt] controller command {command:?} failed: {error:#}");
                    }
                }
                SessionInput::Command(None) => {
                    // A closed command channel is not an audio/session failure. Disable this
                    // branch so it cannot spin; the connection continues normally.
                    self.command_receiver = None;
                }
                SessionInput::Volume(Some((volume, muted))) => {
                    self.push_external_volume(volume, muted).await;
                }
                SessionInput::Volume(None) => {
                    // No Luna backend (or a dropped backend) must not spin or disconnect audio.
                    self.volume_receiver = None;
                }
            }
        }
    }

    /// Sends one controller command through the live connection.
    async fn send_transport(&self, command: &str) -> Result<()> {
        let Some(control) = self.control.clone() else {
            return Err(anyhow!("no connection is established"));
        };
        match command {
            "play" => control.play().await,
            "pause" => control.pause().await,
            "next" => control.next().await,
            "previous" => control.previous().await,
            other if other.starts_with("seek:") => {
                let position = other[5..]
                    .parse::<u64>()
                    .map_err(|_| anyhow!("malformed seek target {other:?}"))?;
                self.set_seeking(true);
                control.seek(position).await
            }
            other if other.starts_with("seek_rel:") => {
                let offset = other[9..]
                    .parse::<i64>()
                    .map_err(|_| anyhow!("malformed seek offset {other:?}"))?;
                self.set_seeking(true);
                control.seek_relative(offset).await
            }
            other => Err(anyhow!("unsupported transport command {other:?}")),
        }
    }

    /// Records one external TV volume/mute event and reports it to MA while playing.
    ///
    /// The TV is authoritative, so its level is recorded raw and reaches MA through
    /// [`ControlHandle::report_volume`], which records the level without asking
    /// [`VOLUME_POLICY`]: the guard limits requests, never the level the output is really at.
    /// The shared state keeps the Luna value, so the UI and MA see the same number.
    async fn push_external_volume(&mut self, volume: u8, muted: bool) {
        let (previous_volume, previous_muted) = {
            let state = self.state.read().expect("app state lock poisoned");
            (state.volume, state.muted)
        };
        let report = self.handle_external_volume(volume, muted);
        let Some(control) = self.control.clone() else {
            return;
        };
        if report && previous_volume != Some(volume) {
            if let Err(error) = control.report_volume(volume).await {
                eprintln!("[takt] volume report failed: {error:#}");
            }
        }
        if report && previous_muted != muted {
            if let Err(error) = control.set_muted(muted).await {
                eprintln!("[takt] mute report failed: {error:#}");
            }
        }
    }

    /// Tells Music Assistant where the TV's volume and mute state really are.
    ///
    /// The level is reported with [`ControlHandle::report_volume`], not `set_volume`: the guard
    /// applies to requests, while this is a statement about the output. A level above the
    /// protocol's range is refused by the client before anything is recorded.
    async fn report_player_state(&mut self, volume: u8, muted: bool) {
        let Some(control) = self.control.clone() else {
            return;
        };
        if let Err(error) = control.report_volume(volume).await {
            eprintln!("[takt] player-state volume report failed: {error:#}");
            return;
        }
        if let Err(error) = control.set_muted(muted).await {
            eprintln!("[takt] player-state mute report failed: {error:#}");
        }
    }

    fn playback_state(&self) -> PlaybackState {
        self.state.read().expect("app state lock poisoned").state
    }

    /// One low-volume line per stream lifecycle event, with a wall-clock timestamp and the
    /// currently known track title so a track change can be correlated with sink restarts.
    /// (`stream/start` arrives before the new track's metadata, so its title is usually the
    /// previous track or `-`; the following track event carries the new title.)
    fn log_stream_event(&self, phase: &str) {
        let title = self
            .state
            .read()
            .expect("app state lock poisoned")
            .title
            .clone();
        eprintln!(
            "[takt] stream/{phase} ts_ms={} title={}",
            unix_millis(),
            title.as_deref().unwrap_or("-")
        );
    }

    fn set_playback_state(&self, playback: PlaybackState) {
        self.state.write().expect("app state lock poisoned").state = playback;
    }

    fn set_muted_state(&self, muted: bool) {
        self.state.write().expect("app state lock poisoned").muted = muted;
    }

    /// Records whether a seek this host sent is still waiting for its stream.
    fn set_seeking(&self, seeking: bool) {
        let mut state = self.state.write().expect("app state lock poisoned");
        if state.seeking != seeking {
            state.seeking = seeking;
        }
    }

    fn set_volume_state(&self, volume: u8) {
        self.state.write().expect("app state lock poisoned").volume = Some(volume);
    }
}

/// Input selected while connected: client event, HTTP command, or external TV audio event.
enum SessionInput {
    Event(Event),
    Command(Option<String>),
    Volume(Option<(u8, bool)>),
}

/// Waits for an HTTP controller command, or forever when no receiver was installed.
async fn next_command(receiver: &mut Option<mpsc::UnboundedReceiver<String>>) -> Option<String> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Waits for an external TV audio event, or forever when no receiver was installed.
async fn next_volume_event(
    receiver: &mut Option<mpsc::UnboundedReceiver<(u8, bool)>>,
) -> Option<(u8, bool)> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Stable name for a [`StreamEndReason`], for the lifecycle log line.
fn end_reason_name(reason: StreamEndReason) -> &'static str {
    match reason {
        StreamEndReason::End => "end",
        StreamEndReason::RoleRemoved => "role_removed",
        // The reason enum is non-exhaustive: a later client revision may add one.
        _ => "unknown",
    }
}

/// Whether a run of failed connection attempts has lasted long enough to show `error`.
pub(crate) fn disconnect_is_error(since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(since) >= DISCONNECT_ERROR_TIMEOUT
}

/// Extracts the MA volume/mute correction from an action list, if the event produced one.
///
/// `execute_actions` ignores [`Action::ReportPlayerState`]; the run loop sends the value once the
/// whole action list (TV volume/mute, sink restart/flush/stop) succeeded, so a failed TV call is
/// never followed by a report that claims it worked.
pub(crate) fn report_from_actions(actions: &[Action]) -> Option<(u8, bool)> {
    actions.iter().find_map(|action| match action {
        Action::ReportPlayerState(volume, muted) => Some((*volume, *muted)),
        _ => None,
    })
}

/// The device description the daemon announced before the client crate existed.
///
/// `client/hello` carries it so Music Assistant shows the hardware rather than only the player
/// name; the software version is this binary's version.
fn device_info() -> DeviceInfo {
    DeviceInfo {
        product_name: "LG webOS TV".to_owned(),
        manufacturer: "LG".to_owned(),
        software_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        mac_address: None,
    }
}

/// Maps an optional metadata text field from the client's empty-string convention to `None`.
fn none_if_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Flattens the client's color scheme into the state's serializable palette.
///
/// `ColorScheme` is non-exhaustive and carries a server timestamp; the daemon only needs the six
/// colors, and the client has already applied the timestamp before delivering the event.
fn theme_colors(scheme: ColorScheme) -> ThemeColors {
    ThemeColors {
        background_dark: scheme.background_dark,
        background_light: scheme.background_light,
        primary: scheme.primary,
        accent: scheme.accent,
        on_dark: scheme.on_dark,
        on_light: scheme.on_light,
    }
}

/// Wall-clock milliseconds since the Unix epoch, for correlating daemon log lines with MA and
/// `pactl` observation on the TV.
fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncspin_client::{clamp_volume_with, NORMAL_PLAYBACK_SPEED};

    #[test]
    fn a_disconnect_run_shows_error_only_after_the_timeout() {
        let start = Instant::now();
        assert!(!disconnect_is_error(start, start));
        assert!(!disconnect_is_error(
            start,
            start + DISCONNECT_ERROR_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(disconnect_is_error(start, start + DISCONNECT_ERROR_TIMEOUT));
        assert!(disconnect_is_error(
            start,
            start + DISCONNECT_ERROR_TIMEOUT + Duration::from_secs(5)
        ));
    }

    #[test]
    fn a_fatal_sink_rebuild_backs_off_unless_the_pipeline_played() {
        // A pipeline that cannot even open its sink must not churn MA at a fixed rate.
        let mut delay = Duration::ZERO;
        let mut delays = Vec::new();
        for _ in 0..6 {
            delay = next_backoff(delay, false, MAX_BACKOFF);
            delays.push(delay.as_secs());
        }
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30]);

        // A connection that played a stream retries in one second, so a one-off sink death
        // after a long session is not penalised.
        assert_eq!(
            next_backoff(Duration::from_secs(30), true, MAX_BACKOFF),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn the_device_info_matches_the_old_client_hello() {
        let info = device_info();
        assert_eq!(info.product_name, "LG webOS TV");
        assert_eq!(info.manufacturer, "LG");
        assert_eq!(
            info.software_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(info.mac_address, None);
    }

    #[test]
    fn report_from_actions_extracts_the_correction() {
        let actions = vec![Action::SetVolume(19), Action::ReportPlayerState(19, true)];

        assert_eq!(report_from_actions(&actions), Some((19, true)));
    }

    #[test]
    fn report_from_actions_is_none_without_a_correction() {
        let actions = vec![
            Action::SetVolume(19),
            Action::UpdateState(PlaybackState::Playing),
        ];

        assert_eq!(report_from_actions(&actions), None);
    }

    #[test]
    fn the_normal_playback_speed_is_the_one_a_track_without_a_speed_gets() {
        // The client fills `playback_speed` with its own default; the daemon forwards it as-is.
        assert_eq!(NORMAL_PLAYBACK_SPEED, 1_000);
    }

    #[test]
    fn the_client_volume_policy_preserves_the_daemon_guard_table() {
        // Up to the threshold a request is applied verbatim.
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 1, 20), 20);
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 0, 40), 40);
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 40, 40), 40);
        // A low baseline may jump up to the threshold, no further.
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 1, 100), 40);
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 39, 41), 41);
        // Exactly at the threshold a full +20 step is allowed.
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 40, 60), 60);
        // Beyond that, only +20 from the current volume.
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 40, 100), 60);
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 50, 100), 70);
        // Saturating at the top of the scale keeps 100 reachable.
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 90, 100), 100);
        // Lowering and zero are always applied.
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 30, 25), 25);
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 90, 50), 50);
        assert_eq!(clamp_volume_with(VOLUME_POLICY, 30, 0), 0);
    }

    #[test]
    fn the_artwork_channel_is_one_album_image_at_the_card_size() {
        let channels = artwork_channels();
        assert_eq!(channels.len(), 1);
        let channel = channels[0].expect("channel 0 is configured");
        assert_eq!(channel.source, ArtworkSource::Album);
        assert_eq!(channel.format, ImageFormat::Jpeg);
        assert_eq!((channel.width, channel.height), (640, 640));
        assert_eq!(MAX_ARTWORK_BYTES, 1_048_576);
    }
}
