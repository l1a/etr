// SPDX-License-Identifier: GPL-3.0-or-later
//! QUIC transport helpers: certificate setup, server/client config, and
//! length-prefixed message framing for control and PTY streams.

use std::io;
use std::sync::Arc;

use prost::Message as _;
use quinn::{ClientConfig, RecvStream, SendStream, ServerConfig};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::protocol::Envelope;

/// Stream tag byte — first byte sent by the client on every bidi QUIC stream.
pub const TAG_CONTROL: u8 = 0x01;
pub const TAG_PTY: u8 = 0x02;
pub const TAG_FORWARD: u8 = 0x03;

/// Generate an ephemeral self-signed cert with SAN = "etr".
///
/// Returns `(cert_der, key_der)` as raw DER bytes.  The cert is transmitted
/// over the authenticated SSH channel; the client pins it so no CA is needed.
pub fn generate_self_signed_cert() -> (CertificateDer<'static>, Vec<u8>) {
    let ck = generate_simple_self_signed(vec!["etr".to_string()])
        .expect("rcgen: cert generation cannot fail");
    let cert_der = ck.cert.der().clone();
    let key_der = ck.signing_key.serialize_der();
    (cert_der, key_der)
}

/// QUIC transport tuning.
///
/// # THIS IS A STOPGAP FOR AN UPSTREAM REGRESSION — REVISIT ON A quinn-proto BUMP
///
/// The underlying defect is **quinn-rs/quinn#2809**, confirmed by the maintainers as a
/// regression: `Assembler::defragment` leaves high-utilisation *contiguous* buffers as separate
/// entries, and the guard then counts retained buffers rather than genuine gaps — so a stream
/// with **no actual gaps at all** can trip `TooManyChunks`. The fix,
/// **quinn-rs/quinn#2814** ("proto: coalesce contiguous chunks during defragment"), was merged
/// on 2026-09-03 and a backport was promised.
///
/// As of this commit the newest *published* quinn-proto is 0.11.17 (2026-08-17), which predates
/// the merge — so there is no released version to upgrade to. Both 0.11.15 (what we pin) and
/// 0.11.17 reproduce it.
///
/// **When a quinn-proto carrying #2814 is released: bump it, then raise this window back.** The
/// small value costs per-stream bandwidth-delay product — roughly 41 Mb/s at 100 ms RTT against
/// ~335 Mb/s at 4 MB — which matters for a tool whose whole point is long-distance sessions.
/// The regression test below encodes the constraint, so it will fail and prompt a decision
/// rather than letting the window drift back silently.
///
/// # `stream_receive_window` is a correctness bound, not a performance dial
///
/// It was 4 MB from the v0.4.x throughput work until v0.9.3, and that is what made **any**
/// sustained forward tear down the whole QUIC connection — every forward and the interactive
/// shell with it:
///
/// ```text
/// ConnectionClose { error_code: INTERNAL_ERROR, reason: "too many gaps in stream buffer" }
/// ```
///
/// ## The mechanism, which is not what the message suggests
///
/// "Gaps" implies packet loss. There is none: measured across every failing run, the kernel's
/// UDP `RcvbufErrors` and `SndbufErrors` counters did not move at all. The real path is:
///
/// 1. A forwarding relay reads a QUIC stream and writes a TCP socket **sequentially**. While
///    `write_all` is blocked by ordinary TCP back-pressure, nothing drains that QUIC stream.
/// 2. quinn keeps accepting data for it, up to `stream_receive_window`, storing **one chunk per
///    received STREAM frame**.
/// 3. `Assembler::defragment` does *not* merge those chunks. A chunk whose bytes fill ≥5/6 of
///    their allocation is marked `defragmented` and kept as its own entry
///    (`try_mark_defragment` in quinn-proto's `assembler.rs`) — which is exactly what a
///    full-size frame is.
/// 4. Past **1024** chunks quinn aborts the *connection* with INTERNAL_ERROR.
///
/// So the governing relationship is a count, not a rate:
///
/// ```text
/// stream_receive_window / frame_payload  <  1024
/// ```
///
/// At an internet-typical 1200-byte payload, 4 MB is ~3500 chunks — over the cap by 3.4×, so it
/// fails as soon as a relay stalls, which under load is constantly. **512 KB is ~437 chunks: a
/// 2.3× margin, and it holds down to 512-byte frames.**
///
/// ## It cost nothing to fix
///
/// Measured on loopback, 64 KiB writes through a `-L` TCP forward, 10-12 s runs, varying only
/// this value:
///
/// | window | outcome | throughput |
/// |---|---|---|
/// | 4 MB | **died in <1 s** | — |
/// | 2 MB | survived | 3.05 Gb/s |
/// | 1.25 MB (quinn default) | survived | 3.11 Gb/s |
/// | 1 MB | survived | 3.03 Gb/s |
/// | **512 KB** | **survived** | **3.40 Gb/s** |
///
/// The 4 MB window bought no throughput whatsoever. Note 2 MB passes *here* only because
/// loopback uses ~2 KB frames; at 1200 bytes it would be ~1750 chunks and would fail. Do not
/// raise this value on the strength of a loopback measurement.
///
/// **Two things that look like fixes and are not**, both tried and reverted: enlarging the UDP
/// socket buffers, and shrinking `send_window`. Both only reduce how much data is in flight, so
/// they delay the chunk count reaching 1024 rather than bounding it — symptom treatment that
/// leaves the failure reachable at a higher rate or on a faster link.
///
/// # The other values
///
/// Connection window 32 MB and send window 32 MB are unchanged: they bound bytes, not chunks,
/// and neither participates in this failure.
///
/// Idle timeout 30 s, with application heartbeats every 5 s; keep-alive 10 s so NAT mappings
/// stay open when no data is in flight.
/// Per-stream receive window. See [`high_throughput_transport`] for why this is a correctness
/// bound rather than a tuning knob, and `stream_window_cannot_exceed_assembler_chunk_cap` for
/// the invariant that keeps it one.
pub const STREAM_RECEIVE_WINDOW: u32 = 512 * 1024;

