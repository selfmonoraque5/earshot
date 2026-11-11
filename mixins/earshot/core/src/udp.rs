//! SVC's UDP voice packets (PROTOCOL.md, section 2).
//!
//! On the wire:
//! - client -> server: `0xff`, sender UUID (16 bytes), VarInt n, n sealed bytes
//! - server -> client: `0xff`, VarInt n, n sealed bytes
//!
//! The sealed bytes are AES-128-GCM ([`crate::crypto`]) with the receiving or sending player's
//! key. Inside is one message whose first byte is its type.

use crate::crypto::{CryptoError, VoiceKey};
use crate::wire::{DecodeError, Reader, Uuid, Writer};

const MAGIC: u8 = 0xff;

/// One decrypted voice message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// 0x01, client -> server: a frame of our microphone.
    Mic(MicFrame),
    /// 0x02, server -> client: a frame of another player's voice, nearby.
    PlayerSound(PlayerSound),
    /// 0x03, server -> client: a frame from someone in our group (any distance, capture 06).
    GroupSound(GroupSound),
    /// 0x05, client -> server: log in to the voice server with the secret.
    Authenticate { player: Uuid, secret: [u8; 16] },
    /// 0x06, server -> client: log-in accepted.
    AuthenticateAck,
    /// 0x08, both ways, about every `keep_alive_ms`.
    KeepAlive,
    /// 0x09, client -> server, after the ack: checks the path works both ways.
    ConnectionCheck,
    /// 0x0a, server -> client: answer to 0x09.
    ConnectionCheckAck,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MicFrame {
    pub opus: Vec<u8>,
    /// Counts frames, +1 each 20 ms frame.
    pub sequence: u64,
    /// Always 0 in the captures (whisper? see PROTOCOL.md).
    pub flag: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSound {
    /// The speaker's UUID in every capture; possibly a group "channel" otherwise.
    pub channel: Uuid,
    pub sender: Uuid,
    /// Byte-identical to the speaker's own frame: the server doesn't re-encode.
    pub opus: Vec<u8>,
    pub sequence: u64,
    /// The server's voice distance (48.0 by default).
    pub distance: f32,
    pub flag: u8,
}

/// A group member's voice: as [`PlayerSound`] but with no distance (groups have no range).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSound {
    /// The speaker's UUID in all 1061 captured packets.
    pub channel: Uuid,
    pub sender: Uuid,
    pub opus: Vec<u8>,
    pub sequence: u64,
    pub flag: u8,
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Message::Mic(m) => {
                w.u8(0x01).bytes(&m.opus).u64(m.sequence).u8(m.flag);
            }
            Message::PlayerSound(s) => {
                w.u8(0x02).uuid(&s.channel).uuid(&s.sender).bytes(&s.opus).u64(s.sequence).f32(s.distance).u8(s.flag);
            }
            Message::GroupSound(s) => {
                w.u8(0x03).uuid(&s.channel).uuid(&s.sender).bytes(&s.opus).u64(s.sequence).u8(s.flag);
            }
            Message::Authenticate { player, secret } => {
                w.u8(0x05).uuid(player).raw(secret);
            }
            Message::AuthenticateAck => {
                w.u8(0x06);
            }
            Message::KeepAlive => {
                w.u8(0x08);
            }
            Message::ConnectionCheck => {
                w.u8(0x09);
            }
            Message::ConnectionCheckAck => {
                w.u8(0x0a);
            }
        }
        w.into_bytes()
    }

    /// Reads the fields each message is known to have and ignores anything after them.
    ///
    /// Fabric's SVC sends exactly these fields (captures 03-05), but a Paper server running the
    /// same SVC version sent replies with extra bytes after them (observed 2026-11-05). Being
    /// strict there threw away every such packet, so missing fields are errors and extra bytes
    /// are not.
    pub fn decode(plain: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(plain);
        let msg = match r.u8("type")? {
            0x01 => Message::Mic(MicFrame {
                opus: r.bytes("opus")?.to_vec(),
                sequence: r.u64("sequence")?,
                flag: r.u8("flag")?,
            }),
            0x02 => Message::PlayerSound(PlayerSound {
                channel: r.uuid("channel")?,
                sender: r.uuid("sender")?,
                opus: r.bytes("opus")?.to_vec(),
                sequence: r.u64("sequence")?,
                distance: r.f32("distance")?,
                flag: r.u8("flag")?,
            }),
            0x03 => Message::GroupSound(GroupSound {
                channel: r.uuid("channel")?,
                sender: r.uuid("sender")?,
                opus: r.bytes("opus")?.to_vec(),
                sequence: r.u64("sequence")?,
                flag: r.u8("flag")?,
            }),
            0x05 => Message::Authenticate {
                player: r.uuid("player")?,
                secret: r.take(16, "secret")?.try_into().unwrap(),
            },
            // These four mean what their type says and have no fields.
            0x06 => Message::AuthenticateAck,
            0x08 => Message::KeepAlive,
            0x09 => Message::ConnectionCheck,
            0x0a => Message::ConnectionCheckAck,
            _ => return Err(DecodeError::Invalid("message type")),
        };
        Ok(msg)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PacketError {
    Decode(DecodeError),
    Crypto(CryptoError),
}

impl From<DecodeError> for PacketError {
    fn from(e: DecodeError) -> Self {
        PacketError::Decode(e)
    }
}

impl From<CryptoError> for PacketError {
    fn from(e: CryptoError) -> Self {
        PacketError::Crypto(e)
    }
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketError::Decode(e) => write!(f, "bad voice packet: {e}"),
            PacketError::Crypto(e) => write!(f, "bad voice packet: {e}"),
        }
    }
}

