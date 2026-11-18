#![allow(dead_code)]

#[path = "../src/visual.rs"]
mod visual;

use std::time::{Duration, Instant};

use visual::{AudioAnalyzer, AudioVisual, VisualLevels};

fn sine(freq: f32, samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 4);
    for i in 0..samples {
        let v = (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin() * 0.5;
        let s = (v * 32767.0) as i16;
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn levels_for(pcm: &[u8]) -> VisualLevels {
    AudioAnalyzer::default().analyze(pcm)
}

#[test]
fn silence_is_zero() {
    let levels = levels_for(&vec![0u8; 4800 * 4]);
    assert_eq!(levels, VisualLevels::default());
}

#[test]
fn bass_sine_dominates_the_bass_band() {
    let levels = levels_for(&sine(100.0, 4800));
    assert!(
        levels.bass > levels.mid,
        "bass={} mid={}",
        levels.bass,
        levels.mid
    );
    assert!(
        levels.bass > levels.treble,
        "bass={} treble={}",
        levels.bass,
        levels.treble
    );
    assert!(levels.level > 0.5, "level={}", levels.level);
}

#[test]
fn treble_sine_dominates_the_treble_band() {
    let levels = levels_for(&sine(5000.0, 4800));
    assert!(
        levels.treble > levels.bass,
        "treble={} bass={}",
        levels.treble,
        levels.bass
    );
    assert!(levels.level > 0.5, "level={}", levels.level);
}

#[test]
fn levels_are_bounded_for_any_pcm() {
    let frequencies = [0.0, 40.0, 100.0, 440.0, 1000.0, 5000.0, 12000.0];
    for freq in frequencies {
        let levels = levels_for(&sine(freq, 2048));
        for (name, value) in [
            ("level", levels.level),
            ("bass", levels.bass),
            ("mid", levels.mid),
            ("treble", levels.treble),
        ] {
            assert!((0.0..=1.0).contains(&value), "{name}={value} for {freq} Hz");
            assert!(value.is_finite(), "non-finite {name} for {freq} Hz");
        }
    }
    let odd = levels_for(&[1, 2, 3]);
    assert_eq!(odd, VisualLevels::default());
    let partial = levels_for(&[1, 2, 3, 4, 5]);
    assert!(partial.level <= 1.0 && partial.level.is_finite());
}

#[test]
fn stale_visual_decays_to_zero() {
    let mut visual = AudioVisual::default();
    let now = Instant::now();
    let loud = VisualLevels {
        level: 1.0,
        bass: 1.0,
        mid: 1.0,
        treble: 1.0,
    };
    visual.update_at(loud, now);
    assert_eq!(visual.snapshot_at(now), loud);
    assert_eq!(
        visual.snapshot_at(now + Duration::from_millis(301)),
        VisualLevels::default()
    );
}
