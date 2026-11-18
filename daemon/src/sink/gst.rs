//! `syncspin_client::Sink` over the `gst-launch` decoder and `pacat` player.
//!
//! The client's [`Sink`](syncspin_client::Sink) trait is synchronous, while the pipeline
//! that [process](super::process) supervises is asynchronous (children, pumps and a
//! tokio `Notify`-based flush barrier). This module bridges the two:
//!
//! ```text
//! client task            sink thread (own tokio runtime)
//!   open/write/flush  ->  bounded std channel  ->  process::Sink (start/restart/flush/write)
//! ```
//!
//! Every call blocks until the pipeline handled it, so an error from `gst-launch`
//! (closed stdin, missing binary) reaches the client on the frame that caused it and the
//! client can end the connection. `write` sends through a bounded channel: when the
//! pipeline is slow the send blocks, which is exactly the backpressure the WebSocket
//! reader used to get from the 1.5 s PCM bridge — nothing is dropped and memory stays
//! bounded.
//!
//! The sink owns the pipeline for the whole connection, across the client's reconnects.
//! [`GstSink::open`] (one per announced stream) restarts both children and writes the
//! stream's codec header before the first frame, [`GstSinkHandle`] lets the daemon restart
//! the children for a seek or stop them on a disconnect, and dropping every handle ends
//! the thread and kills the children.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use syncspin_client::{AudioFormat, AudioFrame, Codec, Sink as ClientSink};

use super::process::{Sink as ProcessSink, DEFAULT_DECODER, DEFAULT_PLAYER};
use crate::visual::SharedAudioVisual;

/// Encoded frames the client may queue ahead of the decoder.
///
/// Only frames waiting for a slow pipeline sit here; once the decoder consumes one the
/// client's next `write` returns. The PCM backlog keeps living in the 1.5 s bridge.
const QUEUED_FRAMES: usize = 16;

/// The client-facing sink: a handle to the thread that owns the pipeline.
pub struct GstSink {
    commands: SyncSender<Command>,
    counters: Arc<Counters>,
    /// Milliseconds of delay after the audio port, shared with the HTTP layer.
    ///
    /// The client polls [`ClientSink::output_delay`] after every `client/state` and every clock
    /// burst, so writing this atomic applies a new value to the running session without a
    /// reconnect.
    output_delay: SharedOutputDelay,
}

/// The live post-port delay in milliseconds, shared by `main`, the sink and the HTTP API.
///
/// Zero means "no delay beyond the audio port" and is reported as `None`, which the protocol
/// carries as `output_delay_ms: 0`.
pub type SharedOutputDelay = Arc<AtomicU64>;

/// A cheap copy of the sink's control channel for the daemon loop.
///
/// The client owns the [`GstSink`] itself; the daemon needs to restart the children after a
/// seek (`stream/clear`) and stop them when the connection drops, so it keeps this handle
/// alive next to the client.
#[derive(Clone)]
pub struct GstSinkHandle {
    commands: SyncSender<Command>,
    counters: Arc<Counters>,
}

/// Diagnostics shared between the sink thread and both handles.
#[derive(Default)]
struct Counters {
    /// Last underrun count reported by the running pipeline; zero while stopped.
    underruns: AtomicU64,
    /// Frames written in the current stream, for the per-stream stats line.
    frames: AtomicU64,
}

/// The announced stream format as the daemon sees it.
///
/// [`AudioFormat`] is non-exhaustive and only the client can build one, so the pipeline is
/// driven through this value: [`ClientSink::open`] converts the client's format, and the
/// hermetic sink tests can describe a stream without a live connection. The `gst-launch`
/// pipeline itself is fixed at S16LE/48 kHz/stereo and the daemon advertises exactly that,
/// so the sample fields document the stream rather than configure the decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFormat {
    /// Codec of the stream.
    pub codec: Codec,
    /// Sample rate in hertz.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u8,
    /// Bits per sample.
    pub bit_depth: u8,
    /// FLAC `fLaC` marker plus `STREAMINFO` for a FLAC stream.
    pub codec_header: Option<Vec<u8>>,
}

