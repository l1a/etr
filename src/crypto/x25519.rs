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

pub struct X25519KeyPair {
    secret: StaticSecret,
    public: PublicKey,
}

impl X25519KeyPair {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.public.as_bytes().to_vec()
    }

    /// Decapsulate: `ciphertext` is the server's ephemeral public key bytes.
    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let arr: [u8; 32] = ciphertext.try_into().map_err(|_| CryptoError::InvalidKey)?;
        let peer_pk = PublicKey::from(arr);
        Ok(self.secret.diffie_hellman(&peer_pk).as_bytes().to_vec())
    }
}

/// Server-side: encapsulate to `client_pk_bytes`.
///
/// Returns `(server_ephemeral_pk_bytes, shared_secret)`.
pub fn encapsulate(client_pk_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    let arr: [u8; 32] = client_pk_bytes.try_into().map_err(|_| CryptoError::InvalidKey)?;
    let client_pk = PublicKey::from(arr);
    let server_esk = StaticSecret::random_from_rng(OsRng);
    let server_epk = PublicKey::from(&server_esk);
    let shared = server_esk.diffie_hellman(&client_pk);
    Ok((server_epk.as_bytes().to_vec(), shared.as_bytes().to_vec()))
}
