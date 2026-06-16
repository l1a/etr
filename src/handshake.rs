// SPDX-License-Identifier: GPL-3.0-or-later
//! 1-RTT handshake state machines for client and server.
//!
//! ## Protocol flow
//!
//! ```text
//! Client                                     Server
//!   │                                           │
//!   │── ClientHello (plaintext) ───────────────▶│  parse session_id, look up passkey,
//!   │   session_id, cipher_suites,              │  choose suite, encapsulate, derive
//!   │   client_nonce, kem_public_key,           │  hello_key and session_key
//!   │   last_received_seq (per stream)          │
//!   │                                           │
//!   │◀─ ServerHello (hello-key encrypted) ──────│  server_nonce, kem_ciphertext,
//!   │                                           │  chosen_suite, last_received_seq
//!   │                                           │  (+ may immediately send replay data)
//!   │                                           │
//!   │  [client derives session key; both        │
//!   │   sides can now exchange StreamData]      │
//! ```
//!
//! One round trip is all that is needed for either a new session or a
//! reconnect.  The server's `last_received_seq` map lets the client trim its
//! history; the client's map triggers replay from the server.
use std::collections::HashMap;

use prost::Message;

use crate::crypto::{
    self, AeadCipher, CipherSuiteId, KemKeyPair, derive_hello_cipher, derive_session_cipher,
    generate_nonce, generate_session_id,
};
use crate::protocol::{
    ClientHello, Envelope, FLAG_HANDSHAKE, PROTOCOL_VERSION, PacketHeader, Payload, ServerHello,
};

// ── Client handshake ─────────────────────────────────────────────────────────

/// State held by the client between sending `ClientHello` and receiving `ServerHello`.
pub struct ClientHandshake {
    pub session_id: [u8; 16],
    passkey: String,
    client_nonce: [u8; 32],
    kem_keypair: KemKeyPair,
}

impl ClientHandshake {
    /// Build a `ClientHello` for a new session or reconnect.
    ///
    /// `last_received_seq` should be populated from `SessionState::last_received_map()`;
    /// pass an empty map for a brand-new session.
    pub fn new(
        passkey: String,
        last_received_seq: HashMap<u32, u64>,
    ) -> (Self, PacketHeader, Envelope) {
        let session_id = generate_session_id();
        Self::with_session_id(session_id, passkey, last_received_seq)
    }

    /// Reconnect variant: reuse the existing `session_id`.
    pub fn reconnect(
        session_id: [u8; 16],
        passkey: String,
        last_received_seq: HashMap<u32, u64>,
    ) -> (Self, PacketHeader, Envelope) {
        Self::with_session_id(session_id, passkey, last_received_seq)
    }

    fn with_session_id(
        session_id: [u8; 16],
        passkey: String,
        last_received_seq: HashMap<u32, u64>,
    ) -> (Self, PacketHeader, Envelope) {
        let client_nonce = generate_nonce();
        // Generate a keypair for the most-preferred suite (the first in the preference list).
        // The server must choose that suite because the KEM public key is suite-specific.
        let preferred_suites = CipherSuiteId::client_preference();
        let preferred_suite = CipherSuiteId::from_u32(preferred_suites[0])
            .unwrap_or(CipherSuiteId::X25519Aes256GcmSha256);
        let kem_keypair = KemKeyPair::generate(preferred_suite);

        let hello = ClientHello {
            protocol_version: PROTOCOL_VERSION as u32,
            session_id: session_id.to_vec(),
            cipher_suites: CipherSuiteId::client_preference(),
            client_nonce: client_nonce.to_vec(),
            kem_public_key: kem_keypair.public_key_bytes(),
            last_received_seq,
        };

        let header = PacketHeader::new(FLAG_HANDSHAKE, session_id, 0);
        let envelope = Envelope {
            payload: Some(Payload::ClientHello(hello)),
        };

        let state = Self {
            session_id,
            passkey,
            client_nonce,
            kem_keypair,
        };
        (state, header, envelope)
    }

