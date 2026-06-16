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
