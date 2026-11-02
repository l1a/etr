// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress-test helpers for etr: TCP/UDP echo servers and bidirectional pumps.
//!
//! Usage:
//!   stress_tool tcp-echo   <port>
//!   stress_tool udp-echo   <port>
//!   stress_tool tcp-pump   <port>
//!   stress_tool udp-pump   <port> [pace_us]        (omitted or 0 = send flat out)
//!   stress_tool tcp-sink   <port>                  ONE-WAY receiver
//!   stress_tool tcp-source <host> <port> <secs>    ONE-WAY sender
//!   stress_tool udp-sink   <port>                  ONE-WAY receiver
//!   stress_tool udp-source <host> <port> <secs> [pace_us]
//!
//! ECHO (pump) vs ONE-WAY (source/sink), and why both exist
//! --------------------------------------------------------
//! The pumps are an *echo* workload: every byte crosses the link twice and the reported
//! rate is the offered rate, not goodput. That is the right shape for a soak -- it exercises
//! both directions of a forward at once -- but it is NOT comparable to what `iperf3`,
//! `nuttcp` or any other throughput tool reports, because those measure one direction.
//!
//! Comparing them anyway is how a real measurement went wrong here: etr's echo pump was set
//! against nuttcp's one-way figure and the ratio read as "etr achieves 29% of the path",
//! when a large part of the gap was simply that one number counted the link twice.
//!
//! The source/sink pair fixes that. The **sink** reports what it actually received, which is
//! goodput and directly comparable to `iperf3 -c` / `nuttcp`. The **source** reports what it
//! offered; the difference is loss (UDP) or in-flight data (TCP).
//!
//! Stats lines, one per process on SIGTERM (or on completion for a source):
//!   TCP    sent=<bytes> recv=<bytes> elapsed=<seconds>      (pump, echo)
//!   UDP    sent=<bytes> recv=<bytes> elapsed=<seconds>      (pump, echo)
//!   SOURCE sent=<bytes> elapsed=<seconds>                   (one-way sender)
//!   SINK   recv=<bytes> elapsed=<seconds>                   (one-way receiver)
//!
//! The pump lines are unchanged so the stress-local awk parser needs no edits.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Set to true by the SIGTERM handler; pump loops check this to stop cleanly.
static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
unsafe extern "C" fn on_sigterm(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Install the SIGTERM handler so pumps and sinks print their stats line before exiting.
fn install_sigterm_handler() {
    #[cfg(unix)]
    // SAFETY: `on_sigterm` is an `extern "C"` function that only performs an
    // async-signal-safe atomic store (`STOP.store(true, Ordering::Relaxed)`).
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: stress_tool <cmd> <port>");
        eprintln!("       stress_tool tcp-source|udp-source <host> <port> <secs> [pace_us]");
        eprintln!("Commands: tcp-echo  udp-echo  tcp-pump  udp-pump");
        eprintln!("          tcp-sink  tcp-source  udp-sink  udp-source   (one-way)");
        std::process::exit(1);
    }

    // The one-way senders take <host> <port> <secs>, so their port is argv[3], not argv[2].
    // Parsing argv[2] as a port unconditionally would panic on the host argument with a
    // message about an "invalid port" that names a hostname -- confusing in exactly the place
    // someone is already fighting a network problem.
    let is_source = matches!(args[1].as_str(), "tcp-source" | "udp-source");
    if is_source && args.len() < 5 {
        eprintln!(
            "Usage: stress_tool {} <host> <port> <secs> [pace_us]",
            args[1]
        );
        std::process::exit(1);
    }
    let port: u16 = if is_source {
        args[3].parse().expect("invalid port")
    } else {
        args[2].parse().expect("invalid port")
    };

    match args[1].as_str() {
        "tcp-echo" => tcp_echo(port),
        "udp-echo" => udp_echo(port),
        "tcp-pump" => {
            // Install SIGTERM handler so the pump can print stats before exiting.
            // Echo servers use the default SIGTERM handler (immediate termination).
            install_sigterm_handler();
            tcp_pump(port)
        }
        "udp-pump" => {
            install_sigterm_handler();
            // Optional pacing, in microseconds between sends. Absent or 0 = flat out.
            // See the comment in udp_pump for why the default is unthrottled.
            let pace_us: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            udp_pump(port, pace_us)
        }
        "tcp-sink" => {
            install_sigterm_handler();
            tcp_sink(port)
        }
        "tcp-source" => {
            let secs: u64 = args[4].parse().expect("invalid seconds");
            tcp_source(&args[2], port, secs)
        }
        "udp-sink" => {
            install_sigterm_handler();
            udp_sink(port)
        }
        "udp-source" => {
            let secs: u64 = args[4].parse().expect("invalid seconds");
            let pace_us: u64 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);
            udp_source(&args[2], port, secs, pace_us)
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
            if let Ok((n, addr)) = sock6.recv_from(&mut buf) {
                let _ = sock6.send_to(&buf[..n], addr);
            }
        }
    });

    let mut buf = vec![0u8; 65535];
    loop {
        if let Ok((n, addr)) = sock4.recv_from(&mut buf) {
            let _ = sock4.send_to(&buf[..n], addr);
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
    //
    // Pacing uses a deadline-advancing hybrid busy-wait pacer (`Pacer`) rather than a bare
    // `thread::sleep` after each send: Linux/macOS timer slack (~50-100 us) plus syscall
    // time otherwise clamps a requested 50 us step to ~111 us (~100 Mb/s ceiling).
    let mut pacer = Pacer::new(pace_us);
    while !STOP.load(Ordering::Relaxed) {
        match socket.send(&chunk) {
            Ok(n) => {
                bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
                pacer.wait();
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
            Err(ref e) if is_transient_udp_send_error(e) => {
                thread::yield_now();
            }
            Err(_) => break,
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

/// Return `true` when a UDP `send()` error represents transient back-pressure or a
/// not-yet-listening peer rather than a fatal socket failure.
fn is_transient_udp_send_error(e: &std::io::Error) -> bool {
    if matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::OutOfMemory
            | std::io::ErrorKind::ConnectionRefused
    ) {
        return true;
    }
    // ENOBUFS has no stable ErrorKind across platforms, so match the raw errno on Unix too.
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ENOBUFS) {
        return true;
    }
    false
}

/// Threshold below which [`Pacer`] busy-waits with [`std::hint::spin_loop`] instead of
/// calling [`thread::sleep`].
///
/// Kernel timer slack on Linux (`nanosleep`) and macOS is roughly 50–100 µs, so a
/// `thread::sleep(Duration::from_micros(50))` actually wakes after ~110 µs — capping
/// 1400-byte datagrams at ~100 Mb/s regardless of how small `pace_us` is. Sleeping only
/// the coarse prefix beyond `SPIN_HORIZON` and spinning for the final window keeps coarse
/// paces (≥ 500 µs) CPU-friendly while making sub-100 µs steps (`50`, `25`, `15`, `10` µs)
/// hit their target deadlines accurately.
const SPIN_HORIZON: Duration = Duration::from_micros(250);

/// Maximum number of steps the pacer is allowed to fall behind before clamping its
/// deadline to `now`.
///
/// Without this clamp, a transient socket stall (`WouldBlock` / `ENOBUFS`) or scheduler
/// preemption of a few milliseconds accumulates dozens of past-due steps, and the loop
/// then fires an unpaced catch-up burst at line rate the instant the socket clears.
const MAX_LAG_STEPS: u32 = 8;

/// Compute `(new_deadline, coarse_sleep)` for one paced step from `current_deadline`.
///
/// Pure helper so unit tests can verify normal advancement, coarse sleep prefix splitting,
/// spin-only windows, and lag clamping without depending on OS scheduler timing.
fn advance_deadline(
    current_deadline: Instant,
    step: Duration,
    now: Instant,
) -> (Instant, Option<Duration>) {
    let target = current_deadline + step;
    if now >= target {
        let max_lag = step.saturating_mul(MAX_LAG_STEPS);
        let clamped = if now.duration_since(target) > max_lag {
            now
        } else {
            target
        };
        return (clamped, None);
    }
    let remaining = target.duration_since(now);
    let coarse_sleep = if remaining > SPIN_HORIZON {
        Some(remaining - SPIN_HORIZON)
    } else {
        None
    };
    (target, coarse_sleep)
}

/// Deadline-advancing UDP pacer that combines coarse `thread::sleep` with fine
/// `std::hint::spin_loop` busy-waiting inside [`SPIN_HORIZON`].
struct Pacer {
    step: Option<Duration>,
    next_deadline: Instant,
}

impl Pacer {
    /// Create a pacer for `pace_us` microseconds between sends (`0` = unpaced flat-out).
    fn new(pace_us: u64) -> Self {
        Self {
            step: (pace_us > 0).then(|| Duration::from_micros(pace_us)),
            next_deadline: Instant::now(),
        }
    }

    /// Wait until the next send deadline. No-op when `pace_us == 0`.
    fn wait(&mut self) {
        let Some(step) = self.step else {
            return;
        };
        let now = Instant::now();
        let (target, coarse_sleep) = advance_deadline(self.next_deadline, step, now);
        self.next_deadline = target;
        if let Some(sleep_dur) = coarse_sleep {
            thread::sleep(sleep_dur);
        }
        while Instant::now() < self.next_deadline {
            std::hint::spin_loop();
        }
    }
}

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

// ── One-way source / sink ─────────────────────────────────────────────────────
//
// These exist so etr can be compared against `iperf3`/`nuttcp` on equal terms. The pumps
// above measure an echo, which counts every byte twice and is not what any standard
// throughput tool reports. The sink reports goodput: bytes that actually arrived.

/// Accept TCP connections and count every byte received, until SIGTERM or until a connection
/// that actually carried data closes.
///
/// Prints `SINK recv=<bytes> elapsed=<seconds>` on SIGTERM or when the peer closes. The
/// clock starts at the **first byte**, not at bind: otherwise the seconds spent waiting for
/// a connection are averaged into the rate and every measurement reads low by however long
/// the harness took to start the sender.
fn tcp_sink(port: u16) {
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).expect("tcp_sink: bind");
    listener
        .set_nonblocking(true)
        .expect("tcp_sink: set_nonblocking");

    let mut buf = vec![0u8; 256 * 1024];
    let mut recv: u64 = 0;
    // FIRST byte to LAST byte, not first byte to SIGTERM. The harness cannot stop a sink at
    // the exact instant the sender finishes, so any idle tail would be averaged into the rate
    // and report a throughput lower than what happened.
    let mut start: Option<Instant> = None;
    let mut last = Instant::now();

    // KEEP ACCEPTING. A sink that exits after its first closed connection is destroyed by any
    // probe: a readiness check that merely opens and closes the port consumes the one
    // connection, the sink reports `recv=0`, and the measurement that follows has nothing
    // listening. That happened here with an `ncat -z` probe through an etr forward. Only a
    // connection that actually delivered bytes ends the run.
    'accept: while !STOP.load(Ordering::Relaxed) {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(_) => break,
        };
        stream.set_nodelay(true).ok();
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();

        let before = recv;
        loop {
            if STOP.load(Ordering::Relaxed) {
                break 'accept;
            }
            match (&stream).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    start.get_or_insert_with(Instant::now);
                    last = Instant::now();
                    recv += n as u64;
                }
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        // A connection that carried data has finished the transfer; one that carried none was
        // a probe, so go back to accepting.
        if recv > before {
            break;
        }
    }

    let elapsed = start
        .map(|s| last.duration_since(s).as_secs_f64().max(0.001))
        .unwrap_or(0.001);
    println!("SINK recv={recv} elapsed={elapsed:.3}");
}

