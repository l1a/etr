// SPDX-License-Identifier: GPL-3.0-or-later
//! AEAD cipher wrappers: AES-256-GCM and ChaCha20-Poly1305.
//!
//! Each cipher is keyed once at construction and then used for many
//! encrypt/decrypt calls, with the 64-bit packet sequence number mapped to
//! a 12-byte nonce via [`seq_to_nonce`].  Nonce uniqueness is guaranteed by
//! the monotonically increasing `packet_seq` field in [`SessionState`].
use aes_gcm::{
    Aes256Gcm, Key as AesKey, Nonce as AesNonce,
    aead::{Aead, KeyInit},
};
use chacha20poly1305::{ChaCha20Poly1305, Key as ChaChaKey, Nonce as ChaChaNonce};

use super::CryptoError;

/// AES-256-GCM AEAD cipher keyed with a 32-byte session key.
pub struct Aes256GcmCipher {
    inner: Aes256Gcm,
}

impl Aes256GcmCipher {
    /// Construct from a 32-byte key.
    pub fn new(key: &[u8; 32]) -> Self {
        let k = AesKey::<Aes256Gcm>::from_slice(key);
        Self {
            inner: Aes256Gcm::new(k),
        }
    }

    /// Encrypt `plaintext` under `packet_seq` as nonce.  Returns authenticated ciphertext.
    pub fn encrypt(&self, packet_seq: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .encrypt(AesNonce::from_slice(&nonce), plaintext)
            .map_err(|_| CryptoError::AeadFailure)
    }

    /// Decrypt and authenticate `ciphertext` under `packet_seq` as nonce.
    pub fn decrypt(&self, packet_seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .decrypt(AesNonce::from_slice(&nonce), ciphertext)
            .map_err(|_| CryptoError::AeadFailure)
    }
}

/// ChaCha20-Poly1305 AEAD cipher keyed with a 32-byte session key.
pub struct ChaCha20Cipher {
    inner: ChaCha20Poly1305,
}

impl ChaCha20Cipher {
    /// Construct from a 32-byte key.
    pub fn new(key: &[u8; 32]) -> Self {
        let k = ChaChaKey::from_slice(key);
        Self {
            inner: ChaCha20Poly1305::new(k),
        }
    }

    /// Encrypt `plaintext` under `packet_seq` as nonce.  Returns authenticated ciphertext.
    pub fn encrypt(&self, packet_seq: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .encrypt(ChaChaNonce::from_slice(&nonce), plaintext)
            .map_err(|_| CryptoError::AeadFailure)
    }

    /// Decrypt and authenticate `ciphertext` under `packet_seq` as nonce.
    pub fn decrypt(&self, packet_seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .decrypt(ChaChaNonce::from_slice(&nonce), ciphertext)
            .map_err(|_| CryptoError::AeadFailure)
    }
}

/// AEAD cipher bound to a derived session key — enum dispatch over the two supported algorithms.
///
/// `Aes256Gcm` is boxed to equalise the variant sizes (AES-256-GCM key schedule
/// is ~1 KB; ChaCha20-Poly1305 is 32 bytes).
pub enum AeadCipher {
    Aes256Gcm(Box<Aes256GcmCipher>),
    ChaCha20Poly1305(ChaCha20Cipher),
}

impl AeadCipher {
    /// Encrypt `plaintext` using `packet_seq` as the AEAD nonce.
    pub fn encrypt(&self, packet_seq: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Aes256Gcm(c) => c.encrypt(packet_seq, plaintext),
            Self::ChaCha20Poly1305(c) => c.encrypt(packet_seq, plaintext),
        }
    }

    /// Decrypt and authenticate `ciphertext` using `packet_seq` as the AEAD nonce.
    pub fn decrypt(&self, packet_seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Aes256Gcm(c) => c.decrypt(packet_seq, ciphertext),
            Self::ChaCha20Poly1305(c) => c.decrypt(packet_seq, ciphertext),
        }
    }
}

