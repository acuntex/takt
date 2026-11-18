//! Shared playback and now-playing state.
//!
//! Task 8 can clone [`SharedAppState`] into its HTTP/SSE handlers without coupling those
//! handlers to the daemon loop. The synchronous lock is only held for short in-memory reads
//! and writes, never across an `.await`.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// High-level daemon/playback lifecycle exposed to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    Idle,
    Connecting,
    Playing,
    Paused,
    Error,
}

/// Serializable application state consumed by Task 8's status and SSE endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppState {
    pub state: PlaybackState,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// Album title, when the server reports one. Shown as the stage's album chip.
    pub album: Option<String>,
    pub artwork_url: Option<String>,
    /// Track position in milliseconds at the time of the last metadata update.
    pub progress_ms: Option<u64>,
    /// Track duration in milliseconds, when reported by the server.
    pub duration_ms: Option<u64>,
    /// Playback speed per mille (1000 = normal), used to interpolate progress between updates.
    pub playback_speed: Option<u32>,
    /// Monotonically increasing revision of metadata events. A changed revision tells the UI to
    /// re-base progress even when `progress_ms` is numerically unchanged (seek to 0, repeat).
    pub metadata_revision: u64,
    /// A seek this host sent is waiting for its stream.
    ///
    /// The server answers a controller seek by ending the stream and starting a new one; a
    /// cold buffer makes that take seconds, and during the gap the daemon has nothing to say
    /// but "paused". This says why, so the UI can show "Seeking" instead of a lie.
    pub seeking: bool,
    /// TV-master volume on the webOS 0–100 scale. `None` until the TV reports it (initial Luna
    /// snapshot or a later external change); MA controller state is not authoritative. While it
    /// is `None`, MA volume commands are ignored and the initial `client/state` reports the safe
    /// volume 0.
    pub volume: Option<u8>,
    pub muted: bool,
    pub server: String,
    pub player_name: String,
    /// Delay after the TV's audio port, in milliseconds, as configured and currently applied.
    ///
    /// The UI reads it back after every write; zero is the default and means none.
    pub output_delay_ms: u16,
}

impl AppState {
    /// Creates the initial idle state, retaining the configured server/player identity.
    pub fn new(config: &Config) -> Self {
        Self {
            state: PlaybackState::Idle,
            title: None,
            artist: None,
            album: None,
            artwork_url: None,
            progress_ms: None,
            duration_ms: None,
            playback_speed: None,
            metadata_revision: 0,
            seeking: false,
            volume: None,
            muted: false,
            server: config.server.clone(),
            player_name: config.player_name.clone(),
            output_delay_ms: config.output_delay_ms,
        }
    }
}

/// Thread-safe handle shared by the daemon loop and, later, the HTTP/SSE API.
pub type SharedAppState = Arc<RwLock<AppState>>;
