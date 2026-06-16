use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use sha2::Sha256;

pub struct SessionCipher {
    cipher: Aes256Gcm,
}

impl SessionCipher {
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

    pub fn encrypt(&self, seq_num: u64, plaintext: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
        let mut nonce_bytes = [0u8; 12];
        // Ensure the sequence number is encoded in big-endian into the nonce
        nonce_bytes[4..12].copy_from_slice(&seq_num.to_be_bytes());
        let nonce = Nonce::from_slice(&nonce_bytes);
        self.cipher.encrypt(nonce, plaintext)
    }

    pub fn decrypt(&self, seq_num: u64, ciphertext: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
        let mut nonce_bytes = [0u8; 12];
        // Ensure the sequence number is encoded in big-endian into the nonce
        nonce_bytes[4..12].copy_from_slice(&seq_num.to_be_bytes());
        let nonce = Nonce::from_slice(&nonce_bytes);
        self.cipher.decrypt(nonce, ciphertext)
    }
}

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
}
