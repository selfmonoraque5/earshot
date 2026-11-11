//! When to send the microphone: push-to-talk, or voice activation by level.

/// Level of a frame in dBFS (0 = full scale), floored at -127 for silence.
pub fn level_dbfs(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -127.0;
    }
    let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    if mean_square <= 0.0 {
        return -127.0;
    }
    (10.0 * mean_square.log10()).max(-127.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Send while the key is held.
    PushToTalk,
    /// Send while the microphone is louder than `threshold_dbfs`.
    Voice { threshold_dbfs: f32 },
}

/// Decides, frame by frame, whether the microphone is on.
#[derive(Debug, Clone)]
pub struct Activation {
    pub mode: Mode,
    /// Frames to keep sending after the level drops (voice mode), so word endings aren't cut.
    pub hold_frames: u32,
    held: u32,
}

impl Activation {
    pub fn new(mode: Mode) -> Self {
        // 15 frames = 300 ms of hold.
        Self { mode, hold_frames: 15, held: 0 }
    }

    /// Whether this 20 ms frame should be sent.
    pub fn frame(&mut self, level_dbfs: f32, key_down: bool) -> bool {
        match self.mode {
            Mode::PushToTalk => key_down,
            Mode::Voice { threshold_dbfs } => {
                if level_dbfs >= threshold_dbfs {
                    self.held = self.hold_frames;
                    true
                } else if self.held > 0 {
                    self.held -= 1;
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(level_dbfs(&[0.0; 960]), -127.0);
        assert_eq!(level_dbfs(&[]), -127.0);
        assert!((level_dbfs(&[1.0; 960]) - 0.0).abs() < 1e-4);
        assert!((level_dbfs(&[0.1; 960]) + 20.0).abs() < 1e-3);
    }

    #[test]
    fn push_to_talk_follows_the_key() {
        let mut a = Activation::new(Mode::PushToTalk);
        assert!(!a.frame(0.0, false));
        assert!(a.frame(-127.0, true));
    }

    #[test]
    fn voice_activation_holds_after_speech() {
        let mut a = Activation::new(Mode::Voice { threshold_dbfs: -40.0 });
        assert!(!a.frame(-60.0, false));
        assert!(a.frame(-30.0, false));
        for _ in 0..15 {
            assert!(a.frame(-60.0, false), "held");
        }
        assert!(!a.frame(-60.0, false), "released after the hold");
    }
}
