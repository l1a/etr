use crate::crypto::SessionCipher;
use crate::protocol::Packet;
use std::collections::VecDeque;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Holds the persistent state of a session that survives disconnections.
pub struct SessionState {
    pub client_id: String,
    pub passkey: String,
    pub next_out_seq: u64,
    pub next_in_seq: u64,
    // Store sent payloads to allow replaying them on reconnect
    pub send_history: VecDeque<(u64, Vec<u8>)>,
    pub max_history_size: usize,
}

impl SessionState {
    pub fn new(client_id: String, passkey: String) -> Self {
        Self {
            client_id,
            passkey,
            next_out_seq: 1, // Start sequence numbers at 1 (0 can be reserved for handshake/unsequenced messages)
            next_in_seq: 1,
            send_history: VecDeque::new(),
            max_history_size: 10000,
        }
    }

    /// Record a packet payload in the send history.
    pub fn record_send(&mut self, seq_num: u64, payload: Vec<u8>) {
        self.send_history.push_back((seq_num, payload));
        if self.send_history.len() > self.max_history_size {
            self.send_history.pop_front();
        }
    }

    /// Evict packets from history that the peer has acknowledged receiving.
    pub fn acknowledge_up_to(&mut self, ack_seq: u64) {
        while let Some(&(seq, _)) = self.send_history.front() {
            if seq <= ack_seq {
                self.send_history.pop_front();
            } else {
                break;
            }
        }
    }

    /// Get all packets that need to be replayed after a reconnection.
    pub fn get_replay_packets(&self, peer_last_received: u64) -> Vec<(u64, Vec<u8>)> {
        self.send_history
            .iter()
            .filter(|&&(seq, _)| seq > peer_last_received)
            .cloned()
            .collect()
    }
}

/// Reads a framed packet from a TcpStream.
/// Each packet is prefixed with a 4-byte big-endian length field.
pub async fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    // Safety guard to avoid allocating huge buffers on corrupted streams
    if len > 10 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Packet frame too large",
        ));
    }

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Writes a framed packet to a TcpStream.
pub async fn write_frame(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    let len = data.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

/// Helper to send an encrypted packet.
pub async fn send_encrypted(
    stream: &mut TcpStream,
    cipher: &SessionCipher,
    seq_num: u64,
    packet: &Packet,
) -> io::Result<()> {
    let raw_bytes =
        bincode::serialize(packet).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let encrypted = cipher.encrypt(seq_num, &raw_bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Encryption failed: {:?}", e),
        )
    })?;

    write_frame(stream, &encrypted).await
}

/// Helper to read and decrypt a packet.
pub async fn recv_encrypted(
    stream: &mut TcpStream,
    cipher: &SessionCipher,
    seq_num: u64,
) -> io::Result<Packet> {
    let encrypted = read_frame(stream).await?;

    let decrypted = cipher.decrypt(seq_num, &encrypted).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Decryption failed: {:?}", e),
        )
    })?;

    let packet: Packet = bincode::deserialize(&decrypted)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok(packet)
}
