//! Microphone clean-up before encoding: a high-pass filter and an adaptive noise gate.
//!
//! - The high-pass (about 80 Hz) removes rumble, desk knocks and mains hum that speech doesn't
//!   need.
//! - The gate learns the room's background level (it follows quiet moments quickly and loud ones
//!   slowly) and turns the microphone down when only that background is there: fans, hiss, a
//!   PC's hum. Speech, which stands well above it, passes unchanged. The gain moves smoothly, so
//!   it doesn't click.
//!
//! This is classic DSP, not a neural denoiser: it can't remove noise *under* someone's voice,
//! but it keeps the line quiet between words.

use crate::activation::level_dbfs;
use crate::codec::{FRAME_SAMPLES, SAMPLE_RATE};

/// One-pole high-pass filter state.
#[derive(Debug, Clone)]
struct HighPass {
    a: f32,
    prev_in: f32,
    prev_out: f32,
}

impl HighPass {
    fn new(cutoff_hz: f32) -> Self {
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
        let dt = 1.0 / SAMPLE_RATE as f32;
        Self { a: rc / (rc + dt), prev_in: 0.0, prev_out: 0.0 }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.a * (self.prev_out + x - self.prev_in);
        self.prev_in = x;
        self.prev_out = y;
        y
    }
}

#[derive(Debug, Clone)]
pub struct NoiseSuppressor {
    hp: HighPass,
    /// Estimated background level, dBFS.
    floor_db: f32,
    /// Gain applied at the end of the last frame (linear).
    gain: f32,
}

/// How far above the background a frame must be to pass untouched.
const OPEN_ABOVE_FLOOR_DB: f32 = 9.0;
/// Below this, the gate is fully closed.
const CLOSED_ABOVE_FLOOR_DB: f32 = 3.0;
/// How much a closed gate turns the microphone down.
const CLOSED_GAIN: f32 = 0.06; // about -24 dB
/// The background estimate never goes below this (digital silence would freeze the gate shut).
const MIN_FLOOR_DB: f32 = -90.0;

impl Default for NoiseSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseSuppressor {
    pub fn new() -> Self {
        Self { hp: HighPass::new(80.0), floor_db: -60.0, gain: 1.0 }
    }

    /// The current background estimate, dBFS (for the menu).
    pub fn floor_dbfs(&self) -> f32 {
        self.floor_db
    }

    /// Cleans one 20 ms frame in place.
    pub fn process(&mut self, frame: &mut [f32; FRAME_SAMPLES]) {
        for s in frame.iter_mut() {
            *s = self.hp.process(*s);
        }
        let level = level_dbfs(frame);

        // Follow the background: down fast (about 0.2 s), up more slowly (about 2 s), so the pauses
        // between words keep it low while a fan that switches on is learned within seconds.
        let rate = if level < self.floor_db { 0.1 } else { 0.01 };
        self.floor_db = (self.floor_db + (level - self.floor_db) * rate).max(MIN_FLOOR_DB);

        let above = level - self.floor_db;
        let t = ((above - CLOSED_ABOVE_FLOOR_DB) / (OPEN_ABOVE_FLOOR_DB - CLOSED_ABOVE_FLOOR_DB)).clamp(0.0, 1.0);
        // Smoothstep between closed and open.
        let target = CLOSED_GAIN + (1.0 - CLOSED_GAIN) * (t * t * (3.0 - 2.0 * t));
        // Open within a frame (don't clip word starts); close over several (no pumping).
        let target = if target > self.gain { target } else { self.gain + (target - self.gain) * 0.35 };

        let (from, n) = (self.gain, FRAME_SAMPLES as f32);
        for (i, s) in frame.iter_mut().enumerate() {
            let k = (i + 1) as f32 / n;
            *s *= from + (target - from) * k;
        }
        self.gain = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white-ish noise at roughly `amplitude`.
    fn noise(seed: &mut u32, amplitude: f32) -> [f32; FRAME_SAMPLES] {
        let mut f = [0f32; FRAME_SAMPLES];
        for s in f.iter_mut() {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s = ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amplitude;
        }
        f
    }

    fn tone(freq: f32, amplitude: f32, frame: usize) -> [f32; FRAME_SAMPLES] {
        let mut f = [0f32; FRAME_SAMPLES];
        for (i, s) in f.iter_mut().enumerate() {
            let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
            *s = amplitude * (2.0 * std::f32::consts::PI * freq * t).sin();
        }
        f
    }

    #[test]
    fn background_noise_is_turned_down() {
        let mut ns = NoiseSuppressor::new();
        let mut seed = 1;
        let mut out_level = 0.0;
        for _ in 0..150 {
            // 3 s of quiet fan-like noise at about -50 dBFS.
            let mut f = noise(&mut seed, 0.005);
            ns.process(&mut f);
            out_level = level_dbfs(&f);
        }
        let raw = level_dbfs(&noise(&mut seed, 0.005));
        assert!(out_level < raw - 15.0, "noise {raw:.1} dB came out at {out_level:.1} dB");
    }

    #[test]
    fn speech_level_sound_passes() {
        let mut ns = NoiseSuppressor::new();
        let mut seed = 7;
        for _ in 0..150 {
            let mut f = noise(&mut seed, 0.005);
            ns.process(&mut f);
        }
        // Then "speech": a 300 Hz tone 30 dB above the noise.
        let mut last = [0f32; FRAME_SAMPLES];
        for i in 0..10 {
            let mut f = tone(300.0, 0.2, i);
            for (s, n) in f.iter_mut().zip(noise(&mut seed, 0.005)) {
                *s += n;
            }
            let raw = level_dbfs(&f);
            ns.process(&mut f);
            last = f;
            if i > 0 {
                assert!(level_dbfs(&f) > raw - 2.0, "speech frame {i} was cut");
            }
        }
        assert!(level_dbfs(&last) > -20.0);
    }

    #[test]
    fn rumble_is_filtered() {
        let mut ns = NoiseSuppressor::new();
        let mut last = [0f32; FRAME_SAMPLES];
        for i in 0..50 {
            let mut f = tone(20.0, 0.3, i);
            ns.process(&mut f);
            last = f;
        }
        let raw = level_dbfs(&tone(20.0, 0.3, 50));
        assert!(level_dbfs(&last) < raw - 10.0, "20 Hz rumble should be well down");
    }

    #[test]
    fn digital_silence_doesnt_break_it() {
        let mut ns = NoiseSuppressor::new();
        for _ in 0..500 {
            let mut f = [0f32; FRAME_SAMPLES];
            ns.process(&mut f);
            assert!(f.iter().all(|s| *s == 0.0));
        }
        assert!(ns.floor_dbfs() >= MIN_FLOOR_DB);
        let mut f = tone(300.0, 0.2, 0);
        ns.process(&mut f);
        assert!(level_dbfs(&f) > -25.0, "a voice after silence opens the gate at once");
    }
}
