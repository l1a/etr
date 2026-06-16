// SPDX-License-Identifier: GPL-3.0-or-later
//! ML-KEM (FIPS 203) post-quantum KEM wrappers.
//!
//! Enabled with the `pqc` cargo feature.  Provides ML-KEM-768 and ML-KEM-1024
//! in a KEM-style interface consistent with the X25519 module.
use ml_kem::{KemCore, MlKem1024, MlKem768};
use ml_kem::kem::{Decapsulate, Encapsulate};
use rand_core::OsRng;

use super::CryptoError;

// ── ML-KEM-768 ───────────────────────────────────────────────────────────────

pub struct MlKem768KeyPair {
    dk: <MlKem768 as KemCore>::DecapsulationKey,
    ek: <MlKem768 as KemCore>::EncapsulationKey,
}

impl MlKem768KeyPair {
    pub fn generate() -> Self {
        let (dk, ek) = MlKem768::generate(&mut OsRng);
        Self { dk, ek }
    }

    pub fn encapsulation_key_bytes(&self) -> Vec<u8> {
        use ml_kem::EncodedSizeUser;
        self.ek.as_bytes().as_slice().to_vec()
    }

    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use ml_kem::EncodedSizeUser;
        let ct_encoded = <MlKem768 as KemCore>::CipherText::from_bytes(
            ml_kem::Encoded::<<MlKem768 as KemCore>::CipherText>::from_slice(ciphertext),
        );
        let ss = self.dk.decapsulate(&ct_encoded).map_err(|_| CryptoError::AeadFailure)?;
        use ml_kem::EncodedSizeUser;
        Ok(ss.as_bytes().as_slice().to_vec())
    }
}

pub fn encapsulate_768(ek_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    use ml_kem::EncodedSizeUser;
    let ek = <MlKem768 as KemCore>::EncapsulationKey::from_bytes(
        ml_kem::Encoded::<<MlKem768 as KemCore>::EncapsulationKey>::from_slice(ek_bytes),
    );
    let (ct, ss) = ek.encapsulate(&mut OsRng).map_err(|_| CryptoError::AeadFailure)?;
    Ok((ct.as_bytes().as_slice().to_vec(), ss.as_bytes().as_slice().to_vec()))
}

// ── ML-KEM-1024 ──────────────────────────────────────────────────────────────

pub struct MlKem1024KeyPair {
    dk: <MlKem1024 as KemCore>::DecapsulationKey,
    ek: <MlKem1024 as KemCore>::EncapsulationKey,
}

impl MlKem1024KeyPair {
    pub fn generate() -> Self {
        let (dk, ek) = MlKem1024::generate(&mut OsRng);
        Self { dk, ek }
    }

    pub fn encapsulation_key_bytes(&self) -> Vec<u8> {
        use ml_kem::EncodedSizeUser;
        self.ek.as_bytes().as_slice().to_vec()
    }

    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use ml_kem::EncodedSizeUser;
        let ct_encoded = <MlKem1024 as KemCore>::CipherText::from_bytes(
            ml_kem::Encoded::<<MlKem1024 as KemCore>::CipherText>::from_slice(ciphertext),
        );
        let ss = self.dk.decapsulate(&ct_encoded).map_err(|_| CryptoError::AeadFailure)?;
        Ok(ss.as_bytes().as_slice().to_vec())
    }
}

pub fn encapsulate_1024(ek_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    use ml_kem::EncodedSizeUser;
    let ek = <MlKem1024 as KemCore>::EncapsulationKey::from_bytes(
        ml_kem::Encoded::<<MlKem1024 as KemCore>::EncapsulationKey>::from_slice(ek_bytes),
    );
    let (ct, ss) = ek.encapsulate(&mut OsRng).map_err(|_| CryptoError::AeadFailure)?;
    Ok((ct.as_bytes().as_slice().to_vec(), ss.as_bytes().as_slice().to_vec()))
}
