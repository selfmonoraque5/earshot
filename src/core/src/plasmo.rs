//! Plasmo Voice 2.x, the client side: what a server running Plasmo Voice sends over the game
//! connection (`plasmo:voice/v2`) and over UDP, and what Earshot answers.
//!
//! Written for Earshot from the protocol facts in PLASMO.md, which were read from Plasmo Voice's
//! source (plasmoapp/plasmo-voice, LGPL-3.0). No Plasmo Voice code is copied here.
//!
//! The order of a connection: the server asks for our player info; we answer with a 2048-bit RSA
//! public key; the server gives a secret and the UDP address; we ping that address with the
//! secret until the server sends its config, whose AES key is encrypted with our public key. From
//! then on audio flows over UDP: Opus frames, AES-128-CBC encrypted with a fresh IV each.

use std::collections::{HashMap, HashSet};

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

use crate::plugin::Codec;
use crate::wire::Uuid;

/// The channel every Plasmo Voice message goes over, and the one a client registers to say it
/// has the mod.
pub const CHANNEL: &str = "plasmo:voice/v2";
pub const INSTALLED: &str = "plasmo:voice/v2/installed";
/// The version we report: the server needs the same major version and at least its configured
/// minimum (2.0.0 unless changed).
pub const VERSION: &str = "2.1.17";
pub const MINECRAFT_VERSION: &str = "26.3";
/// The sample rate Earshot's microphone, Opus codec and mixer run at.
pub const SAMPLE_RATE: i32 = 48_000;

const MAGIC: i32 = 0x4e90_04e9;

/// Milliseconds since 1970, as Java's `System.currentTimeMillis` stamps each datagram.
fn epoch_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
/// Pings to the voice server until it answers with the config, this far apart.
const HELLO_EVERY_MS: u64 = 500;
/// Nothing from the voice server for this long: the connection is gone.
const TIMEOUT_MS: u64 = 30_000;

// ---------------------------------------------------------------------------------------------
// Java's DataOutput, big-endian, as Plasmo Voice's packets are written.

#[derive(Default)]
pub struct Out(pub Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    fn i16(&mut self, v: i16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn uuid(&mut self, v: &Uuid) -> &mut Self {
        self.0.extend_from_slice(&v.0);
        self
    }
    /// `writeUTF`: a 2-byte length, then Java's modified UTF-8.
    fn utf(&mut self, s: &str) -> &mut Self {
        let bytes = modified_utf8(s);
        let n = bytes.len().min(u16::MAX as usize);
        self.0.extend_from_slice(&(n as u16).to_be_bytes());
        self.0.extend_from_slice(&bytes[..n]);
        self
    }
    /// A length (int), then the bytes.
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.i32(b.len() as i32);
        self.0.extend_from_slice(b);
        self
    }
}

/// Java's modified UTF-8: NUL as two bytes, characters past U+FFFF as two 3-byte surrogates.
pub fn modified_utf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for unit in s.encode_utf16() {
        match unit {
            1..=0x7f => out.push(unit as u8),
            0 | 0x80..=0x7ff => {
                out.push(0xc0 | (unit >> 6) as u8);
                out.push(0x80 | (unit & 0x3f) as u8);
            }
            _ => {
                out.push(0xe0 | (unit >> 12) as u8);
                out.push(0x80 | ((unit >> 6) & 0x3f) as u8);
                out.push(0x80 | (unit & 0x3f) as u8);
            }
        }
    }
    out
}

