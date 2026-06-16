# etr Protocol Specification

Version: 1 (`PROTOCOL_VERSION = 0x01`)

This document is the authoritative reference for the etr wire protocol. It is
intended for implementers writing compatible clients or servers in any language.

---

## 1. Transport

etr uses **UDP**. The server binds an OS-assigned port (printed to stdout during
bootstrap; see §2). There is no fixed well-known port. A single UDP socket carries
all traffic for one session.

UDP is used without any fragmentation layer: each datagram is one complete protocol
message. The caller is responsible for keeping messages within MTU (~1200 bytes for
data payloads is a safe ceiling on most paths).

---

## 2. SSH Bootstrap

Before any UDP traffic flows, the client and server exchange credentials over an
SSH-encrypted channel. This gives the server the passkey it needs to authenticate
the first packet, and tells the client which UDP port to use.

### Client → Server (SSH stdin)

```
SESSION_ID_HEX/PASSKEY/TERM\n
```

| Field | Format | Description |
|-------|--------|-------------|
| `SESSION_ID_HEX` | 32 hex chars (16 bytes) | Randomly generated session identifier |
| `PASSKEY` | printable ASCII, ≤ 64 chars | Randomly generated pre-shared secret |
| `TERM` | string | Value of `$TERM` on the client (e.g. `xterm-256color`) |

Fields are separated by `/`. The line is terminated by `\n`.

### Server → Client (SSH stdout)

```
PORT <decimal>\n
```

The server binds a UDP socket on port 0 (OS-assigned), then writes this line before
forking. The client reads it and uses `<decimal>` as the destination UDP port for all
subsequent traffic.

After printing the port, the server forks: the parent exits (allowing SSH to close
cleanly), the child runs the session.

---

## 3. Packet Layout

Every UDP datagram begins with a **26-byte fixed header**, followed immediately by a
**protobuf-encoded `Envelope`** (plaintext or AEAD-encrypted depending on the flags
field).

```
 0       1       2                  18                          26
 ┌───────┬───────┬──────────────────┬──────────────────────────┐
 │version│ flags │   session_id     │        packet_seq        │
 │ 1 byte│ 1 byte│    16 bytes      │         8 bytes          │
 └───────┴───────┴──────────────────┴──────────────────────────┘
 ↑─────────────────── 26-byte header ─────────────────────────↑
 ↑── unencrypted ──────────────────────────────────────────────↑

 [26 bytes: header] [N bytes: Envelope (see §5)]
```

All multi-byte integers are **big-endian**.

### 3.1 Header Fields

| Field | Bytes | Description |
|-------|-------|-------------|
| `version` | 1 | Protocol version. Must be `0x01`. Receivers must reject unknown versions. |
| `flags` | 1 | Bitfield (see §3.2). |
| `session_id` | 16 | Opaque identifier matching the `SESSION_ID_HEX` from bootstrap. Used for server-side routing before decryption. |
| `packet_seq` | 8 | Monotonically increasing per-session counter, starting at 1. Used as the AEAD nonce (see §4.2). |

### 3.2 Flag Bits

| Bit | Name | Meaning |
|-----|------|---------|
| `0x01` | `HANDSHAKE` | Payload is a handshake message. `ClientHello` is plaintext; `ServerHello` is encrypted with the hello key (§4.1), not the session key. |

All other bits are reserved and must be zero.

---

## 4. Cryptography

### 4.1 Cipher Suites

Suites are identified by a `uint32` wire ID negotiated in the handshake.

| ID | KEM | AEAD | KDF | Feature |
|----|-----|------|-----|---------|
| `0x01` | ML-KEM-1024 (FIPS 203) | AES-256-GCM | HKDF-SHA3-256 | `pqc` |
| `0x02` | ML-KEM-768 (FIPS 203) | AES-256-GCM | HKDF-SHA-256 | `pqc` |
| `0x03` | X25519 | AES-256-GCM | HKDF-SHA-256 | always |
| `0x04` | X25519 | ChaCha20-Poly1305 | HKDF-SHA-256 | always |

