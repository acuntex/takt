//! Integration tests for the PCM ring buffer (Task 5).
//!
//! The daemon is a binary-only crate, so the sink module is included here via `#[path]`
//! instead of adding a library target just for tests. The buffer is pure (no I/O, no
//! async runtime), so every case runs synchronously.

#[allow(dead_code)]
#[path = "../src/sink/mod.rs"]
mod sink;

#[allow(dead_code)]
#[path = "../src/visual.rs"]
mod visual;

use sink::bridge::Bridge;

/// 1.5 s of 48 kHz, stereo, 16-bit PCM: 1.5 * 48000 * 2 * 2 = 288_000 bytes.
#[test]
fn capacity_for_one_and_a_half_seconds_is_288_000_bytes() {
    let bridge = Bridge::new(1.5);
    assert_eq!(bridge.capacity(), 288_000);
}

/// Non-finite or negative durations cannot produce a usable capacity; oversized
/// finite values are clamped so the backing allocation stays bounded.
#[test]
fn capacity_input_is_clamped() {
    assert_eq!(Bridge::new(f32::INFINITY).capacity(), 0);
    assert_eq!(Bridge::new(f32::NEG_INFINITY).capacity(), 0);
    assert_eq!(Bridge::new(f32::NAN).capacity(), 0);
    assert_eq!(Bridge::new(-1.5).capacity(), 0);
    assert_eq!(Bridge::new(1_000_000.0).capacity(), 60 * 192_000);
}

/// (a) Pushing more than the capacity never buffers more than `capacity` bytes and
/// reports how much was accepted (partial acceptance, then `Ok(0)` when full).
#[test]
fn push_beyond_capacity_accepts_only_free_space_then_zero() {
    let mut bridge = Bridge::new(1.5);
    let capacity = bridge.capacity();

    assert_eq!(bridge.push(&vec![0xAA; capacity]).unwrap(), capacity);
    assert_eq!(bridge.len(), capacity);

    // Full buffer: nothing more is accepted, but this is not an error.
    assert_eq!(bridge.push(&[0xBB; 64]).unwrap(), 0);
    assert_eq!(bridge.len(), capacity);
    assert!(bridge.len() <= bridge.capacity());
}

/// A full buffer accepts a partial write when the input does not fit completely.
/// The bridge does not keep the unaccepted suffix; the caller must retain it.
#[test]
fn push_into_partially_filled_buffer_accepts_remaining_space() {
    let mut bridge = Bridge::new(0.005); // 960 bytes
    let capacity = bridge.capacity();
    assert_eq!(capacity, 960);

    assert_eq!(bridge.push(&vec![1; capacity - 10]).unwrap(), capacity - 10);

    let frame = [2u8; 25];
    let accepted = bridge.push(&frame).unwrap();
    assert_eq!(accepted, 10, "only the free space may be accepted");
    assert!(accepted < frame.len());
    assert_eq!(bridge.len(), capacity);

    // The bridge stored the prefix; the suffix remains the caller's responsibility.
    let pending = &frame[accepted..];
    assert_eq!(pending.len(), 15);

    // The bridge is full, so draining it makes room before the suffix is retried.
    let mut out = vec![0u8; capacity];
    assert_eq!(bridge.read(&mut out), capacity);
    assert!(out[..capacity - 10].iter().all(|&b| b == 1));
    assert!(out[capacity - 10..].iter().all(|&b| b == 2));

    // Retry the retained suffix *before* accepting a later frame, so ordering is kept.
    assert_eq!(bridge.push(pending).unwrap(), pending.len());
    let later_frame = [3u8; 5];
    assert_eq!(
        bridge.push(&later_frame).unwrap(),
        later_frame.len(),
        "the bridge has room after draining"
    );

    let mut replayed = [0u8; 20];
    assert_eq!(bridge.read(&mut replayed), 20);
    assert_eq!(&replayed[..15], pending);
    assert_eq!(&replayed[15..], &later_frame);
}

/// (b) `flush` empties the buffer completely and reports no buffered bytes.
#[test]
fn flush_empties_the_buffer_completely() {
    let mut bridge = Bridge::new(1.5);
    assert_eq!(bridge.push(&vec![0x5A; 100_000]).unwrap(), 100_000);
    assert_eq!(bridge.len(), 100_000);

    bridge.flush();

    assert_eq!(bridge.len(), 0);
    assert!(bridge.is_empty());
    let mut out = [0u8; 16];
    assert_eq!(bridge.read(&mut out), 0);
}

/// (c) After a flush the buffer starts at offset zero: the next push is read back
/// first, with no stale prefix from before the flush.
#[test]
fn after_flush_buffer_starts_empty_and_at_zero() {
    let mut bridge = Bridge::new(0.005); // 960 bytes
    assert_eq!(bridge.push(&vec![0x11; 500]).unwrap(), 500);
    bridge.flush();

    assert_eq!(bridge.push(&[0x22; 4]).unwrap(), 4);
    assert_eq!(bridge.len(), 4);

    let mut out = [0u8; 8];
    assert_eq!(bridge.read(&mut out), 4);
    assert_eq!(&out[..4], &[0x22; 4]);
}

/// (d) Byte order is preserved across chunked pushes and reads.
#[test]
fn byte_order_is_preserved() {
    let mut bridge = Bridge::new(1.5);

    let first: Vec<u8> = (0..=127u8).collect();
    let second: Vec<u8> = (128..=255u8).collect();
    assert_eq!(bridge.push(&first).unwrap(), first.len());
    assert_eq!(bridge.push(&second).unwrap(), second.len());
    assert_eq!(bridge.len(), 256);

    let mut out = [0u8; 256];
    // Short reads return just the requested prefix.
    assert_eq!(bridge.read(&mut out[..100]), 100);
    assert_eq!(&out[..100], &first[..100]);
    assert_eq!(bridge.len(), 156);

    // Reading more than available copies only what is buffered.
    let mut rest = [0u8; 300];
    assert_eq!(bridge.read(&mut rest), 156);
    assert_eq!(&rest[..28], &first[100..]);
    assert_eq!(&rest[28..156], &second[..]);
    assert_eq!(bridge.len(), 0);
    assert!(bridge.is_empty());
}
