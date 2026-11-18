//! Decoder (`gst-launch-1.0`) and player (`pacat`) child-process supervision.
//!
//! [`Sink`] owns the two child processes and the data path between them:
//!
//! ```text
//! write_pcm(FLAC frame) -> gst stdin -> decode stdout -> Bridge -> pacat stdin
//! ```
//!
//! [`Sink::write_pcm`] writes *encoded* FLAC bytes to the decoder's stdin (the name is
//! historical and misleading — the bytes are the encoded frames the client hands to
//! [`GstSink`](super::gst::GstSink), not PCM); the decoded PCM on gst's stdout is pumped into the
//! [`Bridge`], and a second pump drains the bridge into `pacat`'s stdin.
//!
//! The bridge and flush synchronization state (`epoch`, `flushing`, `in_flight_writes`)
//! live behind a short-held `io` mutex. Neither pump holds this lock across child pipe
//! I/O: the player pump reads a bounded chunk from the bridge, increments
//! `in_flight_writes`, releases the lock, and only then calls `write_all` on `pacat`'s
//! stdin, so the decoder pump can continue pushing into the 1.5 s [`Bridge`] and
//! accumulate the configured jitter backlog while `pacat` is slow or blocked.
//!
//! [`Sink::flush`] remains a barrier for in-flight player writes: it sets
//! `flushing = true`, waits on `flush_notify` (with the lock released) until
//! `in_flight_writes == 0`, clears the bridge, advances `epoch`, clears `flushing`, and
//! notifies waiters. The decoder pump captures `epoch` **before** reading from `gst` and
//! drops its chunk or retained suffix if `epoch` changed or `flushing` is set.
//!
//! Small residual race (by design): a player chunk already read from the bridge before
//! [`Sink::flush`] is called is allowed to finish its `write_all` before `flush()`
//! returns (that completion *is* the barrier), and bytes already buffered in `gst`'s or
//! `pacat`'s OS kernel pipes are outside this daemon-side barrier. A stream end stops
//! both children, and that is what discards them. If `pacat` is alive but never consumes, `flush()` waits for the
//! blocked in-flight write — acceptable backpressure for this task.
//!
//! A supervisor task polls both children with [`Child::try_wait`], and both pumps record
//! failures and reap children directly on read/write errors; the decoder pump, player
//! pump and supervisor may all detect a failure concurrently, so failure recording and
//! reaping are idempotent. The first failure wins and is observable via
//! [`Sink::failure`] / [`Sink::failure_receiver`].

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{watch, Mutex, Notify};
use tokio::task::JoinHandle;

use super::bridge::Bridge;
use crate::visual::{AudioAnalyzer, SharedAudioVisual};

/// Default decoder binary. The TV has `gst-launch-1.0`; the brief's `gst-launch` is
/// shorthand.
pub const DEFAULT_DECODER: &str = "gst-launch-1.0";
/// Default player binary.
pub const DEFAULT_PLAYER: &str = "pacat";

/// Seconds of PCM the bridge buffers.
///
/// The client schedules playout itself now and calls `write` at each frame's pre-port deadline, so
/// the bridge only has to absorb decode bursts - not the server's lead. Half a second is ample and
/// keeps what the analyzer sees close to what is audible.
const BRIDGE_CAPACITY_SECS: f32 = 0.5;

/// Read buffer used to drain the decoder's stdout.
const DECODER_READ_BUF: usize = 8192;
/// Write buffer used to feed the player's stdin.
const PLAYER_WRITE_BUF: usize = 8192;

/// How often the supervisor polls both children for exit.
const SUPERVISE_INTERVAL: Duration = Duration::from_millis(75);

/// The gst-launch pipeline: raw FLAC on stdin -> S16LE PCM on stdout.
const DECODER_PIPELINE: &[&str] = &[
    "-q",
    "fdsrc",
    "!",
    "flacparse",
    "!",
    "flacdec",
    "!",
    "audioconvert",
    "!",
    "audioresample",
    "!",
    "audio/x-raw,format=S16LE,rate=48000,channels=2",
    "!",
    "fdsink",
    "fd=1",
];

