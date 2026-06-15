use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Packet {
    // Handshake
    ConnectRequest {
        client_id: String,
        client_nonce: [u8; 16],
    },
    ConnectResponse {
        server_nonce: [u8; 16],
    },
    Auth {
        mac: [u8; 32], // HMAC or encrypted validation token
    },

    // Session Synchronization (Reconnection)
    SyncRequest {
        last_received_seq: u64,
    },
    SyncResponse {
        last_received_seq: u64,
    },

    // Terminal Stream / Events
    TerminalData {
        seq_num: u64,
        data: Vec<u8>,
    },
    TerminalResize {
        rows: u16,
        cols: u16,
    },

    // Keepalive / Control
    Heartbeat,
    Disconnect,
}
