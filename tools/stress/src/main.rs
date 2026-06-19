// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress-test helpers for etr: TCP/UDP echo servers and bidirectional pumps.
//!
//! Usage:
//!   stress_tool tcp-echo <port>
//!   stress_tool udp-echo <port>
//!   stress_tool tcp-pump <port>
//!   stress_tool udp-pump <port>
//!
//! Each pump prints one stats line to stdout on SIGTERM:
//!   TCP sent=<bytes> recv=<bytes> elapsed=<seconds>
//!   UDP sent=<bytes> recv=<bytes> elapsed=<seconds>
//!
//! The output format is identical to the Python scripts they replace so the
//! stress-local justfile recipe needs no changes to the awk parser.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Set to true by the SIGTERM handler; pump loops check this to stop cleanly.
static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn on_sigterm(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: stress_tool <cmd> <port>");
        eprintln!("Commands: tcp-echo  udp-echo  tcp-pump  udp-pump");
        std::process::exit(1);
    }
    let port: u16 = args[2].parse().expect("invalid port");

    // Install SIGTERM handler for pump subcommands.
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }

    match args[1].as_str() {
        "tcp-echo" => tcp_echo(port),
        "udp-echo" => udp_echo(port),
        "tcp-pump" => tcp_pump(port),
        "udp-pump" => udp_pump(port),
        other => {
            eprintln!("Unknown command: {other}");
            std::process::exit(1);
        }
    }
}

// ── Echo servers ──────────────────────────────────────────────────────────────

/// Accept TCP connections and echo every byte back, one thread per connection.
fn tcp_echo(port: u16) {
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).expect("tcp_echo: bind");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                s.set_nodelay(true).ok();
                thread::spawn(move || echo_tcp_conn(s));
            }
            Err(_) => break,
        }
    }
}

fn echo_tcp_conn(stream: TcpStream) {
    // &TcpStream implements both Read and Write, so we can borrow it for both
    // directions without splitting or cloning — the OS fd handles concurrent
    // reads and writes safely.
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match (&stream).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if (&stream).write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
}

/// Receive UDP datagrams and echo each one back to the sender.
///
/// Binds both `0.0.0.0:port` (IPv4) and `[::1]:port` (IPv6 loopback) so that
/// forwarding targets resolved to either family both reach the echo server.
fn udp_echo(port: u16) {
    let sock4 = UdpSocket::bind(format!("0.0.0.0:{port}")).expect("udp_echo: bind v4");
    let sock6 = UdpSocket::bind(format!("[::1]:{port}")).expect("udp_echo: bind v6");

    thread::spawn(move || {
        let mut buf = vec![0u8; 65535];
        loop {
            match sock6.recv_from(&mut buf) {
                Ok((n, addr)) => {
                    let _ = sock6.send_to(&buf[..n], addr);
                }
                Err(_) => {}
            }
        }
    });

    let mut buf = vec![0u8; 65535];
    loop {
        match sock4.recv_from(&mut buf) {
            Ok((n, addr)) => {
                let _ = sock4.send_to(&buf[..n], addr);
            }
            Err(_) => {}
        }
    }
}

// ── Pumps ─────────────────────────────────────────────────────────────────────

/// Connect to a TCP port and push/drain data as fast as possible.
///
/// Sends 64 KiB chunks; a drain thread counts received bytes. Exits on SIGTERM
/// and prints `TCP sent=<n> recv=<n> elapsed=<s>` to stdout.
fn tcp_pump(port: u16) {
    let stream = tcp_connect_with_retry(port);
    stream.set_nodelay(true).ok();

    let bytes_sent = std::sync::Arc::new(AtomicU64::new(0));
    let bytes_recv = std::sync::Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let chunk = vec![0u8; 65536];

    // Drain thread — counts every byte the echo server sends back.
    let recv_stream = stream.try_clone().expect("try_clone");
    let bytes_recv2 = bytes_recv.clone();
    thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            match (&recv_stream).read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    bytes_recv2.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    });

    while !STOP.load(Ordering::Relaxed) {
        match (&stream).write_all(&chunk) {
            Ok(()) => bytes_sent.fetch_add(chunk.len() as u64, Ordering::Relaxed),
            Err(_) => break,
        };
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "TCP sent={} recv={} elapsed={:.3}",
        bytes_sent.load(Ordering::Relaxed),
        bytes_recv.load(Ordering::Relaxed),
        elapsed,
    );
}

/// Send UDP datagrams to a port and drain replies, matching the Python pump
/// rate-limiting (1 ms sleep between sends) to avoid saturating UDP buffers.
fn udp_pump(port: u16) {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("udp_pump: bind");
    socket
        .connect(format!("127.0.0.1:{port}"))
        .expect("udp_pump: connect");

    let bytes_sent = std::sync::Arc::new(AtomicU64::new(0));
    let bytes_recv = std::sync::Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let chunk = vec![0u8; 1400];

    let recv_sock = socket.try_clone().expect("try_clone");
    let bytes_recv2 = bytes_recv.clone();
    thread::spawn(move || {
        recv_sock
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        let mut buf = vec![0u8; 65535];
        loop {
            match recv_sock.recv(&mut buf) {
                Ok(n) => {
                    bytes_recv2.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
    });

    while !STOP.load(Ordering::Relaxed) {
        match socket.send(&chunk) {
            Ok(n) => bytes_sent.fetch_add(n as u64, Ordering::Relaxed),
            Err(_) => break,
        };
        thread::sleep(Duration::from_millis(1));
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "UDP sent={} recv={} elapsed={:.3}",
        bytes_sent.load(Ordering::Relaxed),
        bytes_recv.load(Ordering::Relaxed),
        elapsed,
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn tcp_connect_with_retry(port: u16) -> TcpStream {
    for _ in 0..50 {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(s) => return s,
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    panic!("tcp_pump: could not connect to 127.0.0.1:{port} after 5s");
}
