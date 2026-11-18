//! Hermetic tests for mDNS server discovery: the resolver runs against a fake finder, so no
//! multicast query leaves the process.

#![allow(dead_code)]

#[path = "../src/discovery.rs"]
mod discovery;

use std::time::Duration;

use discovery::{authority_of, CONFIGURED_PROBE_TIMEOUT, MAX_RECORDS_PROBED, RECORD_PROBE_TIMEOUT};

#[test]
fn a_bare_host_and_port_is_its_own_authority() {
    assert_eq!(authority_of("192.0.2.10:8927"), Some("192.0.2.10:8927"));
}

#[test]
fn a_websocket_url_is_probed_by_its_authority() {
    assert_eq!(
        authority_of("ws://192.0.2.10:8927"),
        Some("192.0.2.10:8927")
    );
    assert_eq!(
        authority_of("ws://192.0.2.10:8927/custom/path"),
        Some("192.0.2.10:8927")
    );
    assert_eq!(
        authority_of("ws://192.0.2.10:8927?token=1#frag"),
        Some("192.0.2.10:8927")
    );
}

#[test]
fn a_host_without_a_usable_port_has_no_authority() {
    assert_eq!(authority_of("music-assistant.local"), None);
    assert_eq!(authority_of("ws://192.0.2.10/sendspin"), None);
    assert_eq!(authority_of("192.0.2.10:"), None);
    assert_eq!(authority_of("ws://"), None);
    assert_eq!(authority_of(""), None);
    assert_eq!(
        authority_of("wss://192.0.2.10:8927"),
        None,
        "TLS is not this build's scheme; discovery must not stand in for it"
    );
}

#[test]
fn the_budgets_are_the_documented_values() {
    assert_eq!(CONFIGURED_PROBE_TIMEOUT, Duration::from_millis(700));
    assert_eq!(RECORD_PROBE_TIMEOUT, Duration::from_secs(1));
    assert_eq!(MAX_RECORDS_PROBED, 4);
}

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use discovery::{resolve, Resolution, ServerFinder, Source};
use futures_util::future::BoxFuture;
use syncspin_client::DiscoveredServer;

/// A finder that answers from maps instead of the network and counts what it was asked.
#[derive(Default)]
struct FakeFinder {
    reachable: HashSet<String>,
    records: Vec<DiscoveredServer>,
    browse_error: Option<String>,
    browses: AtomicUsize,
    record_probes: AtomicUsize,
    probed: Mutex<Vec<String>>,
}

impl FakeFinder {
    fn with_reachable(mut self, addresses: &[&str]) -> Self {
        self.reachable = addresses
            .iter()
            .map(|address| (*address).to_owned())
            .collect();
        self
    }

    fn with_record(mut self, name: &str, address: &str) -> Self {
        self.records.push(DiscoveredServer {
            addresses: vec![address.to_owned()],
            path: "/sendspin".to_owned(),
            name: Some(name.to_owned()),
        });
        self
    }
}

impl ServerFinder for FakeFinder {
    fn probe<'a>(&'a self, authority: &'a str) -> BoxFuture<'a, bool> {
        self.probed
            .lock()
            .expect("probe log")
            .push(authority.to_owned());
        let reachable = self.reachable.contains(authority);
        Box::pin(async move { reachable })
    }

    fn browse<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<DiscoveredServer>>> {
        self.browses.fetch_add(1, Ordering::Relaxed);
        let records = self.records.clone();
        let error = self.browse_error.clone();
        Box::pin(async move {
            match error {
                Some(error) => Err(anyhow::anyhow!(error)),
                None => Ok(records),
            }
        })
    }

    fn probe_record<'a>(&'a self, record: &'a DiscoveredServer) -> BoxFuture<'a, Option<String>> {
        self.record_probes.fetch_add(1, Ordering::Relaxed);
        let answer = record.endpoints().into_iter().find(|endpoint| {
            endpoint
                .strip_prefix("ws://")
                .and_then(|rest| rest.split('/').next())
                .is_some_and(|authority| self.reachable.contains(authority))
        });
        Box::pin(async move { answer })
    }
}

#[tokio::test]
async fn a_reachable_configured_server_is_used_without_browsing() {
    let finder = FakeFinder::default().with_reachable(&["192.0.2.10:8927"]);
    let resolution = resolve("192.0.2.10:8927", &finder).await;
    assert_eq!(
        resolution,
        Resolution {
            server: "192.0.2.10:8927".to_owned(),
            source: Source::Configured,
        }
    );
    assert_eq!(
        finder.browses.load(Ordering::Relaxed),
        0,
        "no browse when the address answers"
    );
}

