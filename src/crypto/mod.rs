// SPDX-License-Identifier: GPL-3.0-or-later
//! Cipher suite negotiation, KEM key exchange, and AEAD session encryption.
//!
//! Suites are listed in descending preference order:
//!
//! | ID   | KEM           | AEAD              | KDF           |
//! |------|---------------|-------------------|---------------|
//! | 0x01 | ML-KEM-1024   | AES-256-GCM       | HKDF-SHA3-256 |
//! | 0x02 | ML-KEM-768    | AES-256-GCM       | HKDF-SHA-256  |
//! | 0x03 | X25519        | AES-256-GCM       | HKDF-SHA-256  |
//! | 0x04 | X25519        | ChaCha20-Poly1305 | HKDF-SHA-256  |
//!
//! PQC suites (0x01, 0x02) require the `pqc` cargo feature.
pub mod aead;
pub mod kdf;
pub mod x25519;
#[cfg(feature = "pqc")]
pub mod kyber;

pub use aead::AeadCipher;

/// Error type for all crypto operations.
#[derive(Debug)]
pub enum CryptoError {
    AeadFailure,
    InvalidKey,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AeadFailure => write!(f, "AEAD operation failed"),
            Self::InvalidKey => write!(f, "Invalid key material"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Wire ID used in handshake negotiation.
/// Also implements `Display` for human-readable diagnostic output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CipherSuiteId {
    #[cfg(feature = "pqc")]
    MlKem1024Aes256GcmSha3 = 1,
    #[cfg(feature = "pqc")]
    MlKem768Aes256GcmSha256 = 2,
    X25519Aes256GcmSha256 = 3,
    X25519ChaCha20Poly1305Sha256 = 4,
}

impl CipherSuiteId {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            #[cfg(feature = "pqc")]
            1 => Some(Self::MlKem1024Aes256GcmSha3),
            #[cfg(feature = "pqc")]
            2 => Some(Self::MlKem768Aes256GcmSha256),
            3 => Some(Self::X25519Aes256GcmSha256),
            4 => Some(Self::X25519ChaCha20Poly1305Sha256),
            _ => None,
        }
    }

    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// Client preference list, highest security first.
    pub fn client_preference() -> Vec<u32> {
        let mut suites = Vec::new();
        #[cfg(feature = "pqc")]
        suites.push(CipherSuiteId::MlKem1024Aes256GcmSha3 as u32);
        #[cfg(feature = "pqc")]
        suites.push(CipherSuiteId::MlKem768Aes256GcmSha256 as u32);
        suites.push(CipherSuiteId::X25519Aes256GcmSha256 as u32);
        suites.push(CipherSuiteId::X25519ChaCha20Poly1305Sha256 as u32);
        suites
    }

    pub fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "pqc")]
            Self::MlKem1024Aes256GcmSha3 => "ML-KEM-1024+AES-256-GCM+SHA3-256",
            #[cfg(feature = "pqc")]
            Self::MlKem768Aes256GcmSha256 => "ML-KEM-768+AES-256-GCM+SHA-256",
            Self::X25519Aes256GcmSha256 => "X25519+AES-256-GCM+SHA-256",
            Self::X25519ChaCha20Poly1305Sha256 => "X25519+ChaCha20-Poly1305+SHA-256",
        }
    }

    fn uses_sha3_kdf(self) -> bool {
        let _ = self;
        #[cfg(feature = "pqc")]
        { if self == CipherSuiteId::MlKem1024Aes256GcmSha3 { return true; } }
        false
    }

    fn uses_chacha20(self) -> bool {
        self == CipherSuiteId::X25519ChaCha20Poly1305Sha256
    }
}

impl std::fmt::Display for CipherSuiteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ── Ephemeral KEM key pair ────────────────────────────────────────────────────

/// Ephemeral KEM key pair generated for a single handshake.
pub enum KemKeyPair {
    X25519(x25519::X25519KeyPair),
    #[cfg(feature = "pqc")]
    MlKem768(kyber::MlKem768KeyPair),
    #[cfg(feature = "pqc")]
    MlKem1024(kyber::MlKem1024KeyPair),
}