The client sends all supported suites in preference order (highest first). The server
picks the first suite it supports from the client's list.

### 4.2 AEAD Nonce Construction

Both AES-256-GCM and ChaCha20-Poly1305 use a 12-byte nonce derived from `packet_seq`:

```
nonce[0..4]  = 0x00 0x00 0x00 0x00   (4 zero bytes)
nonce[4..12] = packet_seq             (8 bytes, big-endian)
```

`packet_seq` starts at 1 and is strictly monotonically increasing. A receiver must
reject any packet whose `packet_seq` it has already seen (replay protection).

### 4.3 Hello Key

Used to encrypt `ServerHello` before the full session key is available. Derived
from the passkey and the client nonce only, so the server can produce it immediately
upon receiving `ClientHello`.

```
hello_key = HKDF-SHA-256(
    ikm  = passkey,
    salt = client_nonce,         // 32 bytes
    info = "etr-hello-v1",
    len  = 32
)
```

`ServerHello` is encrypted with `AES-256-GCM` using `hello_key` and `packet_seq = 0`.

### 4.4 Session Key

Derived after the KEM exchange completes. Both sides derive the same key
independently.

```
ikm         = passkey ‖ kem_shared_secret
salt        = client_nonce ‖ server_nonce     // 64 bytes total
session_key = KDF(ikm, salt, "etr-session-v1", 32)
```

Where `KDF` is:
- `HKDF-SHA3-256` for suite `0x01`
- `HKDF-SHA-256` for suites `0x02`, `0x03`, `0x04`

All subsequent data packets are AEAD-encrypted with `session_key`.

### 4.5 Key Exchange Details

**X25519** (suites `0x03`, `0x04`):
- Client generates an ephemeral X25519 keypair; sends the 32-byte public key in `ClientHello.kem_public_key`.
- Server generates its own ephemeral keypair; computes `shared_secret = DH(server_private, client_public)`; sends the 32-byte server public key in `ServerHello.kem_ciphertext`.
- Client computes `shared_secret = DH(client_private, server_public)`.

**ML-KEM-768** (suite `0x02`) / **ML-KEM-1024** (suite `0x01`):
- Client generates an ephemeral ML-KEM keypair; sends the encapsulation key bytes in `ClientHello.kem_public_key`.
  - ML-KEM-768 encapsulation key: 1184 bytes
  - ML-KEM-1024 encapsulation key: 1568 bytes
- Server encapsulates to the client's key; sends the resulting ciphertext in `ServerHello.kem_ciphertext` and holds the shared secret.
  - ML-KEM-768 ciphertext: 1088 bytes
  - ML-KEM-1024 ciphertext: 1568 bytes
- Client decapsulates the ciphertext with its decapsulation key to recover the shared secret.

---

## 5. Protobuf Envelope

The payload following the 26-byte header is a protobuf-encoded `Envelope`. During
handshake (`HANDSHAKE` flag set), `ClientHello` is sent plaintext; `ServerHello` is
sent as the AEAD ciphertext of an inner `Envelope` (encrypted with the hello key).
After handshake, all `Envelope`s are AEAD-encrypted with the session key.

