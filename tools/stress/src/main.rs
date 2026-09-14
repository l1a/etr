// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress-test helpers for etr: TCP/UDP echo servers and bidirectional pumps.
//!
//! Usage:
//!   stress_tool tcp-echo <port>
//!   stress_tool udp-echo <port>
//!   stress_tool tcp-pump <port>
//!   stress_tool udp-pump <port> [pace_us]   (pace_us omitted or 0 = send flat out)
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

    match args[1].as_str() {
        "tcp-echo" => tcp_echo(port),
        "udp-echo" => udp_echo(port),
        "tcp-pump" => {
            // Install SIGTERM handler so the pump can print stats before exiting.
            // Echo servers use the default SIGTERM handler (immediate termination).
            unsafe {
                libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
            }
            tcp_pump(port)
        }
        "udp-pump" => {
            unsafe {
                libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
            }
            // Optional pacing, in microseconds between sends. Absent or 0 = flat out.
            // See the comment in udp_pump for why the default is unthrottled.
            let pace_us: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            udp_pump(port, pace_us)
        }
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
    let Some(stream) = tcp_connect_with_retry(port) else {
        println!("TCP sent=0 recv=0 elapsed=0.001");
        return;
    };
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
                Ok(0) => {
                    eprintln!("tcp-pump: drain saw EOF (peer closed the connection)");
                    break;
                }
                Err(e) => {
                    eprintln!("tcp-pump: drain stopped: {e} (kind={:?})", e.kind());
                    break;
                }
                Ok(n) => {
                    bytes_recv2.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    });

    while !STOP.load(Ordering::Relaxed) {
        match (&stream).write_all(&chunk) {
            Ok(()) => {
                bytes_sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            // SAY WHY, rather than exiting silently into a plausible-looking stats line.
            //
            // This arm used to be a bare `break`. A pump that dies 40 ms into a 30 s run still
            // prints `TCP sent=... recv=... elapsed=0.039`, which the justfile turns into a
            // Mb/s figure computed over a near-zero interval -- and that figure is what got
            // quoted as this project's TCP throughput. The stats line cannot distinguish "ran
            // for 30 s" from "died immediately", so the reason has to reach stderr.
            Err(e) => {
                eprintln!(
                    "tcp-pump: send stopped after {:.3}s: {e} (kind={:?}, errno={:?})",
                    start.elapsed().as_secs_f64(),
                    e.kind(),
                    e.raw_os_error()
                );
                break;
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "TCP sent={} recv={} elapsed={:.3}",
        bytes_sent.load(Ordering::Relaxed),
        bytes_recv.load(Ordering::Relaxed),
        elapsed,
    );
}

/// Send UDP datagrams to a port and drain replies, as fast as the socket accepts them.
///
/// `pace_us` > 0 inserts that many microseconds between sends, for a deliberate low-rate
/// soak. It defaults to 0 (flat out) because an always-on 1 ms sleep is what made the old
/// "UDP ~9 Mb/s" figure a measurement of the timer rather than of etr.
fn udp_pump(port: u16, pace_us: u64) {
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

    // PACING IS OPT-IN, AND THAT IS THE WHOLE POINT OF THIS LOOP.
    //
    // This loop used to end in an unconditional `thread::sleep(Duration::from_millis(1))`,
    // which capped the pump at <=1000 datagrams/s. At 1400 bytes that is 11.2 Mb/s of offered
    // load before etr is involved at all, and Linux's 1 ms sleep typically lands at 1.1-1.3 ms,
    // so the real ceiling was ~770-900 dgram/s => 8.6-10.1 Mb/s.
    //
    // **The project measured ~9 Mb/s and recorded it in NOTES.md as "limited by per-datagram
    // protobuf encoding overhead".** It was not: it was this sleep. The encode+decode path
    // costs ~345 ns/datagram, a ceiling near 32 Gb/s at this size — about 3,600x above the
    // number being explained. The tcp-pump next door has never had a sleep, so the headline
    // "TCP 320 Mb/s vs UDP 9 Mb/s" compared a throughput measurement against a timer.
    //
    // Unthrottled is now the default so the figure means what its name says. UDP has no flow
    // control, so a flat-out pump will also expose loss — that is information, not a defect,
    // and it is why the stats line reports sent AND recv. Pass a microsecond delay as the
    // third argument when you deliberately want a paced, low-rate soak instead:
    //     stress_tool udp-pump <port> 1000    # ~1000 dgram/s, the old behaviour
    while !STOP.load(Ordering::Relaxed) {
        match socket.send(&chunk) {
            Ok(n) => {
                bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
            }
            // A FULL SOCKET BUFFER IS BACK-PRESSURE, NOT A FAILURE.
            //
            // Sending flat out reaches ENOBUFS/EAGAIN almost immediately on loopback, and
            // treating that as fatal ends the pump within milliseconds. The stats line then
            // reports a few MiB "in 0.0s", which the justfile turns into a Mb/s figure computed
            // over a near-zero interval — a number that looks like a throughput result and is
            // arithmetic noise. Yield and retry instead, so the loop measures the rate the path
            // actually sustains.
            // ConnectionRefused on a *connected UDP socket* is an ICMP port-unreachable from a
            // listener that is not up yet -- which is exactly the state this pump starts in,
            // because the recipe probes the TCP forward port for readiness and has never had an
            // equivalent probe for the UDP one. Treating it as fatal is why three of the four
            // pumps reported "in 0.0s": they exited within milliseconds and the justfile then
            // divided by a near-zero interval. It is transient, so keep going.
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::OutOfMemory
                        | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                thread::yield_now();
            }
            // ENOBUFS has no stable ErrorKind across platforms, so match the raw errno too
            // rather than relying on the classification above catching it.
            Err(ref e) if e.raw_os_error() == Some(libc::ENOBUFS) => {
                thread::yield_now();
            }
            Err(_) => break,
        }
        if pace_us > 0 {
            thread::sleep(Duration::from_micros(pace_us));
        }
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

fn tcp_connect_with_retry(port: u16) -> Option<TcpStream> {
    for _ in 0..50 {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(s) => return Some(s),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    eprintln!("tcp_pump: could not connect to 127.0.0.1:{port} after 5s");
    None
}