impl std::error::Error for PacketError {}

/// Builds a client -> server packet.
pub fn seal_to_server(key: &VoiceKey, sender: &Uuid, msg: &Message) -> Result<Vec<u8>, CryptoError> {
    let sealed = key.seal(&msg.encode())?;
    let mut w = Writer::new();
    w.u8(MAGIC).uuid(sender).bytes(&sealed);
    Ok(w.into_bytes())
}

/// Builds a server -> client packet.
pub fn seal_to_client(key: &VoiceKey, msg: &Message) -> Result<Vec<u8>, CryptoError> {
    let sealed = key.seal(&msg.encode())?;
    let mut w = Writer::new();
    w.u8(MAGIC).bytes(&sealed);
    Ok(w.into_bytes())
}

/// Reads a client -> server packet's header, so a server can find the sender's key.
/// Returns the sender and the sealed bytes.
pub fn split_from_client(packet: &[u8]) -> Result<(Uuid, &[u8]), DecodeError> {
    let mut r = Reader::new(packet);
    if r.u8("magic")? != MAGIC {
        return Err(DecodeError::Invalid("magic"));
    }
    let sender = r.uuid("sender")?;
    let sealed = r.bytes("sealed payload")?;
    r.finish()?;
    Ok((sender, sealed))
}

/// Opens a server -> client packet with our key.
pub fn open_from_server(key: &VoiceKey, packet: &[u8]) -> Result<Message, PacketError> {
    Ok(Message::decode(&open_raw_from_server(key, packet)?)?)
}

/// Checks and decrypts a server -> client packet without decoding the message inside.
pub fn open_raw_from_server(key: &VoiceKey, packet: &[u8]) -> Result<Vec<u8>, PacketError> {
    let mut r = Reader::new(packet);
    if r.u8("magic")? != MAGIC {
        return Err(DecodeError::Invalid("magic").into());
    }
    let sealed = r.bytes("sealed payload")?;
    r.finish()?;
    Ok(key.open(sealed)?)
}

