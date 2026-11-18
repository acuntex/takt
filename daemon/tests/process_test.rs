//! Integration tests for decoder/player child-process supervision (Task 6).
//!
//! The daemon is a binary-only crate, so the sink module is included via `#[path]`. The
//! real `gst-launch-1.0` and `pacat` are replaced by fake shell scripts under
//! `tests/fixtures/` so the tests are hermetic and never touch the host audio stack.
//!
//! The fake scripts read their output/PID paths and behavior mode from process-global
//! environment variables, so all tests here are serialized behind a single mutex.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[allow(dead_code)]
#[path = "../src/sink/mod.rs"]
mod sink;

#[path = "../src/visual.rs"]
mod visual;

use sink::process::{Sink, SinkError};
use visual::{AudioVisual, SharedAudioVisual};

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

/// Whether a process with `pid` currently exists (Linux `/proc`; a reaped process has
/// no entry, so this also rules out zombies).
fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Polls until `path` exists and contains `needle`, or `timeout` elapses.
async fn file_contains(path: &Path, needle: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if String::from_utf8_lossy(&bytes).contains(needle) {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Polls until `path` exists and holds a PID, returning it.
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

/// Polls until the sink reports `expected`, or `timeout` elapses.
async fn failure_becomes(sink: &Sink, expected: SinkError, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if sink.failure() == Some(expected.clone()) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
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

/// Polls until `sink.buffered_bytes()` is at least `min_bytes`, or `timeout` elapses.
///
/// Each `buffered_bytes()` call is bounded by a short timeout so that if a broken
/// implementation holds the bridge lock across a blocked player `write_all`, this helper
/// returns `false` on timeout instead of hanging the test suite.
async fn buffered_at_least(sink: &Sink, min_bytes: usize, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(len) =
            tokio::time::timeout(Duration::from_millis(100), sink.buffered_bytes()).await
        {
            if len >= min_bytes {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Polls until `path` exists and has at least `min_bytes` bytes, or `timeout` elapses.
async fn file_len_at_least(path: &Path, min_bytes: u64, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() >= min_bytes {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sets up a unique temp dir and the environment the fake scripts read.
struct TestEnv {
    tmp: PathBuf,
    gst_pid_file: PathBuf,
    gst_pcm: PathBuf,
    pacat_pid_file: PathBuf,
    pacat_out: PathBuf,
    pacat_gate: PathBuf,
    marker: String,
}

impl TestEnv {
    fn prepare(gst_mode: &str, pacat_mode: &str) -> Self {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("sendspin-sink-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let env = Self {
            gst_pid_file: tmp.join("gst.pid"),
            gst_pcm: tmp.join("gst.pcm"),
            pacat_pid_file: tmp.join("pacat.pid"),
            pacat_out: tmp.join("pacat.out"),
            pacat_gate: tmp.join("pacat.gate"),
            marker: format!("SENDSPIN-PCM-{:08x}-{n}", std::process::id()),
            tmp,
        };
        std::env::set_var("FAKE_GST_PID_FILE", &env.gst_pid_file);
        std::env::set_var("FAKE_GST_PCM_FILE", &env.gst_pcm);
        std::env::set_var("FAKE_PACAT_PID_FILE", &env.pacat_pid_file);
        std::env::set_var("FAKE_PACAT_OUT_FILE", &env.pacat_out);
        std::env::set_var("FAKE_PACAT_GATE_FILE", &env.pacat_gate);
        std::env::set_var("FAKE_GST_MARKER", &env.marker);
        std::env::set_var("FAKE_GST_MODE", gst_mode);
        std::env::set_var("FAKE_PACAT_MODE", pacat_mode);
        std::env::set_var("FAKE_GST_BURST_BYTES", "365000");
        env
    }

    fn release_pacat(&self) {
        std::fs::write(&self.pacat_gate, b"go").expect("create pacat gate file");
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

#[tokio::test]
async fn sink_supervises_decoder_and_player_processes() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare("stream", "cat");

    // ---- start both children ----
    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");
    let gst_pid = sink.gst_pid().expect("decoder pid");
    let pacat_pid = sink.pacat_pid().expect("player pid");
    assert!(alive(gst_pid), "decoder {gst_pid} is alive");
    assert!(alive(pacat_pid), "player {pacat_pid} is alive");

    // The fake scripts record their own PIDs; they must match the child handles.
    let recorded_gst = read_pid_file(&env.gst_pid_file, Duration::from_secs(2))
        .await
        .expect("decoder recorded its pid");
    let recorded_pacat = read_pid_file(&env.pacat_pid_file, Duration::from_secs(2))
        .await
        .expect("player recorded its pid");
    assert_eq!(recorded_gst, gst_pid);
    assert_eq!(recorded_pacat, pacat_pid);

    // ---- write_pcm: bytes reach the decoder's stdin; the decoded marker flows
    //      decode-stdout -> bridge -> pacat-stdin -> output file ----
    sink.write_pcm(b"\xff\xf8\x00\x00\x00\x00\x00\x00\x01\x02\x03")
        .await
        .expect("write_pcm succeeds");
    assert!(
        file_contains(&env.pacat_out, &env.marker, Duration::from_secs(5)).await,
        "decoded marker reached the player output file"
    );

    // ---- restart: kill both old children (no zombies), start fresh PIDs ----
    let old_gst = gst_pid;
    let old_pacat = pacat_pid;
    let sink = sink.restart().await.expect("restart sink");
    let new_gst = sink.gst_pid().expect("new decoder pid");
    let new_pacat = sink.pacat_pid().expect("new player pid");
    assert_ne!(new_gst, old_gst, "restart spawns a new decoder pid");
    assert_ne!(new_pacat, old_pacat, "restart spawns a new player pid");
    assert!(alive(new_gst), "new decoder is alive");
    assert!(alive(new_pacat), "new player is alive");
    assert!(
        !alive(old_gst),
        "old decoder is dead and reaped (no zombie)"
    );
    assert!(
        !alive(old_pacat),
        "old player is dead and reaped (no zombie)"
    );

    // ---- stop: neither child survives ----
    sink.stop().await.expect("stop sink");
    assert!(!alive(new_gst), "decoder stopped");
    assert!(!alive(new_pacat), "player stopped");

    env.cleanup();
}

#[tokio::test]
async fn flush_barrier_drops_chunk_read_during_flush() {
    let _guard = SERIAL.lock().await;
    // The delayed decoder blocks the decoder pump in `read` for 0.5 s, then emits one
    // marker; the flush lands while the pump is still reading, so the decoder captures
    // the pre-flush epoch and must drop the post-read chunk.
    let env = TestEnv::prepare("delayed", "cat");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");
    let gst_pid = sink.gst_pid().expect("decoder pid");
    let pacat_pid = sink.pacat_pid().expect("player pid");

    // Give the decoder pump time to enter its (blocked) read.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Flush while the decoder pump is still reading, before the delayed marker arrives.
    sink.flush().await;

    // Wait past the marker emission and any drain.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // The marker was read after the flush epoch advanced, so the decoder dropped it and
    // it never reached the player.
    let output = std::fs::read_to_string(&env.pacat_out).unwrap_or_default();
    assert!(
        !output.contains(&env.marker),
        "the chunk read during flush was dropped, not delivered"
    );

    // Processes stay alive and the sink remains usable.
    assert!(alive(gst_pid), "decoder still alive after flush");
    assert!(alive(pacat_pid), "player still alive after flush");
    sink.write_pcm(b"\xff\xf8\x00\x00\x00\x00\x00\x00\x01")
        .await
        .expect("write_pcm after flush");

    sink.stop().await.expect("stop sink");
    env.cleanup();
}

#[tokio::test]
async fn player_exit_reports_failure_and_reaps_decoder() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare("stream", "exit");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");
    let gst_pid = sink.gst_pid().expect("decoder pid");

    // The player exits immediately; the player pump's write fails, reports the failure,
    // and reaps both children.
    assert!(
        failure_becomes(&sink, SinkError::Player, Duration::from_secs(5)).await,
        "sink reports a player failure"
    );
    assert!(
        dead_within(gst_pid, Duration::from_secs(5)).await,
        "decoder is reaped after the player dies"
    );
    assert_eq!(sink.failure(), Some(SinkError::Player));
    assert!(!alive(gst_pid), "decoder no longer alive");

    // stop is still safe (idempotent reaping).
    sink.stop().await.expect("stop sink");
    env.cleanup();
}

#[tokio::test]
async fn decoder_exit_reports_failure_and_reaps_player() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare("exit", "cat");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");
    let pacat_pid = sink.pacat_pid().expect("player pid");

    // The decoder exits immediately; its pump sees EOF, reports the failure, and reaps
    // both children.
    assert!(
        failure_becomes(&sink, SinkError::Decoder, Duration::from_secs(5)).await,
        "sink reports a decoder failure"
    );
    assert!(
        dead_within(pacat_pid, Duration::from_secs(5)).await,
        "player is reaped after the decoder dies"
    );
    assert_eq!(sink.failure(), Some(SinkError::Decoder));
    assert!(!alive(pacat_pid), "player no longer alive");

    sink.stop().await.expect("stop sink");
    env.cleanup();
}

#[tokio::test]
async fn pacat_spawn_failure_cleans_up_decoder() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare("stream", "cat");

    let result =
        Sink::start_with_binaries(&fixture("fake-gst.sh"), Path::new("/nonexistent/pacat-xyz"))
            .await;
    assert!(result.is_err(), "start fails when pacat is missing");

    // The decoder was started then killed+reaped. It may or may not have recorded its
    // PID before being reaped (fork/exec race); if it did, that PID must now be gone.
    if let Some(gst_pid) = read_pid_file(&env.gst_pid_file, Duration::from_secs(2)).await {
        assert!(
            !alive(gst_pid),
            "decoder was reaped after the pacat spawn failure"
        );
    }

    env.cleanup();
}

/// Sends SIGKILL to `pid` from outside the sink (simulating an external exit).
fn kill_pid(pid: u32) {
    let status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -KILL {pid} succeeded");
}

#[tokio::test]
async fn pacat_exit_while_bridge_empty_reports_failure() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare("silent", "cat");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");
    let gst_pid = sink.gst_pid().expect("decoder pid");
    let pacat_pid = sink.pacat_pid().expect("player pid");

    // The decoder produces nothing, so the bridge is empty and the player pump is idle
    // (blocked waiting for data). It therefore cannot detect pacat's exit via a write
    // error — only the supervisor's try_wait poll can.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sink.buffered_bytes().await, 0, "bridge is empty while idle");
    assert_eq!(sink.failure(), None, "healthy before the kill");

    kill_pid(pacat_pid);

    assert!(
        failure_becomes(&sink, SinkError::Player, Duration::from_secs(5)).await,
        "sink reports a player failure while the bridge is empty"
    );
    assert!(
        dead_within(gst_pid, Duration::from_secs(5)).await,
        "decoder is reaped after the idle player exits"
    );
    assert_eq!(sink.failure(), Some(SinkError::Player));
    assert!(!alive(gst_pid), "decoder no longer alive");

    sink.stop().await.expect("stop sink");
    env.cleanup();
}

#[tokio::test]
async fn player_pipe_break_reports_failure_and_reaps_both() {
    let _guard = SERIAL.lock().await;
    // The player closes its stdin but stays alive, so the player pump's write fails with
    // EPIPE while the child is still running. The pump must record the failure and reap
    // both children (killing the still-alive player).
    let env = TestEnv::prepare("stream", "close");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");
    let gst_pid = sink.gst_pid().expect("decoder pid");
    let pacat_pid = sink.pacat_pid().expect("player pid");

    assert!(
        failure_becomes(&sink, SinkError::Player, Duration::from_secs(5)).await,
        "sink reports a player failure when its stdin pipe breaks"
    );
    assert!(
        dead_within(gst_pid, Duration::from_secs(5)).await,
        "decoder is reaped after the player pipe breaks"
    );
    assert!(
        dead_within(pacat_pid, Duration::from_secs(5)).await,
        "player is reaped even though its child was still alive"
    );
    assert_eq!(sink.failure(), Some(SinkError::Player));

    sink.stop().await.expect("stop sink");
    env.cleanup();
}

/// The bridge's capacity in bytes: 0.5 s x 48 000 Hz x 2 channels x 2 bytes, matching
/// `BRIDGE_CAPACITY_SECS` in `src/sink/process.rs`.
const BRIDGE_BYTES: usize = 96_000;

#[tokio::test]
async fn backlog_accumulates_in_bridge_while_player_write_is_blocked() {
    let _guard = SERIAL.lock().await;
    // `gated` pacat stays blocked before reading stdin until released, so the player
    // pump fills the OS pipe (~64 KiB) and then blocks inside `write_all` on the next
    // 8 KiB chunk. While that write is in flight, the decoder must still be able to push
    // into the bridge until it is full - 0.5 s, 96_000 B - and the fixture's 365_000 B
    // burst is what proves the refusal and retry path at that capacity.
    let env = TestEnv::prepare("burst", "gated");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");

    let accumulated = buffered_at_least(&sink, BRIDGE_BYTES, Duration::from_secs(2)).await;
    if !accumulated {
        env.release_pacat();
        let _ = sink.stop().await;
        env.cleanup();
        panic!("bridge failed to accumulate its 0.5 s (96_000 B) capacity while player write was blocked");
    }
    assert_eq!(
        sink.buffered_bytes().await,
        BRIDGE_BYTES,
        "the bridge is full at its 0.5 s capacity; the decoder keeps the rest of the burst for \
         the retry that follows the release"
    );

    // Release pacat and verify the entire 365_000 B burst drains through the bridge.
    env.release_pacat();
    assert!(
        file_len_at_least(&env.pacat_out, 365_000, Duration::from_secs(5)).await,
        "all burst bytes reached pacat after it unblocked"
    );
    assert_eq!(
        sink.buffered_bytes().await,
        0,
        "bridge drained completely once pacat resumed reading"
    );

    sink.stop().await.expect("stop sink");
    env.cleanup();
}

#[tokio::test]
async fn flush_waits_for_in_flight_player_write_and_clears_backlog() {
    let _guard = SERIAL.lock().await;
    // With `gated` pacat and a 365_000 B burst, the player pump fills the OS pipe and
    // blocks inside `write_all` (`in_flight_writes == 1`), the bridge fills to 288_000 B,
    // and the decoder pump holds the remaining suffix waiting for bridge space.
    let env = TestEnv::prepare("burst", "gated");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");

    if !buffered_at_least(&sink, BRIDGE_BYTES, Duration::from_secs(2)).await {
        env.release_pacat();
        let _ = sink.stop().await;
        env.cleanup();
        panic!("bridge did not reach its capacity before testing flush barrier");
    }
    // Let the player pump settle into its blocked `write_all`.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Start `flush()` while the player write is still blocked in flight: it must NOT
    // return until that stale write completes.
    {
        let mut flush_fut = std::pin::pin!(sink.flush());
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut flush_fut)
                .await
                .is_err(),
            "flush() must wait while an in-flight player write is still active"
        );

        // Release pacat so the single in-flight write can finish; `flush()` must then
        // complete, clear the 288_000 B bridge backlog, and cause the decoder to drop its
        // retained pre-flush suffix.
        env.release_pacat();
        tokio::time::timeout(Duration::from_secs(2), &mut flush_fut)
            .await
            .expect("flush() completes once the in-flight player write finishes");
    }

    assert_eq!(
        sink.buffered_bytes().await,
        0,
        "bridge is empty immediately after flush()"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sink.buffered_bytes().await,
        0,
        "decoder dropped its retained pre-flush suffix rather than repopulating the bridge"
    );

    assert!(
        file_len_at_least(&env.pacat_out, 1, Duration::from_secs(2)).await,
        "pre-barrier pipe/in-flight bytes drained to pacat"
    );
    let delivered = std::fs::metadata(&env.pacat_out)
        .map(|m| m.len())
        .unwrap_or(0);
    // The bridge held BRIDGE_BYTES when the flush started, so at most the rest of the burst can
    // reach pacat afterwards; the pipe and the in-flight chunk always do.
    assert!(
        delivered <= 365_000 - BRIDGE_BYTES as u64,
        "the {BRIDGE_BYTES} B bridge backlog present at flush time stays dropped ({delivered} B \
         reached pacat)"
    );
    assert!(
        delivered >= 64 * 1024,
        "the pipe and the in-flight chunk drained to pacat before the barrier ({delivered} B)"
    );

    sink.stop().await.expect("stop sink");
    env.cleanup();
}

#[tokio::test]
async fn canceled_flush_does_not_wedge_sink_or_later_flush() {
    let _guard = SERIAL.lock().await;
    // `stream` gst emits markers continuously; `gated` pacat blocks until released so a
    // player write is in flight (`in_flight_writes == 1`) and the bridge fills up.
    let env = TestEnv::prepare("stream", "gated");

    let sink = Sink::start_with_binaries(&fixture("fake-gst.sh"), &fixture("fake-pacat.sh"))
        .await
        .expect("start sink");

    if !buffered_at_least(&sink, BRIDGE_BYTES, Duration::from_secs(2)).await {
        env.release_pacat();
        let _ = sink.stop().await;
        env.cleanup();
        panic!("bridge did not reach its capacity before testing flush cancellation");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Start `flush()` while the player write is blocked in flight, then cancel (drop) the
    // flush future via a short timeout before pacat is released.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sink.flush())
            .await
            .is_err(),
        "first flush() times out and is canceled while player write is still in flight"
    );

    // Release pacat so the in-flight write can drain.
    env.release_pacat();

    // A subsequent `flush()` must complete (not hang forever on a stale `flushing = true`
    // flag left behind by the canceled flush).
    let second_flush = tokio::time::timeout(Duration::from_secs(2), sink.flush()).await;
    if second_flush.is_err() {
        let _ = sink.stop().await;
        env.cleanup();
        panic!("subsequent flush() timed out: canceled flush left the sink wedged");
    }

    // Both pumps and the bridge must continue making progress after the second flush:
    // require >100 KiB of new output (exceeding the OS pipe size) to flow through
    // decoder -> bridge -> player.
    let len_after_flush = std::fs::metadata(&env.pacat_out)
        .map(|m| m.len())
        .unwrap_or(0);
    sink.write_pcm(b"\xff\xf8\x00\x00\x00\x00\x00\x00\x01")
        .await
        .expect("write_pcm after canceled and completed flushes");
    assert!(
        file_len_at_least(
            &env.pacat_out,
            len_after_flush + 100_000,
            Duration::from_secs(3)
        )
        .await,
        "decoder, bridge, and player pumps continue flowing data after a canceled flush"
    );

    sink.stop().await.expect("stop sink");
    env.cleanup();
}

fn sine_pcm(freq: f32, samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 4);
    for i in 0..samples {
        let value = (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin() * 0.5;
        let sample = (value * 32767.0) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[tokio::test]
async fn player_pump_publishes_pcm_levels_to_the_shared_visual() {
    let _guard = SERIAL.lock().await;
    let env = TestEnv::prepare("file", "cat");
    std::fs::write(&env.gst_pcm, sine_pcm(100.0, 4800)).expect("write pcm");

    let visual: SharedAudioVisual = Arc::new(RwLock::new(AudioVisual::default()));
    let sink = Sink::start_with_binaries_and_visual(
        &fixture("fake-gst.sh"),
        &fixture("fake-pacat.sh"),
        Some(Arc::clone(&visual)),
    )
    .await
    .expect("start sink");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let levels = visual.read().expect("visual lock").snapshot();
        if levels.level > 0.1 {
            assert!(
                levels.bass > levels.treble,
                "bass={} treble={}",
                levels.bass,
                levels.treble
            );
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("shared visual never received PCM levels: {levels:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    sink.stop().await.expect("stop sink");
    env.cleanup();
}