fn from_modified_utf8(b: &[u8]) -> String {
    let mut units = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as u16;
        let (unit, len) = if c < 0x80 {
            (c, 1)
        } else if c & 0xe0 == 0xc0 && i + 1 < b.len() {
            (((c & 0x1f) << 6) | (b[i + 1] as u16 & 0x3f), 2)
        } else if c & 0xf0 == 0xe0 && i + 2 < b.len() {
            (((c & 0x0f) << 12) | ((b[i + 1] as u16 & 0x3f) << 6) | (b[i + 2] as u16 & 0x3f), 3)
        } else {
            (0xfffd, 1)
        };
        units.push(unit);
        i += len;
    }
    String::from_utf16_lossy(&units)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError(pub &'static str);

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

pub struct In<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ReadError> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.b.len()).ok_or(ReadError("packet too short"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool, ReadError> {
        Ok(self.u8()? != 0)
    }
    fn i16(&mut self) -> Result<i16, ReadError> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u16(&mut self) -> Result<u16, ReadError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, ReadError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, ReadError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, ReadError> {
        Ok(f64::from_bits(self.i64()? as u64))
    }
    fn uuid(&mut self) -> Result<Uuid, ReadError> {
        Ok(Uuid(self.take(16)?.try_into().unwrap()))
    }
    fn utf(&mut self) -> Result<String, ReadError> {
        let n = self.u16()? as usize;
        Ok(from_modified_utf8(self.take(n)?))
    }
    /// A count (int) in `0..=max`.
    fn count(&mut self, max: i32) -> Result<usize, ReadError> {
        let n = self.i32()?;
        if !(0..=max).contains(&n) {
            return Err(ReadError("count out of range"));
        }
        Ok(n as usize)
    }
    /// A length (int, `1..=max`), then the bytes.
    fn bytes(&mut self, max: i32) -> Result<Vec<u8>, ReadError> {
        let n = self.i32()?;
        if !(1..=max).contains(&n) {
            return Err(ReadError("length out of range"));
        }
        Ok(self.take(n as usize)?.to_vec())
    }
}

// ---------------------------------------------------------------------------------------------
// Ids, keys and the audio cipher.

/// Java's `UUID.nameUUIDFromBytes`: MD5 of the bytes, marked as a version-3 UUID. Plasmo Voice
/// names activations and source lines this way (`proximity_activation`).
pub fn name_uuid(name: &str) -> Uuid {
    use md5::{Digest, Md5};
    let mut h: [u8; 16] = Md5::digest(name.as_bytes()).into();
    h[6] = (h[6] & 0x0f) | 0x30;
    h[8] = (h[8] & 0x3f) | 0x80;
    Uuid(h)
}

/// The RSA key pair a connection proves itself with: the public half goes to the server, which
/// encrypts the audio key with it.
pub struct KeyPair {
    private: rsa::RsaPrivateKey,
    /// X.509 SubjectPublicKeyInfo, DER, as Java's `PublicKey.getEncoded` gives it.
    pub public_der: Vec<u8>,
}

impl KeyPair {
    pub fn generate(bits: usize) -> Result<Self, String> {
        use rsa::pkcs8::EncodePublicKey;
        let mut rng = rsa::rand_core::OsRng;
        let private = rsa::RsaPrivateKey::new(&mut rng, bits).map_err(|e| e.to_string())?;
        let public_der = private.to_public_key().to_public_key_der().map_err(|e| e.to_string())?.as_bytes().to_vec();
        Ok(Self { private, public_der })
    }

    /// `Cipher.getInstance("RSA")`: PKCS#1 v1.5 padding.
    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        self.private.decrypt(rsa::Pkcs1v15Encrypt, data).map_err(|e| e.to_string())
    }
}

/// AES-128-CBC with PKCS#7 padding; on the wire, the 16-byte IV then the ciphertext.
#[derive(Clone)]
pub struct AudioCipher {
    aes: Aes128,
}

impl AudioCipher {
    pub fn new(key: &[u8]) -> Option<Self> {
        (key.len() == 16).then(|| Self { aes: Aes128::new(GenericArray::from_slice(key)) })
    }