/// quinn-proto aborts the **connection** once one stream's reassembler holds more than this
/// many chunks (`assembler.rs`: `if self.data.len() > 1024 { return Err(TooManyChunks) }`).
pub const QUINN_ASSEMBLER_CHUNK_CAP: u32 = 1024;

/// Smallest STREAM-frame payload worth planning for: an internet-typical 1200-byte QUIC
/// datagram. Loopback frames are larger (~2 KB), which is why a loopback test alone will
/// happily bless a window that fails on a real network.
pub const MIN_EXPECTED_FRAME_PAYLOAD: u32 = 1200;

fn high_throughput_transport() -> Arc<quinn::TransportConfig> {
    use std::time::Duration;
    let mut t = quinn::TransportConfig::default();
    t.stream_receive_window(
        quinn::VarInt::from_u32(STREAM_RECEIVE_WINDOW), // per stream
    );
    t.receive_window(
        quinn::VarInt::from_u32(32 * 1024 * 1024), // 32 MB connection
    );
    t.send_window(32 * 1024 * 1024); // 32 MB send budget
    // 30 000 ms = 30 s.  Heartbeats every 5 s reset the timer during normal
    // use; the timeout fires only when the peer truly stops responding.
    t.max_idle_timeout(Some(quinn::VarInt::from_u32(30_000).into()));
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    Arc::new(t)
}

