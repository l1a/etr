// SPDX-License-Identifier: GPL-3.0-or-later
//! AES-256-GCM session encryption and HKDF-SHA-256 key derivation.
//!
//! A [`SessionCipher`] is created once per TCP connection by calling
//! [`SessionCipher::new`] with the shared passkey and the two random nonces
//! (one from the client, one from the server).  All subsequent packets are
//! encrypted with [`SessionCipher::encrypt`] and decrypted with
//! [`SessionCipher::decrypt`], using a monotonically increasing sequence
//! number as the AES-GCM nonce to guarantee nonce uniqueness.
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use sha2::Sha256;

/// AES-256-GCM cipher bound to a single session key.
///
/// The key is derived from the SSH-bootstrapped `passkey` and the two random
/// nonces exchanged during the connection handshake using HKDF-SHA-256.
/// Each call to [`encrypt`](SessionCipher::encrypt) /
/// [`decrypt`](SessionCipher::decrypt) requires a unique, incrementing
/// `seq_num` that is encoded into the 12-byte GCM nonce, preventing nonce
/// reuse across packets.
pub struct SessionCipher {
    cipher: Aes256Gcm,
}

impl SessionCipher {
    /// Derive a new [`SessionCipher`] from the shared passkey and the two
    /// connection nonces.
    ///
    /// The 32-byte AES key is produced by:
    /// ```text
    /// salt  = client_nonce ‖ server_nonce   (32 bytes)
    /// key   = HKDF-SHA-256(ikm=passkey, salt=salt, info="etr-session-key")
    /// ```
    ///
    /// # Arguments
    /// * `passkey`      – the pre-shared secret bootstrapped over SSH.
    /// * `client_nonce` – 16-byte random value from the client's `ConnectRequest`.
    /// * `server_nonce` – 16-byte random value from the server's `ConnectResponse`.
    pub fn new(passkey: &str, client_nonce: &[u8; 16], server_nonce: &[u8; 16]) -> Self {
        let mut salt = [0u8; 32];
        salt[0..16].copy_from_slice(client_nonce);
        salt[16..32].copy_from_slice(server_nonce);

        let hk = Hkdf::<Sha256>::new(Some(&salt), passkey.as_bytes());
        let mut okm = [0u8; 32];
        hk.expand(b"etr-session-key", &mut okm)
            .expect("HKDF expansion failed");

        let key = Key::<Aes256Gcm>::from_slice(&okm);
        let cipher = Aes256Gcm::new(key);
        Self { cipher }
    }

    /// Encrypt `plaintext` and return the AES-256-GCM ciphertext (with the
    /// authentication tag appended).
    ///
    /// `seq_num` is encoded in big-endian into bytes 4–11 of the 12-byte GCM
    /// nonce (bytes 0–3 are zeroed), making every packet's nonce unique as
    /// long as `seq_num` is never reused within a session.
    ///
    /// # Errors
    /// Returns an [`aes_gcm::Error`] if encryption fails (should be
    /// infallible in practice).
    pub fn encrypt(&self, seq_num: u64, plaintext: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
        let mut nonce_bytes = [0u8; 12];
        // Ensure the sequence number is encoded in big-endian into the nonce
        nonce_bytes[4..12].copy_from_slice(&seq_num.to_be_bytes());
        let nonce = Nonce::from_slice(&nonce_bytes);
        self.cipher.encrypt(nonce, plaintext)
    }

    /// Decrypt and authenticate `ciphertext`, returning the original plaintext.
    ///
    /// `seq_num` must match the value used during [`encrypt`](Self::encrypt);
    /// any mismatch causes authentication failure and an error is returned.
    ///
    /// # Errors
    /// Returns an [`aes_gcm::Error`] if the GCM authentication tag does not
    /// verify (wrong key, wrong sequence number, or corrupted ciphertext).
    pub fn decrypt(&self, seq_num: u64, ciphertext: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
        let mut nonce_bytes = [0u8; 12];
        // Ensure the sequence number is encoded in big-endian into the nonce
        nonce_bytes[4..12].copy_from_slice(&seq_num.to_be_bytes());
        let nonce = Nonce::from_slice(&nonce_bytes);
        self.cipher.decrypt(nonce, ciphertext)
    }
}

/// Generate a cryptographically random 16-byte nonce.
///
/// Used by both the client and server to produce their half of the key
/// derivation material exchanged during the handshake.
pub fn generate_nonce() -> [u8; 16] {
    use rand::RngCore;
    let mut nonce = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_derivation_and_encryption() {
        let passkey = "supersecretpasskey123";
        let client_nonce = [1u8; 16];
        let server_nonce = [2u8; 16];

        // Derive cipher on client and server sides
        let client_cipher = SessionCipher::new(passkey, &client_nonce, &server_nonce);
        let server_cipher = SessionCipher::new(passkey, &client_nonce, &server_nonce);

        let plaintext = b"Hello, secure world!";
        let seq = 42;

        // Client encrypts
        let ciphertext = client_cipher
            .encrypt(seq, plaintext)
            .expect("Encryption should succeed");

        // Server decrypts with matching sequence
        let decrypted = server_cipher
            .decrypt(seq, &ciphertext)
            .expect("Decryption should succeed");
        assert_eq!(decrypted, plaintext);

        // Server decrypts with mismatched sequence (different nonce) - should fail
        let decrypt_bad_seq = server_cipher.decrypt(seq + 1, &ciphertext);
        assert!(
            decrypt_bad_seq.is_err(),
            "Decryption with incorrect sequence should fail"
        );

        // Decrypting corrupted ciphertext - should fail
        let mut corrupted = ciphertext.clone();
        if !corrupted.is_empty() {
            corrupted[0] ^= 0xFF; // Flip bits
        }
        let decrypt_corrupted = server_cipher.decrypt(seq, &corrupted);
        assert!(
            decrypt_corrupted.is_err(),
            "Decryption of corrupted ciphertext should fail"
        );
    }

    #[test]
    fn test_generate_nonce() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        assert_ne!(
            nonce1, nonce2,
            "Generated nonces should be random and unique"
        );
    }

    #[test]
    fn test_wrong_passkey_fails_decryption() {
        let client_nonce = [3u8; 16];
        let server_nonce = [4u8; 16];

        let sender = SessionCipher::new("correct-passkey", &client_nonce, &server_nonce);
        let wrong = SessionCipher::new("wrong-passkey", &client_nonce, &server_nonce);

        let ciphertext = sender
            .encrypt(1, b"secret data")
            .expect("Encryption should succeed");

        let result = wrong.decrypt(1, &ciphertext);
        assert!(
            result.is_err(),
            "Decryption with a wrong passkey must fail authentication"
        );
    }

    #[test]
    fn test_nonce_order_matters() {
        // Swapping client and server nonces produces a different key
        let client_nonce = [5u8; 16];
        let server_nonce = [6u8; 16];

        let correct = SessionCipher::new("passkey", &client_nonce, &server_nonce);
        let swapped = SessionCipher::new("passkey", &server_nonce, &client_nonce);

        let ct = correct
            .encrypt(1, b"payload")
            .expect("Encryption should succeed");

        assert!(
            swapped.decrypt(1, &ct).is_err(),
            "Swapped nonce order must produce a different key and fail"
        );
    }
}
