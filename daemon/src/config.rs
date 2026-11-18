//! Persisted daemon configuration.
//!
//! The configuration lives in a small JSON file (production path:
//! `/media/internal/takt.json`). `server`, `player_name`, `client_id`,
//! `client_secret`, `auto_open` and `output_delay_ms` are serialized; the source path is
//! kept separately so [`Config::save`] can write back to the file it was loaded from
//! without exposing the path in the JSON.
//!
//! The client identity is a persistent X25519 keypair used by the native Sendspin
//! Noise handshake: `client_id` is the 43-character unpadded base64url public key and
//! `client_secret` is the matching 32-byte private key in the same encoding. The
//! secret means the config file is created with mode `0600` and an existing file is
//! tightened to `0600` on load/save.
//!
//! A missing file is created with a fresh keypair. Legacy 32-character hex client IDs,
//! missing or malformed key material, individually valid public/secret values that do
//! not belong to the same keypair, and a file that is not valid JSON at all are all
//! repaired by generating a new keypair and atomically replacing the file; the daemon
//! always starts, and each repair logs exactly one line. A repair intentionally changes
//! the client identity, so Music Assistant must approve the new public key again. Only
//! genuine I/O errors (an unreadable path, an unwritable directory) are returned.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use syncspin_client::Identity;

/// Sendspin server used when no config file exists.
///
/// A generic mDNS name rather than a concrete address: the installation's own host is
/// configured in the UI or the config file.
pub const DEFAULT_SERVER: &str = "music-assistant.local:8927";

/// Player name used when no config file exists.
pub const DEFAULT_PLAYER_NAME: &str = "Takt";

/// Largest delay after the audio port the daemon accepts, in milliseconds.
///
/// This is the protocol's own bound for `output_delay_ms` (0-5000): a server may send any value in
/// that range, and capping lower would make the daemon report a delay it did not apply. The UI
/// offers a narrower 0..=200 range for on-ear tuning; the file and API accept the full range.
pub const MAX_OUTPUT_DELAY_MS: u16 = 5_000;

/// Disambiguates temp files created by concurrent saves in the same process.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Attempts to create a fresh temp file before giving up on name collisions.
const TEMP_FILE_ATTEMPTS: usize = 16;

/// Persisted daemon configuration.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Address of the Sendspin server (`host:port`).
    pub server: String,
    /// Human-readable player name announced to the server.
    pub player_name: String,
    /// Stable 43-character unpadded base64url X25519 public key.
    pub client_id: String,
    /// Matching 32-byte X25519 static secret, unpadded base64url.
    ///
    /// Never returned by an HTTP endpoint; the file is kept at mode `0600`.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub client_secret: String,
    /// Bring the bundled UI to the foreground when playback starts. Defaults to on; an older
    /// config without the key keeps the default instead of failing to parse.
    #[serde(default = "default_auto_open")]
    pub auto_open: bool,
    /// Delay after the audio port, in milliseconds, reported as `output_delay_ms`.
    ///
    /// This is the external chain (an ARC or optical receiver), not the pipeline's own buffering:
    /// the client compensates that itself. Missing or out-of-range values are clamped to
    /// `0..=MAX_OUTPUT_DELAY_MS` on load, and [`Config::save`] clamps again before writing.
    #[serde(default, deserialize_with = "deserialize_output_delay")]
    pub output_delay_ms: u16,
    /// Where this config was loaded from; not part of the JSON document.
    #[serde(skip)]
    path: PathBuf,
}

impl fmt::Debug for Config {
    /// Formats the config without exposing the client's static secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("server", &self.server)
            .field("player_name", &self.player_name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("auto_open", &self.auto_open)
            .field("output_delay_ms", &self.output_delay_ms)
            .field("path", &self.path)
            .finish()
    }
}

