//! Takt playback daemon.

mod app;
mod config;
mod discovery;
mod foreground;
mod http;
mod luna;
mod pulse;
#[allow(dead_code)]
mod sink;
mod state;
mod visual;
mod watchdog;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use app::{App, TvControl};
use config::Config;
use luna::audio::{next_subscription_retry_delay, Audio, SUBSCRIPTION_RETRY_INITIAL};
use luna::autoopen::APP_ID;
use luna::tvpower::{TvPower, VETO_RENEWAL_INTERVAL};
use luna::LunaWorker;
use pulse::{PulseSink, PulseVolume};
use tokio::sync::mpsc;
use visual::{AudioVisual, SharedAudioVisual};
use watchdog::Watchdog;

const DEFAULT_CONFIG_PATH: &str = "/media/internal/takt.json";

/// How long startup waits for the initial Luna volume snapshot before connecting with the safe
/// volume 0. A late snapshot still corrects the state through the normal event path.
const INITIAL_VOLUME_TIMEOUT: Duration = Duration::from_secs(2);

/// Fallback used when LS2/glib is unavailable (for example on a development host).
struct LoggingTvControl;

impl TvControl for LoggingTvControl {
    fn set_muted(&self, muted: bool) -> Result<()> {
        println!("[takt] TV mute -> {muted} (logging fallback; Luna unavailable)");
        Ok(())
    }

    fn set_volume(&self, volume: u8) -> Result<()> {
        println!("[takt] TV volume -> {volume} (logging fallback; Luna unavailable)");
        Ok(())
    }

    fn ensure_ui_foreground(&self) -> Result<()> {
        // Without Luna there is no way to query the foreground app; print the decision the real
        // control would take (launch unless the UI is already visible) and change nothing.
        println!("[takt] auto-open: launching {APP_ID} (logging fallback; Luna unavailable)");
        Ok(())
    }
}

/// TV-control facade over the backend-independent `Audio` state machine and Luna worker backend.
struct LunaTvControl {
    audio: Arc<Audio<LunaWorker>>,
    pulse: PulseVolume,
    luna: LunaWorker,
}

impl TvControl for LunaTvControl {
    fn set_muted(&self, muted: bool) -> Result<()> {
        self.audio.set_muted(muted)
    }

    fn set_volume(&self, volume: u8) -> Result<()> {
        // The TV master drives the OSD and MA's logical value; the PulseAudio sink is the
        // audible level on this output path. A sink failure is logged by the writer task and must
        // not fail the TV action, which would disconnect playback.
        if let Err(error) = self.pulse.set_volume(volume) {
            eprintln!("[takt] pulse sink queue failed: {error:#}");
        }
        self.audio.set_volume(volume)
    }

    fn set_playing(&self, playing: bool) -> Result<()> {
        self.audio.set_playing(playing);
        Ok(())
    }

    fn ensure_ui_foreground(&self) -> Result<()> {
        // Same dedicated LS2/glib worker as volume and mute. The query happens before any launch,
        // so an already visible UI keeps the focus; the caller logs a failure and keeps playing.
        self.luna.ensure_ui_foreground()
    }
}

type TvSetup = (
    Box<dyn TvControl>,
    Option<tokio::sync::mpsc::UnboundedReceiver<(u8, bool)>>,
    Option<Arc<TvPower<LunaWorker>>>,
    Option<PulseVolume>,
);

/// Keeps one `getForegroundAppInfo` subscription armed for the daemon's lifetime.
///
/// Each attempt owns its own channel. The worker holds the sender for the life of the accepted
/// subscription; when it refuses a setup or the TV ends the subscription, the worker drops the
/// sender, the forwarding loop ends, and the next attempt arms exactly one replacement with the
/// same bounded backoff the volume subscription uses. The watchdog's receiver stays open across
/// attempts and falls back to its five-second query while no subscription is live.
fn spawn_foreground_subscription(
    worker: LunaWorker,
    events: mpsc::UnboundedSender<Option<String>>,
) {
    tokio::spawn(async move {
        let mut delay = SUBSCRIPTION_RETRY_INITIAL;
        loop {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let subscribe = worker.clone();
            let result =
                tokio::task::spawn_blocking(move || subscribe.subscribe_foreground(sender)).await;
            match result {
                Ok(Ok(())) => {
                    println!("[takt] Luna foreground subscription active");
                    delay = SUBSCRIPTION_RETRY_INITIAL;
                    // Forward every pushed answer. The channel closes when the worker releases a
                    // rejected or ended subscription context.
                    while let Some(app) = receiver.recv().await {
                        if events.send(app).is_err() {
                            return;
                        }
                    }
                    eprintln!("[takt] Luna foreground subscription ended; re-arming");
                }
                Ok(Err(error)) => eprintln!(
                    "[takt] Luna foreground subscription failed: {error:#}; retrying in {}s",
                    delay.as_secs()
                ),
                Err(error) => eprintln!(
                    "[takt] Luna foreground subscription task failed: {error}; retrying in {}s",
                    delay.as_secs()
                ),
            }
            tokio::time::sleep(delay).await;
            delay = next_subscription_retry_delay(delay);
        }
    });
}

