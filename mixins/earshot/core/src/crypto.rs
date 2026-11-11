//! AES-128-GCM as SVC's voice packets use it (PROTOCOL.md, section 2):
//! `nonce (12 bytes) || ciphertext || tag (16 bytes)`, keyed with the 16-byte secret the server
//! sends in `voicechat:secret`, and no additional authenticated data.
//!
//! Horizon's lock file has the `aes` block cipher but no GCM crate, so GCM itself is implemented
//! here, following NIST SP 800-38D: counter mode for confidentiality, GHASH for the tag.

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};

pub const KEY_LEN: usize = 16;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// Bytes that sealing adds to a plaintext.
pub const OVERHEAD: usize = NONCE_LEN + TAG_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// Shorter than a nonce plus a tag.
    TooShort,
    /// The tag didn't match: wrong key, or the packet was altered.
    BadTag,
    /// The operating system couldn't supply random bytes for a nonce.
    NoRandom,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CryptoError::TooShort => "encrypted packet too short",
            CryptoError::BadTag => "authentication tag mismatch",
            CryptoError::NoRandom => "no random bytes available",
        })
    }
}

impl std::error::Error for CryptoError {}

/// One player's voice key.
#[derive(Clone)]
pub struct VoiceKey {
    aes: Aes128,
    /// GHASH key: AES of the all-zero block.
    h: u128,
}

impl VoiceKey {
    pub fn new(secret: &[u8; KEY_LEN]) -> Self {
        let aes = Aes128::new(GenericArray::from_slice(secret));
        let h = u128::from_be_bytes(encrypt_block(&aes, [0; 16]));
        Self { aes, h }
    }

    /// Encrypts with a fresh random nonce, giving `nonce || ciphertext || tag`.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|_| CryptoError::NoRandom)?;
        Ok(self.seal_with_nonce(&nonce, plaintext))
    }

    /// Encrypts with the given nonce. A nonce must never repeat under one key; [`seal`](Self::seal)
    /// picks a random one, as the captured packets suggest SVC does.
    pub fn seal_with_nonce(&self, nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(OVERHEAD + plaintext.len());
        out.extend_from_slice(nonce);
        out.extend_from_slice(plaintext);
        let j0 = j0(nonce);
        self.ctr(j0, &mut out[NONCE_LEN..]);
        let tag = self.tag(j0, &[], &out[NONCE_LEN..]);
        out.extend_from_slice(&tag);
        out
    }

    /// Checks the tag and decrypts `nonce || ciphertext || tag`.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.open_with_aad(sealed, &[])
    }

    fn open_with_aad(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() < OVERHEAD {
            return Err(CryptoError::TooShort);
        }
        let (nonce, rest) = sealed.split_at(NONCE_LEN);
        let (ciphertext, tag) = rest.split_at(rest.len() - TAG_LEN);
        let j0 = j0(nonce.try_into().unwrap());
        let expected = self.tag(j0, aad, ciphertext);
        // Compare without an early exit, so timing doesn't reveal how many bytes matched.
        if expected.iter().zip(tag).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
            return Err(CryptoError::BadTag);
        }
        let mut plain = ciphertext.to_vec();
        self.ctr(j0, &mut plain);
        Ok(plain)
    }

    /// Counter mode starting at inc32(J0), XORed in place.
    fn ctr(&self, j0: [u8; 16], data: &mut [u8]) {
        let mut counter = j0;
        for chunk in data.chunks_mut(16) {
            inc32(&mut counter);
            let keystream = encrypt_block(&self.aes, counter);
            for (d, k) in chunk.iter_mut().zip(keystream) {
                *d ^= k;
            }
        }
    }

    /// Tag = E(K, J0) xor GHASH(H, A, C).
    fn tag(&self, j0: [u8; 16], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
        let mut y = 0u128;
        for data in [aad, ciphertext] {
            for chunk in data.chunks(16) {
                let mut block = [0u8; 16];
                block[..chunk.len()].copy_from_slice(chunk);
                y = gf_mul(y ^ u128::from_be_bytes(block), self.h);
            }
        }
        let lengths = ((aad.len() as u128 * 8) << 64) | (ciphertext.len() as u128 * 8);
        y = gf_mul(y ^ lengths, self.h);
        (u128::from_be_bytes(encrypt_block(&self.aes, j0)) ^ y).to_be_bytes()
    }
}

fn encrypt_block(aes: &Aes128, block: [u8; 16]) -> [u8; 16] {
    let mut b = GenericArray::from(block);
    aes.encrypt_block(&mut b);
    b.into()
}