    pub fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
        let mut iv = [0u8; 16];
        getrandom::fill(&mut iv).map_err(|_| "no random IV".to_string())?;
        Ok(self.seal_with_iv(&iv, plain))
    }

    pub fn seal_with_iv(&self, iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let pad = 16 - plain.len() % 16;
        let mut data = plain.to_vec();
        data.extend(std::iter::repeat_n(pad as u8, pad));
        let mut out = Vec::with_capacity(16 + data.len());
        out.extend_from_slice(iv);
        let mut prev = *iv;
        for chunk in data.chunks_exact(16) {
            let mut block = [0u8; 16];
            for i in 0..16 {
                block[i] = chunk[i] ^ prev[i];
            }
            let mut b = GenericArray::from(block);
            self.aes.encrypt_block(&mut b);
            prev = b.into();
            out.extend_from_slice(&prev);
        }
        out
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        if sealed.len() < 32 || sealed.len() % 16 != 0 {
            return Err("audio data isn't whole AES blocks".into());
        }
        let (iv, body) = sealed.split_at(16);
        let mut prev: [u8; 16] = iv.try_into().unwrap();
        let mut out = Vec::with_capacity(body.len());
        for chunk in body.chunks_exact(16) {
            let mut b = GenericArray::clone_from_slice(chunk);
            self.aes.decrypt_block(&mut b);
            for i in 0..16 {
                out.push(b[i] ^ prev[i]);
            }
            prev = chunk.try_into().unwrap();
        }
        let pad = *out.last().unwrap() as usize;
        if !(1..=16).contains(&pad) || out[out.len() - pad..].iter().any(|b| *b as usize != pad) {
            return Err("bad AES padding".into());
        }
        out.truncate(out.len() - pad);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// The packets Earshot sends.

fn tcp(kind: u8, body: impl FnOnce(&mut Out)) -> Vec<u8> {
    let mut o = Out::default();
    o.u8(kind);
    body(&mut o);
    o.0
}

/// PlayerInfo (10): our voice state, versions and public key.
pub fn player_info(voice_disabled: bool, mic_muted: bool, public_key: &[u8]) -> Vec<u8> {
    tcp(10, |o| {
        o.bool(voice_disabled).bool(mic_muted).utf(MINECRAFT_VERSION).utf(VERSION).bytes(public_key);
    })
}

/// PlayerState (11): voice off, microphone muted.
pub fn player_state(voice_disabled: bool, mic_muted: bool) -> Vec<u8> {
    tcp(11, |o| {
        o.bool(voice_disabled).bool(mic_muted);
    })
}

/// PlayerAudioEnd (12): we stopped talking.
pub fn audio_end(sequence: u64, activation: &Uuid, distance: i16) -> Vec<u8> {
    tcp(12, |o| {
        o.i64(sequence as i64).uuid(activation).i16(distance);
    })
}

/// SourceInfoRequest (15): who is this sound source?
pub fn source_info_request(source: &Uuid) -> Vec<u8> {
    tcp(15, |o| {
        o.uuid(source);
    })
}

fn udp(kind: u8, secret: &Uuid, now_ms: i64, body: impl FnOnce(&mut Out)) -> Vec<u8> {
    let mut o = Out::default();
    o.i32(MAGIC).u8(kind).uuid(secret).i64(now_ms);
    body(&mut o);
    o.0
}

/// Ping (1), with the address we reach the server at while connecting.
pub fn ping(secret: &Uuid, now_ms: i64, server: Option<(&str, u16)>) -> Vec<u8> {
    udp(1, secret, now_ms, |o| {
        o.i64(now_ms);
        if let Some((ip, port)) = server {
            o.utf(ip).i16(port as i16);
        }
    })
}

/// PlayerAudio (2): one encrypted Opus frame.
pub fn player_audio(secret: &Uuid, now_ms: i64, sequence: u64, data: &[u8], activation: &Uuid, distance: i16) -> Vec<u8> {
    udp(2, secret, now_ms, |o| {
        o.i64(sequence as i64).bytes(data).uuid(activation).i16(distance).bool(false);
    })
}

// ---------------------------------------------------------------------------------------------
// What the server sends.

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerInfo {
    pub id: Uuid,
    pub nick: String,
    pub muted: bool,
    pub voice_disabled: bool,
    pub mic_muted: bool,
}

fn read_player(i: &mut In) -> Result<PlayerInfo, ReadError> {
    Ok(PlayerInfo { id: i.uuid()?, nick: i.utf()?, muted: i.bool()?, voice_disabled: i.bool()?, mic_muted: i.bool()? })
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodecInfo {
    pub name: String,
    pub params: Vec<(String, String)>,
}

fn read_codec(i: &mut In) -> Result<CodecInfo, ReadError> {
    let name = i.utf()?;
    let n = i.count(128)?;
    let mut params = Vec::with_capacity(n);
    for _ in 0..n {
        params.push((i.utf()?, i.utf()?));
    }
    Ok(CodecInfo { name, params })
}

fn skip_profile(i: &mut In) -> Result<(), ReadError> {
    i.uuid()?;
    i.utf()?;
    for _ in 0..i.count(100)? {
        i.utf()?;
        i.utf()?;
        i.utf()?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    pub id: Uuid,
    pub name: String,
    pub distances: Vec<i32>,
    pub default_distance: i32,
    pub proximity: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub server_id: Uuid,
    pub sample_rate: i32,
    pub mtu: i32,
    pub codec: Option<CodecInfo>,
    /// The audio key, still RSA-encrypted.
    pub encryption: Option<(String, Vec<u8>)>,
    pub activations: Vec<Activation>,
}

fn read_config(i: &mut In) -> Result<Config, ReadError> {
    let server_id = i.uuid()?;
    let sample_rate = i.i32()?;
    let mtu = i.i32()?;
    let codec = if i.bool()? { Some(read_codec(i)?) } else { None };
    let encryption = if i.bool()? { Some((i.utf()?, i.bytes(2048)?)) } else { None };
    // Source lines: read past them.
    for _ in 0..i.count(127)? {
        i.utf()?;
        i.utf()?;
        i.utf()?;
        i.f64()?;
        i.i32()?;
        if i.bool()? {
            for _ in 0..i.count(32767)? {
                skip_profile(i)?;
            }
        }
    }
    let mut activations = Vec::new();
    for _ in 0..i.count(127)? {
        let name = i.utf()?;
        i.utf()?; // translation
        i.utf()?; // icon
        let n = i.count(64)?;
        let mut distances = Vec::with_capacity(n);
        for _ in 0..n {
            distances.push(i.i32()?);
        }
        let default_distance = i.i32()?;
        let proximity = i.bool()?;
        i.bool()?; // transitive
        i.bool()?; // stereo supported
        if i.bool()? {
            read_codec(i)?;
        }
        i.i32()?; // weight
        activations.push(Activation { id: name_uuid(&format!("{name}_activation")), name, distances, default_distance, proximity });
    }
    // Permissions and the icon config follow; nothing here needs them.
    Ok(Config { server_id, sample_rate, mtu, codec, encryption, activations })
}

/// A sound source, as far as Earshot needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub state: u8,
    /// The player speaking, for a player's voice: placed where that player is.
    pub player: Option<Uuid>,
}

fn read_source(i: &mut In) -> Result<(Uuid, Source), ReadError> {
    let kind = i.utf()?;
    i.utf()?; // addon
    let id = i.uuid()?;
    if i.bool()? {
        i.utf()?; // name
    }
    let state = i.u8()?;
    if i.bool()? {
        read_codec(i)?;
    }
    i.bool()?; // stereo
    i.uuid()?; // line
    i.bool()?; // icon visible
    i.i32()?; // angle
    let player = if kind == "PLAYER" { Some(read_player(i)?.id) } else { None };
    Ok((id, Source { state, player }))
}

#[derive(Debug, Clone, PartialEq)]
pub enum FromServer {
    Connection { secret: Uuid, ip: String, port: u16 },
    PlayerInfoRequest,
    Config(Config),
    PlayerList(Vec<PlayerInfo>),
    PlayerInfoUpdate(PlayerInfo),
    PlayerDisconnect(Uuid),
    SourceInfo(Uuid, Source),
    SourceAudioEnd(Uuid),
    /// One Earshot doesn't act on.
    Other(u8),
}

pub fn decode_tcp(data: &[u8]) -> Result<FromServer, ReadError> {
    let mut i = In::new(data);
    let kind = i.u8()?;
    Ok(match kind {
        1 => FromServer::Connection { secret: i.uuid()?, ip: i.utf()?, port: i.i32()? as u16 },
        2 => FromServer::PlayerInfoRequest,
        3 => FromServer::Config(read_config(&mut i)?),
        7 => {
            let n = i.count(32767)?;
            let mut list = Vec::with_capacity(n);
            for _ in 0..n {
                list.push(read_player(&mut i)?);
            }
            FromServer::PlayerList(list)
        }
        8 => FromServer::PlayerInfoUpdate(read_player(&mut i)?),
        9 => FromServer::PlayerDisconnect(i.uuid()?),
        16 => {
            let (id, s) = read_source(&mut i)?;
            FromServer::SourceInfo(id, s)
        }
        18 => FromServer::SourceAudioEnd(i.uuid()?),
        other => FromServer::Other(other),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Datagram {
    Ping,
    SourceAudio { sequence: u64, data: Vec<u8>, source: Uuid, state: u8, distance: i16 },
    Other(u8),
}

pub fn decode_udp(b: &[u8]) -> Result<Datagram, ReadError> {
    let mut i = In::new(b);
    if i.i32()? != MAGIC {
        return Err(ReadError("not a Plasmo Voice packet"));
    }
    let kind = i.u8()?;
    i.uuid()?; // secret
    i.i64()?; // timestamp
    Ok(match kind {
        1 => Datagram::Ping,
        3 => Datagram::SourceAudio { sequence: i.i64()? as u64, data: i.bytes(2048)?, source: i.uuid()?, state: i.u8()?, distance: i.i16()? },
        other => Datagram::Other(other),
    })
}

// ---------------------------------------------------------------------------------------------
// The connection.

/// What the connection asks the engine to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Do {
    /// Send this over `plasmo:voice/v2`.
    Tcp(Vec<u8>),
    /// Send this datagram to the voice server.
    Udp(Vec<u8>),
    /// The voice server is at this address: open a socket to it.
    Open { host: String, port: u16 },
    /// Configured: encode with this application, frames up to `mtu` bytes, heard up to `distance`.
    Ready { codec: Codec, mtu: usize, distance: i32 },
    /// A voice frame (still Opus): `player` is who's talking when it's a player's voice.
    Audio { source: Uuid, player: Option<Uuid>, sequence: u64, opus: Vec<u8>, distance: f64 },
    /// The connection failed or went quiet.
    Lost(String),
}

#[derive(Default)]
pub struct Plasmo {
    secret: Option<Uuid>,
    target: Option<(String, u16)>,
    cipher: Option<AudioCipher>,
    activation: Option<Activation>,
    configured: bool,
    sources: HashMap<Uuid, Source>,
    asked: HashSet<Uuid>,
    pub players: HashMap<Uuid, PlayerInfo>,
    sequence: u64,
    last_heard: u64,
    last_hello: Option<u64>,
    problem: Option<String>,
    pub voice_disabled: bool,
    pub mic_muted: bool,
}

impl Plasmo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn connected(&self) -> bool {
        self.configured && self.cipher.is_some()
    }

    /// Between being told the address and being configured: the socket is wanted.
    pub fn connecting(&self) -> bool {
        self.secret.is_some()
    }

    pub fn target(&self) -> Option<(String, u16)> {
        self.target.clone()
    }

    /// A few words for the status line.
    pub fn describe(&self) -> String {
        if let Some(p) = &self.problem {
            return format!("Plasmo Voice: {p}");
        }
        match (self.secret.is_some(), self.connected()) {
            (_, true) => "connected (Plasmo Voice)".into(),
            (true, false) => "connecting to Plasmo Voice".into(),
            (false, false) => "Plasmo Voice server found, logging in".into(),
        }
    }

    /// A message on `plasmo:voice/v2`. `key` is our key pair, once made (it takes a moment);
    /// `game_host` is the host the player joined, for a server that names no voice address.
    pub fn on_tcp(&mut self, data: &[u8], key: Option<&KeyPair>, game_host: &str, now: u64) -> Vec<Do> {
        let msg = match decode_tcp(data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[earshot] couldn't read a Plasmo Voice message: {e}");
                return vec![];
            }
        };
        match msg {
            // The server repeats this until it has our key, so without one yet there's no harm
            // in waiting.
            FromServer::PlayerInfoRequest => key.map(|k| self.introduce(k)).unwrap_or_default(),
            FromServer::Connection { secret, ip, port } => {
                let host = if ip.is_empty() || ip == "0.0.0.0" { game_host.to_string() } else { ip };
                self.secret = Some(secret);
                self.target = Some((host.clone(), port));
                self.configured = false;
                self.problem = None;
                self.last_hello = None;
                self.last_heard = now;
                vec![Do::Open { host, port }]
            }
            FromServer::Config(c) => self.on_config(c, key),
            FromServer::PlayerList(list) => {
                self.players = list.into_iter().map(|p| (p.id, p)).collect();
                vec![]
            }
            FromServer::PlayerInfoUpdate(p) => {
                self.players.insert(p.id, p);
                vec![]
            }
            FromServer::PlayerDisconnect(id) => {
                self.players.remove(&id);
                vec![]
            }
            FromServer::SourceInfo(id, s) => {
                self.asked.remove(&id);
                self.sources.insert(id, s);
                vec![]
            }
            FromServer::SourceAudioEnd(_) | FromServer::Other(_) => vec![],
        }
    }

    fn on_config(&mut self, c: Config, key: Option<&KeyPair>) -> Vec<Do> {
        if c.sample_rate != SAMPLE_RATE {
            let why = format!("the server sends {} Hz audio; Earshot supports 48000 Hz", c.sample_rate);
            self.problem = Some(why.clone());
            return vec![Do::Lost(why)];
        }
        self.cipher = match (&c.encryption, key) {
            (Some((alg, sealed)), Some(k)) if alg.starts_with("AES") => match k.decrypt(sealed).ok().and_then(|aes| AudioCipher::new(&aes)) {
                Some(cipher) => Some(cipher),
                None => {
                    let why = "couldn't read the server's audio key".to_string();
                    self.problem = Some(why.clone());
                    return vec![Do::Lost(why)];
                }
            },
            (Some((alg, _)), _) if !alg.starts_with("AES") => {
                let why = format!("unknown encryption {alg}");
                self.problem = Some(why.clone());
                return vec![Do::Lost(why)];
            }
            _ => {
                let why = "the server sent no audio key".to_string();
                self.problem = Some(why.clone());
                return vec![Do::Lost(why)];
            }
        };
        // Proximity chat: the activation named "proximity", or else the first proximity one.
        self.activation = c.activations.iter().find(|a| a.name == "proximity").or_else(|| c.activations.iter().find(|a| a.proximity)).cloned();
        let distance = self.activation.as_ref().map_or(16, |a| a.default_distance);
        let mode = c.codec.as_ref().and_then(|k| k.params.iter().find(|(p, _)| p == "mode")).map(|(_, v)| v.as_str());
        let codec = match mode {
            Some("AUDIO") => Codec::Audio,
            Some("RESTRICTED_LOWDELAY") => Codec::RestrictedLowDelay,
            _ => Codec::Voip,
        };
        self.configured = true;
        self.problem = None;
        self.sequence = 0;
        vec![Do::Ready { codec, mtu: c.mtu.clamp(128, 1275) as usize, distance }]
    }

    /// A datagram from the voice server.
    pub fn on_udp(&mut self, b: &[u8], now: u64) -> Vec<Do> {
        let Ok(d) = decode_udp(b) else { return vec![] };
        self.last_heard = now;
        match d {
            Datagram::Ping => match &self.secret {
                Some(s) => vec![Do::Udp(ping(s, epoch_ms(), None))],
                None => vec![],
            },
            Datagram::SourceAudio { sequence, data, source, state, distance } => {
                let Some(cipher) = &self.cipher else { return vec![] };
                let Ok(opus) = cipher.open(&data) else { return vec![] };
                let mut out = Vec::new();
                // An unknown source, or one whose state changed: ask who it is.
                let known = self.sources.get(&source);
                if known.is_none_or(|s| s.state != state) && self.asked.insert(source) {
                    out.push(Do::Tcp(source_info_request(&source)));
                }
                let player = known.and_then(|s| s.player);
                out.push(Do::Audio { source, player, sequence, opus, distance: distance as f64 });
                out
            }
            Datagram::Other(_) => vec![],
        }
    }

    /// Our player info with `key`'s public half: the answer to the server's request, or sent
    /// unasked (the server takes it either way) when voice chat is switched back on.
    pub fn introduce(&self, key: &KeyPair) -> Vec<Do> {
        vec![Do::Tcp(player_info(self.voice_disabled, self.mic_muted, &key.public_der))]
    }

    /// Now and then: ping until configured, and notice a server that went quiet.
    pub fn poll(&mut self, now: u64) -> Vec<Do> {
        let Some(secret) = self.secret else { return vec![] };
        if now.saturating_sub(self.last_heard) > TIMEOUT_MS {
            let why = "the voice server stopped answering".to_string();
            *self = Self { players: std::mem::take(&mut self.players), voice_disabled: self.voice_disabled, mic_muted: self.mic_muted, ..Self::default() };
            self.problem = Some(why.clone());
            return vec![Do::Lost(why)];
        }
        if self.configured || self.last_hello.is_some_and(|t| now.saturating_sub(t) < HELLO_EVERY_MS) {
            return vec![];
        }
        self.last_hello = Some(now);
        let server = self.target.as_ref().map(|(h, p)| (h.as_str(), *p));
        vec![Do::Udp(ping(&secret, epoch_ms(), server))]
    }

    /// Our microphone's next Opus frame, sealed for the server.
    pub fn audio(&mut self, opus: &[u8]) -> Option<Vec<u8>> {
        let (Some(secret), Some(cipher), Some(act)) = (&self.secret, &self.cipher, &self.activation) else { return None };
        if !self.configured {
            return None;
        }
        let sealed = cipher.seal(opus).ok()?;
        self.sequence += 1;
        Some(player_audio(secret, epoch_ms(), self.sequence, &sealed, &act.id, act.default_distance as i16))
    }

    /// We stopped talking.
    pub fn stopped(&self) -> Option<Vec<u8>> {
        let act = self.activation.as_ref()?;
        self.connected().then(|| audio_end(self.sequence, &act.id, act.default_distance as i16))
    }
}


/// A pretend Plasmo Voice server's packets, for tests here and in the engine.
#[cfg(test)]
pub(crate) mod fake_server {
    use super::*;

    /// A config as a server writes it, built here field by field.
    pub fn config_packet(server_id: &Uuid, sealed_key: &[u8]) -> Vec<u8> {
        tcp(3, |o| {
            o.uuid(server_id).i32(48_000).i32(1024);
            o.bool(true).utf("opus").i32(2).utf("mode").utf("VOIP").utf("bitrate").utf("-1000");
            o.bool(true).utf("AES/CBC/PKCS5Padding").bytes(sealed_key);
            // One source line with a player list.
            o.i32(1).utf("proximity").utf("pv.line").utf("icon").0.extend_from_slice(&0.5f64.to_bits().to_be_bytes());
            o.i32(1).bool(true).i32(1).uuid(&Uuid([9; 16])).utf("Steve").i32(1).utf("textures").utf("v").utf("s");
            // Two activations, proximity second.
            o.i32(2);
            o.utf("group").utf("t").utf("i").i32(0).i32(0).bool(false).bool(false).bool(false).bool(false).i32(1);
            o.utf("proximity").utf("t").utf("i").i32(3).i32(8).i32(16).i32(32).i32(16).bool(true).bool(false).bool(true).bool(true).utf("opus").i32(0).i32(2);
            // Permissions and the icon config: not read.
            o.i32(1).utf("pv.activation.proximity").bool(true);
        })
    }

    pub fn connection(secret: &Uuid, ip: &str, port: u16) -> Vec<u8> {
        tcp(1, |o| {
            o.uuid(secret).utf(ip).i32(port as i32);
        })
    }

    pub fn request() -> Vec<u8> {
        tcp(2, |_| {})
    }

    pub fn player_source(source: &Uuid, player: &Uuid, nick: &str) -> Vec<u8> {
        tcp(16, |o| {
            o.utf("PLAYER").utf("pv").uuid(source).bool(false).u8(1).bool(false).bool(false).uuid(&Uuid([2; 16])).bool(true).i32(0);
            o.uuid(player).utf(nick).bool(false).bool(false).bool(false);
        })
    }

    pub fn source_audio(secret: &Uuid, cipher: &AudioCipher, sequence: u64, source: &Uuid, opus: &[u8]) -> Vec<u8> {
        udp(3, secret, 0, |o| {
            o.i64(sequence as i64).bytes(&cipher.seal(opus).unwrap()).uuid(source).u8(1).i16(16);
        })
    }

    /// The public key at the end of a PlayerInfo message.
    pub fn public_key_of(player_info: &[u8]) -> Vec<u8> {
        let mut i = In::new(player_info);
        assert_eq!(i.u8().unwrap(), 10);
        i.bool().unwrap();
        i.bool().unwrap();
        i.utf().unwrap();
        assert_eq!(i.utf().unwrap(), VERSION);
        i.bytes(2048).unwrap()
    }

    /// Seals `aes_key` to the client's public key, as the server does.
    pub fn seal_key(public_der: &[u8], aes_key: &[u8]) -> Vec<u8> {
        use rsa::pkcs8::DecodePublicKey;
        let public = rsa::RsaPublicKey::from_public_key_der(public_der).unwrap();
        public.encrypt(&mut rsa::rand_core::OsRng, rsa::Pkcs1v15Encrypt, aes_key).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fake_server::config_packet;
    use super::*;

    fn id(n: u8) -> Uuid {
        Uuid([n; 16])
    }

    #[test]
    fn modified_utf8_matches_java() {
        assert_eq!(modified_utf8("ab"), b"ab");
        assert_eq!(modified_utf8("\0"), [0xc0, 0x80]);
        assert_eq!(modified_utf8("é"), "é".as_bytes());
        // U+1F600: two surrogates, three bytes each.
        assert_eq!(modified_utf8("😀"), [0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80]);
        for s in ["", "proximity", "\0x", "日本", "😀!"] {
            assert_eq!(from_modified_utf8(&modified_utf8(s)), s);
        }
    }

    #[test]
    fn activation_ids_are_java_name_uuids() {
        assert_eq!(name_uuid("proximity_activation"), Uuid(*b"\x4a\xec\x07\xba\xd1\x09\x33\x45\x9a\x0a\x92\x02\x2a\x11\x6c\xd0"));
    }

    #[test]
    fn audio_cipher_is_aes_cbc_with_the_iv_first() {
        // NIST SP 800-38A, F.2.1, the first block.
        let hex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
        let c = AudioCipher::new(&hex("2b7e151628aed2a6abf7158809cf4f3c")).unwrap();
        let iv: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let sealed = c.seal_with_iv(&iv, &hex("6bc1bee22e409f96e93d7e117393172a"));
        assert_eq!(&sealed[..16], &iv);
        assert_eq!(&sealed[16..32], &hex("7649abac8119b246cee98e9b12e9197d")[..]);
        assert_eq!(sealed.len(), 48, "a whole block of padding follows");
        assert_eq!(c.open(&sealed).unwrap(), hex("6bc1bee22e409f96e93d7e117393172a"));
        for n in [1, 15, 16, 17, 200] {
            let msg = vec![7u8; n];
            assert_eq!(c.open(&c.seal(&msg).unwrap()).unwrap(), msg);
        }
        let mut bad = c.seal(b"opus").unwrap();
        *bad.last_mut().unwrap() ^= 1;
        assert!(c.open(&bad).is_err());
    }

    #[test]
    fn a_whole_connection_goes_through() {
        let key = KeyPair::generate(1024).unwrap();
        let mut p = Plasmo::new();
        // The request, before and after the key is ready.
        assert!(p.on_tcp(&tcp(2, |_| {}), None, "mc.example", 0).is_empty());
        let out = p.on_tcp(&tcp(2, |_| {}), Some(&key), "mc.example", 0);
        let Do::Tcp(info) = &out[0] else { panic!("{out:?}") };
        assert_eq!(info[0], 10);
        assert!(info.ends_with(&key.public_der));

        // The secret and address; "0.0.0.0" means the game server's host.
        let secret = id(5);
        let conn = tcp(1, |o| {
            o.uuid(&secret).utf("0.0.0.0").i32(60606);
        });
        assert_eq!(p.on_tcp(&conn, Some(&key), "mc.example", 10), vec![Do::Open { host: "mc.example".into(), port: 60606 }]);
        let Do::Udp(hello) = &p.poll(20)[0] else { panic!() };
        assert_eq!(&hello[..4], &MAGIC.to_be_bytes());
        assert_eq!(hello[4], 1);
        assert_eq!(&hello[5..21], &secret.0);
        assert!(p.poll(100).is_empty(), "pings are spaced out");

        // The config, with the audio key sealed to our public key.
        use rsa::pkcs8::DecodePublicKey;
        let public = rsa::RsaPublicKey::from_public_key_der(&key.public_der).unwrap();
        let aes_key = [3u8; 16];
        let sealed = public.encrypt(&mut rsa::rand_core::OsRng, rsa::Pkcs1v15Encrypt, &aes_key).unwrap();
        let out = p.on_tcp(&config_packet(&id(1), &sealed), Some(&key), "mc.example", 30);
        assert_eq!(out, vec![Do::Ready { codec: Codec::Voip, mtu: 1024, distance: 16 }]);
        assert!(p.connected());
        assert!(p.poll(2000).is_empty(), "no more pings once configured");

        // Our audio: sealed, with the proximity activation and its default distance.
        let pkt = p.audio(b"opus-frame").unwrap();
        let mut i = In::new(&pkt);
        assert_eq!(i.i32().unwrap(), MAGIC);
        assert_eq!(i.u8().unwrap(), 2);
        assert_eq!(i.uuid().unwrap(), secret);
        i.i64().unwrap();
        assert_eq!(i.i64().unwrap(), 1, "sequence numbers start at 1");
        let data = i.bytes(2048).unwrap();
        assert_eq!(AudioCipher::new(&aes_key).unwrap().open(&data).unwrap(), b"opus-frame");
        assert_eq!(i.uuid().unwrap(), name_uuid("proximity_activation"));
        assert_eq!(i.i16().unwrap(), 16);
        assert!(!i.bool().unwrap());
        assert_eq!(p.stopped().unwrap()[0], 12);

        // Someone else's voice: unknown at first (we ask), then a known player.
        let cipher = AudioCipher::new(&aes_key).unwrap();
        let voice = |seq: u64| {
            udp(3, &secret, 0, |o| {
                o.i64(seq as i64).bytes(&cipher.seal(b"hi").unwrap()).uuid(&id(7)).u8(1).i16(16);
            })
        };
        let out = p.on_udp(&voice(1), 50);
        assert_eq!(out[0], Do::Tcp(source_info_request(&id(7))));
        assert_eq!(out[1], Do::Audio { source: id(7), player: None, sequence: 1, opus: b"hi".to_vec(), distance: 16.0 });
        let info = tcp(16, |o| {
            o.utf("PLAYER").utf("pv").uuid(&id(7)).bool(false).u8(1).bool(false).bool(false).uuid(&id(2)).bool(true).i32(0);
            o.uuid(&id(8)).utf("Alex").bool(false).bool(false).bool(false);
        });
        p.on_tcp(&info, Some(&key), "mc.example", 60);
        let out = p.on_udp(&voice(2), 70);
        assert_eq!(out, vec![Do::Audio { source: id(7), player: Some(id(8)), sequence: 2, opus: b"hi".to_vec(), distance: 16.0 }]);

        // A server ping gets one back.
        let out = p.on_udp(&ping(&secret, 5, None), 80);
        assert!(matches!(&out[..], [Do::Udp(b)] if b[4] == 1));

        // Silence for too long: lost.
        assert!(matches!(&p.poll(80 + TIMEOUT_MS + 1)[..], [Do::Lost(_)]));
        assert!(!p.connected());
    }

    #[test]
    fn other_sample_rates_are_refused_plainly() {
        let mut p = Plasmo::new();
        let c = tcp(3, |o| {
            o.uuid(&id(1)).i32(24_000).i32(1024).bool(false).bool(false).i32(0).i32(0);
        });
        assert!(matches!(&p.on_tcp(&c, None, "h", 0)[..], [Do::Lost(w)] if w.contains("24000")));
    }

    #[test]
    fn short_or_foreign_packets_are_errors_not_panics() {
        assert!(decode_tcp(&[]).is_err());
        assert!(decode_tcp(&[3, 1, 2]).is_err());
        assert!(decode_udp(&[0, 1, 2, 3, 4]).is_err());
        assert!(decode_udp(&[0x4e, 0x90, 0x04, 0xe9, 3]).is_err());
        let mut p = Plasmo::new();
        assert!(p.on_udp(b"junk", 0).is_empty());
    }
}
