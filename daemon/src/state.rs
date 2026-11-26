//! Shared playback and now-playing state.
//!
//! Task 8 can clone [`SharedAppState`] into its HTTP/SSE handlers without coupling those
//! handlers to the daemon loop. The synchronous lock is only held for short in-memory reads
//! and writes, never across an `.await`.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use syncspin_client::ArtworkImage;

use crate::config::Config;

/// The colors the server derived from the audio, as `[R, G, B]` triples.
///
/// A color the server did not send is `None`. The field names match the wire; the palette is
/// already flattened, so the timestamp and the role lifecycle stay with the client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ThemeColors {
    /// Background for dark mode.
    pub background_dark: Option<[u8; 3]>,
    /// Background for light mode.
    pub background_light: Option<[u8; 3]>,
    /// The dominant color.
    pub primary: Option<[u8; 3]>,
    /// A secondary or complementary color.
    pub accent: Option<[u8; 3]>,
    /// A light color for use on dark backgrounds.
    pub on_dark: Option<[u8; 3]>,
    /// A dark color for use on light backgrounds.
    pub on_light: Option<[u8; 3]>,
}

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
    /// The most recent artwork image the server delivered on channel 0.
    ///
    /// Skipped in the JSON state: the encoded bytes are served by `GET /api/artwork`, and the
    /// status document only carries [`Self::artwork_revision`]. The image has its own lifecycle,
    /// independent of the metadata role, so a cleared track does not clear it.
    #[serde(skip)]
    pub artwork: Option<Arc<ArtworkImage>>,
    /// Monotonically increasing revision of artwork changes. Bumped for both a new image and a
    /// cleared channel, so the UI can reload the artwork it is showing.
    pub artwork_revision: u64,
    /// The server's color palette, when it sent one.
    pub color: Option<ThemeColors>,
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
            artwork: None,
            artwork_revision: 0,
            color: None,
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