/// J0 for a 96-bit nonce: the nonce followed by a 32-bit block counter of 1.
fn j0(nonce: &[u8; NONCE_LEN]) -> [u8; 16] {
    let mut j = [0u8; 16];
    j[..NONCE_LEN].copy_from_slice(nonce);
    j[15] = 1;
    j
}

/// Increments the last 32 bits of the counter block, wrapping.
fn inc32(block: &mut [u8; 16]) {
    let c = u32::from_be_bytes(block[12..].try_into().unwrap()).wrapping_add(1);
    block[12..].copy_from_slice(&c.to_be_bytes());
}

/// Multiplication in GF(2^128) with GCM's bit order (SP 800-38D, Algorithm 1).
/// Branch-free on the data, using masks instead of `if`s.
fn gf_mul(x: u128, y: u128) -> u128 {
    const R: u128 = 0xe1 << 120;
    let (mut z, mut v) = (0u128, y);
    for i in 0..128 {
        let bit = (x >> (127 - i)) & 1;
        z ^= v & 0u128.wrapping_sub(bit);
        let lsb = v & 1;
        v = (v >> 1) ^ (R & 0u128.wrapping_sub(lsb));
    }
    z
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::from_hex;

    fn key(hex: &str) -> VoiceKey {
        VoiceKey::new(&from_hex(hex).try_into().unwrap())
    }

    fn nonce(hex: &str) -> [u8; 12] {
        from_hex(hex).try_into().unwrap()
    }

    // Test cases 2-4 from the GCM specification (McGrew & Viega), as used by NIST.
    const K3: &str = "feffe9928665731c6d6a8f9467308308";
    const IV3: &str = "cafebabefacedbaddecaf888";
    const P3: &str = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
                      1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255";
    const C3: &str = "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e\
                      21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985";

    #[test]
    fn gcm_spec_test_case_2() {
        let k = key("00000000000000000000000000000000");
        let sealed = k.seal_with_nonce(&nonce("000000000000000000000000"), &[0u8; 16]);
        assert_eq!(&sealed[12..], &from_hex("0388dace60b6a392f328c2b971b2fe78 ab6e47d42cec13bdf53a67b21257bddf")[..]);
    }

    #[test]
    fn gcm_spec_test_case_3() {
        let sealed = key(K3).seal_with_nonce(&nonce(IV3), &from_hex(P3));
        let expected = [from_hex(C3), from_hex("4d5c2af327cd64a62cf35abd2ba6fab4")].concat();
        assert_eq!(&sealed[12..], &expected[..]);
        assert_eq!(key(K3).open(&sealed).unwrap(), from_hex(P3));
    }

    #[test]
    fn gcm_spec_test_case_4_with_aad_and_partial_block() {
        // 60-byte plaintext (a partial last block) and 20 bytes of AAD exercise the padding paths.
        let plain = &from_hex(P3)[..60];
        let cipher = &from_hex(C3)[..60];
        let sealed = [from_hex(IV3), cipher.to_vec(), from_hex("5bc94fbc3221a5db94fae95ae7121a47")].concat();
        let aad = from_hex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        assert_eq!(key(K3).open_with_aad(&sealed, &aad).unwrap(), plain);
    }

    #[test]
    fn rejects_altered_packets_and_wrong_keys() {
        let k = key(K3);
        let mut sealed = k.seal(b"hello").unwrap();
        assert_eq!(key("00000000000000000000000000000000").open(&sealed), Err(CryptoError::BadTag));
        sealed[13] ^= 1;
        assert_eq!(k.open(&sealed), Err(CryptoError::BadTag));
        assert_eq!(k.open(&sealed[..27]), Err(CryptoError::TooShort));
    }

    #[test]
    fn random_nonces_differ_and_round_trip() {
        let k = key(K3);
        let (a, b) = (k.seal(b"voice").unwrap(), k.seal(b"voice").unwrap());
        assert_ne!(a[..12], b[..12]);
        assert_eq!(a.len(), 5 + OVERHEAD);
        assert_eq!(k.open(&a).unwrap(), b"voice");
    }

    #[test]
    fn opens_a_real_svc_keep_alive() {
        // Capture 03: server -> client UDP keep-alive (0x08), after the 0xff marker and VarInt
        // length 0x1d. Key = that session's secret from voicechat:secret.
        let k = key("91f06606a6b37a6a2fe55466a7dec84f");
        let sealed = from_hex("e2c3b3af9cd0caf0cee44df3365db6845c3ea15c5def1db0e2ac334524");
        assert_eq!(k.open(&sealed).unwrap(), [0x08]);
    }
}