impl KemKeyPair {
    /// Generate a fresh ephemeral key pair appropriate for the given suite.
    pub fn generate(suite: CipherSuiteId) -> Self {
        match suite {
            #[cfg(feature = "pqc")]
            CipherSuiteId::MlKem1024Aes256GcmSha3 => {
                Self::MlKem1024(kyber::MlKem1024KeyPair::generate())
            }
            #[cfg(feature = "pqc")]
            CipherSuiteId::MlKem768Aes256GcmSha256 => {
                Self::MlKem768(kyber::MlKem768KeyPair::generate())
            }
            CipherSuiteId::X25519Aes256GcmSha256
            | CipherSuiteId::X25519ChaCha20Poly1305Sha256 => {
                Self::X25519(x25519::X25519KeyPair::generate())
            }
        }
    }

    /// Public key / encapsulation key bytes to include in `ClientHello`.
    pub fn public_key_bytes(&self) -> Vec<u8> {
        match self {
            Self::X25519(kp) => kp.public_key_bytes(),
            #[cfg(feature = "pqc")]
            Self::MlKem768(kp) => kp.encapsulation_key_bytes(),
            #[cfg(feature = "pqc")]
            Self::MlKem1024(kp) => kp.encapsulation_key_bytes(),
        }
    }

    /// Decapsulate the server's ciphertext to recover the shared secret.
    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::X25519(kp) => kp.decapsulate(ciphertext),
            #[cfg(feature = "pqc")]
            Self::MlKem768(kp) => kp.decapsulate(ciphertext),
            #[cfg(feature = "pqc")]
            Self::MlKem1024(kp) => kp.decapsulate(ciphertext),
        }
    }
}

// ── Server-side encapsulation ─────────────────────────────────────────────────

/// Encapsulate to the client's public key.
///
/// Returns `(ciphertext_for_ServerHello, shared_secret)`.
pub fn encapsulate(
    suite: CipherSuiteId,
    peer_public_key: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    match suite {
        #[cfg(feature = "pqc")]
        CipherSuiteId::MlKem1024Aes256GcmSha3 => kyber::encapsulate_1024(peer_public_key),
        #[cfg(feature = "pqc")]
        CipherSuiteId::MlKem768Aes256GcmSha256 => kyber::encapsulate_768(peer_public_key),
        CipherSuiteId::X25519Aes256GcmSha256
        | CipherSuiteId::X25519ChaCha20Poly1305Sha256 => {
            x25519::encapsulate(peer_public_key)
        }
    }
}

// ── Session key derivation ────────────────────────────────────────────────────

/// Derive a session AEAD cipher from the passkey, KEM shared secret, and nonces.
///
/// ```text
/// ikm          = passkey ‖ kem_shared_secret
/// salt         = client_nonce ‖ server_nonce
/// session_key  = KDF(ikm, salt, "etr-session-v1")
/// ```
pub fn derive_session_cipher(
    suite: CipherSuiteId,
    passkey: &[u8],
    kem_shared_secret: &[u8],
    client_nonce: &[u8],
    server_nonce: &[u8],
) -> AeadCipher {
    let ikm = [passkey, kem_shared_secret].concat();
    let salt = [client_nonce, server_nonce].concat();
    let key_bytes = if suite.uses_sha3_kdf() {
        kdf::hkdf_sha3_256(&ikm, &salt, b"etr-session-v1", 32)
    } else {
        kdf::hkdf_sha256(&ikm, &salt, b"etr-session-v1", 32)
    };
    let key: [u8; 32] = key_bytes.try_into().expect("KDF output is always 32 bytes");
    if suite.uses_chacha20() {
        AeadCipher::ChaCha20Poly1305(aead::ChaCha20Cipher::new(&key))
    } else {
        AeadCipher::Aes256Gcm(aead::Aes256GcmCipher::new(&key))
    }
}

