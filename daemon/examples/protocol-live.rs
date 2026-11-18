//! Live connection check against a real Music Assistant Sendspin server.
//!
//! Not part of `cargo test` (network-dependent). Connects with the `syncspin-client` crate,
//! logs the connection and the first stream format, and exits cleanly once the connection is
//! up. The client may land in `_held_unapproved` until approved in the MA UI - expected.
//!
//! Usage:
//!   cargo run --example protocol-live -- [server] [player_name]

#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;

use std::time::Duration;

use anyhow::Result;
use config::{Config, DEFAULT_SERVER};
use syncspin_client::{AudioFormat, AudioFrame, Client, ClientConfig, Event, Sink, VolumePolicy};

/// A sink that accepts the stream and plays nothing, so a headless host can verify a connection.
struct NullSink;

impl Sink for NullSink {
    fn open(&mut self, format: AudioFormat) -> Result<()> {
        println!(
            "[protocol-live] stream: {} {} Hz {} ch {} bit",
            format.codec, format.sample_rate, format.channels, format.bit_depth
        );
        Ok(())
    }

    fn write(&mut self, _frame: AudioFrame<'_>, _due: std::time::Instant) -> Result<()> {
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let server = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| DEFAULT_SERVER.to_owned());
    let player_name = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "TV (protocol live)".to_owned());

    // A temporary config path gives us a valid config plus a freshly generated identity.
    let tmp_path = std::env::temp_dir().join(format!(
        "takt-live-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    ));
    let mut cfg: Config = Config::load(&tmp_path)?;
    cfg.server = server;
    cfg.player_name = player_name;
    let identity = cfg
        .identity()
        .ok_or_else(|| anyhow::anyhow!("the generated config has no identity"))?;
    println!("[protocol-live] client_id={}", cfg.client_id);
    println!("[protocol-live] connecting to {}", cfg.server);

    let mut client_config =
        ClientConfig::new(cfg.server.clone(), cfg.player_name.clone(), identity);
    client_config.audio_mode = syncspin_client::AudioMode::Passthrough;
    client_config.volume_policy = VolumePolicy::default();
    client_config.max_backoff = Duration::from_secs(30);

    let (client, mut events) = Client::connect(client_config, Box::new(NullSink)).await?;

    loop {
        match events.next().await {
            Some(Event::Connected { server_id, .. }) => {
                println!("[protocol-live] connected to server {server_id}");
                break;
            }
            Some(Event::Disconnected { reason, retry_in }) => println!(
                "[protocol-live] disconnected: {reason} (retry in {}s)",
                retry_in.as_secs()
            ),
            Some(_) => {}
            None => {
                println!("[protocol-live] the connection ended before it was established");
                break;
            }
        }
    }

    // Dropping the stream and the handle ends the connection and the sink.
    drop(events);
    drop(client);
    let _ = std::fs::remove_file(&tmp_path);
    Ok(())
}
