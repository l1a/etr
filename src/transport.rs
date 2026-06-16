// SPDX-License-Identifier: GPL-3.0-or-later
//! UDP transport: send and receive [`PacketHeader`] + protobuf [`Envelope`] datagrams.
//!
//! Every datagram is laid out as:
//! ```text
//! [ PacketHeader (26 bytes, plaintext) ][ payload bytes ]
//! ```
//!
//! For DATA packets the payload bytes are AEAD-encrypted protobuf.
//! For HANDSHAKE packets (`FLAG_HANDSHAKE` set) the payload is either plaintext
//! protobuf (ClientHello) or hello-key-encrypted protobuf (ServerHello).
use std::net::SocketAddr;

use prost::Message;
use tokio::net::UdpSocket;

use crate::crypto::AeadCipher;
use crate::protocol::{Envelope, PacketHeader, HEADER_SIZE};

/// Maximum UDP payload we will send.  Stays safely under common path MTUs
/// while leaving room for the 26-byte header and UDP/IP overhead.
/// Handshake packets carrying large ML-KEM keys may exceed this and will
/// be fragmented at the IP layer; that is acceptable for one-time setup.
pub const MAX_DATAGRAM: usize = 65_507; // absolute UDP max; OS will fragment if needed

/// Combined header + payload extracted from a received datagram.
pub struct ReceivedPacket {
    pub peer: SocketAddr,
    pub header: PacketHeader,
    /// Raw payload bytes (encrypted or plaintext depending on `header.is_handshake()`).
    pub payload_bytes: Vec<u8>,
}

