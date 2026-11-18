//! Integration tests for the client-facing audio sink.
//!
//! `GstSink` implements `syncspin_client::Sink` over the same decoder/player children that
//! `process_test.rs` covers; the real `gst-launch-1.0` and `pacat` are replaced by the fake
//! scripts under `tests/fixtures/` so these tests stay hermetic.
//!
//! The fake scripts read process-global environment variables, so the tests here are
//! serialized behind one mutex and each one uses its own temp directory.

#![allow(dead_code)]

#[path = "../src/sink/mod.rs"]
mod sink;

#[path = "../src/visual.rs"]
mod visual;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sink::gst::{GstSink, GstSinkHandle, StreamFormat};
use syncspin_client::{AudioFrame, Codec, Sink as ClientSink};

/// Serializes tests that mutate process-global environment variables.
static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Distinguishes the per-test temp directories (all tests share one process).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Whether a process with `pid` currently exists (a reaped process has no `/proc` entry).
fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Polls until `path` exists and its bytes start with `prefix`, or `timeout` elapses.
async fn file_starts_with(path: &Path, prefix: &[u8], timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if bytes.starts_with(prefix) {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Polls until `path` holds a PID, returning it.
async fn read_pid_file(path: &Path, timeout: Duration) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                return Some(pid);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Polls until `path` holds a PID other than `previous`, returning it.
async fn read_new_pid(path: &Path, previous: u32, timeout: Duration) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if pid != previous {
                    return Some(pid);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Polls until `pid` is dead (no `/proc` entry), or `timeout` elapses.
async fn dead_within(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Unique temp dir plus the environment the fake scripts read.
struct TestEnv {
    tmp: PathBuf,
    gst_pid_file: PathBuf,
    gst_stdin: PathBuf,
    pacat_pid_file: PathBuf,
}

impl TestEnv {
    fn prepare() -> Self {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("sendspin-gst-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let env = Self {
            gst_pid_file: tmp.join("gst.pid"),
            gst_stdin: tmp.join("gst.stdin"),
            pacat_pid_file: tmp.join("pacat.pid"),
            tmp,
        };
        std::env::set_var("FAKE_GST_PID_FILE", &env.gst_pid_file);
        std::env::set_var("FAKE_GST_STDIN_FILE", &env.gst_stdin);
        std::env::set_var("FAKE_PACAT_PID_FILE", &env.pacat_pid_file);
        std::env::set_var("FAKE_PACAT_OUT_FILE", env.tmp.join("pacat.out"));
        std::env::set_var("FAKE_GST_MODE", "stdin");
        std::env::set_var("FAKE_PACAT_MODE", "idle");
        env
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// An encoded FLAC stream with the codec header Music Assistant sends.
fn flac_format() -> StreamFormat {
    StreamFormat {
        codec: Codec::Flac,
        sample_rate: 48_000,
        channels: 2,
        bit_depth: 16,
        codec_header: Some(b"fLaC\x80\x00\x00\x22".to_vec()),
    }
}

fn start() -> (GstSink, GstSinkHandle) {
    GstSink::start_with_binaries(
        &fixture("fake-gst.sh"),
        &fixture("fake-pacat.sh"),
        None,
        Arc::new(AtomicU64::new(0)),
    )
    .expect("start sink")
}

#[tokio::test]
async fn a_shared_output_delay_applies_live_and_zero_reports_none() {
    // No stream is opened, so this only covers the value the client polls: it must follow the
    // shared atomic between two state reports, and zero must stay the protocol's "no delay".
    let delay = Arc::new(AtomicU64::new(0));
    let (sink, handle) = GstSink::start_with_binaries(
        &fixture("fake-gst.sh"),
        &fixture("fake-pacat.sh"),
        None,
        Arc::clone(&delay),
    )
    .expect("start sink");

    assert_eq!(sink.output_delay(), None);
    delay.store(120, Ordering::Relaxed);
    assert_eq!(sink.output_delay(), Some(Duration::from_millis(120)));
    delay.store(500, Ordering::Relaxed);
    assert_eq!(sink.output_delay(), Some(Duration::from_millis(500)));
    delay.store(0, Ordering::Relaxed);
    assert_eq!(sink.output_delay(), None);

    handle.stop().await.expect("stop the idle pipeline");
}

#[tokio::test]
async fn open_primes_the_decoder_with_the_codec_header_before_every_frame() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare();
    let (mut sink, handle) = start();

    sink.open_stream(flac_format())
        .expect("open the flac stream");
    sink.write(
        AudioFrame::Encoded {
            codec: Codec::Flac,
            data: b"frame-1",
        },
        std::time::Instant::now(),
    )
    .expect("write the first frame");
    sink.write(
        AudioFrame::Encoded {
            codec: Codec::Flac,
            data: b"frame-2",
        },
        std::time::Instant::now(),
    )
    .expect("write the second frame");

    // The header must precede the frames, and the frames keep their wire order.
    assert!(
        file_starts_with(
            &env.gst_stdin,
            b"fLaC\x80\x00\x00\x22frame-1frame-2",
            Duration::from_secs(5)
        )
        .await,
        "the decoder received the codec header first, then the frames in order"
    );
    assert_eq!(
        sink.pre_port_delay(),
        Duration::from_millis(700),
        "pacat's 500 ms target buffer plus the slack that absorbs scheduling jitter"
    );
    assert_eq!(
        sink.output_delay(),
        None,
        "no delay beyond the audio port: the bridge is the host's own buffering, which the \
         protocol does not ask for in output_delay_ms"
    );

    handle.stop().await.expect("stop the pipeline");
    env.cleanup();
}

#[tokio::test]
async fn flush_is_a_barrier_that_keeps_the_children_alive() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare();
    let (mut sink, handle) = start();
    sink.open_stream(flac_format())
        .expect("open the flac stream");

    let gst_pid = read_pid_file(&env.gst_pid_file, Duration::from_secs(2))
        .await
        .expect("decoder recorded its pid");
    let pacat_pid = read_pid_file(&env.pacat_pid_file, Duration::from_secs(2))
        .await
        .expect("player recorded its pid");

    // The client flushes on every stream boundary. It only empties the daemon's bridge; a pause
    // stops the children instead (see the next test), because `pacat` has no flush API.
    sink.flush().expect("flush the sink");

    assert!(alive(gst_pid), "a flush must not restart the decoder");
    assert!(alive(pacat_pid), "a flush must not restart the player");

    handle.stop().await.expect("stop the pipeline");
    env.cleanup();
}

#[tokio::test]
async fn a_seek_restart_replaces_the_children_and_reprimes_the_header() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare();
    let (mut sink, handle) = start();
    sink.open_stream(flac_format())
        .expect("open the flac stream");
    let old_gst = read_pid_file(&env.gst_pid_file, Duration::from_secs(2))
        .await
        .expect("decoder recorded its pid");
    assert!(file_starts_with(&env.gst_stdin, b"fLaC", Duration::from_secs(5)).await);

    handle.restart().await.expect("restart after a seek");

    // The restarted decoder truncates its stdin file and is primed again with the header.
    assert!(
        file_starts_with(
            &env.gst_stdin,
            b"fLaC\x80\x00\x00\x22",
            Duration::from_secs(5)
        )
        .await,
        "the restarted decoder was re-primed with the cached header"
    );
    let new_gst = read_new_pid(&env.gst_pid_file, old_gst, Duration::from_secs(2))
        .await
        .expect("restarted decoder recorded a new pid");
    assert!(alive(new_gst), "the restarted decoder is alive");
    assert!(
        dead_within(old_gst, Duration::from_secs(5)).await,
        "the old decoder was reaped"
    );

    handle.stop().await.expect("stop the pipeline");
    env.cleanup();
}

#[tokio::test]
async fn stopping_the_sink_kills_both_children_and_open_starts_them_again() {
    // This is the pause path: `stream/end` stops the sink (which discards the buffered audio)
    // and the next `stream/start` opens a fresh pipeline.
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare();
    let (mut sink, handle) = start();
    sink.open_stream(flac_format())
        .expect("open the flac stream");

    let gst_pid = read_pid_file(&env.gst_pid_file, Duration::from_secs(2))
        .await
        .expect("decoder recorded its pid");
    let pacat_pid = read_pid_file(&env.pacat_pid_file, Duration::from_secs(2))
        .await
        .expect("player recorded its pid");

    handle.stop().await.expect("stop the pipeline");
    assert!(
        dead_within(gst_pid, Duration::from_secs(5)).await,
        "the decoder was stopped and reaped"
    );
    assert!(
        dead_within(pacat_pid, Duration::from_secs(5)).await,
        "the player was stopped and reaped"
    );

    // A disconnect and the next stream must be able to start a fresh pipeline.
    sink.open_stream(flac_format())
        .expect("re-open after a stop");
    let restarted_gst = read_new_pid(&env.gst_pid_file, gst_pid, Duration::from_secs(2))
        .await
        .expect("reopened decoder recorded a pid");
    assert!(alive(restarted_gst), "the reopened decoder is alive");

    handle.stop().await.expect("stop the pipeline again");
    env.cleanup();
}

#[tokio::test]
async fn raw_pcm_and_foreign_codecs_are_rejected() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare();
    let (mut sink, handle) = start();

    let error = sink
        .write(AudioFrame::Pcm(&[0, 0, 0, 0]), std::time::Instant::now())
        .expect_err("a passthrough sink cannot play raw PCM");
    assert!(format!("{error:#}").contains("raw PCM"));

    let error = sink
        .open_stream(StreamFormat {
            codec: Codec::Opus,
            sample_rate: 48_000,
            channels: 2,
            bit_depth: 16,
            codec_header: None,
        })
        .expect_err("the gst pipeline decodes FLAC only");
    assert!(format!("{error:#}").contains("FLAC only"));

    handle.stop().await.expect("stop the empty pipeline");
    env.cleanup();
}