/// Connect to `host:port` and send flat out for `secs`, then print what was offered.
///
/// Takes a host because the pumps hardcode `127.0.0.1`, which makes them useless for
/// measuring a real path: the raw-link baseline you need to compare etr against cannot be
/// taken without pointing a sender at the far end directly.
fn tcp_source(host: &str, port: u16, secs: u64) {
    let target = format!("{host}:{port}");
    let stream = match TcpStream::connect(&target) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tcp-source: cannot connect to {target}: {e}");
            println!("SOURCE sent=0 elapsed=0.001");
            return;
        }
    };
    stream.set_nodelay(true).ok();

    let chunk = vec![0u8; 256 * 1024];
    let start = Instant::now();
    let deadline = Duration::from_secs(secs);
    let mut sent: u64 = 0;
    while start.elapsed() < deadline && !STOP.load(Ordering::Relaxed) {
        match (&stream).write_all(&chunk) {
            Ok(()) => sent += chunk.len() as u64,
            Err(e) => {
                eprintln!(
                    "tcp-source: send stopped after {:.3}s: {e} (kind={:?})",
                    start.elapsed().as_secs_f64(),
                    e.kind()
                );
                break;
            }
        }
    }
    // Flush what the kernel still holds before reporting, so `sent` is not inflated by data
    // sitting in the socket buffer when the clock stops.
    let _ = (&stream).flush();
    let elapsed = start.elapsed().as_secs_f64();
    println!("SOURCE sent={sent} elapsed={elapsed:.3}");
}

