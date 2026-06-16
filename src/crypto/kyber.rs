// SPDX-License-Identifier: GPL-3.0-or-later
//! ML-KEM (FIPS 203) post-quantum KEM wrappers.
//!
//! Enabled with the `pqc` cargo feature.  Provides ML-KEM-768 and ML-KEM-1024
//! in a KEM-style interface consistent with the X25519 module.
//!
//! # Security level
//! - **ML-KEM-768**: NIST category 3 (~AES-192 equivalent)
//! - **ML-KEM-1024**: NIST category 5 (~AES-256 equivalent)
use ml_kem::kem::{Decapsulate, Encapsulate};
use ml_kem::{KemCore, MlKem768, MlKem1024};
use rand_core::OsRng;

use super::CryptoError;

// ── ML-KEM-768 ───────────────────────────────────────────────────────────────

/// Client-side ML-KEM-768 ephemeral keypair (NIST category 3).
///
/// The encapsulation key bytes are sent in `ClientHello`; call [`decapsulate`]
/// with the server's ciphertext to recover the shared secret.
pub struct MlKem768KeyPair {
    dk: <MlKem768 as KemCore>::DecapsulationKey,
    ek: <MlKem768 as KemCore>::EncapsulationKey,
}

impl MlKem768KeyPair {
    /// Generate a fresh ephemeral ML-KEM-768 keypair.
    pub fn generate() -> Self {
        let (dk, ek) = MlKem768::generate(&mut OsRng);
        Self { dk, ek }
    }

    /// Serialise the encapsulation key for inclusion in `ClientHello`.
    pub fn encapsulation_key_bytes(&self) -> Vec<u8> {
        use ml_kem::EncodedSizeUser;
        self.ek.as_bytes().as_slice().to_vec()
    }

    /// Recover the shared secret from the server's KEM ciphertext.
    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use ml_kem::EncodedSizeUser;
        let ct_encoded =
            <MlKem768 as KemCore>::CipherText::from_bytes(ml_kem::Encoded::<
                <MlKem768 as KemCore>::CipherText,
            >::from_slice(ciphertext));
        let ss = self
            .dk
            .decapsulate(&ct_encoded)
            .map_err(|_| CryptoError::AeadFailure)?;
        use ml_kem::EncodedSizeUser;
        Ok(ss.as_bytes().as_slice().to_vec())
    }
}

/// Server-side ML-KEM-768 encapsulation.
///
/// Encapsulates to the client's public key and returns
/// `(ciphertext_for_ServerHello, shared_secret)`.
pub fn encapsulate_768(ek_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    use ml_kem::EncodedSizeUser;
    let ek = <MlKem768 as KemCore>::EncapsulationKey::from_bytes(ml_kem::Encoded::<
        <MlKem768 as KemCore>::EncapsulationKey,
    >::from_slice(ek_bytes));
    let (ct, ss) = ek
        .encapsulate(&mut OsRng)
        .map_err(|_| CryptoError::AeadFailure)?;
    Ok((
        ct.as_bytes().as_slice().to_vec(),
        ss.as_bytes().as_slice().to_vec(),
    ))
}

// ── ML-KEM-1024 ──────────────────────────────────────────────────────────────

/// Client-side ML-KEM-1024 ephemeral keypair (NIST category 5).
///
/// The encapsulation key bytes are sent in `ClientHello`; call [`decapsulate`]
/// with the server's ciphertext to recover the shared secret.
pub struct MlKem1024KeyPair {
    dk: <MlKem1024 as KemCore>::DecapsulationKey,
    ek: <MlKem1024 as KemCore>::EncapsulationKey,
}

impl MlKem1024KeyPair {
    /// Generate a fresh ephemeral ML-KEM-1024 keypair.
    pub fn generate() -> Self {
        let (dk, ek) = MlKem1024::generate(&mut OsRng);
        Self { dk, ek }
    }