impl From<AudioFormat> for StreamFormat {
    fn from(format: AudioFormat) -> Self {
        Self {
            codec: format.codec,
            sample_rate: format.sample_rate,
            channels: format.channels,
            bit_depth: format.bit_depth,
            codec_header: format.codec_header,
        }
    }
}

/// One request for the sink thread.
///
/// Every variant that can fail carries a reply channel: the caller has to see the error
/// on the call that caused it, because a sink error is fatal for the client connection.
enum Command {
    Open {
        format: StreamFormat,
        done: SyncSender<Result<()>>,
    },
    Write {
        data: Vec<u8>,
        done: SyncSender<Result<()>>,
    },
    Flush {
        done: SyncSender<()>,
    },
    Restart {
        done: SyncSender<Result<()>>,
    },
    Stop {
        done: SyncSender<Result<()>>,
    },
}

impl GstSink {
    /// Starts the sink thread with the TV's default `gst-launch-1.0` and `pacat`.
    pub fn start(
        visual: Option<SharedAudioVisual>,
        output_delay: SharedOutputDelay,
    ) -> Result<(GstSink, GstSinkHandle)> {
        Self::start_with_binaries(
            Path::new(DEFAULT_DECODER),
            Path::new(DEFAULT_PLAYER),
            visual,
            output_delay,
        )
    }

    /// Starts the sink thread with explicit binaries (hermetic tests pass the fake scripts).
    ///
    /// No child is started here: [`ClientSink::open`] does that once a stream announces its
    /// format, so an idle player never runs a decoder or a `pacat` that could grab the TV's
    /// audio sink.
    pub fn start_with_binaries(
        gst: &Path,
        pacat: &Path,
        visual: Option<SharedAudioVisual>,
        output_delay: SharedOutputDelay,
    ) -> Result<(GstSink, GstSinkHandle)> {
        let (commands, receiver) = sync_channel(QUEUED_FRAMES);
        let counters = Arc::new(Counters::default());
        let runner = Runner {
            gst: gst.to_path_buf(),
            pacat: pacat.to_path_buf(),
            visual,
            counters: Arc::clone(&counters),
        };
        std::thread::Builder::new()
            .name("takt-sink".to_owned())
            .spawn(move || runner.run(receiver))
            .context("failed to start the audio sink thread")?;
        let sink = GstSink {
            commands: commands.clone(),
            counters: Arc::clone(&counters),
            output_delay,
        };
        Ok((sink, GstSinkHandle { commands, counters }))
    }

    /// Restarts the children and writes the stream's codec header before any frame.
    ///
    /// A new stream reuses neither the decoder's format state nor the player's queue, so both
    /// children are replaced, exactly as the daemon did on every `stream/start`. A pause stops
    /// the children, so this is also the resume cost; the elapsed time is logged once per stream
    /// start so it can be measured on the TV.
    pub fn open_stream(&mut self, format: StreamFormat) -> Result<()> {
        if format.codec != Codec::Flac {
            bail!(
                "the gst pipeline decodes FLAC only, not the announced `{}` stream",
                format.codec
            );
        }
        let started = Instant::now();
        let result = request(&self.commands, |done| Command::Open { format, done })?;
        eprintln!("[takt] pipeline open {} ms", started.elapsed().as_millis());
        result
    }
}

impl ClientSink for GstSink {
    /// See [`GstSink::open_stream`].
    fn open(&mut self, format: AudioFormat) -> Result<()> {
        self.open_stream(format.into())
    }

    /// Forwards one encoded frame to the decoder; raw PCM is rejected.
    fn write(&mut self, frame: AudioFrame<'_>, _due: Instant) -> Result<()> {
        let data = match frame {
            AudioFrame::Encoded { data, .. } => data.to_vec(),
            AudioFrame::Pcm(_) => {
                bail!("the gst pipeline takes encoded FLAC frames; it cannot play raw PCM")
            }
        };
        self.counters.frames.fetch_add(1, Ordering::Relaxed);
        request(&self.commands, |done| Command::Write { data, done })?
    }