/// The pacat arguments: play raw S16LE stereo PCM read from stdin.
///
/// `--latency-msec=500` is the player's own cushion. The client feeds just in time, so a buffer
/// that only just covers the write-ahead makes every hiccup audible: at 200 ms the group offset
/// wandered ("sometimes better, sometimes worse"). Half a second absorbs the jitter and leaves a
/// constant offset the audio delay setting can trim.
const PLAYER_ARGS: &[&str] = &[
    "--raw",
    "--format=s16le",
    "--rate=48000",
    "--channels=2",
    "--latency-msec=500",
    "--client-name=takt",
];

/// Why the sink stopped producing audio: one child exited or a pump I/O failed.
///
/// Observable via [`Sink::failure`] / [`Sink::failure_receiver`]; the first failure wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    /// The decoder (`gst-launch-1.0`) exited or its stdout read failed.
    Decoder,
    /// The player (`pacat`) exited or its stdin write failed.
    Player,
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Decoder => write!(f, "decoder (gst-launch) exited unexpectedly"),
            SinkError::Player => write!(f, "player (pacat) exited unexpectedly"),
        }
    }
}

impl std::error::Error for SinkError {}

/// The two child handles, under one lock so failure reaping cannot deadlock.
struct Children {
    gst: Child,
    pacat: Child,
}

/// The bridge and flush-barrier state, protected by a short-held mutex.
///
/// Neither pump holds this lock across child pipe I/O (`read` or `write_all`), keeping
/// the 1.5 s bridge backlog usable while [`Sink::flush`] synchronizes with in-flight
/// player writes via `flushing` and `in_flight_writes`.
struct Io {
    bridge: Bridge,
    epoch: u64,
    flushing: bool,
    in_flight_writes: usize,
}

/// State shared between [`Sink`], its two pump tasks, and its supervisor task.
struct Shared {
    /// Decoder and player handles (kill + reap on stop/failure).
    children: Mutex<Children>,
    /// Write end of the decoder's stdin, for [`Sink::write_pcm`].
    gst_stdin: Mutex<ChildStdin>,
    /// PCM ring buffer and flush barrier state, under one short-held synchronous lock
    /// (never held across `.await`, allowing synchronous cleanup in [`FlushGuard::drop`]).
    io: StdMutex<Io>,
    /// Signals the decoder pump that the bridge has space again (or a flush started).
    space_notify: Notify,
    /// Signals the player pump that the bridge has data again.
    data_notify: Notify,
    /// Coordinates [`Sink::flush`] with `flushing` and `in_flight_writes`.
    flush_notify: Notify,
    /// First observed failure (decoder or player died); `None` while healthy.
    failure: watch::Sender<Option<SinkError>>,
    /// Times the player asked for audio and the bridge was empty since that audio started.
    ///
    /// Diagnostic only, as [`Sink::underruns`] describes: the daemon reports it to the
    /// server but never changes its behaviour because of it.
    underruns: AtomicU64,
}

/// Supervises the decoder and player child processes and the PCM path between them.
pub struct Sink {
    shared: Arc<Shared>,
    decoder_task: JoinHandle<()>,
    player_task: JoinHandle<()>,
    supervisor_task: JoinHandle<()>,
    gst_binary: PathBuf,
    pacat_binary: PathBuf,
    gst_pid: Option<u32>,
    pacat_pid: Option<u32>,
    visual: Option<SharedAudioVisual>,
}

impl Sink {
    /// Starts the decoder and player with the default binaries.
    pub async fn start() -> Result<Self> {
        Self::start_with_binaries(Path::new(DEFAULT_DECODER), Path::new(DEFAULT_PLAYER)).await
    }

