//! Mixes everyone we can hear into one stereo stream.
//!
//! Each speaker has a jitter buffer and an Opus decoder of their own. Once per 20 ms the mixer
//! takes one frame from each, decodes it (or conceals a lost one), applies the speaker's
//! distance and direction gains from [`crate::spatial`], and sums them.

use std::collections::HashMap;

use crate::codec::{Decoder, FRAME_SAMPLES};
use crate::jitter::{self, JitterBuffer, Pop};
use crate::spatial::{self, Falloff, Listener};
use crate::wire::Uuid;

/// One 20 ms stereo frame, interleaved left/right.
pub type StereoFrame = [f32; FRAME_SAMPLES * 2];

/// Gain for a speaker whose position the client doesn't know (out of the client's entity range,
/// say). The server only forwards speakers in range, so play them centred at a moderate level.
const UNKNOWN_POSITION_GAIN: f32 = 0.5;
/// Group voice: no fading with distance.
const NO_FALLOFF: Falloff = Falloff { full_volume_distance: f64::INFINITY, max_distance: f64::INFINITY };
/// A speaker counts as talking for this many ticks after their last frame (for the indicator).
const TALKING_TICKS: u64 = 10;
/// A speaker silent this long is dropped, freeing their decoder (10 s).
const FORGET_TICKS: u64 = 500;

struct Speaker {
    jitter: JitterBuffer,
    decoder: Option<Decoder>,
    /// Gains used for the last frame; the next frame ramps from these to the new ones.
    gains: (f32, f32),
    last_frame_tick: Option<u64>,
    last_push_tick: u64,
    /// Their last frame came through our group: full volume at any distance.
    group: bool,
}

pub struct Mixer {
    speakers: HashMap<Uuid, Speaker>,
    positions: HashMap<Uuid, [f64; 3]>,
    listener: Option<Listener>,
    falloff: Falloff,
    jitter_cfg: jitter::Config,
    /// Master output volume (1.0 = unchanged).
    pub volume: f32,
    /// Per-player volume multipliers.
    pub player_volume: HashMap<Uuid, f32>,
    /// Deafened: keep the buffers moving but output silence.
    pub deafened: bool,
    tick: u64,
}

impl Mixer {
    pub fn new(max_distance: f64) -> Self {
        Self {
            speakers: HashMap::new(),
            positions: HashMap::new(),
            listener: None,
            falloff: Falloff::with_max(max_distance),
            jitter_cfg: jitter::Config::default(),
            volume: 1.0,
            player_volume: HashMap::new(),
            deafened: false,
            tick: 0,
        }
    }

    pub fn set_max_distance(&mut self, max_distance: f64) {
        self.falloff = Falloff::with_max(max_distance);
    }

    /// Where we hear from (the camera), or `None` before the world is drawn.
    pub fn set_listener(&mut self, listener: Option<Listener>) {
        self.listener = listener;
    }

    /// Positions of the players the client knows about, replacing the previous set.
    pub fn set_positions(&mut self, positions: impl IntoIterator<Item = (Uuid, [f64; 3])>) {
        self.positions.clear();
        self.positions.extend(positions);
    }

    /// A voice frame from the network.
    pub fn push(&mut self, sender: Uuid, sequence: u64, opus: Vec<u8>) {
        self.push_frame(sender, sequence, opus, false);
    }

    /// A frame from someone in our group: heard at any distance (capture 06), so it plays at
    /// full volume, still panned toward them when they're in sight.
    pub fn push_group(&mut self, sender: Uuid, sequence: u64, opus: Vec<u8>) {
        self.push_frame(sender, sequence, opus, true);
    }

    fn push_frame(&mut self, sender: Uuid, sequence: u64, opus: Vec<u8>, group: bool) {
        let (cfg, tick) = (self.jitter_cfg, self.tick);
        let speaker = self.speakers.entry(sender).or_insert_with(|| Speaker {
            jitter: JitterBuffer::new(cfg),
            decoder: None,
            gains: (0.0, 0.0),
            last_frame_tick: None,
            last_push_tick: tick,
            group,
        });
        speaker.last_push_tick = tick;
        speaker.group = group;
        speaker.jitter.push(sequence, opus);
    }

    /// Players heard in the last 200 ms.
    pub fn talking(&self) -> Vec<Uuid> {
        self.speakers
            .iter()
            .filter(|(_, s)| s.last_frame_tick.is_some_and(|t| self.tick.saturating_sub(t) <= TALKING_TICKS))
            .map(|(id, _)| *id)
            .collect()
    }

    /// The next 20 ms of output.
    pub fn mix(&mut self) -> StereoFrame {
        self.tick += 1;
        let mut out = [0f32; FRAME_SAMPLES * 2];
        let tick = self.tick;
        self.speakers.retain(|_, s| tick.saturating_sub(s.last_push_tick) < FORGET_TICKS || !s.jitter.is_empty());

        for (id, speaker) in self.speakers.iter_mut() {
            let packet = match speaker.jitter.pop() {
                Pop::Frame(p) => Some(p),
                Pop::Lost => None,
                Pop::Silent => continue,
            };
            if speaker.decoder.is_none() {
                match Decoder::new() {
                    Ok(d) => speaker.decoder = Some(d),
                    Err(_) => continue,
                }
            }
            let Ok(pcm) = speaker.decoder.as_mut().unwrap().decode(packet.as_deref()) else { continue };
            speaker.last_frame_tick = Some(tick);

            let falloff = if speaker.group { NO_FALLOFF } else { self.falloff };
            let target = target_gains(self.listener.as_ref(), self.positions.get(id), &falloff);
            let scale = self.volume * self.player_volume.get(id).copied().unwrap_or(1.0);
            let target = (target.0 * scale, target.1 * scale);
            // Ramp across the frame from the previous gains, so moving or turning doesn't click.
            let (from, n) = (speaker.gains, FRAME_SAMPLES as f32);
            for (i, sample) in pcm.iter().enumerate() {
                let k = (i + 1) as f32 / n;
                out[2 * i] += sample * (from.0 + (target.0 - from.0) * k);
                out[2 * i + 1] += sample * (from.1 + (target.1 - from.1) * k);
            }
            speaker.gains = target;
        }

        if self.deafened {
            return [0f32; FRAME_SAMPLES * 2];
        }
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        out
    }
}