#[allow(clippy::type_complexity)]
#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::var_os("TAKT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(primary_error) => {
            // The Homebrew Channel runs native services inside the `native_devmode` jail, which
            // cannot read /media/internal. setup.sh copies the identity next to the binary, so
            // fall back to it instead of dying on the primary path.
            let sibling = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("takt.json")));
            match sibling {
                Some(path) if path != config_path => {
                    eprintln!(
                        "[takt] cannot read {} ({primary_error:#}); using {}",
                        config_path.display(),
                        path.display()
                    );
                    Config::load(&path)
                        .with_context(|| format!("failed to load config {}", path.display()))?
                }
                _ => {
                    return Err(primary_error).with_context(|| {
                        format!("failed to load config {}", config_path.display())
                    })
                }
            }
        }
    };

    println!(
        "[takt] starting player {:?} for server {}",
        config.player_name, config.server
    );

    let mut foreground_cache = None;
    let mut foreground_events = None;
    let (tv, mut volume_events, tv_power, pulse_volume): TvSetup = match LunaWorker::start() {
        Ok(worker) => {
            let audio = Arc::new(Audio::new(worker.clone()));
            // The TV control keeps its own worker handle for auto-open; the same thread and
            // command channel serve volume, mute and foreground calls in order.
            let luna = worker.clone();
            // `GET /api/foreground` and the departure watchdog's fallback share this worker: both
            // read through the same cache, so a UI read and a fallback tick cost one query.
            let foreground_worker = worker.clone();
            foreground_cache = Some(foreground::ForegroundCache::new(move || {
                foreground_worker.foreground_app()
            }));
            // The pushed foreground answers reach the watchdog through a stable channel; the
            // supervisor owns the held subscription and re-arms it when it drops.
            let (events_sender, events_receiver) = mpsc::unbounded_channel();
            foreground_events = Some(events_receiver);
            spawn_foreground_subscription(worker.clone(), events_sender);
            // A single writer task owns the sink. MA commands and external TV events both queue
            // here, so an older event can never overwrite a newer command. `pactl` runs on a
            // blocking worker, never on the LS2/glib thread.
            let pulse = Arc::new(PulseSink::from_env());
            let pulse_volume = PulseVolume::spawn(pulse);
            let (volume_sender, volume_receiver) = tokio::sync::mpsc::unbounded_channel();
            // Never block Tokio startup on LS2 setup, and retry only until the first successful
            // subscription. The worker keeps at most one live VolumeContext.
            let subscribe_audio = Arc::clone(&audio);
            let subscribe_pulse = pulse_volume.clone();
            tokio::spawn(async move {
                let mut delay = SUBSCRIPTION_RETRY_INITIAL;
                loop {
                    let audio = Arc::clone(&subscribe_audio);
                    let sender = volume_sender.clone();
                    let pulse_volume = subscribe_pulse.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        audio.subscribe(move |volume, muted| {
                            println!(
                                "[takt] external TV volume event volume={volume} muted={muted}"
                            );
                            if let Err(error) = pulse_volume.set_volume(volume) {
                                eprintln!("[takt] pulse sink queue failed: {error:#}");
                            }
                            let _ = sender.send((volume, muted));
                        })
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {
                            println!("[takt] Luna volume subscription active");
                            break;
                        }
                        Ok(Err(error)) => eprintln!(
                            "[takt] Luna volume subscription failed: {error:#}; retrying in {}s",
                            delay.as_secs()
                        ),
                        Err(error) => eprintln!(
                            "[takt] Luna volume subscription task failed: {error}; retrying in {}s",
                            delay.as_secs()
                        ),
                    }
                    tokio::time::sleep(delay).await;
                    delay = next_subscription_retry_delay(delay);
                }
            });
            let volume_events = Some(volume_receiver);
            let power = Arc::new(TvPower::new(worker));
            if let Err(error) = power.keep_awake() {
                eprintln!("[takt] initial screensaver veto failed: {error:#}");
            }
            (
                Box::new(LunaTvControl {
                    audio,
                    pulse: pulse_volume.clone(),
                    luna,
                }),
                volume_events,
                Some(power),
                Some(pulse_volume),
            )
        }
        Err(error) => {
            eprintln!("[takt] Luna unavailable; using logging TV control: {error:#}");
            (Box::new(LoggingTvControl), None, None, None)
        }
    };

    // Luna's volume subscription is asynchronous. Wait briefly for its first snapshot so the
    // initial `client/state` can advertise the real TV volume; without one, connect with the safe
    // volume 0 and let later events correct it.
    let initial_volume = match volume_events.take() {
        Some(mut receiver) => {
            match tokio::time::timeout(INITIAL_VOLUME_TIMEOUT, receiver.recv()).await {
                Ok(Some((volume, muted))) => {
                    println!("[takt] initial TV volume {volume} muted={muted}");
                    volume_events = Some(receiver);
                    Some((volume, muted))
                }
                Ok(None) => {
                    eprintln!("[takt] Luna volume channel closed before the initial snapshot");
                    None
                }
                Err(_) => {
                    eprintln!(
                        "[takt] no initial TV volume within {}s; using safe volume 0",
                        INITIAL_VOLUME_TIMEOUT.as_secs()
                    );
                    volume_events = Some(receiver);
                    None
                }
            }
        }
        None => None,
    };

    // If the TV snapshot never arrived, MA is told the safe value 0. Apply that to the audible path
    // too so the advertised state and the sound match; a late snapshot corrects both.
    if initial_volume.is_none() {
        if let Some(pulse) = &pulse_volume {
            match pulse.set_volume(0) {
                Ok(()) => println!("[takt] no initial TV volume; PulseAudio sink set to 0%"),
                Err(error) => eprintln!("[takt] pulse sink queue failed: {error:#}"),
            }
        }
    }

    if let Some(power) = tv_power {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(VETO_RENEWAL_INTERVAL);
            // The initial registration already happened above; interval's first tick is immediate.
            interval.tick().await;
            loop {
                interval.tick().await;
                let power = Arc::clone(&power);
                match tokio::task::spawn_blocking(move || power.keep_awake()).await {
                    Ok(Ok(())) => println!("[takt] screensaver veto renewal acknowledged"),
                    Ok(Err(error)) => {
                        eprintln!("[takt] screensaver veto renewal failed: {error:#}")
                    }
                    Err(error) => eprintln!("[takt] veto renewal task failed: {error}"),
                }
            }
        });
    }

    let (command_sender, command_receiver) = tokio::sync::mpsc::unbounded_channel();
    let visual: SharedAudioVisual = Arc::new(RwLock::new(AudioVisual::default()));
    // The sink reads this on every client/state and clock burst, so an HTTP config write applies
    // without a reconnect. The config is the seed; a change is logged by the handler.
    let output_delay = Arc::new(AtomicU64::new(u64::from(config.output_delay_ms)));
    let mut app = App::new(config, tv)
        .with_command_receiver(command_receiver)
        .with_visual(Arc::clone(&visual))
        .with_output_delay(Arc::clone(&output_delay));
    if let Some((volume, muted)) = initial_volume {
        app = app.with_tv_volume(volume, muted);
    }
    if let Some(volume_events) = volume_events {
        app = app.with_volume_events(volume_events);
    }
    app = app.with_server_finder(Arc::new(discovery::LiveFinder::new()));
    // An auto-open is a launch in flight: until it lands the foreign app is still in front, and
    // pausing there would kill the playback the viewer just asked for.
    let grace: foreground::GraceUntil = Default::default();
    app = app.with_grace(std::sync::Arc::clone(&grace));
    // The daemon owns the "viewer left" rule: webOS suspends a hidden page before the UI could
    // see its own departure. The TV pushes every foreground change on a held Luna subscription;
    // the watchdog pauses on the first foreign app and falls back to a five-second query.
    if let (Some(foreground), Some(events)) = (foreground_cache.clone(), foreground_events) {
        tokio::spawn(
            Watchdog::new(app.shared_state(), Some(foreground), command_sender.clone())
                .with_grace(grace.clone())
                .with_events(events)
                .run(),
        );
    }
    match http::PinStore::generate(http::PIN_FILE_PATH) {
        Ok(pin) => {
            let mut http_state =
                http::HttpState::new(app.shared_state(), &config_path, pin, command_sender)
                    .with_visual(visual)
                    .with_output_delay(Arc::clone(&output_delay));
            if let Some(foreground) = foreground_cache {
                http_state = http_state.with_foreground(foreground);
            }
            tokio::spawn(async move {
                match tokio::net::TcpListener::bind(http::HTTP_BIND_ADDR).await {
                    Ok(listener) => {
                        println!("[takt] HTTP API listening on {}", http::HTTP_BIND_ADDR);
                        if let Err(error) = http::serve(listener, http_state).await {
                            eprintln!("[takt] HTTP server failed: {error:#}");
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "[takt] HTTP bind failed on {}: {error}",
                            http::HTTP_BIND_ADDR
                        )
                    }
                }
            });
        }
        Err(error) => eprintln!("[takt] HTTP PIN setup failed: {error:#}"),
    }

    app.run().await
}