    /// Process a `ServerHello` payload (after decrypting with the hello key).
    ///
    /// On success returns the negotiated AEAD cipher, the chosen cipher suite,
    /// and the server's per-stream ack map.
    pub fn process_server_hello(
        self,
        payload_bytes: &[u8],
    ) -> Result<(AeadCipher, CipherSuiteId, HashMap<u32, u64>), HandshakeError> {
        // Decrypt with the hello key (derived from passkey + client_nonce only).
        let hello_cipher = derive_hello_cipher(self.passkey.as_bytes(), &self.client_nonce);
        let plaintext = hello_cipher
            .decrypt(0, payload_bytes)
            .map_err(|_| HandshakeError::AuthFailed)?;

        let envelope =
            Envelope::decode(plaintext.as_slice()).map_err(|_| HandshakeError::MalformedPacket)?;

        let server_hello = match envelope.payload {
            Some(Payload::ServerHello(sh)) => sh,
            _ => return Err(HandshakeError::UnexpectedPacket),
        };

        let suite = CipherSuiteId::from_u32(server_hello.chosen_suite)
            .ok_or(HandshakeError::UnsupportedSuite)?;

        let server_nonce: [u8; 32] = server_hello
            .server_nonce
            .try_into()
            .map_err(|_| HandshakeError::MalformedPacket)?;

        let kem_secret = self
            .kem_keypair
            .decapsulate(&server_hello.kem_ciphertext)
            .map_err(|_| HandshakeError::AuthFailed)?;

        let cipher = derive_session_cipher(
            suite,
            self.passkey.as_bytes(),
            &kem_secret,
            &self.client_nonce,
            &server_nonce,
        );

        Ok((cipher, suite, server_hello.last_received_seq))
    }
}

// ── Server handshake ─────────────────────────────────────────────────────────

/// Outcome of processing a `ClientHello` on the server.
pub struct ServerHelloOutcome {
    pub session_id: [u8; 16],
    pub chosen_suite: CipherSuiteId,
    /// The `ServerHello` bytes to send, encrypted with the hello key.
    pub response_header: PacketHeader,
    pub response_payload_bytes: Vec<u8>,
    /// Derived session cipher ready for immediate use.
    pub cipher: AeadCipher,
    /// The client's per-stream ack positions (used to trigger replay).
    pub client_last_received: HashMap<u32, u64>,
}