/// Derive the ephemeral key used to encrypt `ServerHello` before the full
/// session key is available.  Uses only the client nonce so the server can
/// produce it immediately upon receiving `ClientHello`.
///
/// ```text
/// hello_key = HKDF-SHA-256(ikm=passkey, salt=client_nonce, info="etr-hello-v1")
/// ```
pub fn derive_hello_cipher(passkey: &[u8], client_nonce: &[u8]) -> AeadCipher {
    let key_bytes = kdf::hkdf_sha256(passkey, client_nonce, b"etr-hello-v1", 32);
    let key: [u8; 32] = key_bytes.try_into().expect("KDF output is always 32 bytes");
    AeadCipher::Aes256Gcm(aead::Aes256GcmCipher::new(&key))
}

/// Generate a cryptographically random 32-byte nonce.
pub fn generate_nonce() -> [u8; 32] {
    use rand::RngCore;
    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

/// Generate a random 16-byte session ID.
pub fn generate_session_id() -> [u8; 16] {
    use rand::RngCore;
    let mut id = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_x25519_round_trip() {
        let suite = CipherSuiteId::X25519Aes256GcmSha256;
        let client_kp = KemKeyPair::generate(suite);
        let (ct, server_secret) = encapsulate(suite, &client_kp.public_key_bytes()).unwrap();
        let client_secret = client_kp.decapsulate(&ct).unwrap();
        assert_eq!(client_secret, server_secret);
    }

    #[test]
    fn test_session_cipher_encrypt_decrypt() {
        let suite = CipherSuiteId::X25519Aes256GcmSha256;
        let client_kp = KemKeyPair::generate(suite);
        let (ct, shared) = encapsulate(suite, &client_kp.public_key_bytes()).unwrap();
        let client_shared = client_kp.decapsulate(&ct).unwrap();

        let cn = generate_nonce();
        let sn = generate_nonce();
        let passkey = b"test-passkey";

        let enc = derive_session_cipher(suite, passkey, &shared, &cn, &sn);
        let dec = derive_session_cipher(suite, passkey, &client_shared, &cn, &sn);

        let ciphertext = enc.encrypt(1, b"hello").unwrap();
        let plaintext = dec.decrypt(1, &ciphertext).unwrap();
        assert_eq!(plaintext, b"hello");
    }

    #[test]
    fn test_chacha20_round_trip() {
        let suite = CipherSuiteId::X25519ChaCha20Poly1305Sha256;
        let client_kp = KemKeyPair::generate(suite);
        let (ct, shared) = encapsulate(suite, &client_kp.public_key_bytes()).unwrap();
        let client_shared = client_kp.decapsulate(&ct).unwrap();
        let cn = generate_nonce();
        let sn = generate_nonce();
        let enc = derive_session_cipher(suite, b"pk", &shared, &cn, &sn);
        let dec = derive_session_cipher(suite, b"pk", &client_shared, &cn, &sn);
        let ct2 = enc.encrypt(42, b"chacha test").unwrap();
        assert_eq!(dec.decrypt(42, &ct2).unwrap(), b"chacha test");
    }

    #[test]
    fn test_hello_cipher() {
        let passkey = b"my-passkey";
        let nonce = generate_nonce();
        let c1 = derive_hello_cipher(passkey, &nonce);
        let c2 = derive_hello_cipher(passkey, &nonce);
        let ct = c1.encrypt(0, b"server hello").unwrap();
        assert_eq!(c2.decrypt(0, &ct).unwrap(), b"server hello");
    }

    #[test]
    fn test_wrong_passkey_fails() {
        let suite = CipherSuiteId::X25519Aes256GcmSha256;
        let kp = KemKeyPair::generate(suite);
        let (_, shared) = encapsulate(suite, &kp.public_key_bytes()).unwrap();
        let cn = generate_nonce();
        let sn = generate_nonce();
        let enc = derive_session_cipher(suite, b"right", &shared, &cn, &sn);
        let dec = derive_session_cipher(suite, b"wrong", &shared, &cn, &sn);
        let ct = enc.encrypt(1, b"secret").unwrap();
        assert!(dec.decrypt(1, &ct).is_err());
    }
}