    /// Starts the default binaries and publishes analyzed PCM levels to the UI.
    pub async fn start_with_visual(visual: SharedAudioVisual) -> Result<Self> {
        Self::start_with_binaries_and_visual(
            Path::new(DEFAULT_DECODER),
            Path::new(DEFAULT_PLAYER),
            Some(visual),
        )
        .await
    }

    /// Starts the decoder and player using the given binaries (hermetic tests pass the
    /// fake scripts here).
    pub async fn start_with_binaries(gst: &Path, pacat: &Path) -> Result<Self> {
        Self::start_with_binaries_and_visual(gst, pacat, None).await
    }

    /// Starts the decoder/player and optionally analyzes the PCM handed to the player.
    pub async fn start_with_binaries_and_visual(
        gst: &Path,
        pacat: &Path,
        visual: Option<SharedAudioVisual>,
    ) -> Result<Self> {
        let mut gst_child = Command::new(gst)
            .args(DECODER_PIPELINE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start decoder {}", gst.display()))?;
        let gst_stdin = gst_child
            .stdin
            .take()
            .context("decoder stdin was not piped")?;
        let gst_stdout = gst_child
            .stdout
            .take()
            .context("decoder stdout was not piped")?;
        let gst_pid = gst_child.id();

        let mut pacat_child = match Command::new(pacat)
            .args(PLAYER_ARGS)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                // The decoder is already running: kill and reap it explicitly (not just
                // `kill_on_drop`) so no process or zombie is left behind on this path.
                let _ = gst_child.start_kill();
                let _ = gst_child.wait().await;
                return Err(err)
                    .with_context(|| format!("failed to start player {}", pacat.display()));
            }
        };
        let pacat_stdin = pacat_child
            .stdin
            .take()
            .context("player stdin was not piped")?;
        let pacat_pid = pacat_child.id();

        let shared = Arc::new(Shared {
            children: Mutex::new(Children {
                gst: gst_child,
                pacat: pacat_child,
            }),
            gst_stdin: Mutex::new(gst_stdin),
            io: StdMutex::new(Io {
                bridge: Bridge::new(BRIDGE_CAPACITY_SECS),
                epoch: 0,
                flushing: false,
                in_flight_writes: 0,
            }),
            space_notify: Notify::new(),
            data_notify: Notify::new(),
            flush_notify: Notify::new(),
            failure: watch::channel(None).0,
            underruns: AtomicU64::new(0),
        });

        let decoder_task = tokio::spawn(pump_decoder_to_bridge(gst_stdout, Arc::clone(&shared)));
        let player_task = tokio::spawn(pump_bridge_to_player(
            pacat_stdin,
            Arc::clone(&shared),
            visual.clone(),
        ));
        let supervisor_task = tokio::spawn(supervise(Arc::clone(&shared)));

