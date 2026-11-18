//! Best-effort PCM level analysis for the reactive UI.

use std::f32::consts::PI;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;

/// How long a reported level stays valid before the UI falls back to silence.
pub const STALE_AFTER: Duration = Duration::from_millis(300);

const SAMPLE_RATE: f32 = 48_000.0;
const BASS_HZ: f32 = 150.0;
const MID_HZ: f32 = 2_000.0;
const DB_FLOOR: f32 = -60.0;

/// Smoothed, serializable levels consumed by the UI. Every field is in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct VisualLevels {
    pub level: f32,
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
}

/// Shared last-known levels plus the time they were produced.
#[derive(Debug, Default)]
pub struct AudioVisual {
    levels: VisualLevels,
    updated_at: Option<Instant>,
}

impl AudioVisual {
    pub fn update(&mut self, levels: VisualLevels) {
        self.update_at(levels, Instant::now());
    }

    pub fn update_at(&mut self, levels: VisualLevels, now: Instant) {
        self.levels = bounded(levels);
        self.updated_at = Some(now);
    }

    pub fn snapshot(&self) -> VisualLevels {
        self.snapshot_at(Instant::now())
    }

    /// Returns the last levels while fresh, otherwise silence. A stopped sink must not leave the UI
    /// pulsing forever at the last chunk.
    pub fn snapshot_at(&self, now: Instant) -> VisualLevels {
        match self.updated_at {
            Some(updated) if now.saturating_duration_since(updated) <= STALE_AFTER => self.levels,
            _ => VisualLevels::default(),
        }
    }
}

pub type SharedAudioVisual = Arc<RwLock<AudioVisual>>;

/// Stateful three-band analyzer. Cheap enough to run on every PCM chunk.
#[derive(Debug, Default)]
pub struct AudioAnalyzer {
    low150: f32,
    low2k: f32,
    level: f32,
    bass: f32,
    mid: f32,
    treble: f32,
}

impl AudioAnalyzer {
    pub fn analyze(&mut self, pcm: &[u8]) -> VisualLevels {
        let mut sum_all = 0.0f32;
        let mut sum_bass = 0.0f32;
        let mut sum_mid = 0.0f32;
        let mut sum_treble = 0.0f32;
        let mut frames = 0u32;

        for frame in pcm.as_chunks::<4>().0 {
            let left = i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32_768.0;
            let right = i16::from_le_bytes([frame[2], frame[3]]) as f32 / 32_768.0;
            let sample = (left + right) * 0.5;

            self.low150 += alpha(BASS_HZ) * (sample - self.low150);
            self.low2k += alpha(MID_HZ) * (sample - self.low2k);

            let bass = self.low150;
            let mid = self.low2k - self.low150;
            let treble = sample - self.low2k;

            sum_all += sample * sample;
            sum_bass += bass * bass;
            sum_mid += mid * mid;
            sum_treble += treble * treble;
            frames += 1;
        }

        if frames == 0 {
            return self.current();
        }

        let count = frames as f32;
        self.level = smooth(self.level, db_level((sum_all / count).sqrt()));
        self.bass = smooth(self.bass, db_level((sum_bass / count).sqrt()));
        self.mid = smooth(self.mid, db_level((sum_mid / count).sqrt()));
        self.treble = smooth(self.treble, db_level((sum_treble / count).sqrt()));
        self.current()
    }

    fn current(&self) -> VisualLevels {
        bounded(VisualLevels {
            level: round(self.level),
            bass: round(self.bass),
            mid: round(self.mid),
            treble: round(self.treble),
        })
    }
}

fn alpha(hz: f32) -> f32 {
    1.0 - (-2.0 * PI * hz / SAMPLE_RATE).exp()
}

fn db_level(rms: f32) -> f32 {
    if !rms.is_finite() || rms <= 1e-6 {
        return 0.0;
    }
    ((20.0 * rms.log10() - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0)
}

fn smooth(previous: f32, next: f32) -> f32 {
    if next > previous {
        next
    } else {
        previous * 0.82 + next * 0.18
    }
}

fn round(value: f32) -> f32 {
    (value * 1_000.0).round() / 1_000.0
}

fn bounded(levels: VisualLevels) -> VisualLevels {
    VisualLevels {
        level: clamp(levels.level),
        bass: clamp(levels.bass),
        mid: clamp(levels.mid),
        treble: clamp(levels.treble),
    }
}

fn clamp(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}
