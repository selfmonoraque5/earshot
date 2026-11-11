//! Primitive encodings used by SVC's messages (see PROTOCOL.md):
//! big-endian fixed-size numbers, Minecraft-style VarInts, 16-byte UUIDs and
//! VarInt-length-prefixed UTF-8 strings.

use std::fmt;

/// A player UUID as 16 raw bytes, in the order they appear on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    /// From the `u128` form the game uses, most significant byte first (the usual text order).
    pub fn from_u128(v: u128) -> Uuid {
        Uuid(v.to_be_bytes())
    }

    /// Parses the usual `8-4-4-4-12` hex form.
    pub fn parse(s: &str) -> Option<Uuid> {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 {
            return None;
        }
        let mut b = [0u8; 16];
        for (i, out) in b.iter_mut().enumerate() {
            *out = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Uuid(b))
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Why a message couldn't be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended before the field `what` was complete.
    Truncated(&'static str),
    /// A VarInt used more than 5 bytes.
    VarIntTooLong,
    /// A string wasn't valid UTF-8.
    BadUtf8,
    /// Bytes were left over after the last field.
    TrailingBytes(usize),
    /// A field held a value we don't accept (`what` names it).
    Invalid(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated(what) => write!(f, "input ended inside {what}"),
            DecodeError::VarIntTooLong => write!(f, "VarInt longer than 5 bytes"),
            DecodeError::BadUtf8 => write!(f, "string is not UTF-8"),
            DecodeError::TrailingBytes(n) => write!(f, "{n} unexpected bytes after the message"),
            DecodeError::Invalid(what) => write!(f, "invalid {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Reads fields from a byte slice, front to back.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> &'a [u8] {
        self.buf
    }

    pub fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], DecodeError> {
        if self.buf.len() < n {
            return Err(DecodeError::Truncated(what));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N, what)?.try_into().unwrap())
    }

    pub fn u8(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        Ok(self.array::<1>(what)?[0])
    }

    /// A one-byte boolean; anything other than 0 or 1 is rejected.
    pub fn bool(&mut self, what: &'static str) -> Result<bool, DecodeError> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::Invalid(what)),
        }
    }

    pub fn i32(&mut self, what: &'static str) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.array(what)?))
    }

    pub fn u64(&mut self, what: &'static str) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.array(what)?))
    }

    pub fn f32(&mut self, what: &'static str) -> Result<f32, DecodeError> {
        Ok(f32::from_be_bytes(self.array(what)?))
    }

    pub fn f64(&mut self, what: &'static str) -> Result<f64, DecodeError> {
        Ok(f64::from_be_bytes(self.array(what)?))
    }

    pub fn uuid(&mut self, what: &'static str) -> Result<Uuid, DecodeError> {
        Ok(Uuid(self.array(what)?))
    }

    /// A Minecraft VarInt: 7 bits per byte, least significant group first, high bit = "more".
    pub fn varint(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        let mut value = 0u32;
        for i in 0..5 {
            let byte = self.u8(what)?;
            value |= ((byte & 0x7f) as u32) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(DecodeError::VarIntTooLong)
    }

    /// VarInt length, then that many bytes.
    pub fn bytes(&mut self, what: &'static str) -> Result<&'a [u8], DecodeError> {
        let n = self.varint(what)? as usize;
        self.take(n, what)
    }

    /// VarInt length, then that many bytes of UTF-8.
    pub fn string(&mut self, what: &'static str) -> Result<String, DecodeError> {
        let b = self.bytes(what)?;
        String::from_utf8(b.to_vec()).map_err(|_| DecodeError::BadUtf8)
    }

    /// Fails unless every byte has been read.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.buf.is_empty() { Ok(()) } else { Err(DecodeError::TrailingBytes(self.buf.len())) }
    }
}

/// Appends fields to a byte vector.
#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn uuid(&mut self, v: &Uuid) -> &mut Self {
        self.raw(&v.0)
    }

    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    pub fn varint(&mut self, mut v: u32) -> &mut Self {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                return self;
            }
            self.buf.push(byte | 0x80);
        }
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.varint(v.len() as u32).raw(v)
    }

    pub fn string(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Decodes a hex string; used by tests and fixtures.
pub fn from_hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips_and_matches_minecraft_examples() {
        // Examples from the Minecraft protocol's VarInt definition.
        let cases: &[(u32, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (255, &[0xff, 0x01]),
            (25565, &[0xdd, 0xc7, 0x01]),
            (2_147_483_647, &[0xff, 0xff, 0xff, 0xff, 0x07]),
        ];
        for (value, bytes) in cases {
            let mut w = Writer::new();
            w.varint(*value);
            assert_eq!(w.buf, *bytes, "encode {value}");
            assert_eq!(Reader::new(bytes).varint("v").unwrap(), *value, "decode {value}");
        }
    }

    #[test]
    fn varint_rejects_six_bytes() {
        assert_eq!(Reader::new(&[0x80; 6]).varint("v"), Err(DecodeError::VarIntTooLong));
    }

    #[test]
    fn truncated_input_names_the_field() {
        assert_eq!(Reader::new(&[0, 0]).i32("port"), Err(DecodeError::Truncated("port")));
    }

    #[test]
    fn fixed_size_numbers_are_big_endian() {
        let mut w = Writer::new();
        w.i32(24454).f64(48.0).f32(48.0).u64(2);
        assert_eq!(w.buf, from_hex("00005f86 4048000000000000 42400000 0000000000000002"));
        let mut r = Reader::new(&w.buf);
        assert_eq!(r.i32("a").unwrap(), 24454);
        assert_eq!(r.f64("b").unwrap(), 48.0);
        assert_eq!(r.f32("c").unwrap(), 48.0);
        assert_eq!(r.u64("d").unwrap(), 2);
        r.finish().unwrap();
    }

    #[test]
    fn strings_are_varint_prefixed_utf8() {
        let mut w = Writer::new();
        w.string("NullPort404");
        assert_eq!(w.buf[0], 11);
        assert_eq!(Reader::new(&w.buf).string("name").unwrap(), "NullPort404");
    }

    #[test]
    fn uuid_formats_and_parses() {
        let u = Uuid::parse("c5c602ae-651d-3857-9a04-b3e776be1219").unwrap();
        assert_eq!(u.0[0], 0xc5);
        assert_eq!(u.to_string(), "c5c602ae-651d-3857-9a04-b3e776be1219");
    }

    #[test]
    fn bools_must_be_zero_or_one() {
        assert_eq!(Reader::new(&[1]).bool("b"), Ok(true));
        assert_eq!(Reader::new(&[2]).bool("b"), Err(DecodeError::Invalid("b")));
    }
}
