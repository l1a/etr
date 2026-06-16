// SPDX-License-Identifier: GPL-3.0-or-later
//! AEAD cipher wrappers: AES-256-GCM and ChaCha20-Poly1305.
use aes_gcm::{
    Aes256Gcm, Key as AesKey, Nonce as AesNonce,
    aead::{Aead, KeyInit},
};
use chacha20poly1305::{ChaCha20Poly1305, Key as ChaChaKey, Nonce as ChaChaNonce};

use super::CryptoError;

pub struct Aes256GcmCipher {
    inner: Aes256Gcm,
}

impl Aes256GcmCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        let k = AesKey::<Aes256Gcm>::from_slice(key);
        Self { inner: Aes256Gcm::new(k) }
    }

    pub fn encrypt(&self, packet_seq: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .encrypt(AesNonce::from_slice(&nonce), plaintext)
            .map_err(|_| CryptoError::AeadFailure)
    }

    pub fn decrypt(&self, packet_seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .decrypt(AesNonce::from_slice(&nonce), ciphertext)
            .map_err(|_| CryptoError::AeadFailure)
    }
}

pub struct ChaCha20Cipher {
    inner: ChaCha20Poly1305,
}

impl ChaCha20Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        let k = ChaChaKey::from_slice(key);
        Self { inner: ChaCha20Poly1305::new(k) }
    }

    pub fn encrypt(&self, packet_seq: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .encrypt(ChaChaNonce::from_slice(&nonce), plaintext)
            .map_err(|_| CryptoError::AeadFailure)
    }

    pub fn decrypt(&self, packet_seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = seq_to_nonce(packet_seq);
        self.inner
            .decrypt(ChaChaNonce::from_slice(&nonce), ciphertext)
            .map_err(|_| CryptoError::AeadFailure)
    }
}

/// AEAD cipher bound to a derived session key — enum dispatch over the two supported algorithms.
pub enum AeadCipher {
    Aes256Gcm(Aes256GcmCipher),
    ChaCha20Poly1305(ChaCha20Cipher),
}

impl AeadCipher {
    pub fn encrypt(&self, packet_seq: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Aes256Gcm(c) => c.encrypt(packet_seq, plaintext),
            Self::ChaCha20Poly1305(c) => c.encrypt(packet_seq, plaintext),
        }
    }

    pub fn decrypt(&self, packet_seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Aes256Gcm(c) => c.decrypt(packet_seq, ciphertext),
            Self::ChaCha20Poly1305(c) => c.decrypt(packet_seq, ciphertext),
        }
    }
}

/// Encode a 64-bit packet sequence number into a 12-byte AEAD nonce (big-endian, zero-padded).
fn seq_to_nonce(seq: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}
