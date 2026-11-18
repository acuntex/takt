//! Backend-independent screensaver-veto registration.
//!
//! The veto is an LS2 subscription: holding that subscription is its real lifetime. Re-registering
//! every 60 seconds is therefore an idempotent, harmless renewal. It covers only screensaver /
//! panel-off, **not** the TV's idle-to-standby path, and never wakes the TV from standby.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize)]
struct WireVetoReply {
    #[serde(rename = "returnValue", default)]
    accepted: bool,
    #[serde(default)]
    subscribed: bool,
    #[serde(rename = "errorText")]
    error_text: Option<String>,
}

/// Recognises an incoming screensaver request that requires an explicit `ack=false` response.
///
/// The TV answers the initial `registerScreenSaverRequest` subscription with `subscribed:true`,
/// but later pushes a request (`state:"Active"`, or `returnValue:true` with a timestamp and no
/// `subscribed:true`) that must be answered on `responseScreenSaverRequest`.
pub(crate) fn veto_request_timestamp(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let timestamp = value.get("timestamp")?.as_str()?.to_owned();
    let active_state = value.get("state").and_then(|value| value.as_str()) == Some("Active");
    let return_value = value.get("returnValue").and_then(|value| value.as_bool()) == Some(true);
    let subscribed = value.get("subscribed").and_then(|value| value.as_bool()) == Some(true);
    if active_state || (return_value && !subscribed) {
        Some(timestamp)
    } else {
        None
    }
}

/// Exact `responseScreenSaverRequest` body: refuse the request for `client_name`.
pub(crate) fn response_screen_saver_payload(client_name: &str, timestamp: &str) -> String {
    format!(
        "{{\"clientName\":{},\"ack\":false,\"timestamp\":{}}}",
        serde_json::to_string(client_name).expect("client name serializes"),
        serde_json::to_string(timestamp).expect("timestamp serializes"),
    )
}

/// A veto exists only when LS2 confirms both the call and the held subscription.
pub(crate) fn veto_reply_accepted(payload: &str) -> Result<bool> {
    let reply: WireVetoReply = serde_json::from_str(payload)?;
    // On this webOS 4 TV an idempotent renewal answers `returnValue:false` with this exact
    // message. It confirms that the original held subscription still exists; it is not a release.
    Ok((reply.accepted && reply.subscribed)
        || reply.error_text.as_deref() == Some("The client is already registered"))
}

/// How often `main` harmlessly re-registers the held veto subscription.
pub const VETO_RENEWAL_INTERVAL: Duration = Duration::from_secs(60);

/// Small transport seam implemented by the dedicated Luna worker and hermetic test fakes.
pub trait TvPowerBackend: Send + Sync {
    fn register_screen_saver_veto(&self, timestamp_secs: u64) -> Result<()>;
}

/// Registers the screensaver veto for `com.github.acuntex.takt` through its backend.
pub struct TvPower<B: TvPowerBackend> {
    backend: B,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl<B: TvPowerBackend> TvPower<B> {
    pub fn new(backend: B) -> Self {
        Self::with_clock(backend, || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0)
        })
    }

    pub fn with_clock(backend: B, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            backend,
            clock: Arc::new(clock),
        }
    }

    /// Registers/holds the subscription. Calling this again for renewal has the same bus effect.
    pub fn keep_awake(&self) -> Result<()> {
        self.backend.register_screen_saver_veto((self.clock)())
    }
}
