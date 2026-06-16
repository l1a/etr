// SPDX-License-Identifier: GPL-3.0-or-later
//! HKDF helpers: SHA-256 and SHA-3-256 variants.
use hkdf::Hkdf;
use sha2::Sha256;
use sha3::Sha3_256;

/// Derive `len` bytes via HKDF-SHA-256.
pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = vec![0u8; len];
    hk.expand(info, &mut okm).expect("HKDF-SHA-256 expand failed");
    okm
}

/// Derive `len` bytes via HKDF-SHA3-256.
pub fn hkdf_sha3_256(ikm: &[u8], salt: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha3_256>::new(Some(salt), ikm);
    let mut okm = vec![0u8; len];
    hk.expand(info, &mut okm).expect("HKDF-SHA3-256 expand failed");
    okm
}