/// Build a [`quinn::ServerConfig`] from the given cert + PKCS#8 key.
pub fn server_config(cert: CertificateDer<'static>, key_der: Vec<u8>) -> io::Result<ServerConfig> {
    let key = PrivateKeyDer::Pkcs8(key_der.into());
    let mut cfg = ServerConfig::with_single_cert(vec![cert], key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    cfg.transport_config(high_throughput_transport());
    Ok(cfg)
}

/// Build a [`quinn::ClientConfig`] that trusts exactly the supplied DER cert.
///
/// Because the cert was received over the authenticated SSH channel, this is
/// equivalent to SSH host-key pinning — no CA verification is needed.
pub fn client_config(cert: CertificateDer<'static>) -> io::Result<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(cert)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut cfg = ClientConfig::with_root_certificates(Arc::new(roots))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    cfg.transport_config(high_throughput_transport());
    Ok(cfg)
}

/// A one-line description of the TLS configuration in use for QUIC connections.
///
/// QUIC mandates TLS 1.3; quinn/rustls negotiates one of three cipher suites
/// (AES-256-GCM-SHA384, AES-128-GCM-SHA256, or ChaCha20-Poly1305-SHA256).
/// The specific suite chosen by a given handshake is not exposed in quinn's
/// public API, so we describe the full configured set.
pub fn tls_info() -> &'static str {
    "TLS 1.3/QUIC \
     (AES-256-GCM-SHA384 | AES-128-GCM-SHA256 | ChaCha20-Poly1305-SHA256, \
     cert-pinned)"
}

/// Read the 1-byte stream tag from a recv stream.
pub async fn read_tag(recv: &mut RecvStream) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    Ok(buf[0])
}

/// Write a 4-byte-length-prefixed protobuf [`Envelope`] to a send stream.
///
/// The length prefix and the body go out in **one** `write_all`. They used to be two, which
/// doubled the number of stream writes for every message on every stream. That matters most on
/// the UDP forward path, where one message is one datagram — but it is free everywhere else too,
/// and the v0.4.x throughput work already recorded the general lesson for this codebase: "more
/// syscalls, not fewer copies, determines throughput here".
pub async fn write_msg(send: &mut SendStream, env: &Envelope) -> io::Result<()> {
    let body_len = env.encoded_len();
    let mut framed = Vec::with_capacity(4 + body_len);
    framed.extend_from_slice(&(body_len as u32).to_be_bytes());
    env.encode(&mut framed)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_framed(send, &framed).await
}

/// Write an already-framed `[4-byte big-endian length][body]` message.
///
/// This is the escape hatch for hot paths that build their own frame to avoid per-message
/// allocation — see [`forward::UdpFrameEncoder`](crate::forward::UdpFrameEncoder). The bytes
/// must already carry the length prefix; nothing here inspects or adds one.
pub async fn write_framed(send: &mut SendStream, framed: &[u8]) -> io::Result<()> {
    send.write_all(framed)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
}

/// Read a 4-byte-length-prefixed protobuf [`Envelope`] from a recv stream.
///
/// Returns `Ok(None)` when the peer cleanly closed the stream.
pub async fn read_msg(recv: &mut RecvStream) -> io::Result<Option<Envelope>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, e.to_string())),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 4 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control message too large",
        ));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    Envelope::decode(buf.as_slice())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Write a PTY/stdin chunk: `[8-byte seq big-endian][4-byte data_len][data]`.
pub async fn write_pty_chunk(send: &mut SendStream, seq: u64, data: &[u8]) -> io::Result<()> {
    let mut hdr = [0u8; 12];
    hdr[..8].copy_from_slice(&seq.to_be_bytes());
    hdr[8..12].copy_from_slice(&(data.len() as u32).to_be_bytes());
    send.write_all(&hdr)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    send.write_all(data)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    Ok(())
}