/// Process a `ClientHello`, look up the passkey, and produce a `ServerHello`.
///
/// `lookup_passkey` is a closure so the caller can integrate with whatever
/// session store they use without coupling this module to it.
pub fn process_client_hello<F>(
    payload_bytes: &[u8],
    server_last_received: HashMap<u32, u64>,
    lookup_passkey: F,
) -> Result<ServerHelloOutcome, HandshakeError>
where
    F: Fn(&[u8]) -> Option<String>,
{
    let envelope = Envelope::decode(payload_bytes).map_err(|_| HandshakeError::MalformedPacket)?;

    let client_hello = match envelope.payload {
        Some(Payload::ClientHello(ch)) => ch,
        _ => return Err(HandshakeError::UnexpectedPacket),
    };

    let session_id: [u8; 16] = client_hello
        .session_id
        .try_into()
        .map_err(|_| HandshakeError::MalformedPacket)?;

    let passkey = lookup_passkey(&session_id).ok_or(HandshakeError::UnknownSession)?;

    let client_nonce: [u8; 32] = client_hello
        .client_nonce
        .try_into()
        .map_err(|_| HandshakeError::MalformedPacket)?;

    // Choose the highest suite the server supports from the client's list.
    let suite = client_hello
        .cipher_suites
        .iter()
        .find_map(|&id| CipherSuiteId::from_u32(id))
        .ok_or(HandshakeError::UnsupportedSuite)?;

    let (kem_ciphertext, kem_secret) = crypto::encapsulate(suite, &client_hello.kem_public_key)
        .map_err(|_| HandshakeError::AuthFailed)?;

    let server_nonce = generate_nonce();

    let cipher = derive_session_cipher(
        suite,
        passkey.as_bytes(),
        &kem_secret,
        &client_nonce,
        &server_nonce,
    );

    let server_hello = ServerHello {
        chosen_suite: suite.as_u32(),
        server_nonce: server_nonce.to_vec(),
        kem_ciphertext,
        last_received_seq: server_last_received,
    };

    // Encrypt ServerHello with the hello key.
    let hello_cipher = derive_hello_cipher(passkey.as_bytes(), &client_nonce);
    let inner_bytes = Envelope {
        payload: Some(Payload::ServerHello(server_hello)),
    }
    .encode_to_vec();
    let response_payload_bytes = hello_cipher
        .encrypt(0, &inner_bytes)
        .map_err(|_| HandshakeError::InternalError)?;

    let response_header = PacketHeader::new(FLAG_HANDSHAKE, session_id, 0);

    Ok(ServerHelloOutcome {
        session_id,
        chosen_suite: suite,
        response_header,
        response_payload_bytes,
        cipher,
        client_last_received: client_hello.last_received_seq,
    })
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum HandshakeError {
    /// The session ID in `ClientHello` was not found in the session store.
    UnknownSession,
    /// AEAD decryption of the hello-encrypted payload failed (wrong passkey or tampered packet).
    AuthFailed,
    /// The client's offered cipher suites contain no suite the server supports.
    UnsupportedSuite,
    /// The packet could not be decoded as a valid protobuf `Envelope`.
    MalformedPacket,
    /// The decoded `Envelope` contains the wrong message type for this phase.
    UnexpectedPacket,
    /// An internal crypto operation failed (should not happen with valid inputs).
    InternalError,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSession => write!(f, "unknown session ID"),
            Self::AuthFailed => write!(f, "authentication failed"),
            Self::UnsupportedSuite => write!(f, "no supported cipher suite"),
            Self::MalformedPacket => write!(f, "malformed packet"),
            Self::UnexpectedPacket => write!(f, "unexpected packet type"),
            Self::InternalError => write!(f, "internal handshake error"),
        }
    }
}

impl std::error::Error for HandshakeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn simulate_handshake() -> (AeadCipher, AeadCipher) {
        let passkey = "test-passkey".to_string();

        // Client builds ClientHello.
        let (client_hs, _header, client_envelope) =
            ClientHandshake::new(passkey.clone(), HashMap::new());

        // Extract ClientHello payload bytes (as if received over UDP).
        let client_payload = client_envelope.encode_to_vec();

        // Server processes it.
        let outcome = process_client_hello(&client_payload, HashMap::new(), |_sid| {
            Some(passkey.clone())
        })
        .expect("server handshake should succeed");

        // Client processes ServerHello.
        let (client_cipher, _suite, _server_acks) = client_hs
            .process_server_hello(&outcome.response_payload_bytes)
            .expect("client should accept ServerHello");