    /// Serialise the encapsulation key for inclusion in `ClientHello`.
    pub fn encapsulation_key_bytes(&self) -> Vec<u8> {
        use ml_kem::EncodedSizeUser;
        self.ek.as_bytes().as_slice().to_vec()
    }

    /// Recover the shared secret from the server's KEM ciphertext.
    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use ml_kem::EncodedSizeUser;
        let ct_encoded =
            <MlKem1024 as KemCore>::CipherText::from_bytes(ml_kem::Encoded::<
                <MlKem1024 as KemCore>::CipherText,
            >::from_slice(ciphertext));
        let ss = self
            .dk
            .decapsulate(&ct_encoded)
            .map_err(|_| CryptoError::AeadFailure)?;
        Ok(ss.as_bytes().as_slice().to_vec())
    }
}

/// Server-side ML-KEM-1024 encapsulation.
///
/// Encapsulates to the client's public key and returns
/// `(ciphertext_for_ServerHello, shared_secret)`.
pub fn encapsulate_1024(ek_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    use ml_kem::EncodedSizeUser;
    let ek = <MlKem1024 as KemCore>::EncapsulationKey::from_bytes(ml_kem::Encoded::<
        <MlKem1024 as KemCore>::EncapsulationKey,
    >::from_slice(ek_bytes));
    let (ct, ss) = ek
        .encapsulate(&mut OsRng)
        .map_err(|_| CryptoError::AeadFailure)?;
    Ok((
        ct.as_bytes().as_slice().to_vec(),
        ss.as_bytes().as_slice().to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mlkem768_round_trip() {
        let client = MlKem768KeyPair::generate();
        let ek_bytes = client.encapsulation_key_bytes();
        let (ct, server_secret) = encapsulate_768(&ek_bytes).unwrap();
        let client_secret = client.decapsulate(&ct).unwrap();
        assert_eq!(client_secret, server_secret);
        assert!(!client_secret.is_empty());
    }

    #[test]
    fn mlkem1024_round_trip() {
        let client = MlKem1024KeyPair::generate();
        let ek_bytes = client.encapsulation_key_bytes();
        let (ct, server_secret) = encapsulate_1024(&ek_bytes).unwrap();
        let client_secret = client.decapsulate(&ct).unwrap();
        assert_eq!(client_secret, server_secret);
        assert!(!client_secret.is_empty());
    }

    #[test]
    fn mlkem768_different_sessions_differ() {
        let c1 = MlKem768KeyPair::generate();
        let c2 = MlKem768KeyPair::generate();
        let (_, s1) = encapsulate_768(&c1.encapsulation_key_bytes()).unwrap();
        let (_, s2) = encapsulate_768(&c2.encapsulation_key_bytes()).unwrap();
        assert_ne!(s1, s2);
    }

    #[test]
    fn mlkem768_wrong_ciphertext_decapsulate_differs() {
        // ML-KEM uses implicit rejection: a wrong ciphertext produces a
        // pseudo-random (but deterministic) shared secret rather than an error.
        // Verify client and server secrets diverge when the ciphertext is wrong.
        let client = MlKem768KeyPair::generate();
        let ek_bytes = client.encapsulation_key_bytes();
        let (mut ct, server_secret) = encapsulate_768(&ek_bytes).unwrap();
        ct[0] ^= 0xFF;
        let bad_client_secret = client.decapsulate(&ct).unwrap();
        assert_ne!(bad_client_secret, server_secret);
    }

    #[test]
    fn mlkem1024_wrong_ciphertext_decapsulate_differs() {
        let client = MlKem1024KeyPair::generate();
        let ek_bytes = client.encapsulation_key_bytes();
        let (mut ct, server_secret) = encapsulate_1024(&ek_bytes).unwrap();
        ct[0] ^= 0xFF;
        let bad_client_secret = client.decapsulate(&ct).unwrap();
        assert_ne!(bad_client_secret, server_secret);
    }
}
