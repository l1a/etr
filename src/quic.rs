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

/// Per-stream receive window. Safe at this size **only** on quinn-proto >= [`QUINN_PROTO_FIXED`];
/// see `high_throughput_transport` and the `quinn` floor in `Cargo.toml`.
pub const STREAM_RECEIVE_WINDOW: u32 = 4 * 1024 * 1024;

/// quinn-proto aborts the **connection** once one stream's reassembler retains more than this
/// many chunks (`assembler.rs`: `MAX_CHUNKS`). Still present in 0.11.18 — what changed is that
/// contiguous chunks are now coalesced, so the count no longer scales with the window.
pub const QUINN_ASSEMBLER_CHUNK_CAP: u32 = 1024;

/// Smallest STREAM-frame payload worth planning for: an internet-typical 1200-byte QUIC
/// datagram. Loopback frames are larger (~2 KB), which is why a loopback measurement alone will
/// happily bless a window that fails on a real network.
pub const MIN_EXPECTED_FRAME_PAYLOAD: u32 = 1200;

/// First quinn-proto release carrying quinn-rs/quinn#2814, which makes the retained-chunk count
/// self-limiting. Below this, `STREAM_RECEIVE_WINDOW` must stay under
/// `QUINN_ASSEMBLER_CHUNK_CAP * MIN_EXPECTED_FRAME_PAYLOAD`.
pub const QUINN_PROTO_FIXED: (u32, u32, u32) = (0, 11, 18);

/// QUIC transport tuning.
///
/// # `stream_receive_window` is coupled to the quinn-proto version — do not decouple them
///
/// This was 4 MB from the v0.4.x throughput work, dropped to 512 KB in v0.9.3, and restored to
/// 4 MB in v0.9.4 once the upstream defect was fixed. The history matters, because the window
/// on its own is not the safety property.
///
/// ## What went wrong
///
/// **quinn-rs/quinn#2809**, confirmed a regression by the quinn maintainers: `defragment()`
/// kept high-utilisation *contiguous* buffers as separate entries, and the guard counted
/// retained buffers rather than genuine gaps. A stream with **no actual gaps at all** could
/// therefore trip `TooManyChunks`, which becomes `TransportError::INTERNAL_ERROR` — and RFC 9000
/// scopes that to the **connection**. In etr that meant a saturated `-L`/`-R` forward tore down
/// every other forward *and* the user's interactive shell.
///
/// The trigger was ordinary TCP back-pressure, not loss: a relay blocked in `write_all` stops
/// draining its QUIC stream, quinn buffers up to the window as one chunk per frame, and 4 MB is
/// ~3500 chunks at a 1200-byte frame against a 1024 cap. Measured across every failing run, the
/// kernel's UDP `RcvbufErrors`/`SndbufErrors` were **0** — there was never any packet loss.
///
/// ## Why 4 MB is safe again
///
/// **quinn-proto 0.11.18** carries the fix (#2814). `defragment()` now computes
/// `min_chunk_size = max(buffered / MAX_CHUNKS, MIN_RETAINED_CHUNK_SIZE)` and only keeps a
/// chunk separate when it is at least that large; everything smaller is coalesced into its
/// contiguous run. **The retained count is therefore self-limiting by arithmetic** — as buffered
/// data grows, the minimum retained chunk size grows with it — so it no longer scales with the
/// window. That is a structural fix upstream, not a bigger limit.
///
/// Verified here before restoring the window: 4 MB survives 3/3 twelve-second saturating runs on
/// 0.11.18 (~3.0 Gb/s), and the full five-stream soak — two saturating TCP forwards plus two
/// unpaced UDP floods offering ~1.9 Gb/s each, ~6.2 Gb/s total — completes 32 s with every flow
/// intact and the shell responsive. The same configuration on 0.11.15 died in under 0.1 s, 5/5.
///
/// ## The coupling, and where it is enforced
///
/// A 4 MB window is safe **only** on a quinn-proto that coalesces, so `Cargo.toml` requires
/// `quinn = "0.11.12"` — the first release depending on quinn-proto >= 0.11.18. cargo then
/// cannot resolve a vulnerable pair at all, which is a stronger guarantee than a test: verified
/// by `cargo update -p quinn --precise 0.11.11` being refused.
///
/// Two test-shaped guards were tried first and rejected as unfit — one could never fail, the
/// other passed on the broken version too. The reasoning is recorded in `transport_bounds_tests`
/// because "we tried a test and it did not discriminate" is worth more than a silent absence.
///
/// ## Two things that looked like fixes and were not
///
/// Both implemented, measured and reverted during the v0.9.3 investigation: enlarging the UDP
/// socket buffers, and shrinking `send_window`. Each only reduces how much data is in flight, so
/// they moved the threshold (one forward went from 2.5 s to 30.7 s of survival) without bounding
/// the chunk count. Kept here because the reasoning was wrong in an instructive way: a fix that
/// relocates a limit is not a fix.
///
/// # The other values
///
/// Connection window 32 MB and send window 32 MB: they bound bytes, not chunks, and neither
/// participated in this failure.
///
/// Idle timeout 30 s, with application heartbeats every 5 s; keep-alive 10 s so NAT mappings
/// stay open when no data is in flight.
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

    /// The window is only this large because the dependency floor guarantees a quinn-proto that
    /// coalesces contiguous chunks. This asserts the two stay coupled in the *documentation*
    /// sense; the enforcement lives in `Cargo.toml`, not here.
    ///
    /// **Why the enforcement is not a test, which is the interesting part.** Two attempts were
    /// made and both were unfit:
    ///
    /// 1. *Read the pinned quinn-proto version from `Cargo.lock`.* It could never fail —
    ///    `cargo test` re-resolves and rewrites the lockfile before the test runs, so a
    ///    downgrade was silently undone and the guard always saw a fixed version.
    /// 2. *Reproduce the teardown behaviourally* — stall the reader, fill the window, assert the
    ///    connection survives. It **passed on the vulnerable quinn-proto 0.11.15 as well**, so it
    ///    discriminated nothing. The real failure needs the receive path to batch datagrams (GRO)
    ///    so that frames are small slices of large allocations, which is what drives quinn's
    ///    `over_allocation` past its threshold. An in-process loopback test at modest rate never
    ///    gets there.
    ///
    /// So the floor is expressed as `quinn = "0.11.12"` in `Cargo.toml`, where cargo enforces it
    /// at resolution time and cannot select a vulnerable pair at all. Verified: `cargo update -p
    /// quinn --precise 0.11.11` is refused. The behavioural coverage that *does* discriminate is
    /// `just stress-local`, which died 5/5 on the old pair and passes on this one.
    #[test]
    fn window_and_chunk_cap_relationship_is_documented() {
        let worst_case_chunks = STREAM_RECEIVE_WINDOW / MIN_EXPECTED_FRAME_PAYLOAD;
        assert!(
            worst_case_chunks > QUINN_ASSEMBLER_CHUNK_CAP,
            "STREAM_RECEIVE_WINDOW now allows only {worst_case_chunks} chunks, which is safe on \
             ANY quinn-proto. That is fine, but high_throughput_transport's comment and the \
             Cargo.toml floor both claim the size depends on quinn-proto >= {}.{}.{} — update \
             them rather than leaving a rationale that no longer applies.",
            QUINN_PROTO_FIXED.0,
            QUINN_PROTO_FIXED.1,
            QUINN_PROTO_FIXED.2,
        );
    }
}
