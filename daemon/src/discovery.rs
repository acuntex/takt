//! Finding the Sendspin server: the configured address first, mDNS as the fallback.
//!
//! Self-contained on purpose: the integration suites include this file by `#[path]`, so it must
//! not reach into `crate::`.

use std::time::Duration;

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use syncspin_client::{discover_servers, reachable_endpoint, DiscoveredServer, MdnsDiscovery};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// How long the configured address gets, name lookup included, before discovery starts.
///
/// The default config is a `.local` name, and without an mDNS NSS module `getaddrinfo` can spend
/// seconds on it. One budget covers the lookup and every connect attempt, so this constant is the
/// real deadline rather than a per-address timeout.
pub const CONFIGURED_PROBE_TIMEOUT: Duration = Duration::from_millis(700);

/// How long one advertised address gets before the next record is tried.
///
/// This daemon's budget: shorter than the crate's own 3 s `PROBE_TIMEOUT`, longer than a Wi-Fi SYN
/// retransmit - anything shorter would skip a slow but healthy record.
pub const RECORD_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How many advertised records one fallback probes.
///
/// A browse can return many records - a hostile or misconfigured publisher can return dozens -
/// and each probe costs up to [`RECORD_PROBE_TIMEOUT`]. This bounds the fallback at four seconds
/// of probing.
pub const MAX_RECORDS_PROBED: usize = 4;

/// Returns the `host:port` part of a configured server address, or `None` when it names none.
///
/// The connection uses the configured string unchanged; only the reachability probe wants the
/// authority. `ws://` is the only scheme this build accepts, the path never matters for a TCP
/// connection, and a query or fragment is not part of the authority.
pub fn authority_of(configured: &str) -> Option<&str> {
    // Only `ws://` is stripped. Another scheme such as `wss://` yields no usable authority on
    // purpose: the client's own TLS error is clearer than silently joining a different server.
    let rest = configured.strip_prefix("ws://").unwrap_or(configured);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = authority.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(authority)
}

/// Where the address the daemon dials came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Configured,
    Discovered { name: Option<String> },
}

/// The address to dial and how it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub server: String,
    pub source: Source,
}

/// The outside world the resolver needs, behind one seam so tests never touch multicast.
pub trait ServerFinder: Send + Sync {
    fn probe<'a>(&'a self, authority: &'a str) -> BoxFuture<'a, bool>;
    fn browse<'a>(&'a self) -> BoxFuture<'a, Result<Vec<DiscoveredServer>>>;
    fn probe_record<'a>(&'a self, record: &'a DiscoveredServer) -> BoxFuture<'a, Option<String>>;
}

/// Picks the address to dial: the configured one when it answers, else the first reachable record.
///
/// An address [`authority_of`] cannot parse is never a discovery trigger: a TLS URL or an empty
/// address is fatal in the client, and one without a port keeps today's default-port dialling.
/// None of them may silently become a different server.
pub async fn resolve(configured: &str, finder: &dyn ServerFinder) -> Resolution {
    let keep_configured = || Resolution {
        server: configured.to_owned(),
        source: Source::Configured,
    };
    let Some(authority) = authority_of(configured) else {
        eprintln!("[takt] server {configured} cannot be probed; keeping it as configured");
        return keep_configured();
    };
    if finder.probe(authority).await {
        return keep_configured();
    }
    eprintln!(
        "[takt] server {configured} is unreachable; looking for the Sendspin server over mDNS"
    );
    let records = match finder.browse().await {
        Ok(records) => records,
        Err(error) => {
            eprintln!("[takt] mDNS discovery failed: {error:#}; keeping {configured}");
            return keep_configured();
        }
    };
    for record in records.iter().take(MAX_RECORDS_PROBED) {
        eprintln!(
            "[takt] advertised server {:?} path={}",
            record.name.as_deref().unwrap_or("-"),
            record.path
        );
    }
    if records.len() > MAX_RECORDS_PROBED {
        eprintln!(
            "[takt] {} further advertised record(s) beyond the probe budget",
            records.len() - MAX_RECORDS_PROBED
        );
    }
    for record in records.iter().take(MAX_RECORDS_PROBED) {
        if let Some(endpoint) = finder.probe_record(record).await {
            eprintln!(
                "[takt] discovered {:?} at {endpoint}",
                record.name.as_deref().unwrap_or("-")
            );
            return Resolution {
                server: endpoint,
                source: Source::Discovered {
                    name: record.name.clone(),
                },
            };
        }
    }
    eprintln!("[takt] mDNS found no reachable Sendspin server; keeping {configured}");
    keep_configured()
}

/// Resolves to true as soon as one attempt succeeds.
///
/// `join_all` would wait for every address, so one hanging route could let the budget expire and
/// report a healthy server as unreachable.
pub(crate) async fn any_success(
    attempts: impl IntoIterator<Item = BoxFuture<'_, std::io::Result<()>>>,
) -> bool {
    let mut pending: FuturesUnordered<_> = attempts.into_iter().collect();
    while let Some(result) = pending.next().await {
        if result.is_ok() {
            return true;
        }
    }
    false
}

/// The productive finder: a TCP probe, and the crate's mDNS backend created on first use.
///
/// Lazily, because a daemon whose configured address answers never needs multicast: it should
/// neither hold a 5353 socket for its lifetime nor lose discovery for the whole process when the
/// first creation fails. A failed creation is retried on the next fallback.
pub struct LiveFinder {
    mdns: OnceLock<Arc<MdnsDiscovery>>,
}

impl LiveFinder {
    pub fn new() -> Self {
        Self {
            mdns: OnceLock::new(),
        }
    }

    fn mdns(&self) -> Result<Arc<MdnsDiscovery>> {
        if let Some(mdns) = self.mdns.get() {
            return Ok(Arc::clone(mdns));
        }
        let mdns = Arc::new(MdnsDiscovery::new()?);
        let _ = self.mdns.set(Arc::clone(&mdns));
        Ok(mdns)
    }
}

impl Default for LiveFinder {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerFinder for LiveFinder {
    fn probe<'a>(&'a self, authority: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // One deadline over lookup *and* connects: the default config is a `.local` name and
            // its lookup is the slow part. Every address is dialled at once and the first success
            // answers, so a dead route cannot eat the budget a working one needs.
            let attempt = async {
                let Ok(addresses) = tokio::net::lookup_host(authority).await else {
                    return false;
                };
                let attempts = addresses.map(|address| {
                    Box::pin(async move { TcpStream::connect(address).await.map(|_| ()) })
                        as BoxFuture<'static, std::io::Result<()>>
                });
                any_success(attempts).await
            };
            timeout(CONFIGURED_PROBE_TIMEOUT, attempt)
                .await
                .unwrap_or(false)
        })
    }

    fn browse<'a>(&'a self) -> BoxFuture<'a, Result<Vec<DiscoveredServer>>> {
        Box::pin(async move {
            let mdns = self.mdns()?;
            // The crate's browse blocks for its one-second window; running it here would hold a
            // runtime worker and delay playback events.
            tokio::task::spawn_blocking(move || discover_servers(mdns.as_ref()))
                .await
                .map_err(|error| anyhow::anyhow!("discovery task failed: {error}"))?
        })
    }

    fn probe_record<'a>(&'a self, record: &'a DiscoveredServer) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { reachable_endpoint(record, RECORD_PROBE_TIMEOUT).await.ok() })
    }
}