```protobuf
message Envelope {
  oneof payload {
    ClientHello    client_hello    = 1;
    ServerHello    server_hello    = 2;
    StreamOpen     stream_open     = 3;
    StreamClose    stream_close    = 4;
    StreamData     stream_data     = 5;
    StreamAck      stream_ack      = 6;
    TerminalResize terminal_resize = 7;
    Heartbeat      heartbeat       = 8;
    Disconnect     disconnect      = 9;
  }
}

message ClientHello {
  uint32 protocol_version             = 1;  // Must be 1
  bytes  session_id                   = 2;  // 16 bytes
  repeated uint32 cipher_suites       = 3;  // In preference order
  bytes  client_nonce                 = 4;  // 32 bytes
  bytes  kem_public_key               = 5;  // X25519: 32B; ML-KEM-768: 1184B; ML-KEM-1024: 1568B
  map<uint32, uint64> last_received_seq = 6; // Per-stream highest seq received (reconnect)
}

message ServerHello {
  uint32 chosen_suite                 = 1;  // Selected cipher suite ID
  bytes  server_nonce                 = 2;  // 32 bytes
  bytes  kem_ciphertext               = 3;  // X25519: 32B; ML-KEM-768: 1088B; ML-KEM-1024: 1568B
  map<uint32, uint64> last_received_seq = 4; // Per-stream highest seq received by server
}

message StreamOpen {
  uint32 stream_id   = 1;
  StreamType type    = 2;
  string remote_host = 3;  // Port-forward only
  uint32 remote_port = 4;  // Port-forward only
}

message StreamClose {
  uint32 stream_id  = 1;
  uint32 error_code = 2;  // 0 = clean close
}

message StreamData {
  uint32 stream_id = 1;
  uint64 seq_num   = 2;  // Per-stream, monotonically increasing from 1
  bytes  data      = 3;
}

message StreamAck {
  uint32 stream_id = 1;
  uint64 ack_seq   = 2;  // Acknowledges all seq_num ≤ ack_seq
}

message TerminalResize {
  uint32 rows = 1;
  uint32 cols = 2;
}

message Heartbeat {}

message Disconnect {}

enum StreamType {
  TERMINAL     = 0;
  PORT_FORWARD = 1;
}
```

---

## 6. Handshake Flow

```
Client                                          Server
  │                                               │
  │  [SSH bootstrap: send SESSION_ID/PASSKEY/TERM]│
  │  [SSH bootstrap: recv PORT <n>]               │
  │                                               │
  │── UDP: header(HANDSHAKE, session_id, seq=1) ─►│
  │        Envelope { ClientHello }   (plaintext) │
  │                                               │  lookup passkey by session_id
  │                                               │  select cipher suite
  │                                               │  run KEM encapsulate
  │                                               │  derive hello_key, session_key
  │                                               │
  │◄─ UDP: header(HANDSHAKE, session_id, seq=1) ──│
  │        AES-256-GCM(hello_key, Envelope{       │
  │            ServerHello })                     │
  │                                               │
  │  derive hello_key                             │
  │  decrypt ServerHello                          │
  │  run KEM decapsulate                          │
  │  derive session_key                           │
  │                                               │
  │◄══════════ encrypted session data ═══════════►│
```

The handshake completes in **one round trip**. Both sides can send data
immediately after the handshake — the server may begin sending replayed stream
data before receiving the first data packet from the client.

---

## 7. Stream Multiplexing

Multiple logical streams share one session. Each stream has an independent
per-stream sequence number space (not the packet-level `packet_seq`).

| Stream ID | Purpose |
|-----------|---------|
| 0 | Terminal PTY (always exists) |
| 1+ | Port-forward connections |

`StreamData.seq_num` starts at 1 and increments by 1 per chunk. `StreamAck.ack_seq`
acknowledges all chunks up to and including that sequence number, allowing the sender
to trim its replay buffer. Unacknowledged chunks are replayed on reconnect.

---

## 8. Reconnect

When the client detects a missed heartbeat (15-second idle timeout), it sends a new
`ClientHello` with:
- The same `session_id` as the original session
- A fresh `client_nonce` and `kem_public_key` (new ephemeral keys per reconnect)
- `last_received_seq` populated from the client's stream state

The server matches on `session_id`, looks up the passkey, runs a fresh KEM
encapsulation, and responds with `ServerHello` including its own
`last_received_seq`. Both sides then replay any unacknowledged stream data.

The server keeps session state alive for **30 minutes** after the last packet.
A reconnecting client may come from a different source IP/port — the session is
keyed by `session_id`, not network address.

---

## 9. Disconnect

A clean disconnect is signalled by sending `Envelope { Disconnect }`. The receiver
should not attempt to reconnect. The sender may close the socket immediately after.

If the connection drops without a `Disconnect` (crash, network loss), the server
holds state for the reconnect window (§8).

---

## 10. Heartbeat

Both sides send `Envelope { Heartbeat }` periodically to prevent idle timeout.
The client uses a 5-second heartbeat interval. The server considers the client
gone after 15 seconds without any packet.