impl Config {
    /// Loads the configuration from `path`, creating defaults if the file is missing.
    ///
    /// A missing file is persisted atomically before it is returned, so a later load
    /// sees the same generated keypair. A file that cannot be parsed, a legacy client ID
    /// (32-character hex) and missing, invalid or mismatched key material all produce a
    /// fresh keypair that is saved over the old file, with one log line per repair; the
    /// daemon never fails to start because of its identity file. A valid keypair is
    /// never rotated. The file mode is tightened to `0600`. Only I/O errors on the path
    /// are returned.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        match fs::read(&path) {
            Ok(bytes) => {
                let config = match serde_json::from_slice::<Self>(&bytes) {
                    Ok(config) => config,
                    Err(error) => {
                        eprintln!(
                            "[takt] config {} is not readable JSON ({error}); generated a new \
                             identity (Music Assistant approval must be renewed)",
                            path.display()
                        );
                        Self::fresh(path.clone())?
                    }
                };
                let mut config = config;
                config.path = path.clone();

                if config.identity().is_some() {
                    // Repair permissions of an older 0644 file even when nothing else changes.
                    set_private_permissions(&path)?;
                    Ok(config)
                } else {
                    let (client_id, client_secret) = generate_keypair();
                    config.client_id = client_id;
                    config.client_secret = client_secret;
                    eprintln!(
                        "[takt] config {} had no valid matching X25519 keypair; generated a \
                         new identity (Music Assistant approval must be renewed)",
                        path.display()
                    );
                    config
                        .save()
                        .context("failed to persist migrated client keypair")?;
                    Ok(config)
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::fresh(path)?),
            Err(err) => {
                Err(err).with_context(|| format!("failed to read config {}", path.display()))
            }
        }
    }

    /// Builds and persists a default configuration with a fresh keypair at `path`.
    fn fresh(path: PathBuf) -> Result<Self> {
        let (client_id, client_secret) = generate_keypair();
        let mut config = Self {
            server: DEFAULT_SERVER.to_owned(),
            player_name: DEFAULT_PLAYER_NAME.to_owned(),
            client_id,
            client_secret,
            auto_open: default_auto_open(),
            output_delay_ms: 0,
            path,
        };
        config.save().context("failed to persist default config")?;
        Ok(config)
    }

    /// Clamps a requested post-port delay into the supported range.
    pub fn clamp_output_delay_ms(requested: u64) -> u16 {
        requested.min(u64::from(MAX_OUTPUT_DELAY_MS)) as u16
    }

    /// Stores a delay the server asked for and returns the value that was written.
    ///
    /// The file is the shared source of truth: the HTTP API writes it too, and this instance's
    /// snapshot may be older. So the value is merged into what is on disk right now instead of
    /// writing this snapshot back, which could undo a player name saved in the meantime. A file
    /// that cannot be read or parsed is an error and never a reason to regenerate the identity;
    /// that repair belongs to [`Config::load`] at startup.
    pub fn store_output_delay(&mut self, requested: u64) -> Result<u16> {
        let bytes = fs::read(&self.path)
            .with_context(|| format!("failed to read config {}", self.path.display()))?;
        let mut stored: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("config {} is not readable JSON", self.path.display()))?;
        // `path` is skipped in the JSON, so the parsed copy has to inherit it to save there.
        stored.path = self.path.clone();
        stored.output_delay_ms = Self::clamp_output_delay_ms(requested);
        stored.save()?;
        self.output_delay_ms = stored.output_delay_ms;
        Ok(stored.output_delay_ms)
    }

    /// Returns the persisted identity, or `None` when the stored pair is not a canonical,
    /// matching X25519 keypair.
    ///
    /// [`Config::load`] guarantees a `Some` for a configuration it returned, so the daemon
    /// can treat `None` here as "the file changed underneath us".
    pub fn identity(&self) -> Option<Identity> {
        let identity = Identity::from_secret_base64(&self.client_secret).ok()?;
        if identity.public_base64() != self.client_id {
            return None;
        }
        Some(identity)
    }

    /// Persists the configuration to the path it was loaded from.
    ///
    /// The post-port delay is clamped before serialization, so no write can store a value the
    /// daemon refuses to report. The JSON is written to a temporary file (created with mode
    /// `0600`) in the same directory and then renamed over the target, so readers never observe a
    /// partially written file and no temporary file is left behind on success.
    pub fn save(&mut self) -> Result<()> {
        self.output_delay_ms = self.output_delay_ms.min(MAX_OUTPUT_DELAY_MS);
        let (tmp_path, mut file) = self.create_temp_file()?;

        // From here on this save exclusively owns `tmp_path`, so a failed write or
        // rename only ever cleans up its own temp file.
        if let Err(err) = self.write_temp(&tmp_path, &mut file) {
            drop(file);
            let _ = fs::remove_file(&tmp_path);
            return Err(err);
        }
        drop(file);

        if let Err(err) = fs::rename(&tmp_path, &self.path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(err)
                .with_context(|| format!("failed to replace config {}", self.path.display()));
        }
        Ok(())
    }

    /// Creates a uniquely named temp file in the config's directory, exclusively.
    fn create_temp_file(&self) -> Result<(PathBuf, File)> {
        let file_name = self
            .path
            .file_name()
            .ok_or_else(|| anyhow!("config path {} has no file name", self.path.display()))?;
        let dir = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let stem = file_name.to_string_lossy();

        for _ in 0..TEMP_FILE_ATTEMPTS {
            let nonce = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let tmp_path = dir.join(format!(
                ".{stem}.{}.{timestamp}.{nonce}.tmp",
                std::process::id()
            ));

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }

            match options.open(&tmp_path) {
                Ok(file) => return Ok((tmp_path, file)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to create temporary config {}", tmp_path.display())
                    })
                }
            }
        }

        Err(anyhow!(
            "failed to create a unique temporary config in {} after {TEMP_FILE_ATTEMPTS} attempts",
            dir.display()
        ))
    }

    fn write_temp(&self, tmp_path: &Path, file: &mut File) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("failed to serialize config")?;
        file.write_all(&json)
            .with_context(|| format!("failed to write temporary config {}", tmp_path.display()))?;
        // Enforce 0600 even if the process umask was unusually permissive.
        set_private_permissions(tmp_path)?;
        file.sync_all()
            .with_context(|| format!("failed to sync temporary config {}", tmp_path.display()))?;
        Ok(())
    }
}

/// Default for [`Config::auto_open`]: the UI follows a playback start.
fn default_auto_open() -> bool {
    true
}

/// Deserializes the post-port delay, mapping a missing or `null` value to zero and clamping
/// anything larger than [`MAX_OUTPUT_DELAY_MS`] instead of rejecting the whole config.
fn deserialize_output_delay<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    // A negative value is a typo, not a reason to regenerate the identity: clamp it to zero.
    let requested = Option::<i64>::deserialize(deserializer)?.unwrap_or(0);
    Ok(Config::clamp_output_delay_ms(requested.max(0) as u64))
}

/// Deserializes a string field, mapping JSON `null` to an empty string.
///
/// A missing `client_secret` (old config) or an explicit `null` is treated as invalid
/// key material by [`Config::load`] and triggers keypair migration instead of a parse
/// error. Non-string values are still rejected.
fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// Generates an X25519 keypair as `(public client_id, private client_secret)`.
///
/// [`Identity::generate`] documents a panic when the operating system's CSPRNG is
/// unavailable: a silent fallback would be a weak key, so there is nothing sensible to
/// return instead.
fn generate_keypair() -> (String, String) {
    let identity = Identity::generate();
    (identity.public_base64(), identity.secret_base64())
}

/// Restricts an existing config file to owner read/write only.
#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on config {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