        Ok(Self {
            shared,
            decoder_task,
            player_task,
            supervisor_task,
            gst_binary: gst.to_path_buf(),
            pacat_binary: pacat.to_path_buf(),
            gst_pid,
            pacat_pid,
            visual,
        })
    }

    /// PID of the decoder child, if it was spawned.
    pub fn gst_pid(&self) -> Option<u32> {
        self.gst_pid
    }

    /// PID of the player child, if it was spawned.
    pub fn pacat_pid(&self) -> Option<u32> {
        self.pacat_pid
    }

    /// Writes one encoded FLAC frame to the decoder's stdin.
    ///
    /// Despite the name, `data` is the *encoded* stream payload the client forwarded
    /// (FLAC), not PCM; it is written verbatim to the decoder's stdin. Returns an error
    /// with context if the decoder has already exited (its stdin pipe is closed).
    pub async fn write_pcm(&self, data: &[u8]) -> Result<()> {
        let mut stdin = self.shared.gst_stdin.lock().await;
        stdin
            .write_all(data)
            .await
            .context("failed to write FLAC frame to decoder stdin (decoder exited?)")
    }

    /// Clears the PCM buffer and advances the flush epoch, as a barrier for in-flight
    /// player writes.
    ///
    /// Called on pause/stream-clear. Sets `flushing = true`, waits (with the `io` lock
    /// released) until `in_flight_writes == 0` so any player write that started before
    /// the flush finishes before this method returns, then clears the bridge, advances
    /// `epoch`, sets `flushing = false`, and notifies waiters.
    ///
    /// While player writes are in flight during normal playback, `io` is not held across
    /// `write_all`, so the decoder pump can still fill the 1.5 s bridge backlog. During
    /// a flush, the decoder pump drops any chunk or retained suffix whose epoch changed
    /// or that observes `flushing == true`.
    ///
    /// Residual race: a player chunk read from the bridge right before `flush()` is
    /// allowed to finish writing before `flush()` returns (that is the barrier), and
    /// bytes already inside `gst`'s/`pacat`'s OS pipes are outside this daemon barrier.
    /// A stream end stops the children, which discards them.
    ///
    /// Cancellation safety: if the returned future is dropped while waiting for
    /// `in_flight_writes == 0`, [`FlushGuard`] resets `flushing = false` and notifies
    /// waiters so the pumps and subsequent flushes do not wedge.
    pub async fn flush(&self) {
        loop {
            let notified = self.shared.flush_notify.notified();
            tokio::pin!(notified);
            let acquired = {
                let mut io = self.shared.io.lock().expect("io mutex poisoned");
                if io.flushing {
                    notified.as_mut().enable();
                    false
                } else {
                    io.flushing = true;
                    true
                }
            };
            if acquired {
                break;
            }
            notified.await;
        }

        let mut guard = FlushGuard::arm(&self.shared);

        loop {
            let notified = self.shared.flush_notify.notified();
            tokio::pin!(notified);
            let done = {
                let mut io = self.shared.io.lock().expect("io mutex poisoned");
                if io.in_flight_writes == 0 {
                    io.bridge.flush();
                    io.epoch += 1;
                    io.flushing = false;
                    guard.disarm();
                    true
                } else {
                    notified.as_mut().enable();
                    false
                }
            };
            if done {
                break;
            }
            notified.await;
        }

        self.shared.flush_notify.notify_waiters();
        self.shared.space_notify.notify_waiters();
    }

    /// Number of PCM bytes currently buffered (diagnostic; useful to verify a flush
    /// took effect or to pace writes).
    pub async fn buffered_bytes(&self) -> usize {
        self.shared
            .io
            .lock()
            .expect("io mutex poisoned")
            .bridge
            .len()
    }

    /// The first observed failure, if either child has exited or a pump I/O failed.
    pub fn failure(&self) -> Option<SinkError> {
        self.shared.failure.borrow().clone()
    }

    /// How often the player asked for audio and the bridge was empty since it started.
    ///
    /// Diagnostic only: the value never changes how the sink behaves. It resets when the
    /// sink is restarted, which starts a new player process.
    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// A receiver that is notified when [`Sink::failure`] first becomes `Some`, for
    /// callers that prefer to await failure rather than poll.
    pub fn failure_receiver(&self) -> watch::Receiver<Option<SinkError>> {
        self.shared.failure.subscribe()
    }

    /// Stops the decoder and player: aborts the pump and supervisor tasks, kills and
    /// reaps both children so neither a live process nor a zombie remains.
    pub async fn stop(self) -> Result<()> {
        self.decoder_task.abort();
        self.player_task.abort();
        self.supervisor_task.abort();

        let mut children = self.shared.children.lock().await;
        let _ = children.gst.start_kill();
        let _ = children.pacat.start_kill();

        if let Err(err) = children.gst.wait().await {
            return Err(err).context("failed to reap decoder (gst) process");
        }
        if let Err(err) = children.pacat.wait().await {
            return Err(err).context("failed to reap player (pacat) process");
        }
        Ok(())
    }

    /// Stops the current children and starts fresh ones (new PIDs).
    pub async fn restart(self) -> Result<Self> {
        let gst = self.gst_binary.clone();
        let pacat = self.pacat_binary.clone();
        let visual = self.visual.clone();
        self.stop().await?;
        Self::start_with_binaries_and_visual(&gst, &pacat, visual).await
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // Safety net: if `stop`/`restart` was never called, abort the pumps and
        // supervisor so their `Arc<Shared>` is released and the children are killed via
        // `kill_on_drop`.
        self.decoder_task.abort();
        self.player_task.abort();
        self.supervisor_task.abort();
    }
}

