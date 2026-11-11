//! Streaming sample-rate conversion between the sound card's rate and Opus's 48 kHz.
//!
//! Linear interpolation: cheap, latency-free and good enough for speech (a microphone rarely
//! carries anything above 8 kHz that matters). It is output-driven: each call to [`pull`]
//! produces one output frame and takes input frames from a source as it needs them, so the same
//! type serves the microphone (input arrives, output is drained) and the speakers (the device asks
//! for output, input is drawn from the mixer).
//!
//! [`pull`]: Resampler::pull

/// Converts frames of `N` channels from `from` Hz to `to` Hz.
#[derive(Debug, Clone)]
pub struct Resampler<const N: usize> {
    /// Input frames per output frame.
    step: f64,
    /// Position between `prev` (0.0) and `next` (1.0).
    t: f64,
    prev: Option<[f32; N]>,
    next: Option<[f32; N]>,
}

impl<const N: usize> Resampler<N> {
    pub fn new(from: u32, to: u32) -> Self {
        assert!(from > 0 && to > 0, "sample rates must be positive");
        Self { step: from as f64 / to as f64, t: 0.0, prev: None, next: None }
    }

    /// Produces one output frame, or `None` when the source has run dry. Nothing is lost on
    /// `None`: call again once the source has more.
    pub fn pull(&mut self, source: &mut impl FnMut() -> Option<[f32; N]>) -> Option<[f32; N]> {
        if self.prev.is_none() {
            self.prev = Some(source()?);
        }
        if self.next.is_none() {
            self.next = Some(source()?);
        }
        while self.t >= 1.0 {
            let n = source()?;
            self.prev = self.next;
            self.next = Some(n);
            self.t -= 1.0;
        }
        let (a, b) = (self.prev.unwrap(), self.next.unwrap());
        let t = self.t as f32;
        let mut out = [0f32; N];
        for c in 0..N {
            out[c] = a[c] + (b[c] - a[c]) * t;
        }
        self.t += self.step;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut r = Resampler::<1>::new(from, to);
        let mut it = input.iter().copied();
        let mut src = || it.next().map(|s| [s]);
        std::iter::from_fn(|| r.pull(&mut src)).map(|f| f[0]).collect()
    }

    #[test]
    fn same_rate_passes_samples_through() {
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = run(48_000, 48_000, &input);
        // One frame stays held back as the interpolation's look-ahead.
        assert_eq!(out, input[..99]);
    }

    #[test]
    fn output_length_follows_the_rate_ratio() {
        let input = vec![0.0; 44_100];
        let out = run(44_100, 48_000, &input);
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
        let out = run(48_000, 16_000, &vec![0.0; 48_000]);
        assert!((out.len() as i64 - 16_000).abs() <= 2, "{}", out.len());
    }

    #[test]
    fn a_tone_keeps_its_pitch() {
        // 1 kHz at 44.1 kHz, resampled to 48 kHz: count zero crossings over one second.
        let input: Vec<f32> = (0..44_100).map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44_100.0).sin()).collect();
        let out = run(44_100, 48_000, &input);
        let crossings = out.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert!((1995..=2005).contains(&crossings), "{crossings} crossings, expected about 2000");
    }

    #[test]
    fn interpolates_between_samples() {
        // Doubling the rate puts a midpoint between each pair.
        assert_eq!(run(1, 2, &[0.0, 1.0, 2.0]), [0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn running_dry_loses_nothing() {
        let mut r = Resampler::<2>::new(48_000, 48_000);
        let mut queue: std::collections::VecDeque<[f32; 2]> = [[1.0, -1.0]].into();
        assert_eq!(r.pull(&mut || queue.pop_front()), None);
        queue.extend([[2.0, -2.0], [3.0, -3.0]]);
        assert_eq!(r.pull(&mut || queue.pop_front()), Some([1.0, -1.0]));
        assert_eq!(r.pull(&mut || queue.pop_front()), Some([2.0, -2.0]));
    }
}
