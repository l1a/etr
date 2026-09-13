// SPDX-License-Identifier: GPL-3.0-or-later
use criterion::{Criterion, criterion_group, criterion_main};
use etr::quic::{
    client_config, generate_self_signed_cert, read_pty_chunk, read_tag, server_config,
    write_pty_chunk,
};
use std::sync::Arc;
use tokio::runtime::Runtime;

fn make_endpoints() -> (quinn::Endpoint, quinn::Endpoint) {
    let (cert, key) = generate_self_signed_cert();
    let srv_cfg = server_config(cert.clone(), key).unwrap();
    let cli_cfg = client_config(cert).unwrap();

    let server_ep = quinn::Endpoint::server(srv_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut client_ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client_ep.set_default_client_config(cli_cfg);
    (server_ep, client_ep)
}

fn bench_all(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let _guard = rt.enter();

    // 1. Benchmark cert generation (sync)
    c.bench_function("cert_generation", |b| {
        b.iter(|| {
            generate_self_signed_cert();
        })
    });

    // 2. Set up endpoints once
    let (server_ep, client_ep) = make_endpoints();
    let server_addr = server_ep.local_addr().unwrap();

    // Spawn the server acceptor task
    rt.spawn(async move {
        while let Some(conn) = server_ep.accept().await {
            tokio::spawn(async move {
                if let Ok(c) = conn.await {
                    // Accept and handle bi-directional streams
                    while let Ok((mut srv_send, mut srv_recv)) = c.accept_bi().await {
                        tokio::spawn(async move {
                            // If client sends Tag 0x02 (PTY), run echo loop
                            if let Ok(etr::quic::TAG_PTY) = read_tag(&mut srv_recv).await {
                                while let Ok(Some((seq, data))) =
                                    read_pty_chunk(&mut srv_recv).await
                                {
                                    if write_pty_chunk(&mut srv_send, seq, &data).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        });
                    }
                }
            });
        }
    });

    // 3. Benchmark connection handshake
    c.bench_function("quic_connection_handshake", |b| {
        b.to_async(&rt).iter(|| {
            let client_ep = client_ep.clone();
            async move {
                let conn = client_ep
                    .connect(server_addr, "etr")
                    .unwrap()
                    .await
                    .unwrap();
                conn.close(0u32.into(), b"done");
            }
        })
    });

    // 4. Benchmark round trip (established connection)
    let (client_send, client_recv) = rt.block_on(async {
        let conn = client_ep
            .connect(server_addr, "etr")
            .unwrap()
            .await
            .unwrap();
        let (mut send, recv) = conn.open_bi().await.unwrap();
        // Write the PTY tag
        send.write_all(&[etr::quic::TAG_PTY]).await.unwrap();
        (send, recv)
    });

    let client_send = Arc::new(tokio::sync::Mutex::new(client_send));
    let client_recv = Arc::new(tokio::sync::Mutex::new(client_recv));

    c.bench_function("pty_chunk_round_trip_100b", |b| {
        b.to_async(&rt).iter(|| {
            let client_send = client_send.clone();
            let client_recv = client_recv.clone();
            async move {
                let mut send = client_send.lock().await;
                let mut recv = client_recv.lock().await;
                write_pty_chunk(&mut send, 1, &[0x42; 100]).await.unwrap();
                let _ = read_pty_chunk(&mut recv).await.unwrap().unwrap();
            }
        })
    });

    // 5. Benchmark throughput
    let payload = vec![0x42; 65536];
    c.bench_function("pty_throughput_64kb", |b| {
        b.to_async(&rt).iter(|| {
            let client_send = client_send.clone();
            let client_recv = client_recv.clone();
            let payload_ref = &payload;
            async move {
                let mut send = client_send.lock().await;
                let mut recv = client_recv.lock().await;
                write_pty_chunk(&mut send, 1, payload_ref).await.unwrap();
                let (_, data) = read_pty_chunk(&mut recv).await.unwrap().unwrap();
                assert_eq!(data.len(), 65536);
            }
        })
    });
}

/// Per-datagram cost of the UDP forwarding path, measured without a network.
///
/// WHY A CPU-ONLY BENCH, when `just stress-local` exists: the stress harness measures a whole
/// system (two pumps, an echo server, a QUIC link and the kernel's UDP buffers) and is
/// therefore very easy to misread. It was misread — its UDP pump slept 1 ms between sends, so
/// the "~9 Mb/s" it reported was the sleep, and NOTES.md attributed that number to
/// "per-datagram protobuf encoding overhead". This bench isolates exactly the code that claim
/// was about, so the claim can be checked rather than repeated.
///
/// 1400 bytes is the payload size the stress pump uses and a typical sub-MTU datagram.
fn bench_udp_forward(c: &mut Criterion) {
    use etr::protocol::{Envelope, Payload, UdpDatagram};
    use prost::Message;
    use std::net::SocketAddr;

    let payload = vec![0xABu8; 1400];
    let src: SocketAddr = "192.168.1.50:54321".parse().unwrap();

    // The send half, BOTH WAYS, so the improvement is reproducible rather than asserted.
    //
    // `_inline` is the shape the forwarding loops used before v0.9.2: a fresh `Envelope` per
    // datagram, `to_string` for the peer, `to_vec` for the payload, `encode_to_vec` for the
    // body. `_reused` is what ships now. They are required to emit identical bytes — a unit
    // test in `forward.rs` pins that, because a faster encoder that changed the wire format
    // would be a silent protocol break rather than an optimisation.
    c.bench_function("udp_forward_encode_inline_1400b", |b| {
        b.iter(|| {
            let env = Envelope {
                payload: Some(Payload::UdpDatagram(UdpDatagram {
                    peer_addr: src.ip().to_string(),
                    peer_port: src.port() as u32,
                    data: payload[..].to_vec(),
                })),
            };
            let body = env.encode_to_vec();
            let mut framed = Vec::with_capacity(4 + body.len());
            framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
            framed.extend_from_slice(&body);
            std::hint::black_box(framed.len())
        })
    });

    c.bench_function("udp_forward_encode_reused_1400b", |b| {
        let mut enc = etr::forward::UdpFrameEncoder::new();
        b.iter(|| std::hint::black_box(enc.frame(src, &payload).len()))
    });

    // The receive half: decode, then work out where the datagram has to be sent. Resolving the
    // destination is part of the per-datagram cost and is easy to leave out of a benchmark by
    // accident — which would flatter exactly the code being changed.
    let wire = Envelope {
        payload: Some(Payload::UdpDatagram(UdpDatagram {
            peer_addr: src.ip().to_string(),
            peer_port: src.port() as u32,
            data: payload.clone(),
        })),
    }
    .encode_to_vec();

    c.bench_function("udp_forward_decode_and_resolve_1400b", |b| {
        b.iter(|| {
            let env = Envelope::decode(wire.as_slice()).unwrap();
            let mut out = 0usize;
            if let Some(Payload::UdpDatagram(dg)) = env.payload {
                let dest: SocketAddr =
                    etr::forward::datagram_peer_addr(&dg.peer_addr, dg.peer_port)
                        .expect("benchmark address must parse");
                out = dg.data.len() + dest.port() as usize;
            }
            std::hint::black_box(out)
        })
    });
}

criterion_group!(benches, bench_all, bench_udp_forward);
criterion_main!(benches);