fn target_gains(listener: Option<&Listener>, position: Option<&[f64; 3]>, falloff: &Falloff) -> (f32, f32) {
    match (listener, position) {
        (Some(l), Some(p)) => spatial::stereo_gains(l, *p, falloff),
        _ => {
            let g = UNKNOWN_POSITION_GAIN * std::f32::consts::FRAC_1_SQRT_2;
            (g, g)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Encoder;
    use crate::plugin::Codec;

    fn tone_packets(n: usize) -> Vec<Vec<u8>> {
        let mut enc = Encoder::new(Codec::Voip).unwrap();
        (0..n)
            .map(|f| {
                let mut frame = [0f32; FRAME_SAMPLES];
                for (i, s) in frame.iter_mut().enumerate() {
                    *s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * (f * FRAME_SAMPLES + i) as f32 / 48_000.0).sin();
                }
                enc.encode(&frame, 1275).unwrap()
            })
            .collect()
    }

    fn energy(frame: &StereoFrame) -> (f32, f32) {
        let (mut l, mut r) = (0.0, 0.0);
        for i in 0..FRAME_SAMPLES {
            l += frame[2 * i] * frame[2 * i];
            r += frame[2 * i + 1] * frame[2 * i + 1];
        }
        (l, r)
    }

    fn mixer_with_speaker_east(distance: f64) -> (Mixer, Uuid) {
        let mut m = Mixer::new(48.0);
        let alice = Uuid([1; 16]);
        // Facing north: east is to the right.
        m.set_listener(Some(Listener { pos: [0.0, 64.0, 0.0], yaw_degrees: 180.0 }));
        m.set_positions([(alice, [distance, 64.0, 0.0])]);
        for (seq, p) in tone_packets(30).into_iter().enumerate() {
            m.push(alice, seq as u64, p);
        }
        (m, alice)
    }

    #[test]
    fn silent_until_someone_talks() {
        let mut m = Mixer::new(48.0);
        assert_eq!(energy(&m.mix()), (0.0, 0.0));
        assert!(m.talking().is_empty());
    }

    #[test]
    fn a_speaker_to_the_east_comes_out_on_the_right() {
        let (mut m, alice) = mixer_with_speaker_east(5.0);
        let mut last = m.mix();
        for _ in 0..10 {
            last = m.mix();
        }
        let (l, r) = energy(&last);
        assert!(r > 1.0 && l < r * 0.01, "l {l} r {r}");
        assert_eq!(m.talking(), [alice]);
    }

    #[test]
    fn far_away_is_quieter() {
        let mix_energy = |d: f64| {
            let (mut m, _) = mixer_with_speaker_east(d);
            (0..10).map(|_| energy(&m.mix()).1).last().unwrap()
        };
        let (near, far) = (mix_energy(5.0), mix_energy(40.0));
        assert!(far < near * 0.1, "near {near} far {far}");
    }

    #[test]
    fn a_group_member_far_away_is_still_heard_in_full() {
        let mut m = Mixer::new(48.0);
        let bob = Uuid([3; 16]);
        m.set_listener(Some(Listener { pos: [0.0, 64.0, 0.0], yaw_degrees: 180.0 }));
        // 200 blocks east: silent as a nearby voice, full as a group voice.
        m.set_positions([(bob, [200.0, 64.0, 0.0])]);
        for (seq, p) in tone_packets(30).into_iter().enumerate() {
            m.push_group(bob, seq as u64, p);
        }
        let mut last = m.mix();
        for _ in 0..10 {
            last = m.mix();
        }
        let (l, r) = energy(&last);
        assert!(r > 1.0, "heard on the right, at full volume: r {r}");
        assert!(l < r * 0.01, "still panned toward them: l {l}");
    }

    #[test]
    fn deafened_outputs_silence_but_keeps_up() {
        let (mut m, alice) = mixer_with_speaker_east(5.0);
        m.deafened = true;
        for _ in 0..10 {
            assert_eq!(energy(&m.mix()), (0.0, 0.0));
        }
        assert_eq!(m.talking(), [alice], "still tracked for the indicator");
    }

    #[test]
    fn per_player_volume_applies() {
        let (mut m, alice) = mixer_with_speaker_east(5.0);
        m.player_volume.insert(alice, 0.0);
        for _ in 0..10 {
            m.mix();
        }
        assert!(energy(&m.mix()).1 < 1e-6);
    }

    #[test]
    fn unknown_position_plays_centred() {
        let mut m = Mixer::new(48.0);
        let bob = Uuid([2; 16]);
        for (seq, p) in tone_packets(20).into_iter().enumerate() {
            m.push(bob, seq as u64, p);
        }
        let mut last = m.mix();
        for _ in 0..8 {
            last = m.mix();
        }
        let (l, r) = energy(&last);
        assert!(l > 0.1 && (l - r).abs() < l * 1e-3, "l {l} r {r}");
    }
}