/// Reset guard that clears `io.flushing` and wakes waiters if [`Sink::flush`] is
/// canceled (dropped) while waiting for `in_flight_writes` to drain.
struct FlushGuard<'a> {
    shared: &'a Shared,
    armed: bool,
}

impl<'a> FlushGuard<'a> {
    fn arm(shared: &'a Shared) -> Self {
        Self {
            shared,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut io = self
            .shared
            .io
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        io.flushing = false;
        drop(io);
        self.shared.flush_notify.notify_waiters();
        self.shared.space_notify.notify_waiters();
    }
}

/// Pumps decoded PCM from the decoder's stdout into the bridge.
///
/// Captures the flush epoch **before** reading from gst, then appends the chunk only
/// while the epoch is unchanged and `flushing` is false; if [`Sink::flush`] is active or
/// advanced the epoch, the chunk (or the unaccepted suffix) is discarded so pre-flush
/// PCM cannot repopulate the bridge. Because the player pump releases the `io` lock
/// across `write_all`, decoder pushes are not blocked by slow player writes.
async fn pump_decoder_to_bridge(mut stdout: ChildStdout, shared: Arc<Shared>) {
    let mut buf = vec![0u8; DECODER_READ_BUF];
    loop {
        let epoch = shared.io.lock().expect("io mutex poisoned").epoch;
        let n = match stdout.read(&mut buf).await {
            // EOF or read error: record the failure and reap both children.
            Ok(0) | Err(_) => {
                fail(&shared, SinkError::Decoder).await;
                return;
            }
            Ok(n) => n,
        };

        let mut remaining = &buf[..n];
        loop {
            let space_notified = shared.space_notify.notified();
            tokio::pin!(space_notified);
            let accepted = {
                let mut io = shared.io.lock().expect("io mutex poisoned");
                if io.epoch != epoch || io.flushing {
                    // flush() is active or advanced the epoch: drop the chunk/suffix.
                    None
                } else {
                    let accepted = io.bridge.push(remaining).unwrap_or_default();
                    if accepted < remaining.len() {
                        space_notified.as_mut().enable();
                    }
                    Some(accepted)
                }
            };
            match accepted {
                None => break,
                Some(accepted) => {
                    if accepted > 0 {
                        shared.data_notify.notify_one();
                    }
                    if accepted == remaining.len() {
                        break;
                    }
                    remaining = &remaining[accepted..];
                    space_notified.await;
                }
            }
        }
    }
}

/// Pumps PCM from the bridge into the player's stdin.
///
/// Waits for data outside the lock; under the `io` lock, waits if `flushing` is active,
/// reads a bounded chunk from the bridge, increments `in_flight_writes`, and unlocks
/// before calling `write_all` on `pacat`'s stdin. After `write_all` finishes, re-locks
/// to decrement `in_flight_writes` and notify [`Sink::flush`]. This lets the decoder
/// fill the 1.5 s bridge backlog during slow player writes while keeping `flush()` a
/// barrier for in-flight writes.
async fn pump_bridge_to_player(
    mut stdin: ChildStdin,
    shared: Arc<Shared>,
    visual: Option<SharedAudioVisual>,
) {
    enum Step {
        WaitFlush,
        WaitData,
        Write(usize),
    }

    let mut analyzer = AudioAnalyzer::default();
    let mut buf = vec![0u8; PLAYER_WRITE_BUF];
    // Underrun bookkeeping: `delivered` says audio reached the player since the bridge last
    // went empty, so a single idle period counts once instead of once per loop iteration.
    let mut delivered = false;
    let mut epoch = 0;
    loop {
        let flush_notified = shared.flush_notify.notified();
        tokio::pin!(flush_notified);
        let data_notified = shared.data_notify.notified();
        tokio::pin!(data_notified);

        let step = {
            let mut io = shared.io.lock().expect("io mutex poisoned");
            if io.epoch != epoch {
                // A flush discarded everything still queued; the next empty bridge is the
                // result of that flush, not an underrun.
                epoch = io.epoch;
                delivered = false;
            }
            if io.flushing {
                flush_notified.as_mut().enable();
                Step::WaitFlush
            } else {
                let n = io.bridge.read(&mut buf);
                if n == 0 {
                    if delivered {
                        shared.underruns.fetch_add(1, Ordering::Relaxed);
                        delivered = false;
                    }
                    data_notified.as_mut().enable();
                    Step::WaitData
                } else {
                    delivered = true;
                    io.in_flight_writes += 1;
                    Step::Write(n)
                }
            }
        };

        let n = match step {
            Step::WaitFlush => {
                flush_notified.await;
                continue;
            }
            Step::WaitData => {
                data_notified.await;
                continue;
            }
            Step::Write(n) => n,
        };
        shared.space_notify.notify_one();

        // Best-effort: a missing or poisoned visual state must never delay or fail audio.
        if let Some(visual) = &visual {
            let levels = analyzer.analyze(&buf[..n]);
            if let Ok(mut visual) = visual.write() {
                visual.update(levels);
            }
        }

        let write_res = stdin.write_all(&buf[..n]).await;

        {
            let mut io = shared.io.lock().expect("io mutex poisoned");
            io.in_flight_writes -= 1;
            if io.in_flight_writes == 0 {
                shared.flush_notify.notify_waiters();
            }
        }

        if write_res.is_err() {
            fail(&shared, SinkError::Player).await;
            return;
        }
    }
}

/// Polls both children for exit and, on the first exit, records the failure and
/// kills+reaps both children. Detects child exit independently of data flow, so an
/// exited player is caught even while the bridge is empty and the player pump is idle.
async fn supervise(shared: Arc<Shared>) {
    loop {
        let (gst_exited, pacat_exited) = {
            let mut children = shared.children.lock().await;
            (
                children.gst.try_wait().ok().flatten().is_some(),
                children.pacat.try_wait().ok().flatten().is_some(),
            )
        };

        if gst_exited {
            fail(&shared, SinkError::Decoder).await;
            return;
        }
        if pacat_exited {
            fail(&shared, SinkError::Player).await;
            return;
        }

        tokio::time::sleep(SUPERVISE_INTERVAL).await;
    }
}

/// Records the first failure (later failures are ignored) and reaps both children.
/// Idempotent, so the pumps and supervisor may call it concurrently.
async fn fail(shared: &Arc<Shared>, failure: SinkError) {
    shared.failure.send_if_modified(|current| {
        if current.is_none() {
            *current = Some(failure);
            true
        } else {
            false
        }
    });
    reap_all(&shared.children).await;
}

/// Kills and reaps both children (idempotent — safe after a prior `wait` or `try_wait`),
/// under a single lock so concurrent failure handling cannot deadlock on lock ordering.
async fn reap_all(children: &Mutex<Children>) {
    let mut children = children.lock().await;
    let _ = children.gst.start_kill();
    let _ = children.pacat.start_kill();
    let _ = children.gst.wait().await;
    let _ = children.pacat.wait().await;
}
