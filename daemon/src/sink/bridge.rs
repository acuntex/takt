//! Fixed-capacity PCM ring buffer with flush and non-blocking backpressure.
//!
//! The buffer stores raw PCM bytes in arrival order; it does no sample scaling and
//! knows nothing about timestamps or volume. Its one job is to absorb network jitter
//! up to a bounded amount of audio and to hand bytes to the player in order.
//!
//! [`Bridge::push`] is deliberately non-blocking: it accepts as many bytes as fit and
//! returns that count, so the reader keeps running even when the player is slow. The
//! result is `Ok(accepted)`; there is no error path today and `Ok(0)` simply means the
//! buffer is full. Any `accepted < pcm.len()` (including `Ok(0)`) is the backpressure
//! signal.
//!
//! A short push stores only a prefix; it does **not** retain the unaccepted suffix.
//! The caller must keep `pcm[accepted..]`, retry that suffix after [`Bridge::read`] has
//! drained enough space, and only then read the next WebSocket frame. Pausing frame
//! reads by itself is not enough: the suffix belongs to the current frame and would be
//! lost otherwise.

use std::collections::VecDeque;

use anyhow::Result;

/// PCM sample rate of the Sendspin audio stream in Hz.
pub const SAMPLE_RATE: usize = 48_000;
/// Number of interleaved channels (stereo).
pub const CHANNELS: usize = 2;
/// Bytes per sample (signed 16-bit little-endian).
pub const BYTES_PER_SAMPLE: usize = 2;

/// Bytes of audio buffered per second of capacity.
const BYTES_PER_SECOND: usize = SAMPLE_RATE * CHANNELS * BYTES_PER_SAMPLE;

/// Upper bound for the configured capacity, guarding the backing allocation against
/// absurd or overflowing values (e.g. `f32::INFINITY`). One minute is far more audio
/// than a jitter buffer ever needs.
const MAX_CAPACITY_SECS: f32 = 60.0;

/// Bounded FIFO of PCM bytes.
///
/// `capacity` is the logical byte limit; the backing allocation may be larger.
#[derive(Debug)]
pub struct Bridge {
    buf: VecDeque<u8>,
    capacity: usize,
}

impl Bridge {
    /// Creates an empty buffer holding `capacity_secs` seconds of PCM.
    ///
    /// The byte capacity is `round(capacity_secs * 48_000 * 2 * 2)`; for example
    /// 1.5 s is exactly 288_000 bytes. Non-positive or non-finite values yield a
    /// zero-capacity buffer; finite values above 60 s are clamped to 60 s so the
    /// backing allocation stays bounded.
    pub fn new(capacity_secs: f32) -> Self {
        let capacity_secs = if capacity_secs.is_finite() {
            capacity_secs.clamp(0.0, MAX_CAPACITY_SECS)
        } else {
            0.0
        };
        let capacity = (capacity_secs * BYTES_PER_SECOND as f32).round() as usize;
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Appends as many bytes of `pcm` as fit and returns `Ok(accepted)`.
    ///
    /// Never blocks and never buffers more than [`capacity`](Self::capacity). There is
    /// no error path today, so this always returns `Ok`; `Ok(0)` means the buffer is
    /// full.
    ///
    /// # Short-write contract
    ///
    /// A short push (`accepted < pcm.len()`, including `Ok(0)`) accepts only a prefix
    /// and does **not** store the unaccepted suffix. The caller must:
    ///
    /// 1. retain the suffix `&pcm[accepted..]`;
    /// 2. keep calling [`read`](Self::read) to drain the bridge, then retry that suffix
    ///    before reading any later WebSocket frame;
    /// 3. treat every short return as backpressure — pause frame reads until the retry
    ///    succeeds in full.
    ///
    /// Pausing frame reads alone can lose the rest of the current frame, because the
    /// suffix lives in the caller's buffer, not in the bridge.
    pub fn push(&mut self, pcm: &[u8]) -> Result<usize> {
        let free = self.capacity - self.buf.len();
        let accepted = pcm.len().min(free);
        self.buf.extend(&pcm[..accepted]);
        Ok(accepted)
    }

    /// Copies `min(out.len(), len())` buffered bytes into `out`, in order, and
    /// returns the number copied.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let copied = out.len().min(self.buf.len());
        for (slot, byte) in out.iter_mut().zip(self.buf.drain(..copied)) {
            *slot = byte;
        }
        copied
    }

    /// Discards every buffered byte, leaving the buffer empty.
    pub fn flush(&mut self) {
        self.buf.clear();
    }

    /// Number of bytes currently buffered.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Maximum number of bytes the buffer can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether no bytes are currently buffered.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}