/// Read a PTY/stdin chunk: `[8-byte seq][4-byte data_len][data]`.
///
/// Returns `Ok(None)` on clean stream close.
pub async fn read_pty_chunk(recv: &mut RecvStream) -> io::Result<Option<(u64, Vec<u8>)>> {
    let mut hdr = [0u8; 12];
    match recv.read_exact(&mut hdr).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, e.to_string())),
    }
    let seq = u64::from_be_bytes(hdr[..8].try_into().unwrap());
    let len = u32::from_be_bytes(hdr[8..12].try_into().unwrap()) as usize;
    if len > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PTY chunk too large",
        ));
    }
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    Ok(Some((seq, data)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Heartbeat, Payload};

    fn make_endpoints() -> (quinn::Endpoint, quinn::Endpoint) {
        let (cert, key) = generate_self_signed_cert();
        let srv_cfg = server_config(cert.clone(), key).unwrap();
        let cli_cfg = client_config(cert).unwrap();

        let server_ep = quinn::Endpoint::server(srv_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut client_ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client_ep.set_default_client_config(cli_cfg);
        (server_ep, client_ep)
    }

    #[test]
    fn test_cert_and_config_creation() {
        let (cert, key) = generate_self_signed_cert();
        let _srv = server_config(cert.clone(), key).unwrap();
        let _cli = client_config(cert).unwrap();
    }

    // Spawn the server side of the test as a separate task so the tokio
    // scheduler can interleave the quinn background tasks with the test body.
    // Using tokio::join! in a single task can deadlock because quinn's internal
    // tasks share the same single-threaded test runtime.
    #[tokio::test]
    async fn test_write_read_msg_round_trip() {
        let (server_ep, client_ep) = make_endpoints();
        let server_addr = server_ep.local_addr().unwrap();

        let srv_task = tokio::spawn(async move {
            let conn = server_ep
                .accept()
                .await
                .unwrap()
                .accept()
                .unwrap()
                .await
                .unwrap();
            let (_, mut recv) = conn.accept_bi().await.unwrap();
            read_msg(&mut recv).await.unwrap().unwrap()
        });

        let conn = client_ep
            .connect(server_addr, "etr")
            .unwrap()
            .await
            .unwrap();
        let (mut send, _) = conn.open_bi().await.unwrap();

        let env = Envelope {
            payload: Some(Payload::Heartbeat(Heartbeat::default())),
        };
        write_msg(&mut send, &env).await.unwrap();
        let got = srv_task.await.unwrap();
        assert_eq!(got, env);
    }

    #[tokio::test]
    async fn test_write_read_pty_chunk_round_trip() {
        let (server_ep, client_ep) = make_endpoints();
        let server_addr = server_ep.local_addr().unwrap();

        let srv_task = tokio::spawn(async move {
            let conn = server_ep
                .accept()
                .await
                .unwrap()
                .accept()
                .unwrap()
                .await
                .unwrap();
            let (_, mut recv) = conn.accept_bi().await.unwrap();
            read_pty_chunk(&mut recv).await.unwrap().unwrap()
        });

        let conn = client_ep
            .connect(server_addr, "etr")
            .unwrap()
            .await
            .unwrap();
        let (mut send, _) = conn.open_bi().await.unwrap();

        write_pty_chunk(&mut send, 42, b"hello pty").await.unwrap();
        let (seq, data) = srv_task.await.unwrap();
        assert_eq!(seq, 42);
        assert_eq!(&data, b"hello pty");
    }

    #[tokio::test]
    async fn test_read_tag_round_trip() {
        let (server_ep, client_ep) = make_endpoints();
        let server_addr = server_ep.local_addr().unwrap();

        let srv_task = tokio::spawn(async move {
            let conn = server_ep
                .accept()
                .await
                .unwrap()
                .accept()
                .unwrap()
                .await
                .unwrap();
            let (_, mut recv) = conn.accept_bi().await.unwrap();
            read_tag(&mut recv).await.unwrap()
        });

        let conn = client_ep
            .connect(server_addr, "etr")
            .unwrap()
            .await
            .unwrap();
        let (mut send, _) = conn.open_bi().await.unwrap();
        send.write_all(&[TAG_FORWARD]).await.unwrap();
        let tag = srv_task.await.unwrap();
        assert_eq!(tag, TAG_FORWARD);
    }

    #[tokio::test]
    async fn test_read_msg_rejects_oversized_message() {
        let (server_ep, client_ep) = make_endpoints();
        let server_addr = server_ep.local_addr().unwrap();

        let srv_task = tokio::spawn(async move {
            let conn = server_ep
                .accept()
                .await
                .unwrap()
                .accept()
                .unwrap()
                .await
                .unwrap();
            let (_, mut recv) = conn.accept_bi().await.unwrap();
            read_msg(&mut recv).await
        });

        let conn = client_ep
            .connect(server_addr, "etr")
            .unwrap()
            .await
            .unwrap();
        let (mut send, _) = conn.open_bi().await.unwrap();
        // Send a length prefix of 4 MB + 1 byte — just over the limit.
        let oversized_len: u32 = 4 * 1024 * 1024 + 1;
        send.write_all(&oversized_len.to_be_bytes()).await.unwrap();

        let result = srv_task.await.unwrap();
        assert!(result.is_err(), "expected error for oversized message");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_read_pty_chunk_rejects_oversized_chunk() {
        let (server_ep, client_ep) = make_endpoints();
        let server_addr = server_ep.local_addr().unwrap();

        let srv_task = tokio::spawn(async move {
            let conn = server_ep
                .accept()
                .await
                .unwrap()
                .accept()
                .unwrap()
                .await
                .unwrap();
            let (_, mut recv) = conn.accept_bi().await.unwrap();
            read_pty_chunk(&mut recv).await
        });

        let conn = client_ep
            .connect(server_addr, "etr")
            .unwrap()
            .await
            .unwrap();
        let (mut send, _) = conn.open_bi().await.unwrap();
        // Header: seq=1, len=1 MB + 1 — just over the PTY chunk limit.
        let mut hdr = [0u8; 12];
        hdr[..8].copy_from_slice(&1u64.to_be_bytes());
        let oversized_len: u32 = 1024 * 1024 + 1;
        hdr[8..12].copy_from_slice(&oversized_len.to_be_bytes());
        send.write_all(&hdr).await.unwrap();

        let result = srv_task.await.unwrap();
        assert!(result.is_err(), "expected error for oversized PTY chunk");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}

#[cfg(test)]
mod transport_bounds_tests {
    use super::*;

    /// **The regression test for the v0.9.3 connection-teardown bug.**
    ///
    /// A forwarding relay that is blocked writing to TCP stops draining its QUIC stream, and
    /// quinn then buffers up to `STREAM_RECEIVE_WINDOW` as one chunk per received frame —
    /// chunks it will not merge, because a full-size frame is marked `defragmented` on arrival.
    /// Past `QUINN_ASSEMBLER_CHUNK_CAP` chunks quinn aborts the whole **connection**, taking
    /// every other forward and the interactive shell with it.
    ///
    /// The window was 4 MB, i.e. ~3500 chunks at a 1200-byte frame — 3.4× over the cap. This
    /// asserts the relationship rather than the number, so raising the window fails here with
    /// the reason instead of failing in production under load.
    #[test]
    fn stream_window_cannot_exceed_assembler_chunk_cap() {
        let worst_case_chunks = STREAM_RECEIVE_WINDOW / MIN_EXPECTED_FRAME_PAYLOAD;
        assert!(
            worst_case_chunks < QUINN_ASSEMBLER_CHUNK_CAP,
            "stream_receive_window of {} bytes allows up to {} buffered chunks at a {}-byte \
             frame, but quinn aborts the CONNECTION past {}. A stalled relay would tear down \
             every stream on the connection, shell included. Lower the window — it bought no \
             measurable throughput above 512 KB.",
            STREAM_RECEIVE_WINDOW,
            worst_case_chunks,
            MIN_EXPECTED_FRAME_PAYLOAD,
            QUINN_ASSEMBLER_CHUNK_CAP,
        );
    }

    /// Keep a real safety margin rather than sitting on the cap. quinn's own 1.25 MB default is
    /// ~1041 chunks at this frame size — already past it — so "matches the default" is not a
    /// justification for raising this.
    #[test]
    fn stream_window_keeps_a_safety_margin() {
        let worst_case_chunks = STREAM_RECEIVE_WINDOW / MIN_EXPECTED_FRAME_PAYLOAD;
        assert!(
            worst_case_chunks * 2 <= QUINN_ASSEMBLER_CHUNK_CAP,
            "only {}x margin under the {}-chunk cap; want at least 2x so that smaller frames \
             (a peer with a lower MTU, or a path that fragments) cannot reach it",
            QUINN_ASSEMBLER_CHUNK_CAP as f32 / worst_case_chunks as f32,
            QUINN_ASSEMBLER_CHUNK_CAP,
        );
    }
}