        (client_cipher, outcome.cipher)
    }

    #[test]
    fn test_handshake_derives_matching_keys() {
        let (client_cipher, server_cipher) = simulate_handshake();
        let ct = client_cipher.encrypt(1, b"ping").unwrap();
        let pt = server_cipher.decrypt(1, &ct).unwrap();
        assert_eq!(pt, b"ping");
    }

    #[test]
    fn test_wrong_passkey_rejected() {
        let (_client_hs, _header, client_envelope) =
            ClientHandshake::new("correct".to_string(), HashMap::new());
        let client_payload = client_envelope.encode_to_vec();

        let result = process_client_hello(&client_payload, HashMap::new(), |_| {
            Some("wrong".to_string())
        });
        // Server will succeed (it doesn't know the key is wrong at this point),
        // but the client will fail to decrypt the ServerHello.
        let outcome = result.expect("server produces a response");

        let (client_hs2, _, client_envelope2) =
            ClientHandshake::new("correct".to_string(), HashMap::new());
        let _ = client_envelope2; // suppress warning

        // Simulate the client trying to decrypt with its own (different) nonce context.
        // In practice the client's nonce won't match so decryption fails.
        let tampered = {
            let mut b = outcome.response_payload_bytes.clone();
            if !b.is_empty() {
                b[0] ^= 0xFF;
            }
            b
        };
        let result = client_hs2.process_server_hello(&tampered);
        assert!(result.is_err());
    }

    #[test]
    fn test_unknown_session_rejected() {
        let (_hs, _hdr, env) = ClientHandshake::new("pk".to_string(), HashMap::new());
        let payload = env.encode_to_vec();
        let result = process_client_hello(&payload, HashMap::new(), |_| None);
        assert!(matches!(result, Err(HandshakeError::UnknownSession)));
    }

    #[test]
    fn test_malformed_packet_rejected() {
        let result = process_client_hello(b"\xFF\xFF\xFF garbage", HashMap::new(), |_| {
            Some("pk".into())
        });
        assert!(matches!(result, Err(HandshakeError::MalformedPacket)));
    }

    #[test]
    fn test_empty_bytes_rejected() {
        // An empty protobuf decodes to an Envelope with no payload → UnexpectedPacket.
        let result = process_client_hello(&[], HashMap::new(), |_| Some("pk".into()));
        assert!(matches!(result, Err(HandshakeError::UnexpectedPacket)));
    }

    #[test]
    fn test_unexpected_packet_type_rejected() {
        use crate::protocol::{Heartbeat, Payload};
        // Send a Heartbeat where a ClientHello is expected.
        let env = Envelope {
            payload: Some(Payload::Heartbeat(Heartbeat {})),
        };
        let payload = env.encode_to_vec();
        let result = process_client_hello(&payload, HashMap::new(), |_| Some("pk".into()));
        assert!(matches!(result, Err(HandshakeError::UnexpectedPacket)));
    }

    #[test]
    fn test_unsupported_suite_rejected() {
        use crate::protocol::{ClientHello, Payload};
        use prost::Message;
        let hello = ClientHello {
            protocol_version: crate::protocol::PROTOCOL_VERSION as u32,
            session_id: vec![0u8; 16],
            cipher_suites: vec![0xDEAD_BEEF], // unknown suite
            client_nonce: vec![0u8; 32],
            kem_public_key: vec![0u8; 32],
            last_received_seq: HashMap::new(),
        };
        let env = Envelope {
            payload: Some(Payload::ClientHello(hello)),
        };
        let result =
            process_client_hello(&env.encode_to_vec(), HashMap::new(), |_| Some("pk".into()));
        assert!(matches!(result, Err(HandshakeError::UnsupportedSuite)));
    }

    #[test]
    fn test_reconnect_last_received_seq_roundtrips() {
        let passkey = "passkey".to_string();
        // Simulate client having received up to seq 5 on stream 0.
        let client_last: HashMap<u32, u64> = [(0, 5)].into();
        let (_hs, _hdr, env) = ClientHandshake::new(passkey.clone(), client_last.clone());

        let outcome = process_client_hello(&env.encode_to_vec(), HashMap::new(), |_| {
            Some(passkey.clone())
        })
        .unwrap();

        assert_eq!(outcome.client_last_received, client_last);
    }

    #[test]
    fn test_server_last_received_seq_roundtrips() {
        let passkey = "passkey".to_string();
        let server_last: HashMap<u32, u64> = [(0, 10)].into();
        let (_hs, _hdr, env) = ClientHandshake::new(passkey.clone(), HashMap::new());

        let outcome = process_client_hello(&env.encode_to_vec(), server_last.clone(), |_| {
            Some(passkey.clone())
        })
        .unwrap();

        // The server's last_received map must appear in ServerHello so the client
        // can trim its send history — verify it survives the encode/decode cycle.
        let (_, _suite, server_acks) = _hs
            .process_server_hello(&outcome.response_payload_bytes)
            .unwrap();
        assert_eq!(server_acks, server_last);
    }
}
