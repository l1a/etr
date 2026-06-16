// SPDX-License-Identifier: GPL-3.0-or-later
//! X25519 ephemeral key exchange, adapted to a KEM-style interface.
//!
//! The client generates an ephemeral keypair and sends its public key.
//! The server encapsulates by generating its own ephemeral key, running ECDH
//! against the client public key, and returning its ephemeral public key as
//! the "ciphertext".  Both sides derive the same 32-byte shared secret.
use rand_core::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

use super::CryptoError;

/// Client-side ephemeral X25519 keypair.
///
/// The public key is sent in `ClientHello` as the KEM encapsulation key.
/// After receiving `ServerHello`, call [`decapsulate`] to recover the shared secret.
pub struct X25519KeyPair {
    secret: StaticSecret,
    public: PublicKey,
}

impl X25519KeyPair {
    /// Generate a fresh ephemeral keypair using the OS RNG.
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Return the 32-byte public key to include in `ClientHello`.
    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.public.as_bytes().to_vec()
    }

    /// Decapsulate: `ciphertext` is the server's ephemeral public key bytes.
    ///
    /// Returns `CryptoError::InvalidKey` if `ciphertext` is not exactly 32 bytes.
    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let arr: [u8; 32] = ciphertext.try_into().map_err(|_| CryptoError::InvalidKey)?;
        let peer_pk = PublicKey::from(arr);
        Ok(self.secret.diffie_hellman(&peer_pk).as_bytes().to_vec())
    }
}

/// Server-side: encapsulate to `client_pk_bytes`.
///
/// Generates a fresh server ephemeral keypair, performs ECDH, and returns
/// `(server_ephemeral_pk_bytes, shared_secret)`.
///
/// Returns `CryptoError::InvalidKey` if `client_pk_bytes` is not exactly 32 bytes.
pub fn encapsulate(client_pk_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    let arr: [u8; 32] = client_pk_bytes
        .try_into()
        .map_err(|_| CryptoError::InvalidKey)?;
    let client_pk = PublicKey::from(arr);
    let server_esk = StaticSecret::random_from_rng(OsRng);
    let server_epk = PublicKey::from(&server_esk);
    let shared = server_esk.diffie_hellman(&client_pk);
    Ok((server_epk.as_bytes().to_vec(), shared.as_bytes().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_shared_secret() {
        let client = X25519KeyPair::generate();
        let (server_ct, server_secret) = encapsulate(&client.public_key_bytes()).unwrap();
        let client_secret = client.decapsulate(&server_ct).unwrap();
        assert_eq!(client_secret, server_secret);
        assert_eq!(client_secret.len(), 32);
    }

    #[test]
    fn different_keypairs_different_secrets() {
        let c1 = X25519KeyPair::generate();
        let c2 = X25519KeyPair::generate();
        let (ct1, s1) = encapsulate(&c1.public_key_bytes()).unwrap();
        let (ct2, s2) = encapsulate(&c2.public_key_bytes()).unwrap();
        // Each encapsulation produces a unique shared secret and ciphertext.
        assert_ne!(s1, s2);
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn decapsulate_wrong_length_returns_invalid_key() {
        let client = X25519KeyPair::generate();
        assert!(matches!(
            client.decapsulate(&[0u8; 31]),
            Err(CryptoError::InvalidKey)
        ));
        assert!(matches!(
            client.decapsulate(&[0u8; 33]),
            Err(CryptoError::InvalidKey)
        ));
        assert!(matches!(
            client.decapsulate(&[]),
            Err(CryptoError::InvalidKey)
        ));
    }

    #[test]
    fn encapsulate_wrong_length_returns_invalid_key() {
        assert!(matches!(
            encapsulate(&[0u8; 31]),
            Err(CryptoError::InvalidKey)
        ));
        assert!(matches!(encapsulate(&[]), Err(CryptoError::InvalidKey)));
    }

    #[test]
    fn public_key_bytes_length() {
        let kp = X25519KeyPair::generate();
        assert_eq!(kp.public_key_bytes().len(), 32);
    }
}
