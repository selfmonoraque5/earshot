//! Opus at SVC's settings: 48 kHz, mono, 20 ms frames (960 samples), observed in every capture.
//! Samples are `f32` in -1.0..=1.0.

use crate::opus::{Application, OpusDecoder, OpusEncoder};
use crate::plugin::Codec;

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples in one 20 ms frame.
pub const FRAME_SAMPLES: usize = 960;
/// Opus's largest possible packet; also SVC's default `mtu_size`.
pub const MAX_PACKET: usize = 1275;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub &'static str);

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "opus: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

/// Encodes our microphone, one frame at a time.
pub struct Encoder {
    inner: OpusEncoder,
    out: Vec<u8>,
}

impl Encoder {
    /// `codec` is the application mode the server asked for in `voicechat:secret`.
    pub fn new(codec: Codec) -> Result<Self, CodecError> {
        let app = match codec {
            Codec::Voip => Application::Voip,
            Codec::Audio => Application::Audio,
            Codec::RestrictedLowDelay => Application::RestrictedLowDelay,
        };
        let inner = OpusEncoder::new(SAMPLE_RATE as i32, 1, app).map_err(CodecError)?;
        Ok(Self { inner, out: vec![0; MAX_PACKET] })
    }

    /// Encodes exactly one 20 ms frame. `max_bytes` caps the packet (the server's MTU).
    pub fn encode(&mut self, frame: &[f32; FRAME_SAMPLES], max_bytes: usize) -> Result<Vec<u8>, CodecError> {
        let cap = max_bytes.clamp(1, MAX_PACKET);
        let n = self.inner.encode(frame, FRAME_SAMPLES, &mut self.out[..cap]).map_err(CodecError)?;
        Ok(self.out[..n].to_vec())
    }
}

/// Decodes one speaker's frames. Keep one per speaker: Opus decoders are stateful.
pub struct Decoder {
    inner: OpusDecoder,
}

impl Decoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self { inner: OpusDecoder::new(SAMPLE_RATE as i32, 1).map_err(CodecError)? })
    }

    /// Decodes a frame, or with `None` makes up a plausible one for a lost packet
    /// (Opus packet-loss concealment), so a gap doesn't click.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> Result<[f32; FRAME_SAMPLES], CodecError> {
        let mut out = [0f32; FRAME_SAMPLES];
        let n = self.inner.decode(packet.unwrap_or(&[]), FRAME_SAMPLES, &mut out).map_err(CodecError)?;
        if n != FRAME_SAMPLES {
            return Err(CodecError("frame is not 20 ms"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::from_hex;

    fn tone(freq: f32, frame_index: usize) -> [f32; FRAME_SAMPLES] {
        let mut f = [0f32; FRAME_SAMPLES];
        for (i, s) in f.iter_mut().enumerate() {
            let t = (frame_index * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
            *s = 0.5 * (2.0 * std::f32::consts::PI * freq * t).sin();
        }
        f
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn decodes_a_captured_svc_frame() {
        // NullPort404's first frame (captures 03 and 05).
        let frame = from_hex("780be4c136f8455dea51258bd593cf3a29592005396b51ee9559584a3e1b296cff00");
        let pcm = Decoder::new().unwrap().decode(Some(&frame)).unwrap();
        assert!(pcm.iter().all(|s| s.is_finite() && s.abs() <= 1.5));
    }

    #[test]
    fn a_tone_survives_encode_and_decode() {
        let mut enc = Encoder::new(Codec::Voip).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut last = [0f32; FRAME_SAMPLES];
        for i in 0..25 {
            // Half a second, so the encoder and decoder settle.
            let packet = enc.encode(&tone(440.0, i), MAX_PACKET).unwrap();
            assert!(!packet.is_empty() && packet.len() <= MAX_PACKET);
            // TOC frame-count code 0: one frame per packet, like SVC's.
            assert_eq!(packet[0] & 0x03, 0);
            last = dec.decode(Some(&packet)).unwrap();
        }
        let level = rms(&last);
        let expected = 0.5 / 2f32.sqrt();
        assert!((level - expected).abs() < expected * 0.3, "tone level {level}, expected about {expected}");
    }

    #[test]
    fn our_packets_use_svc_frame_settings() {
        // The TOC byte's top 5 bits select mode, bandwidth and frame size; SVC's frames are
        // 20 ms (configs 3, 7, 11, 13, 15, 19, 23, 27 or 31 in RFC 6716's table).
        let mut enc = Encoder::new(Codec::Voip).unwrap();
        let packet = enc.encode(&tone(300.0, 0), MAX_PACKET).unwrap();
        let config = packet[0] >> 3;
        assert!([3, 7, 11, 13, 15, 19, 23, 27, 31].contains(&config), "config {config} is not a 20 ms frame");
        assert_eq!(packet[0] & 0x04, 0, "mono");
    }

    #[test]
    fn concealment_fills_a_lost_frame() {
        let mut enc = Encoder::new(Codec::Voip).unwrap();
        let mut dec = Decoder::new().unwrap();
        for i in 0..10 {
            dec.decode(Some(&enc.encode(&tone(440.0, i), MAX_PACKET).unwrap())).unwrap();
        }
        let concealed = dec.decode(None).unwrap();
        assert!(rms(&concealed) > 0.01, "a lost frame mid-tone shouldn't be silence");
    }

    #[test]
    fn respects_the_packet_size_cap() {
        let mut enc = Encoder::new(Codec::Audio).unwrap();
        let packet = enc.encode(&tone(1000.0, 0), 40).unwrap();
        assert!(packet.len() <= 40);
    }
}