/// Encode and send one datagram.
///
/// If `cipher` is `Some`, the envelope is AEAD-encrypted with `header.packet_seq`
/// as the nonce.  If `None`, the envelope bytes are sent in the clear.
pub async fn send_packet(
    socket: &UdpSocket,
    dest: SocketAddr,
    header: &PacketHeader,
    envelope: &Envelope,
    cipher: Option<&AeadCipher>,
) -> std::io::Result<()> {
    let env_bytes = envelope.encode_to_vec();
    let payload = match cipher {
        Some(c) => c
            .encrypt(header.packet_seq, &env_bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
        None => env_bytes,
    };

    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
    buf.extend_from_slice(&header.encode());
    buf.extend_from_slice(&payload);
    socket.send_to(&buf, dest).await?;
    Ok(())
}

/// Receive one datagram, parse the header, and return a [`ReceivedPacket`].
///
/// Returns `None` on a parse error (unknown version, truncated header, etc.)
/// so the caller can simply skip malformed datagrams.
pub async fn recv_packet(socket: &UdpSocket) -> std::io::Result<Option<ReceivedPacket>> {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let (len, peer) = socket.recv_from(&mut buf).await?;
    let data = &buf[..len];

    let Some(header) = PacketHeader::decode(data) else {
        return Ok(None);
    };
    let payload_bytes = data[HEADER_SIZE..].to_vec();
    Ok(Some(ReceivedPacket { peer, header, payload_bytes }))
}

/// Decrypt and decode the payload of a DATA packet.
pub fn decode_data_packet(
    payload_bytes: &[u8],
    packet_seq: u64,
    cipher: &AeadCipher,
) -> std::io::Result<Envelope> {
    let plaintext = cipher
        .decrypt(packet_seq, payload_bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Envelope::decode(plaintext.as_slice())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Decode a plaintext (handshake) payload — no decryption.
pub fn decode_plaintext_packet(payload_bytes: &[u8]) -> std::io::Result<Envelope> {
    Envelope::decode(payload_bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{AeadCipher, CipherSuiteId, KemKeyPair, derive_session_cipher,
                        encapsulate, generate_nonce};
    use crate::protocol::{Envelope, Heartbeat, Payload, PacketHeader};

    fn make_cipher() -> AeadCipher {
        let suite = CipherSuiteId::X25519Aes256GcmSha256;
        let kp = KemKeyPair::generate(suite);
        let (ct, server_secret) = encapsulate(suite, &kp.public_key_bytes()).unwrap();
        let client_secret = kp.decapsulate(&ct).unwrap();
        let cn = generate_nonce();
        let sn = generate_nonce();
        // Both sides derive the same cipher; we just need one for these tests.
        derive_session_cipher(suite, b"passkey", &server_secret, &cn, &sn);
        derive_session_cipher(suite, b"passkey", &client_secret, &cn, &sn)
    }

    fn heartbeat_envelope() -> Envelope {
        Envelope { payload: Some(Payload::Heartbeat(Heartbeat {})) }
    }

    // ── decode_data_packet ────────────────────────────────────────────────────

    #[test]
    fn decode_data_packet_round_trip() {
        let cipher = make_cipher();
        let envelope = heartbeat_envelope();
        let plaintext = prost::Message::encode_to_vec(&envelope);
        let ciphertext = cipher.encrypt(42, &plaintext).unwrap();
        let decoded = decode_data_packet(&ciphertext, 42, &cipher).unwrap();
        assert!(matches!(decoded.payload, Some(Payload::Heartbeat(_))));
    }

    #[test]
    fn decode_data_packet_wrong_key_fails() {
        let enc_cipher = make_cipher();
        let dec_cipher = make_cipher(); // independent key
        let plaintext = prost::Message::encode_to_vec(&heartbeat_envelope());
        let ciphertext = enc_cipher.encrypt(1, &plaintext).unwrap();
        assert!(decode_data_packet(&ciphertext, 1, &dec_cipher).is_err());
    }

    #[test]
    fn decode_data_packet_mutated_ciphertext_fails() {
        let cipher = make_cipher();
        let plaintext = prost::Message::encode_to_vec(&heartbeat_envelope());
        let mut ciphertext = cipher.encrypt(1, &plaintext).unwrap();
        ciphertext[0] ^= 0xFF;
        assert!(decode_data_packet(&ciphertext, 1, &cipher).is_err());
    }

    #[test]
    fn decode_data_packet_wrong_seq_fails() {
        let cipher = make_cipher();
        let plaintext = prost::Message::encode_to_vec(&heartbeat_envelope());
        let ciphertext = cipher.encrypt(1, &plaintext).unwrap();
        assert!(decode_data_packet(&ciphertext, 2, &cipher).is_err());
    }

    // ── decode_plaintext_packet ───────────────────────────────────────────────

    #[test]
    fn decode_plaintext_packet_valid() {
        use prost::Message;
        let env = heartbeat_envelope();
        let bytes = env.encode_to_vec();
        let decoded = decode_plaintext_packet(&bytes).unwrap();
        assert!(matches!(decoded.payload, Some(Payload::Heartbeat(_))));
    }

    #[test]
    fn decode_plaintext_packet_invalid_protobuf_fails() {
        assert!(decode_plaintext_packet(b"\xFF\xFF\xFF\xFF garbage").is_err());
    }

    #[test]
    fn decode_plaintext_packet_empty_bytes_gives_empty_envelope() {
        // Empty bytes decode to an Envelope with no payload (all fields optional in proto3).
        let decoded = decode_plaintext_packet(&[]).unwrap();
        assert!(decoded.payload.is_none());
    }

    // ── send_packet / recv_packet loopback ────────────────────────────────────

    #[tokio::test]
    async fn send_recv_plaintext_loopback() {
        use prost::Message;
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();

        let session_id = [1u8; 16];
        let header = PacketHeader::new(0, session_id, 7);
        let envelope = heartbeat_envelope();

        send_packet(&client, server_addr, &header, &envelope, None).await.unwrap();

        let pkt = recv_packet(&server).await.unwrap().unwrap();
        assert_eq!(pkt.header.session_id, session_id);
        assert_eq!(pkt.header.packet_seq, 7);
        let decoded = decode_plaintext_packet(&pkt.payload_bytes).unwrap();
        assert_eq!(decoded.encode_to_vec(), envelope.encode_to_vec());
    }

    #[tokio::test]
    async fn send_recv_encrypted_loopback() {
        use prost::Message;
        let server_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_sock.local_addr().unwrap();

        let cipher = make_cipher();
        let session_id = [2u8; 16];
        let header = PacketHeader::new(0, session_id, 3);
        let envelope = heartbeat_envelope();

        send_packet(&client_sock, server_addr, &header, &envelope, Some(&cipher)).await.unwrap();

        let pkt = recv_packet(&server_sock).await.unwrap().unwrap();
        let decoded = decode_data_packet(&pkt.payload_bytes, 3, &cipher).unwrap();
        assert_eq!(decoded.encode_to_vec(), envelope.encode_to_vec());
    }

    #[tokio::test]
    async fn recv_packet_truncated_header_returns_none() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        // Send fewer bytes than HEADER_SIZE.
        client.send_to(&[0u8; 10], server_addr).await.unwrap();
        let result = recv_packet(&server).await.unwrap();
        assert!(result.is_none());
    }
}
