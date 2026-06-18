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

criterion_group!(benches, bench_all);
criterion_main!(benches);
