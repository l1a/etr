# etr Wire Protocol

This document is the authoritative reference for the etr wire protocol as of **v0.3.0**.

For the history of the pre-0.3.0 UDP/KEM-based protocol, see the git log.

---

## 1. Transport

etr uses **QUIC** (RFC 9000) via the [quinn](https://crates.io/crates/quinn) crate.
QUIC provides reliable, ordered, multiplexed streams with TLS 1.3 and congestion
control built in. There is no fixed well-known port; the server binds an OS-assigned
port and communicates it to the client over SSH (see §2).

---

## 2. SSH Bootstrap

Before any QUIC traffic flows, the client and server exchange credentials over an
SSH-encrypted channel. This gives the server the passkey it needs to authenticate
the first message, tells the client which QUIC port to use, and transfers the server's
ephemeral TLS certificate for pinning.

### Client → Server (SSH stdin)

```
SESSION_ID_HEX/PASSKEY/TERM\n
[KEY=VALUE\n ...]
[ETRCMD:<command>\n]
```

The first line is required. It is followed by zero or more optional lines, each
terminated by `\n`, sent before `stdin` is closed:

| Line | Format | Description |
|------|--------|-------------|
| Header | `SESSION_ID_HEX/PASSKEY/TERM` | Required. Fields separated by `/`. |
| Env var | `KEY=VALUE` | Optional, repeatable. Sets an environment variable in the remote shell/command. Automatically includes `LANG`, `LC_*`, `COLORTERM`, and `TERM_PROGRAM*` if set on the client. |
| Remote command | `ETRCMD:<command>` | Optional, at most once. If present, the server runs `$SHELL -c <command>` instead of an interactive shell and sends `Disconnect` when the command exits. Lines without a `=` and not starting with `ETRCMD:` are ignored for forward compatibility. |

| Header field | Format | Description |
|-------|--------|-------------|
| `SESSION_ID_HEX` | 32 hex chars (16 bytes) | Randomly generated session identifier |
| `PASSKEY` | printable ASCII, ≤ 64 chars | Randomly generated pre-shared secret |
| `TERM` | string | Value of `$TERM` on the client (e.g. `xterm-256color`) |

### Server → Client (SSH stdout)

```
PORT <decimal> CERT <cert_der_hex>\n
```

| Field | Description |
|-------|-------------|
| `PORT <n>` | QUIC port the server has bound |
| `CERT <hex>` | DER-encoded self-signed TLS certificate, hex-encoded |

After printing this line, the server forks: the parent exits (allowing SSH to close
cleanly), the child calls `setsid()`, redirects stdio to `/dev/null`, and runs the
session loop.

The client pins the received certificate — analogous to SSH host-key trust — and uses
it as the sole trusted root for the QUIC connection. No CA verification is performed.

---

## 3. TLS

QUIC mandates TLS 1.3. The server generates a fresh ephemeral self-signed certificate
per session using `rcgen`. The client verifies only the certificate's SHA-256 SPKI
fingerprint against what was received over SSH. The negotiated cipher suite is one of:

- `TLS_AES_256_GCM_SHA384`
- `TLS_AES_128_GCM_SHA256`
- `TLS_CHACHA20_POLY1305_SHA256`

Selected by `rustls` based on platform capability.

---

## 4. QUIC Stream Layout

Every QUIC bidirectional stream opened by the **client** begins with a **1-byte stream
tag** identifying its purpose:

| Tag | Stream | Content |
|-----|--------|---------|
| `0x01` | Control | 4-byte-length-prefixed protobuf `Envelope` messages |
| `0x02` | PTY | Seq-numbered raw chunks (see §5.2) |
| `0x03` | Port-forward | `StreamOpen` header then raw bytes (TCP) or `UdpDatagram` envelopes (UDP) |

The server reads the tag byte and routes accordingly. Multiple forward streams (one per
TCP connection; one per UDP `-L` spec) may be open simultaneously alongside the control
and PTY streams.

---

## 5. Message Formats

### 5.1 Control stream (tag `0x01`)

After the tag byte, both directions use **4-byte big-endian length-prefixed protobuf**:

```
[4-byte len BE][protobuf bytes]
```

**Session establishment** (control stream only, in order):

```
client → server: [len][SessionOpen]
server → client: [len][SessionAccept]
```

After `SessionAccept`, both sides send `Envelope` messages on the control stream for
the lifetime of the session.

### 5.2 PTY stream (tag `0x02`)

Each chunk in both directions:

```
[8-byte seq BE][4-byte data_len BE][data]
```

`seq` starts at 1 and increments by 1 per chunk. The receiver tracks the highest
seen seq number as the `last_received_seq` watermark for replay on reconnect.

**Server → client**: PTY output (terminal bytes)  
**Client → server**: stdin keypresses

### 5.3 Forward stream (tag `0x03`)

After the tag byte, the client sends a length-prefixed `StreamOpen` proto, then:

- **TCP**: raw bytes flow bidirectionally until either end closes the QUIC stream.
- **UDP**: each datagram is wrapped in a length-prefixed `UdpDatagram` proto in both
  directions. `peer_addr` + `peer_port` in `UdpDatagram` carry the UDP sender address
  for last-sender reply routing.

---

## 6. Protobuf Definitions

```protobuf
syntax = "proto3";

message Envelope {
  oneof payload {
    Heartbeat      heartbeat       = 8;
    TerminalResize terminal_resize = 7;
    Disconnect     disconnect      = 9;
  }
}

message SessionOpen {
  bytes                   session_id        = 1;  // 16 bytes
  string                  passkey           = 2;
  map<uint32, uint64>     last_received_seq = 3;  // per-stream watermarks
  repeated string         reverse_forwards  = 4;  // "-R" specs from the client
  bool                    gateway_ports     = 5;  // bind reverse listeners to [::] (all interfaces)
}

message SessionAccept {
  map<uint32, uint64>     last_received_seq = 1;  // per-stream watermarks
}

message StreamOpen {
  uint32     stream_id      = 1;
  StreamType type           = 2;
  string     remote_host    = 3;  // port-forward target host
  uint32     remote_port    = 4;  // port-forward target port
  ForwardProto forward_proto = 5; // TCP or UDP
}

message StreamClose {
  uint32 stream_id  = 1;
  uint32 error_code = 2;  // 0 = clean close
}

message UdpDatagram {
  bytes  data      = 1;
  string peer_addr = 2;
  uint32 peer_port = 3;
}

message TerminalResize {
  uint32 rows = 1;
  uint32 cols = 2;
}

message Heartbeat {
  map<uint32, uint64> last_received_seq = 1;  // piggybacked ack watermarks
}

message Disconnect {}

enum StreamType {
  TERMINAL     = 0;
  PORT_FORWARD = 1;
}
```

---

## 7. Session Establishment Flow

```
Client                                          Server
  │                                               │
  │  [SSH: send SESSION_ID/PASSKEY/TERM\n]        │
  │  [SSH: recv PORT <n> CERT <hex>\n]            │
  │                                               │
  │── QUIC connect (TLS 1.3, pinned cert) ───────►│
  │                                               │
  │── stream tag 0x01 ───────────────────────────►│
  │── [len][SessionOpen{                           │
  │         session_id, passkey,                   │
  │         last_received_seq,                     │
  │         reverse_forwards,    // -R specs       │
  │         gateway_ports}] ─────────────────────►│
  │                                               │  verify passkey
  │                                               │  bind reverse-forward listeners
  │                                               │  replay PTY history if reconnect
  │◄── [len][SessionAccept{last_received_seq}] ───│
  │                                               │
  │── stream tag 0x02 (PTY) ────────────────────►│
  │                                               │  server may replay missed chunks
  │                                               │  immediately after SessionAccept
  │◄══════════ live session data ════════════════►│
```

---

## 8. Reconnect

When the client detects missed heartbeats (15-second idle timeout), any pending QUIC
tasks fail and the client re-enters the connection loop:

1. Opens a new QUIC connection to the same server address and port.
2. Sends a new `SessionOpen` with the same `session_id` and `passkey`, and the
   current `last_received_seq` map.
3. The server matches on `session_id` + `passkey`, looks up the live session, and
   sends `SessionAccept` with its own `last_received_seq`.
4. Both sides replay unacknowledged chunks (seq > peer's watermark) on the PTY stream.
5. Port-forward streams are re-opened fresh; they are not replayed.

The server keeps session state alive for **30 minutes** after the last packet. A
reconnecting client may come from a different source IP/port.

---

## 9. Heartbeat and Ack Piggybacking

Both sides send `Envelope { Heartbeat { last_received_seq } }` on the control stream
every 5 seconds. The `last_received_seq` map lets the sender trim its replay buffer
continuously — acknowledged history entries are discarded immediately, so in normal
use the buffer stays near zero.

The replay buffer is additionally capped at **4 MB per stream** (byte-based, oldest
entries evicted first) to bound memory if acks are delayed.

---

## 10. Disconnect

A clean disconnect is signalled by sending `Envelope { Disconnect }` on the control
stream. The receiver closes the session immediately and the `etrs` process exits.

If the connection drops without a `Disconnect` (crash, network loss), the server holds
state for the reconnect window (§8).
