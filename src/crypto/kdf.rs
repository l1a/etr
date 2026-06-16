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

#[cfg(test)]
mod tests {
    use super::*;

    const IKM: &[u8] = b"input-key-material";
    const SALT: &[u8] = b"test-salt";
    const INFO: &[u8] = b"test-info";

    // ── hkdf_sha256 ──────────────────────────────────────────────────────────

    #[test]
    fn sha256_deterministic() {
        let a = hkdf_sha256(IKM, SALT, INFO, 32);
        let b = hkdf_sha256(IKM, SALT, INFO, 32);
        assert_eq!(a, b);
    }

    #[test]
    fn sha256_output_length() {
        for len in [16, 32, 64] {
            assert_eq!(hkdf_sha256(IKM, SALT, INFO, len).len(), len);
        }
    }

    #[test]
    fn sha256_different_salts_differ() {
        let a = hkdf_sha256(IKM, b"salt-a", INFO, 32);
        let b = hkdf_sha256(IKM, b"salt-b", INFO, 32);
        assert_ne!(a, b);
    }

    #[test]
    fn sha256_different_ikm_differs() {
        let a = hkdf_sha256(b"key-a", SALT, INFO, 32);
        let b = hkdf_sha256(b"key-b", SALT, INFO, 32);
        assert_ne!(a, b);
    }

    #[test]
    fn sha256_different_info_differs() {
        let a = hkdf_sha256(IKM, SALT, b"info-a", 32);
        let b = hkdf_sha256(IKM, SALT, b"info-b", 32);
        assert_ne!(a, b);
    }

    // ── hkdf_sha3_256 ────────────────────────────────────────────────────────

    #[test]
    fn sha3_deterministic() {
        let a = hkdf_sha3_256(IKM, SALT, INFO, 32);
        let b = hkdf_sha3_256(IKM, SALT, INFO, 32);
        assert_eq!(a, b);
    }

    #[test]
    fn sha3_output_length() {
        for len in [16, 32, 64] {
            assert_eq!(hkdf_sha3_256(IKM, SALT, INFO, len).len(), len);
        }
    }

    #[test]
    fn sha3_different_salts_differ() {
        let a = hkdf_sha3_256(IKM, b"salt-a", INFO, 32);
        let b = hkdf_sha3_256(IKM, b"salt-b", INFO, 32);
        assert_ne!(a, b);
    }

    // ── Cross-algorithm ───────────────────────────────────────────────────────

    #[test]
    fn sha256_and_sha3_produce_different_output() {
        let a = hkdf_sha256(IKM, SALT, INFO, 32);
        let b = hkdf_sha3_256(IKM, SALT, INFO, 32);
        assert_ne!(a, b, "SHA-256 and SHA3-256 must produce distinct keys");
    }
}
