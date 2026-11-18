//! Integration tests for persisted configuration (Task 3 + native Noise identity).
//!
//! The daemon is a binary-only crate, so the module is included here via `#[path]`
//! instead of adding a library target just for tests.

#[path = "../src/config.rs"]
mod config;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use config::Config;
use serde_json::{json, Value};

/// Temporary directory that is removed when the test finishes.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "takt-config-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Decodes a canonical unpadded base64url encoding of exactly 32 bytes.
fn decode_key(value: &str) -> Option<[u8; 32]> {
    if value.len() != 43 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != value {
        return None;
    }
    bytes.try_into().ok()
}

fn assert_key(value: &str, label: &str) {
    assert!(
        decode_key(value).is_some(),
        "{label} must be a canonical 43-character base64url encoding of 32 bytes, got {value:?}"
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read config file")).expect("valid JSON")
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .expect("config metadata")
        .permissions()
        .mode()
        & 0o777
}

#[cfg(unix)]
fn assert_mode_0600(path: &Path) {
    assert_eq!(
        file_mode(path),
        0o600,
        "config {} must be private (0600)",
        path.display()
    );
}

#[test]
fn missing_file_creates_defaults_with_a_stable_keypair() {
    let dir = TempDir::new("defaults");
    let path = dir.path().join("config.json");
    assert!(!path.exists());

    let first = Config::load(&path).expect("load creates defaults for a missing file");

    assert_eq!(first.server, "music-assistant.local:8927");
    assert_eq!(first.player_name, "Takt");
    assert_key(&first.client_id, "client_id");
    assert_key(&first.client_secret, "client_secret");
    assert!(
        first.identity().is_some(),
        "a missing identity file must leave a usable identity"
    );
    assert_eq!(first.output_delay_ms, 0, "the delay defaults to zero");
    assert!(path.is_file(), "load must persist the defaults");
    #[cfg(unix)]
    assert_mode_0600(&path);

    // A second load must return the persisted pair, not generate a new identity.
    let second = Config::load(&path).expect("load existing config");
    assert_eq!(second.client_id, first.client_id);
    assert_eq!(second.client_secret, first.client_secret);
    assert_eq!(second.server, first.server);
    assert_eq!(second.player_name, first.player_name);

    // The on-disk JSON has exactly the six public fields, never the source path.
    let object = read_json(&path);
    let object = object.as_object().expect("config is a JSON object");
    assert_eq!(object.len(), 6, "unexpected fields in serialized config");
    assert!(object.contains_key("server"));
    assert!(object.contains_key("player_name"));
    assert!(object.contains_key("client_id"));
    assert!(object.contains_key("client_secret"));
    assert_eq!(object["auto_open"], true);
    assert_eq!(object["output_delay_ms"], 0);
    assert!(!object.contains_key("path"));

    // Atomic save must not leave a temp file behind.
    assert_eq!(
        fs::read_dir(dir.path()).expect("read temp dir").count(),
        1,
        "only config.json should remain after save"
    );
}

#[test]
fn save_persists_changes_and_keeps_the_keypair() {
    let dir = TempDir::new("save");
    let path = dir.path().join("config.json");

    let mut config = Config::load(&path).expect("load creates defaults");
    let client_id = config.client_id.clone();
    let client_secret = config.client_secret.clone();

    config.server = "192.0.2.10:8927".to_owned();
    config.player_name = "Living Room".to_owned();
    config.save().expect("save updated config");

    let reloaded = Config::load(&path).expect("load saved config");
    assert_eq!(reloaded.server, "192.0.2.10:8927");
    assert_eq!(reloaded.player_name, "Living Room");
    assert_eq!(reloaded.client_id, client_id);
    assert_eq!(reloaded.client_secret, client_secret);
    let json = read_json(&path);
    assert_eq!(json["client_secret"], client_secret);

    assert_eq!(
        fs::read_dir(dir.path()).expect("read temp dir").count(),
        1,
        "only config.json should remain after save"
    );
}

#[test]
fn auto_open_defaults_on_and_an_explicit_false_survives_a_reload() {
    let dir = TempDir::new("auto-open");
    let path = dir.path().join("config.json");

    let config = Config::load(&path).expect("load creates defaults");
    assert!(config.auto_open, "auto-open must default to on");
    assert_eq!(read_json(&path)["auto_open"], true);

    let mut config = config;
    config.auto_open = false;
    config.save().expect("save auto-open off");

    let reloaded = Config::load(&path).expect("load saved config");
    assert!(!reloaded.auto_open, "an explicit false must be persisted");
    assert_eq!(read_json(&path)["auto_open"], false);
}

#[test]
fn output_delay_round_trips_and_is_clamped_on_load_and_save() {
    let dir = TempDir::new("output-delay");
    let path = dir.path().join("config.json");

    let mut config = Config::load(&path).expect("load creates defaults");
    assert_eq!(config.output_delay_ms, 0);
    assert_eq!(read_json(&path)["output_delay_ms"], 0);

    for (stored, expected) in [
        (-10_i64, 0_u16),
        (0, 0),
        (200, 200),
        (500, 500),
        (5_000, 5_000),
        (5_001, 5_000),
    ] {
        let mut document = read_json(&path);
        document["output_delay_ms"] = json!(stored);
        fs::write(&path, document.to_string()).expect("write config with delay");
        let loaded = Config::load(&path).expect("load delay config");
        assert_eq!(loaded.output_delay_ms, expected, "stored {stored}");
    }

    // Save is the second gate: an in-memory value above the bound cannot reach the file.
    config.output_delay_ms = 5_001;
    config.save().expect("save clamps the delay");
    assert_eq!(config.output_delay_ms, 5_000);
    assert_eq!(read_json(&path)["output_delay_ms"], 5_000);
    assert_eq!(Config::load(&path).expect("reload").output_delay_ms, 5_000);

    assert_eq!(Config::clamp_output_delay_ms(0), 0);
    assert_eq!(Config::clamp_output_delay_ms(200), 200);
    assert_eq!(Config::clamp_output_delay_ms(500), 500);
    assert_eq!(Config::clamp_output_delay_ms(5_000), 5_000);
    assert_eq!(Config::clamp_output_delay_ms(5_001), 5_000);
    assert_eq!(Config::clamp_output_delay_ms(10_000), 5_000);
}

#[test]
fn a_server_chosen_delay_merges_with_the_file_on_disk() {
    let dir = TempDir::new("store-delay");
    let path = dir.path().join("config.json");

    // Two writers hold snapshots of the same file: the daemon's (older) and the HTTP API's.
    let mut daemon = Config::load(&path).expect("load creates defaults");
    let mut api = Config::load(&path).expect("second load sees the same file");
    api.player_name = "Renamed in the UI".to_owned();
    api.save().expect("API write");

    let applied = daemon
        .store_output_delay(120)
        .expect("the server's delay is stored");
    assert_eq!(applied, 120);
    assert_eq!(daemon.output_delay_ms, 120);

    let stored = Config::load(&path).expect("reload");
    assert_eq!(stored.output_delay_ms, 120, "the delay reached the file");
    assert_eq!(
        stored.player_name, "Renamed in the UI",
        "writing the delay must not undo a name saved in the meantime"
    );
    assert_eq!(
        stored.client_id, daemon.client_id,
        "the identity is untouched by a delay write"
    );

    // The protocol's ceiling is the daemon's ceiling: nothing is capped below what a server may
    // send, and beyond the range the stored value is the clamped one the sink reports.
    assert_eq!(
        daemon.store_output_delay(5_001).expect("clamped store"),
        5_000
    );
    assert_eq!(Config::load(&path).expect("reload").output_delay_ms, 5_000);
}

#[test]
fn a_delay_write_fails_instead_of_replacing_an_unreadable_file() {
    let dir = TempDir::new("store-delay-broken");
    let path = dir.path().join("config.json");

    let mut config = Config::load(&path).expect("load creates defaults");
    let identity = config.client_id.clone();
    fs::write(&path, "{ not json").expect("break the file");

    assert!(
        config.store_output_delay(80).is_err(),
        "an unreadable file is an error, not a reason to regenerate the identity"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("file still there"),
        "{ not json",
        "the broken file is left for `load` to repair at the next start"
    );
    assert_eq!(config.client_id, identity, "the running identity is kept");
}

#[test]
fn corrupt_identity_file_is_replaced_instead_of_blocking_startup() {
    let dir = TempDir::new("corrupt");
    let path = dir.path().join("config.json");
    let broken = br#"{"server": "192.0.2.10:8927", "player_name": "TV", "client_id": "#;
    fs::write(&path, broken).expect("write corrupt config");

    // A corrupt identity file must not stop the daemon: it logs one line, generates a keypair,
    // persists the replacement and hands back a usable configuration.
    let config = Config::load(&path).expect("a corrupt identity file must be repaired");
    assert!(
        config.identity().is_some(),
        "the repaired config must carry a matching keypair"
    );
    assert_key(&config.client_id, "regenerated client_id");
    assert_key(&config.client_secret, "regenerated client_secret");
    assert_eq!(config.server, "music-assistant.local:8927");
    assert_eq!(config.player_name, "Takt");
    assert_ne!(
        fs::read(&path).expect("read repaired config"),
        broken,
        "the corrupt bytes must be replaced"
    );
    let json = read_json(&path);
    assert_eq!(json["client_id"], config.client_id);
    assert_eq!(json["client_secret"], config.client_secret);
    assert_eq!(json.as_object().expect("object").len(), 6);
    #[cfg(unix)]
    assert_mode_0600(&path);
    assert_eq!(
        fs::read_dir(dir.path()).expect("read temp dir").count(),
        1,
        "the repair must not leave a temp file behind"
    );

    // The repair happens once: the newly generated identity is not rotated on the next start.
    let reloaded = Config::load(&path).expect("load repaired config");
    assert_eq!(reloaded.client_id, config.client_id);
    assert_eq!(reloaded.client_secret, config.client_secret);
}

#[test]
fn a_valid_identity_is_never_rotated() {
    let dir = TempDir::new("stable-identity");
    let path = dir.path().join("config.json");

    let original = Config::load(&path).expect("load creates defaults");
    let identity = original.identity().expect("valid identity");

    // Repeated loads, an unrelated save, and a loosened file mode must all keep the keypair.
    for _ in 0..3 {
        let loaded = Config::load(&path).expect("load existing config");
        assert_eq!(loaded.client_id, original.client_id);
        assert_eq!(loaded.client_secret, original.client_secret);
        assert_eq!(
            loaded.identity().expect("identity").public_base64(),
            identity.public_base64()
        );
    }

    let mut changed = Config::load(&path).expect("load existing config");
    changed.server = "198.51.100.20:9000".to_owned();
    changed.player_name = "Bedroom".to_owned();
    changed.save().expect("save unrelated change");

    let reloaded = Config::load(&path).expect("load saved config");
    assert_eq!(reloaded.client_id, original.client_id);
    assert_eq!(reloaded.client_secret, original.client_secret);
}

#[test]
fn old_32_hex_client_id_is_migrated_to_a_keypair() {
    let dir = TempDir::new("migration");
    let path = dir.path().join("config.json");
    let old_id = "0123456789abcdef0123456789abcdef";
    let contents =
        format!(r#"{{"server":"192.0.2.10:8927","player_name":"TV","client_id":"{old_id}"}}"#);
    fs::write(&path, &contents).expect("write legacy config");

    let config = Config::load(&path).expect("legacy config migrates instead of failing");

    assert_ne!(config.client_id, old_id);
    assert_key(&config.client_id, "migrated client_id");
    assert_key(&config.client_secret, "migrated client_secret");
    assert_eq!(config.server, "192.0.2.10:8927");
    assert_eq!(config.player_name, "TV");

    let json = read_json(&path);
    assert_eq!(json["client_id"], config.client_id);
    assert_eq!(json["client_secret"], config.client_secret);
    assert_eq!(json.as_object().expect("object").len(), 6);
    #[cfg(unix)]
    assert_mode_0600(&path);

    // Migration is stable: a second load keeps the newly generated identity.
    let reloaded = Config::load(&path).expect("load migrated config");
    assert_eq!(reloaded.client_id, config.client_id);
    assert_eq!(reloaded.client_secret, config.client_secret);
    assert_eq!(
        fs::read_dir(dir.path()).expect("read temp dir").count(),
        1,
        "only config.json should remain after migration"
    );
}

#[test]
fn missing_or_invalid_key_material_is_regenerated() {
    let dir = TempDir::new("invalid-material");
    let valid_id = URL_SAFE_NO_PAD.encode([7u8; 32]);
    let cases: [(&str, String, Option<&str>); 4] = [
        (
            "old-hex",
            "0123456789ABCDEF0123456789ABCDEF".to_owned(),
            None,
        ),
        ("missing-secret", valid_id.clone(), None),
        ("short-secret", valid_id.clone(), Some("too-short")),
        (
            "non-base64-secret",
            valid_id.clone(),
            Some("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"),
        ),
    ];

    for (index, (label, client_id, client_secret)) in cases.into_iter().enumerate() {
        let path = dir.path().join(format!("{label}.json"));
        let mut document = json!({
            "server": "192.0.2.10:8927",
            "player_name": "TV",
            "client_id": client_id,
        });
        if let Some(secret) = client_secret {
            document["client_secret"] = json!(secret);
        }
        fs::write(&path, document.to_string()).expect("write config with invalid material");

        let config = Config::load(&path).expect("invalid key material must be regenerated");
        assert_ne!(
            config.client_id, client_id,
            "invalid material must replace the client_id ({label})"
        );
        assert_key(&config.client_id, "regenerated client_id");
        assert_key(&config.client_secret, "regenerated client_secret");
        assert_eq!(
            config.server, "192.0.2.10:8927",
            "server preserved ({label})"
        );
        assert_eq!(config.player_name, "TV", "player_name preserved ({label})");

        let json = read_json(&path);
        assert_eq!(json["client_id"], config.client_id);
        assert_eq!(json["client_secret"], config.client_secret);
        assert_eq!(json.as_object().expect("object").len(), 6);
        #[cfg(unix)]
        assert_mode_0600(&path);
        assert_eq!(
            fs::read_dir(dir.path()).expect("read temp dir").count(),
            index + 1,
            "failed migration must not leave a temp file behind ({label})"
        );
    }
}

#[test]
fn individually_valid_but_mismatched_keypair_is_regenerated() {
    let dir = TempDir::new("mismatched-pair");
    let first = Config::load(dir.path().join("first.json")).expect("load first keypair");
    let second = Config::load(dir.path().join("second.json")).expect("load second keypair");
    assert_ne!(first.client_id, second.client_id);
    assert_ne!(first.client_secret, second.client_secret);

    // Mix the public key from one valid keypair with the secret from another one.
    let path = dir.path().join("config.json");
    let document = json!({
        "server": "192.0.2.10:8927",
        "player_name": "Kitchen",
        "client_id": first.client_id,
        "client_secret": second.client_secret,
    });
    fs::write(&path, document.to_string()).expect("write mismatched config");

    // Both fields are canonical 32-byte keys on their own; only the derivation catches
    // that they belong to different keypairs.
    assert_key(first.client_id.as_str(), "mixed client_id");
    assert_key(second.client_secret.as_str(), "mixed client_secret");

    let config = Config::load(&path).expect("mismatched keypair must be regenerated");
    assert_ne!(
        config.client_id, first.client_id,
        "mismatched public key must be replaced"
    );
    assert_key(&config.client_id, "regenerated client_id");
    assert_key(&config.client_secret, "regenerated client_secret");
    assert_eq!(config.server, "192.0.2.10:8927");
    assert_eq!(config.player_name, "Kitchen");

    let json = read_json(&path);
    assert_eq!(json["client_id"], config.client_id);
    assert_eq!(json["client_secret"], config.client_secret);
    assert_eq!(json.as_object().expect("object").len(), 6);

    // Regeneration is persisted and stable: a second load keeps the new identity.
    let reloaded = Config::load(&path).expect("load regenerated config");
    assert_eq!(reloaded.client_id, config.client_id);
    assert_eq!(reloaded.client_secret, config.client_secret);
    assert_eq!(
        fs::read_dir(dir.path()).expect("read temp dir").count(),
        3,
        "only the two keypair sources and config.json should remain"
    );
}

#[test]
fn debug_output_redacts_the_client_secret() {
    let dir = TempDir::new("debug");
    let config = Config::load(dir.path().join("config.json")).expect("load defaults");

    let debug = format!("{config:?}");
    assert!(
        debug.contains("<redacted>"),
        "debug output should mark the secret as redacted, got: {debug}"
    );
    assert!(
        !debug.contains(&config.client_secret),
        "debug output must not contain the client secret"
    );
    assert!(
        debug.contains(&config.client_id),
        "public key should remain"
    );
    assert!(debug.contains(&config.server), "server should remain");
    assert!(
        debug.contains(&config.player_name),
        "player_name should remain"
    );
    assert!(
        debug.contains("output_delay_ms"),
        "the output delay should be visible in the debug output"
    );
}

#[cfg(unix)]
#[test]
fn an_existing_0644_config_is_tightened_on_save_and_load() {
    let dir = TempDir::new("permissions");
    let path = dir.path().join("config.json");
    let mut config = Config::load(&path).expect("load creates defaults");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("loosen mode");
    assert_eq!(file_mode(&path), 0o644);

    // Loading a valid config repairs the mode without changing the keypair.
    let loaded = Config::load(&path).expect("load valid config");
    assert_eq!(loaded.client_id, config.client_id);
    assert_eq!(loaded.client_secret, config.client_secret);
    assert_mode_0600(&path);

    // Saving after an external chmod also leaves the file private.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("loosen mode");
    config.server = "203.0.113.5:9000".to_owned();
    config.save().expect("save config");
    assert_mode_0600(&path);
}

#[test]
fn concurrent_saves_succeed_and_leave_valid_json() {
    let dir = TempDir::new("concurrent");
    let path = dir.path().join("config.json");
    let base = Config::load(&path).expect("load creates defaults");

    for round in 0..20 {
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for writer in 0..2 {
            let mut config = base.clone();
            config.server = format!("192.0.2.{round}:{}", 9000 + writer);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                config.save()
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("concurrent save thread panicked")
                .expect("concurrent save must succeed");
        }

        let json = read_json(&path);
        assert_eq!(
            json["client_id"].as_str(),
            Some(base.client_id.as_str()),
            "client_id changed after concurrent saves (round {round})"
        );
        assert_eq!(
            json["client_secret"].as_str(),
            Some(base.client_secret.as_str()),
            "client_secret changed after concurrent saves (round {round})"
        );
        assert_eq!(
            fs::read_dir(dir.path()).expect("read temp dir").count(),
            1,
            "temp file left behind after concurrent save (round {round})"
        );
    }
}

#[test]
fn unreadable_or_unwritable_path_returns_an_error() {
    let dir = TempDir::new("io-error");
    // The parent directory does not exist, so defaults cannot be persisted.
    let path = dir.path().join("missing-parent").join("config.json");

    let err = Config::load(&path).expect_err("load must fail when defaults cannot be saved");
    let message = format!("{err:#}");
    assert!(
        message.contains("config.json"),
        "error should name the config path, got: {message}"
    );
    assert!(
        !dir.path().join("missing-parent").exists(),
        "load must not create a partial directory tree"
    );
}
