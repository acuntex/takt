//! PulseAudio sink volume control for the audible output path.

use std::process::Command;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::mpsc;

pub const DEFAULT_SINK: &str = "pcm_output";
pub const SINK_ENV: &str = "TAKT_PULSE_SINK";

/// The command seam: called with `(sink, percent)` and expected to apply the write.
pub(crate) type PactlRunner = Arc<dyn Fn(&str, &str) -> Result<()> + Send + Sync>;

pub struct PulseSink {
    sink: String,
    runner: PactlRunner,
}

impl PulseSink {
    pub fn new(sink: impl Into<String>) -> Self {
        Self::with_runner(sink, Arc::new(run_pactl))
    }

    pub fn from_env() -> Self {
        let sink = std::env::var(SINK_ENV).unwrap_or_else(|_| DEFAULT_SINK.to_owned());
        Self::new(sink)
    }

    pub(crate) fn with_runner(sink: impl Into<String>, runner: PactlRunner) -> Self {
        Self {
            sink: sink.into(),
            runner,
        }
    }

    pub fn sink(&self) -> &str {
        &self.sink
    }

    /// Applies `volume` percent to the configured sink. The caller owns error logging.
    pub fn set_volume(&self, volume: u8) -> Result<()> {
        let percent = volume_percent(volume)?;
        (self.runner)(self.sink(), &percent)
    }
}

/// Cloneable handle to the single task that owns the sink writer.
///
/// MA commands and external TV events both queue here. The task applies values in send order, so
/// an older queued event can never overwrite a newer command. A `pactl` failure is logged by the
/// task and does not propagate to the playback/session loop.
#[derive(Clone)]
pub struct PulseVolume {
    sender: mpsc::UnboundedSender<u8>,
}

impl PulseVolume {
    /// Starts the one writer task for `sink`; it lives for the daemon's lifetime.
    pub fn spawn(sink: Arc<PulseSink>) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel::<u8>();
        tokio::spawn(async move {
            while let Some(volume) = receiver.recv().await {
                let sink = Arc::clone(&sink);
                match tokio::task::spawn_blocking(move || sink.set_volume(volume)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        eprintln!("[takt] pulse sink volume {volume}% failed: {error:#}")
                    }
                    Err(error) => eprintln!("[takt] pulse sink task failed: {error}"),
                }
            }
        });
        Self { sender }
    }

    /// Queues a value. Only a closed queue is an error; `pactl` failures never reach the caller.
    pub fn set_volume(&self, volume: u8) -> Result<()> {
        self.sender
            .send(volume)
            .map_err(|_| anyhow!("PulseAudio volume queue is closed"))
    }
}

fn run_pactl(sink: &str, percent: &str) -> Result<()> {
    let output = Command::new("pactl")
        .arg("set-sink-volume")
        .arg(sink)
        .arg(percent)
        .output()
        .with_context(|| format!("failed to run pactl for sink {sink}"))?;
    if !output.status.success() {
        bail!(
            "pactl set-sink-volume {sink} {percent} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    println!("[takt] pulse sink {sink} volume={percent}");
    Ok(())
}

pub(crate) fn volume_percent(volume: u8) -> Result<String> {
    if volume > 100 {
        bail!("sink volume {volume} out of range (0-100)");
    }
    Ok(format!("{}%", sink_percent(volume)))
}

/// Maps the logical 0–100 TV/MA volume to the PulseAudio sink percentage.
///
/// PulseAudio's percentage is cubic in amplitude: 20% is about -42 dB, far quieter than a TV's
/// 20 step. A square-root curve lifts low logical values into the audible range while keeping 0
/// silent and 100 at full scale.
pub(crate) fn sink_percent(volume: u8) -> u8 {
    if volume == 0 {
        return 0;
    }
    if volume >= 100 {
        return 100;
    }
    let ratio = f64::from(volume) / 100.0;
    (ratio.sqrt() * 100.0).round() as u8
}