    /// Drops decoded audio buffered in the daemon's bridge.
    ///
    /// The client calls this on every stream boundary (start, end, clear and after a
    /// connection attempt), so it must stay cheap. It is only a barrier for in-flight writes:
    /// `pacat` has no flush API, so the player's own buffer and PulseAudio's queue are untouched
    /// here, and only stopping the children discards them (see [`GstSinkHandle::stop`]).
    fn flush(&mut self) -> Result<()> {
        request(&self.commands, |done| Command::Flush { done })
    }

    /// Reports the live delay after the audio port.
    ///
    /// The protocol's `output_delay_ms` is the delay *after* the audio output (external speakers,
    /// an amplifier), not the pipeline's own buffering - the client compensates that itself. The
    /// crate maps this to `output_delay_ms`, so reporting the bridge depth there told the server
    /// the TV had 1.5 s of external delay; it then scheduled 2.3 s ahead to keep the same headroom.
    /// Takt plays through the TV's own output by default, so the value starts at zero. A TV that
    /// routes audio over ARC to an external device has a real delay the TV cannot report, so the
    /// viewer sets it in the UI; the shared atomic makes a change apply without a reconnect.
    fn output_delay(&self) -> Option<Duration> {
        let millis = self.output_delay.load(Ordering::Relaxed);
        if millis == 0 {
            None
        } else {
            Some(Duration::from_millis(millis))
        }
    }

    /// The time between this `write` and the audio port.
    ///
    /// The client schedules playout itself and writes each frame this much before its deadline,
    /// so the frame reaches the port on time. It has to cover `pacat`'s 500 ms target buffer plus
    /// the slack that keeps it off the drain edge: at exactly the buffer size the client writes on
    /// the same edge the player reads, and every frame waits (measured: 571 of 572). The slack is
    /// also what absorbs scheduling jitter, so the offset against other players stays constant.
    fn pre_port_delay(&self) -> Duration {
        Duration::from_millis(700)
    }

    /// Reports the pipeline's underrun count; diagnostic only.
    fn underruns(&self) -> u64 {
        self.counters.underruns.load(Ordering::Relaxed)
    }
}

impl GstSinkHandle {
    /// Restarts both children and re-primes the decoder with the current stream's header.
    ///
    /// Used for `stream/clear`: a seek may resume mid-frame, so the decoder must not keep
    /// any of its previous stream state.
    pub async fn restart(&self) -> Result<()> {
        let commands = self.commands.clone();
        let answered = tokio::task::spawn_blocking(move || {
            request(&commands, |done| Command::Restart { done })
        })
        .await
        .context("the audio sink request task panicked")?;
        answered?
    }

    /// Stops and reaps both children, discarding the pipeline.
    ///
    /// This is how buffered audio is dropped. `pacat` has no flush API, so the bytes already in
    /// its stdin pipe and in PulseAudio's buffer can only go away with the process; killing it is
    /// this pipeline's equivalent of `snd_pcm_drop`. The sink object stays alive for the client's
    /// next `stream/start`, where [`ClientSink::open`] starts fresh children.
    pub async fn stop(&self) -> Result<()> {
        let commands = self.commands.clone();
        let answered =
            tokio::task::spawn_blocking(move || request(&commands, |done| Command::Stop { done }))
                .await
                .context("the audio sink request task panicked")?;
        answered?
    }
}

/// Sends one request and waits for its answer.
fn request<T>(
    commands: &SyncSender<Command>,
    build: impl FnOnce(SyncSender<T>) -> Command,
) -> Result<T> {
    let (done, reply) = sync_channel(1);
    commands
        .send(build(done))
        .map_err(|_| anyhow!("the audio sink thread has stopped"))?;
    reply
        .recv()
        .map_err(|_| anyhow!("the audio sink thread stopped before it answered"))
}

/// The thread that owns the child processes.
struct Runner {
    gst: PathBuf,
    pacat: PathBuf,
    visual: Option<SharedAudioVisual>,
    counters: Arc<Counters>,
}

