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

/// QUIC flow-control windows tuned for high-throughput forwarding streams.
///
/// Stream receive window: 4 MB per stream — allows the sender to keep the
/// pipeline full even with RTT latency, and lets a single read_chunk() drain
/// up to 4 MB before back-pressure picks in.
/// Connection window: 32 MB — accommodates many concurrent forwarded streams
/// without the connection-level window becoming the bottleneck.
/// Send window: 32 MB — symmetric with the receive window.
///
/// Idle timeout: 30 s.  Application heartbeats every 5 s keep the timer alive
/// during normal operation; if the peer disappears (crash, reboot, network
/// partition) the connection is declared dead within 30 s so the client can
/// reconnect or print a meaningful error.
///
/// Keep-alive interval: 10 s.  Sends QUIC PING frames so that NAT mappings
/// and firewalls stay open even when no application data is in flight.
fn high_throughput_transport() -> Arc<quinn::TransportConfig> {
    use std::time::Duration;
    let mut t = quinn::TransportConfig::default();
    t.stream_receive_window(
        quinn::VarInt::from_u32(4 * 1024 * 1024), // 4 MB per stream
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
pub async fn write_msg(send: &mut SendStream, env: &Envelope) -> io::Result<()> {
    let bytes = env.encode_to_vec();
    let len = (bytes.len() as u32).to_be_bytes();
    send.write_all(&len)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    send.write_all(&bytes)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    Ok(())
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
}