#[tokio::test]
async fn an_unreachable_configured_server_falls_back_to_the_advertised_one() {
    let finder = FakeFinder::default()
        .with_reachable(&["192.0.2.20:8927"])
        .with_record("Music Assistant", "192.0.2.20:8927");
    let resolution = resolve("192.0.2.10:8927", &finder).await;
    assert_eq!(resolution.server, "ws://192.0.2.20:8927/sendspin");
    assert_eq!(
        resolution.source,
        Source::Discovered {
            name: Some("Music Assistant".to_owned())
        }
    );
    assert_eq!(finder.browses.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn the_first_reachable_record_wins() {
    let finder = FakeFinder::default()
        .with_reachable(&["192.0.2.21:8927"])
        .with_record("Stale", "192.0.2.20:8927")
        .with_record("Live", "192.0.2.21:8927");
    let resolution = resolve("192.0.2.10:8927", &finder).await;
    assert_eq!(resolution.server, "ws://192.0.2.21:8927/sendspin");
    assert_eq!(
        resolution.source,
        Source::Discovered {
            name: Some("Live".to_owned())
        },
        "records with no reachable address are skipped, not chosen"
    );
}

#[tokio::test]
async fn at_most_four_records_are_probed() {
    let mut finder = FakeFinder::default();
    for index in 1..=5 {
        finder = finder.with_record(&format!("Server {index}"), &format!("192.0.2.{index}:8927"));
    }
    let finder = finder.with_reachable(&["192.0.2.5:8927"]);
    let resolution = resolve("192.0.2.10:8927", &finder).await;
    assert_eq!(
        resolution.source,
        Source::Configured,
        "the fifth record is beyond the probe budget"
    );
    assert_eq!(
        finder.record_probes.load(Ordering::Relaxed),
        MAX_RECORDS_PROBED
    );
}

#[tokio::test]
async fn no_advertised_server_keeps_the_configured_address() {
    let finder = FakeFinder::default();
    let resolution = resolve("192.0.2.10:8927", &finder).await;
    assert_eq!(
        resolution,
        Resolution {
            server: "192.0.2.10:8927".to_owned(),
            source: Source::Configured,
        }
    );
    assert_eq!(finder.browses.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_browse_error_keeps_the_configured_address() {
    let finder = FakeFinder {
        browse_error: Some("multicast unavailable".to_owned()),
        ..Default::default()
    };
    let resolution = resolve("192.0.2.10:8927", &finder).await;
    assert_eq!(resolution.server, "192.0.2.10:8927");
    assert_eq!(resolution.source, Source::Configured);
}

#[tokio::test]
async fn an_address_that_cannot_be_probed_keeps_the_configured_value() {
    let finder = FakeFinder::default()
        .with_reachable(&["192.0.2.20:8927"])
        .with_record("Music Assistant", "192.0.2.20:8927");
    let resolution = resolve("wss://192.0.2.10:8927", &finder).await;
    assert_eq!(
        resolution,
        Resolution {
            server: "wss://192.0.2.10:8927".to_owned(),
            source: Source::Configured,
        },
        "the client must report its own TLS error instead of joining a different server"
    );
    assert_eq!(
        finder.browses.load(Ordering::Relaxed),
        0,
        "an unparseable address is not a discovery trigger"
    );
}

#[tokio::test]
async fn a_websocket_url_is_probed_by_its_authority_only() {
    let finder = FakeFinder::default().with_reachable(&["192.0.2.10:8927"]);
    let resolution = resolve("ws://192.0.2.10:8927/custom/path", &finder).await;
    assert_eq!(
        resolution.server, "ws://192.0.2.10:8927/custom/path",
        "the connection keeps the whole URL, path included"
    );
    assert_eq!(
        finder.probed.lock().expect("probe log").as_slice(),
        ["192.0.2.10:8927"]
    );
}

use discovery::LiveFinder;
use tokio::net::TcpListener;

#[tokio::test]
async fn a_success_counts_even_while_another_attempt_hangs() {
    // The probe's budget exists to bound silence, not to wait for every address: one hanging route
    // must not turn a healthy address into "unreachable".
    let success: BoxFuture<'static, std::io::Result<()>> = Box::pin(async { Ok(()) });
    let hanging: BoxFuture<'static, std::io::Result<()>> =
        Box::pin(std::future::pending::<std::io::Result<()>>());

    assert!(discovery::any_success([hanging, success]).await);
}

#[tokio::test]
async fn the_live_probe_accepts_a_listening_port_and_refuses_a_closed_one() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local address").to_string();
    let finder = LiveFinder::new();

    assert!(
        finder.probe(&address).await,
        "a listening loopback port is reachable"
    );
    drop(listener);
    assert!(
        !finder.probe(&address).await,
        "a closed loopback port is not"
    );
}
