//! Foreground-app query and launch decisions for auto-open.
//!
//! webOS answers the foreground app through
//! `com.webos.applicationManager/getForegroundAppInfo` and launches an app through
//! `com.webos.applicationManager/launch`. The payload parsing, the "is our UI already visible?"
//! decision and both launch payloads (first attempt and the one retry) live here so the Luna FFI
//! module and the hermetic tests share exactly one interpretation; nothing in this module touches
//! LS2 or a TV.

use anyhow::{bail, Result};
use serde::Deserialize;

/// Application id of the bundled TV UI; also the LS2 service name and the app the leave rule
/// treats as "still here".
pub use crate::foreground::APP_ID;

#[derive(Debug, Deserialize)]
struct WireForegroundReply {
    #[serde(rename = "returnValue", default)]
    accepted: bool,
    // webOS 4 answers `appId`; some services name the same field `foregroundAppId`.
    #[serde(rename = "appId", default, alias = "foregroundAppId")]
    app_id: Option<String>,
    #[serde(rename = "errorText")]
    error_text: Option<String>,
}

/// Parses a `getForegroundAppInfo` reply.
///
/// `Ok(None)` means the TV named no foreground app (for example the launcher is visible) and
/// still warrants a launch. A rejected query is an error: a failed query must never be treated
/// as "we are already in the foreground", and it must not trigger a blind launch either.
pub fn parse_foreground_app(payload: &str) -> Result<Option<String>> {
    let reply: WireForegroundReply = serde_json::from_str(payload)?;
    if !reply.accepted {
        bail!(
            "getForegroundAppInfo rejected: {}",
            reply.error_text.as_deref().unwrap_or(payload)
        );
    }
    Ok(reply.app_id)
}

/// What [`APP_ID`]'s current foreground state asks the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundDecision {
    /// Our UI already has the foreground; launching it again would steal focus.
    AlreadyForeground,
    /// Another app (or none) is visible; launch the UI.
    Launch,
}

/// Decides whether auto-open may launch without stealing focus from our own UI.
pub fn foreground_decision(app_id: Option<&str>) -> ForegroundDecision {
    if app_id == Some(APP_ID) {
        ForegroundDecision::AlreadyForeground
    } else {
        ForegroundDecision::Launch
    }
}

/// Delay between an accepted launch and the foreground verification, in milliseconds.
///
/// The application manager accepts a launch before the app is on screen, and a launch issued
/// while the app is still starting can be swallowed entirely. Re-asking after this delay is what
/// turns that case into a retry instead of a silent failure.
pub const VERIFY_DELAY_MS: u32 = 1_500;

/// Hard upper bound for one auto-open request, in milliseconds.
///
/// The waiting Luna command acknowledges with a 5 s timeout, so this leaves room for the
/// verification query and the optional relaunch while still answering the caller first.
pub const REQUEST_DEADLINE_MS: u32 = 4_000;

/// Payload for the first `com.webos.applicationManager/launch` attempt.
///
/// Byte-identical to what every earlier build sent, so the first attempt stays the plain request.
pub fn launch_payload() -> String {
    format!("{{\"id\":\"{APP_ID}\"}}")
}

/// Payload for the single retry after a launch that did not bring the UI to the foreground.
///
/// `token` is supplied by the caller (a monotonic counter), so this function stays pure. The
/// added `params.relaunch` makes the retry a distinct request instead of a repeat of the one the
/// application manager just accepted and dropped.
pub fn retry_launch_payload(token: u64) -> String {
    format!("{{\"id\":\"{APP_ID}\",\"params\":{{\"relaunch\":{token}}}}}")
}

#[derive(Debug, Deserialize)]
struct WireLaunchReply {
    #[serde(rename = "returnValue", default)]
    accepted: bool,
    #[serde(rename = "errorText")]
    error_text: Option<String>,
}

/// Accepts a launch reply only when the application manager confirmed it.
pub fn launch_accepted(payload: &str) -> Result<()> {
    let reply: WireLaunchReply = serde_json::from_str(payload)?;
    if !reply.accepted {
        bail!(
            "launch rejected: {}",
            reply.error_text.as_deref().unwrap_or(payload)
        );
    }
    Ok(())
}