/// Opens a client -> server packet, once the sender's key is known.
pub fn open_from_client(key: &VoiceKey, packet: &[u8]) -> Result<(Uuid, Message), PacketError> {
    let (sender, sealed) = split_from_client(packet)?;
    Ok((sender, Message::decode(&key.open(sealed)?)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::from_hex;

    /// Capture 03's secret (NullPort404) and capture 05's secret for AIOMarcus.
    fn key_03() -> VoiceKey {
        VoiceKey::new(&from_hex("91f06606a6b37a6a2fe55466a7dec84f").try_into().unwrap())
    }
    fn key_05_aio() -> VoiceKey {
        VoiceKey::new(&from_hex("24d290e8f7bb3b0393c8e00384f80927").try_into().unwrap())
    }
    fn nullport() -> Uuid {
        Uuid::parse("c5c602ae-651d-3857-9a04-b3e776be1219").unwrap()
    }

    // Raw UDP payloads exactly as captured.
    const AUTH_03: &str = "ffc5c602ae651d38579a04b3e776be12193d872d8ad54bab2cc9278c0d3a2c85544c13effa66e8196359501bc15d54272f6343f0156d65371a514155148ad71a106ef54190c9a95fc726e013115a1a";
    const AUTH_ACK_03: &str = "ff1daa0204e0f4e32a18213b3d3986ae31c82f6515cb015c8b8a37f468f81f";
    const CHECK_03: &str = "ffc5c602ae651d38579a04b3e776be12191dbb2100369886a188a2fe273b5f0e090bd9e869e3947c4f3a9ccdddec79";
    const CHECK_ACK_03: &str = "ff1d9e56f03583dd63fa05dd9f56d1132aaf54dd45f14cf088447985d407b3";
    const KEEP_ALIVE_03: &str = "ff1de2c3b3af9cd0caf0cee44df3365db6845c3ea15c5def1db0e2ac334524";
    const MIC_03: &str = "ffc5c602ae651d38579a04b3e776be1219490493ebcc0028336c39a86f11cb0010e5597e56968c5c58684e5aa88b9df0032f3f0190f5fd8e038154387594fd9b4f12b861a5bc685adf97b2e409aa72ee4db36b90444c56b040e3b1";
    const SOUND_05_TO_AIO: &str = "ff6d2d1fcbf42bb03b6026c91b8b8a441615d43ae8037c64875cd122d58f7d762241c73aea9c990075dd4ae1a8335cbb73ad15d5b1b4596d93aeb6397f943a3eed6e7c1a8df667928d8c460cc64c36f312eae606db9c3dfb93535a8fba73c8b35ed55cd55c8e7326b8c9ac29781f1e";
    /// The first Opus frame NullPort404 sent in capture 05 (also the first in capture 03).
    const FIRST_FRAME: &str = "780be4c136f8455dea51258bd593cf3a29592005396b51ee9559584a3e1b296cff00";

    #[test]
    fn opens_the_captured_handshake() {
        let k = key_03();
        let (sender, auth) = open_from_client(&k, &from_hex(AUTH_03)).unwrap();
        assert_eq!(sender, nullport());
        assert_eq!(
            auth,
            Message::Authenticate { player: nullport(), secret: from_hex("91f06606a6b37a6a2fe55466a7dec84f").try_into().unwrap() }
        );
        assert_eq!(open_from_server(&k, &from_hex(AUTH_ACK_03)).unwrap(), Message::AuthenticateAck);
        assert_eq!(open_from_client(&k, &from_hex(CHECK_03)).unwrap().1, Message::ConnectionCheck);
        assert_eq!(open_from_server(&k, &from_hex(CHECK_ACK_03)).unwrap(), Message::ConnectionCheckAck);
        assert_eq!(open_from_server(&k, &from_hex(KEEP_ALIVE_03)).unwrap(), Message::KeepAlive);
    }

    #[test]
    fn opens_a_captured_mic_frame() {
        let (_, msg) = open_from_client(&key_03(), &from_hex(MIC_03)).unwrap();
        let Message::Mic(m) = msg else { panic!("expected mic frame, got {msg:?}") };
        assert_eq!(m.sequence, 0);
        assert_eq!(m.flag, 0);
        assert_eq!(m.opus, from_hex(FIRST_FRAME));
        assert_eq!(m.opus[0], 0x78, "Opus TOC: hybrid fullband 20 ms");
    }

    #[test]
    fn opens_a_captured_sound_from_another_player() {
        let msg = open_from_server(&key_05_aio(), &from_hex(SOUND_05_TO_AIO)).unwrap();
        let Message::PlayerSound(s) = msg else { panic!("expected player sound, got {msg:?}") };
        assert_eq!(s.sender, nullport());
        assert_eq!(s.channel, nullport());
        assert_eq!(s.opus, from_hex(FIRST_FRAME), "relayed byte-for-byte");
        assert_eq!((s.sequence, s.distance, s.flag), (0, 48.0, 0));
    }

    #[test]
    fn opens_a_captured_group_sound() {
        // Capture 06 at 532.38 s: NullPort404's voice to AIOMarcus, both in "TestPassword",
        // sealed with AIOMarcus's key of that session.
        let key = VoiceKey::new(&from_hex("12fb9ec059cb64ca9ff2a46af90cd2ad").try_into().unwrap());
        let raw = from_hex("ff4acbce969fa12d548b400d9c1e97dd69ad13fece9c9985f87fb9d3e9a5a7b0c8fe05f3ba75a79f63035dec81a9978966a84b45c91372f1054933f823590473a51303ba8cfe5a1cbd66b47f");
        let msg = open_from_server(&key, &raw).unwrap();
        let Message::GroupSound(g) = &msg else { panic!("expected group sound, got {msg:?}") };
        assert_eq!((g.sender, g.channel), (nullport(), nullport()));
        assert_eq!((g.opus.as_slice(), g.sequence, g.flag), (&[0xf8, 0xff, 0xfe][..], 275, 0));
        assert_eq!(Message::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn wrong_key_is_a_crypto_error() {
        assert_eq!(open_from_server(&key_05_aio(), &from_hex(KEEP_ALIVE_03)), Err(PacketError::Crypto(CryptoError::BadTag)));
    }

    #[test]
    fn every_message_round_trips_through_a_sealed_packet() {
        let k = key_03();
        let msgs = [
            Message::Mic(MicFrame { opus: vec![0x78, 1, 2, 3], sequence: 41, flag: 0 }),
            Message::PlayerSound(PlayerSound {
                channel: nullport(),
                sender: nullport(),
                opus: vec![0x78; 200], // 200 bytes: a two-byte VarInt length
                sequence: u64::MAX,
                distance: 48.0,
                flag: 0,
            }),
            Message::Authenticate { player: nullport(), secret: [9; 16] },
            Message::AuthenticateAck,
            Message::KeepAlive,
            Message::ConnectionCheck,
            Message::ConnectionCheckAck,
        ];
        for m in msgs {
            let to_server = seal_to_server(&k, &nullport(), &m).unwrap();
            assert_eq!(open_from_client(&k, &to_server).unwrap(), (nullport(), m.clone()));
            let to_client = seal_to_client(&k, &m).unwrap();
            assert_eq!(open_from_server(&k, &to_client).unwrap(), m);
        }
    }

    #[test]
    fn sealed_sizes_match_the_captures() {
        // A 1-byte message gives a 31-byte server -> client packet and a 47-byte client -> server one.
        let k = key_03();
        assert_eq!(seal_to_client(&k, &Message::KeepAlive).unwrap().len(), 31);
        assert_eq!(seal_to_server(&k, &nullport(), &Message::KeepAlive).unwrap().len(), 47);
    }

    #[test]
    fn rejects_garbage() {
        let k = key_03();
        assert!(open_from_server(&k, &[]).is_err());
        assert!(open_from_server(&k, &[0x00, 0x01, 0x02]).is_err());
        assert!(split_from_client(&from_hex("ff00")).is_err());
        assert_eq!(Message::decode(&[0x42]), Err(DecodeError::Invalid("message type")));
        // Missing fields are still errors.
        let sound = Message::PlayerSound(PlayerSound {
            channel: nullport(),
            sender: nullport(),
            opus: vec![0x78; 10],
            sequence: 1,
            distance: 48.0,
            flag: 0,
        })
        .encode();
        assert!(Message::decode(&sound[..sound.len() - 1]).is_err());
    }

    #[test]
    fn voice_with_extra_bytes_after_its_fields_still_plays() {
        // What a Paper server's packets looked like: the known fields, then more bytes.
        let sound = PlayerSound { channel: nullport(), sender: nullport(), opus: vec![0x78, 1, 2], sequence: 9, distance: 48.0, flag: 0 };
        let mut plain = Message::PlayerSound(sound.clone()).encode();
        plain.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(Message::decode(&plain), Ok(Message::PlayerSound(sound)));
        let mut mic = Message::Mic(MicFrame { opus: vec![1], sequence: 0, flag: 0 }).encode();
        mic.push(0);
        assert!(matches!(Message::decode(&mic), Ok(Message::Mic(_))));
    }

    #[test]
    fn signal_messages_tolerate_a_body() {
        // A server that adds data after an ack's type still gets through the handshake.
        assert_eq!(Message::decode(&[0x06, 1, 2, 3]), Ok(Message::AuthenticateAck));
        assert_eq!(Message::decode(&[0x0a, 0xff]), Ok(Message::ConnectionCheckAck));
        assert_eq!(Message::decode(&[0x08, 0x00]), Ok(Message::KeepAlive));
    }

    /// Opens a packet in two steps, for diagnostics: the decrypted bytes come back even when
    /// they don't decode.
    #[test]
    fn open_raw_returns_undecodable_plaintext() {
        let k = key_03();
        let packet = seal_to_client(&k, &Message::KeepAlive).unwrap();
        assert_eq!(open_raw_from_server(&k, &packet).unwrap(), [0x08]);
    }
}
