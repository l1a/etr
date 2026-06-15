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