/// Encode a 64-bit packet sequence number into a 12-byte AEAD nonce (big-endian, zero-padded).
///
/// The seq is placed in bytes 4-11, leaving bytes 0-3 as zero.  This ensures
/// every distinct sequence number produces a unique nonce for a given key.
fn seq_to_nonce(seq: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aes_key() -> [u8; 32] {
        [0x42u8; 32]
    }
    fn cha_key() -> [u8; 32] {
        [0x99u8; 32]
    }
    fn alt_key() -> [u8; 32] {
        [0x01u8; 32]
    }

    // ── AES-256-GCM ──────────────────────────────────────────────────────────

    #[test]
    fn aes_round_trip() {
        let c = Aes256GcmCipher::new(&aes_key());
        let ct = c.encrypt(1, b"hello aes").unwrap();
        assert_eq!(c.decrypt(1, &ct).unwrap(), b"hello aes");
    }

    #[test]
    fn aes_wrong_key_fails() {
        let enc = Aes256GcmCipher::new(&aes_key());
        let dec = Aes256GcmCipher::new(&alt_key());
        let ct = enc.encrypt(1, b"secret").unwrap();
        assert!(dec.decrypt(1, &ct).is_err());
    }

    #[test]
    fn aes_mutated_ciphertext_fails() {
        let c = Aes256GcmCipher::new(&aes_key());
        let mut ct = c.encrypt(1, b"tamper me").unwrap();
        ct[0] ^= 0xFF;
        assert!(c.decrypt(1, &ct).is_err());
    }

    #[test]
    fn aes_wrong_seq_fails() {
        let c = Aes256GcmCipher::new(&aes_key());
        let ct = c.encrypt(1, b"seq matters").unwrap();
        assert!(c.decrypt(2, &ct).is_err());
    }

    #[test]
    fn aes_empty_plaintext_round_trip() {
        let c = Aes256GcmCipher::new(&aes_key());
        let ct = c.encrypt(0, b"").unwrap();
        assert_eq!(c.decrypt(0, &ct).unwrap(), b"");
    }

    // ── ChaCha20-Poly1305 ────────────────────────────────────────────────────

    #[test]
    fn chacha_round_trip() {
        let c = ChaCha20Cipher::new(&cha_key());
        let ct = c.encrypt(7, b"hello chacha").unwrap();
        assert_eq!(c.decrypt(7, &ct).unwrap(), b"hello chacha");
    }

    #[test]
    fn chacha_wrong_key_fails() {
        let enc = ChaCha20Cipher::new(&cha_key());
        let dec = ChaCha20Cipher::new(&alt_key());
        let ct = enc.encrypt(1, b"secret").unwrap();
        assert!(dec.decrypt(1, &ct).is_err());
    }

    #[test]
    fn chacha_mutated_ciphertext_fails() {
        let c = ChaCha20Cipher::new(&cha_key());
        let mut ct = c.encrypt(3, b"tamper me").unwrap();
        *ct.last_mut().unwrap() ^= 0xFF;
        assert!(c.decrypt(3, &ct).is_err());
    }

    #[test]
    fn chacha_wrong_seq_fails() {
        let c = ChaCha20Cipher::new(&cha_key());
        let ct = c.encrypt(10, b"seq matters").unwrap();
        assert!(c.decrypt(11, &ct).is_err());
    }

    #[test]
    fn chacha_empty_plaintext_round_trip() {
        let c = ChaCha20Cipher::new(&cha_key());
        let ct = c.encrypt(0, b"").unwrap();
        assert_eq!(c.decrypt(0, &ct).unwrap(), b"");
    }

    // ── AeadCipher dispatch ───────────────────────────────────────────────────

    #[test]
    fn aead_cipher_aes_dispatch() {
        let c = AeadCipher::Aes256Gcm(Box::new(Aes256GcmCipher::new(&aes_key())));
        let ct = c.encrypt(5, b"dispatch").unwrap();
        assert_eq!(c.decrypt(5, &ct).unwrap(), b"dispatch");
    }

    #[test]
    fn aead_cipher_chacha_dispatch() {
        let c = AeadCipher::ChaCha20Poly1305(ChaCha20Cipher::new(&cha_key()));
        let ct = c.encrypt(5, b"dispatch").unwrap();
        assert_eq!(c.decrypt(5, &ct).unwrap(), b"dispatch");
    }

    // ── seq_to_nonce ─────────────────────────────────────────────────────────

    #[test]
    fn seq_to_nonce_unique() {
        let n0 = seq_to_nonce(0);
        let n1 = seq_to_nonce(1);
        let nmax = seq_to_nonce(u64::MAX);
        assert_ne!(n0, n1);
        assert_ne!(n1, nmax);
    }

    #[test]
    fn seq_to_nonce_big_endian() {
        let n = seq_to_nonce(1);
        // bytes 0-3 zero-padded; byte 11 == 1
        assert_eq!(&n[..4], &[0u8; 4]);
        assert_eq!(n[11], 1);
    }
}