impl Runner {
    /// Runs the command loop until every handle was dropped.
    ///
    /// The runtime is separate from the daemon's: the client's synchronous sink calls block
    /// waiting for this loop, so they must never wait on a worker thread of the runtime the
    /// client itself runs on.
    fn run(self, commands: Receiver<Command>) {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("[takt] failed to start the audio sink runtime: {error}");
                return;
            }
        };

        let mut pipeline = Pipeline::default();
        while let Ok(command) = commands.recv() {
            match command {
                Command::Open { format, done } => {
                    let result = runtime.block_on(pipeline.open(&self, format));
                    self.refresh(&pipeline);
                    let _ = done.send(result);
                }
                Command::Write { data, done } => {
                    let result = runtime.block_on(pipeline.write(&data));
                    self.refresh(&pipeline);
                    let _ = done.send(result);
                }
                Command::Flush { done } => {
                    runtime.block_on(pipeline.flush());
                    self.refresh(&pipeline);
                    let _ = done.send(());
                }
                Command::Restart { done } => {
                    let result = runtime.block_on(pipeline.restart());
                    self.refresh(&pipeline);
                    let _ = done.send(result);
                }
                Command::Stop { done } => {
                    // One line per stopped stream. Only frames: the pipeline is fed just in time
                    // now, so the player asks for data between most writes and the old underrun
                    // counter would call every one of those a gap. The client reports real
                    // trouble as late audio instead.
                    let frames = self.counters.frames.load(Ordering::Relaxed);
                    if frames > 0 {
                        eprintln!("[takt] stream stats: {frames} frames played");
                    }
                    self.counters.frames.store(0, Ordering::Relaxed);
                    let result = runtime.block_on(pipeline.stop());
                    self.refresh(&pipeline);
                    let _ = done.send(result);
                }
            }
        }

        // Every handle is gone: leave no child or zombie behind.
        let _ = runtime.block_on(pipeline.stop());
    }

    /// Publishes the pipeline's diagnostic counters to the handles.
    fn refresh(&self, pipeline: &Pipeline) {
        let underruns = pipeline.sink.as_ref().map_or(0, |sink| sink.underruns());
        self.counters.underruns.store(underruns, Ordering::Relaxed);
    }
}

/// The running pipeline plus the format a seek has to re-prime the decoder with.
#[derive(Default)]
struct Pipeline {
    sink: Option<ProcessSink>,
    format: Option<StreamFormat>,
}

impl Pipeline {
    /// Replaces both children and primes the decoder with `format`'s codec header.
    async fn open(&mut self, runner: &Runner, format: StreamFormat) -> Result<()> {
        self.stop().await?;
        let sink = ProcessSink::start_with_binaries_and_visual(
            &runner.gst,
            &runner.pacat,
            runner.visual.clone(),
        )
        .await?;
        if let Some(header) = &format.codec_header {
            // FLAC `fLaC` + STREAMINFO must precede every audio frame of the stream.
            sink.write_pcm(header)
                .await
                .context("failed to write the codec header to the decoder")?;
        }
        self.sink = Some(sink);
        self.format = Some(format);
        Ok(())
    }

    /// Writes one encoded frame to the decoder's stdin.
    async fn write(&self, data: &[u8]) -> Result<()> {
        match &self.sink {
            Some(sink) => sink.write_pcm(data).await,
            None => Err(anyhow!("the audio pipeline is not open")),
        }
    }

    /// Drops decoded audio without touching the children.
    async fn flush(&self) {
        if let Some(sink) = &self.sink {
            sink.flush().await;
        }
    }

    /// Replaces both children, keeping the announced format.
    async fn restart(&mut self) -> Result<()> {
        let Some(sink) = self.sink.take() else {
            return Ok(());
        };
        let sink = sink.restart().await?;
        if let Some(header) = self
            .format
            .as_ref()
            .and_then(|format| format.codec_header.as_ref())
        {
            sink.write_pcm(header)
                .await
                .context("failed to re-prime the decoder with the codec header")?;
        }
        self.sink = Some(sink);
        Ok(())
    }

    /// Kills and reaps both children (a no-op when nothing is running).
    async fn stop(&mut self) -> Result<()> {
        match self.sink.take() {
            Some(sink) => sink.stop().await,
            None => Ok(()),
        }
    }
}