/// Count UDP datagrams arriving on `port`. Pairs with `udp-source`; the difference between
/// its `sent` and this `recv` is loss, which for UDP is the number that matters.
fn udp_sink(port: u16) {
    let sock = UdpSocket::bind(format!("0.0.0.0:{port}")).expect("udp_sink: bind");
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let mut buf = vec![0u8; 65535];
    let mut recv: u64 = 0;
    // See tcp_sink: first byte to last byte, so the idle tail before SIGTERM is not counted.
    let mut start: Option<Instant> = None;
    let mut last = Instant::now();
    while !STOP.load(Ordering::Relaxed) {
        match sock.recv(&mut buf) {
            Ok(n) => {
                start.get_or_insert_with(Instant::now);
                last = Instant::now();
                recv += n as u64;
            }
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    let elapsed = start
        .map(|s| last.duration_since(s).as_secs_f64().max(0.001))
        .unwrap_or(0.001);
    println!("SINK recv={recv} elapsed={elapsed:.3}");
}

/// Send 1400-byte UDP datagrams to `host:port` for `secs`.
///
/// `pace_us` > 0 inserts that delay between sends. Unpaced is the default for the same
/// reason as `udp-pump`: an always-on sleep measures the timer rather than the path.
fn udp_source(host: &str, port: u16, secs: u64, pace_us: u64) {
    let sock = UdpSocket::bind("0.0.0.0:0").expect("udp_source: bind");
    let target = format!("{host}:{port}");
    if let Err(e) = sock.connect(&target) {
        eprintln!("udp-source: cannot connect to {target}: {e}");
        println!("SOURCE sent=0 elapsed=0.001");
        return;
    }
    let chunk = vec![0u8; 1400];
    let start = Instant::now();
    let deadline = Duration::from_secs(secs);
    let mut sent: u64 = 0;
    let mut pacer = Pacer::new(pace_us);
    while start.elapsed() < deadline && !STOP.load(Ordering::Relaxed) {
        match sock.send(&chunk) {
            Ok(n) => {
                sent += n as u64;
                pacer.wait();
            }
            // Back-pressure and a not-yet-listening peer are both transient for UDP; see the
            // same handling in udp_pump for why treating them as fatal ruins the measurement.
            Err(ref e) if is_transient_udp_send_error(e) => {
                thread::yield_now();
            }
            Err(_) => break,
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!("SOURCE sent={sent} elapsed={elapsed:.3}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_zero_is_unpaced() {
        let mut pacer = Pacer::new(0);
        assert!(pacer.step.is_none());
        let start = Instant::now();
        for _ in 0..1000 {
            pacer.wait();
        }
        assert!(
            start.elapsed() < Duration::from_millis(5),
            "unpaced wait() should be a no-op, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn advance_deadline_splits_coarse_sleep_and_spin_window() {
        let t0 = Instant::now();
        // Step = 1000 us, now = t0 -> remaining = 1000 us > SPIN_HORIZON (250 us),
        // so coarse_sleep should be 750 us.
        let (target, sleep) = advance_deadline(t0, Duration::from_micros(1000), t0);
        assert_eq!(target, t0 + Duration::from_micros(1000));
        assert_eq!(sleep, Some(Duration::from_micros(750)));

        // Step = 50 us <= SPIN_HORIZON (250 us) -> spin-only, no thread::sleep.
        let (target_fine, sleep_fine) = advance_deadline(t0, Duration::from_micros(50), t0);
        assert_eq!(target_fine, t0 + Duration::from_micros(50));
        assert_eq!(sleep_fine, None);
    }

    #[test]
    fn advance_deadline_clamps_excessive_lag_without_bursting() {
        let t0 = Instant::now();
        let step = Duration::from_micros(50);
        // Within MAX_LAG_STEPS (e.g. 3 steps behind = 150 us): preserves cadence.
        let slightly_late = t0 + Duration::from_micros(150);
        let (target, sleep) = advance_deadline(t0, step, slightly_late);
        assert_eq!(target, t0 + step);
        assert_eq!(sleep, None);

        // Beyond MAX_LAG_STEPS (e.g. 10 ms stall on a 50 us step): clamps to `now`
        // so the sender does not fire 200 unpaced datagrams in a catch-up burst.
        let stalled = t0 + Duration::from_millis(10);
        let (clamped, sleep_stalled) = advance_deadline(t0, step, stalled);
        assert_eq!(clamped, stalled);
        assert_eq!(sleep_stalled, None);
    }

    #[test]
    fn pacer_busy_waits_sub_100us_accurately() {
        // 100 steps at 50 us = 5.0 ms target. With the old `thread::sleep(50us)`,
        // timer slack made each step ~111 us (~11.1 ms total). Assert we land near
        // 5 ms (at least 4.8 ms, and well below the old 10+ ms sleep floor).
        let mut pacer = Pacer::new(50);
        let start = Instant::now();
        for _ in 0..100 {
            pacer.wait();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_micros(4800),
            "50 us * 100 steps finished too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_micros(9000),
            "50 us * 100 steps took {elapsed:?} (hit sleep timer slack ceiling)"
        );
    }
}
