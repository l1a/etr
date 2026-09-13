// SPDX-License-Identifier: GPL-3.0-or-later
//! Local port-forwarding spec parsing.
//!
//! Accepts the same syntax as `ssh -L`:
//! ```text
//! [bind_address:]local_port:remote_host:remote_port[/tcp|/udp]
//! ```
//!
//! Examples:
//! ```text
//! 8080:localhost:80          TCP (default)
//! 8080:localhost:80/tcp      explicit TCP
//! 5353:192.168.1.1:53/udp   UDP
//! 5432:[::1]:5432            IPv6 remote host, TCP
//! 127.0.0.1:8080:localhost:80 Explicit bind address
//! *:8080:localhost:80        Wildcard bind address
//! ```
use crate::addrfam::AddrPref;
use crate::protocol::ForwardProto;
use std::net::{IpAddr, SocketAddr};

/// A parsed `-L` or `-R` forwarding specification.
#[derive(Debug, Clone)]
pub struct ForwardSpec {
    /// The local IP/host to bind to (e.g. "127.0.0.1", "[::1]", "*", "0.0.0.0"), if specified.
    pub bind_address: Option<String>,
    /// The port to listen on.
    pub local_port: u16,
    /// The destination host to forward connections to.
    pub remote_host: String,
    /// The destination port to forward connections to.
    pub remote_port: u16,
    /// The forward protocol (TCP or UDP).
    pub proto: ForwardProto,
}

/// Split a spec string by colons, ignoring any colons that appear inside square brackets
/// (which typically enclose IPv6 addresses).
fn split_ignoring_brackets(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_brackets = false;
    for c in s.chars() {
        if c == '[' {
            in_brackets = true;
            current.push(c);
        } else if c == ']' {
            in_brackets = false;
            current.push(c);
        } else if c == ':' && !in_brackets {
            parts.push(current);
            current = String::new();
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    parts
}

impl ForwardSpec {
    /// Parse a forwarding spec string.
    ///
    /// Format: `[bind_address:]local_port:remote_host:remote_port[/tcp|/udp]`
    ///
    /// The remote host and bind address may be IPv6 addresses in brackets (`[::1]`).
    pub fn parse(s: &str) -> Result<Self, String> {
        // Strip optional /tcp or /udp suffix.
        let (rest, proto) = if let Some(base) = s.strip_suffix("/udp") {
            (base, ForwardProto::Udp)
        } else {
            (s.strip_suffix("/tcp").unwrap_or(s), ForwardProto::Tcp)
        };

        let parts = split_ignoring_brackets(rest);
        let (bind_address, local_port_str, remote_host, remote_port_str) = match parts.len() {
            3 => (None, &parts[0], &parts[1], &parts[2]),
            4 => (Some(parts[0].clone()), &parts[1], &parts[2], &parts[3]),
            _ => {
                return Err(format!(
                    "invalid spec '{s}': expected [bind_address:]local_port:remote_host:remote_port"
                ));
            }
        };

        let local_port = local_port_str
            .parse::<u16>()
            .map_err(|_| format!("invalid local port '{local_port_str}' in spec '{s}'"))?;
        let remote_port = remote_port_str
            .parse::<u16>()
            .map_err(|_| format!("invalid remote port '{remote_port_str}' in spec '{s}'"))?;

        if remote_host.is_empty() {
            return Err(format!("empty remote host in spec '{s}'"));
        }

        Ok(Self {
            bind_address,
            local_port,
            remote_host: remote_host.clone(),
            remote_port,
            proto,
        })
    }

    /// Resolve the concrete bind addresses based on the parsed bind_address and gateway flag.
    ///
    /// Returns the list of socket address strings to bind.  Each entry is passed verbatim to
    /// `TcpListener::bind` / `UdpSocket::bind` as `"<addr>:<port>"`, so IPv6 addresses must
    /// already be in bracket form (e.g. `"[::1]"`).
    ///
    /// # Strategy
    ///
    /// * **Wildcard / gateway mode** — returns a **single** `"[::]"` entry.  On dual-stack Linux
    ///   (the common case, `net.ipv6.bindv6only = 0`), a `[::]` socket already accepts both
    ///   IPv4-mapped connections (shown as `::ffff:a.b.c.d`) and native IPv6 connections, so no
    ///   separate `0.0.0.0` socket is needed.  Binding `0.0.0.0` *first* and then `[::]` on the
    ///   same port causes the second bind to fail with `EADDRINUSE` on dual-stack kernels.
    ///
    /// * **Loopback (default)** — returns two entries: `"127.0.0.1"` and `"[::1]"`.  These are
    ///   genuinely distinct addresses, so two sockets are required.
    ///
    /// * **Explicit bind address** — returned as-is in a one-element vec.
    pub fn get_bind_addresses(&self, gateway: bool) -> Vec<String> {
        if let Some(ref addr) = self.bind_address {
            if addr == "*" || addr == "0.0.0.0" || addr == "::" || addr.is_empty() {
                // Wildcard explicit bind: single dual-stack [::] socket.
                vec!["[::]".to_string()]
            } else {
                vec![addr.clone()]
            }
        } else if gateway {
            // -g / --gateway-ports: single dual-stack [::] socket covers both IPv4 and IPv6.
            vec!["[::]".to_string()]
        } else {
            // Default: loopback only on both IPv4 and IPv6.
            vec!["127.0.0.1".to_string(), "[::1]".to_string()]
        }
    }
}

impl std::fmt::Display for ForwardSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let proto = match self.proto {
            ForwardProto::Tcp => "tcp",
            ForwardProto::Udp => "udp",
        };
        if let Some(ref bind) = self.bind_address {
            write!(
                f,
                "{}:{}:{}:{}/{}",
                bind, self.local_port, self.remote_host, self.remote_port, proto
            )
        } else {
            write!(
                f,
                "{}:{}:{}/{}",
                self.local_port, self.remote_host, self.remote_port, proto
            )
        }
    }
}

/// Resolve a `"host:port"` string to a `SocketAddr` for UDP forwarding,
/// honouring the caller's `-4`/`-6` preference.
///
/// Candidates of the preferred family are tried first; for each one, routing is
/// probed by binding an ephemeral UDP socket and calling `connect()` on it —
/// which checks the routing table without sending any packets.  The first
/// address whose probe succeeds is returned, so an unroutable preferred family
/// falls back to the other one rather than failing.
///
/// [`AddrPref::Auto`] means **IPv6 first**, which is what this function has done
/// since v0.4.x and what its regression test asserts; the flag only overrides
/// that order, it does not introduce it.
///
/// Returns `None` when the name does not resolve, or resolves only to addresses
/// the kernel has no route to at all.
pub async fn resolve_udp_target(addr_str: &str, pref: AddrPref) -> Option<std::net::SocketAddr> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr_str).await.ok()?.collect();
    let ordered = crate::addrfam::order_by_family(&addrs, pref.or(AddrPref::Ipv6));
    crate::addrfam::first_routable(&ordered)
}

/// Connect to a forwarded TCP target, trying the preferred address family first.
///
/// `tokio::net::TcpStream::connect("host:port")` already walks every resolved
/// address, but in whatever order the resolver returned them — so a `-4`/`-6`
/// preference would be ignored for forwarded TCP while being honoured for the
/// QUIC connection and for UDP forwards.  This reorders the candidate list and
/// then keeps the same "try each until one connects" behaviour, so the flag
/// changes *which family is tried first*, never whether the forward works.
///
/// The error returned when every candidate fails is the last connect error,
/// matching what `TcpStream::connect` would have reported.
pub async fn connect_tcp_preferred(
    addr_str: &str,
    pref: AddrPref,
) -> std::io::Result<tokio::net::TcpStream> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr_str).await?.collect();
    let ordered = crate::addrfam::order_by_family(&addrs, pref);
    let mut last_err = std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("could not resolve {addr_str}"),
    );
    for addr in ordered {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_tcp() {
        let s = ForwardSpec::parse("8080:localhost:80").unwrap();
        assert_eq!(s.bind_address, None);
        assert_eq!(s.local_port, 8080);
        assert_eq!(s.remote_host, "localhost");
        assert_eq!(s.remote_port, 80);
        assert_eq!(s.proto, ForwardProto::Tcp);
    }

    #[test]
    fn test_parse_explicit_tcp() {
        let s = ForwardSpec::parse("8080:localhost:80/tcp").unwrap();
        assert_eq!(s.proto, ForwardProto::Tcp);
    }

    #[test]
    fn test_parse_udp() {
        let s = ForwardSpec::parse("5353:192.168.1.1:53/udp").unwrap();
        assert_eq!(s.local_port, 5353);
        assert_eq!(s.remote_host, "192.168.1.1");
        assert_eq!(s.remote_port, 53);
        assert_eq!(s.proto, ForwardProto::Udp);
    }

    #[test]
    fn test_parse_ipv6_remote() {
        let s = ForwardSpec::parse("5432:[::1]:5432").unwrap();
        assert_eq!(s.remote_host, "[::1]");
        assert_eq!(s.remote_port, 5432);
    }

    #[test]
    fn test_parse_ipv6_remote_udp() {
        let s = ForwardSpec::parse("1234:[::1]:5678/udp").unwrap();
        assert_eq!(s.remote_host, "[::1]");
        assert_eq!(s.proto, ForwardProto::Udp);
    }

    #[test]
    fn test_parse_with_bind_address() {
        let s = ForwardSpec::parse("127.0.0.1:8080:localhost:80").unwrap();
        assert_eq!(s.bind_address, Some("127.0.0.1".to_string()));
        assert_eq!(s.local_port, 8080);
        assert_eq!(s.remote_host, "localhost");
        assert_eq!(s.remote_port, 80);

        let s = ForwardSpec::parse("[::1]:8080:localhost:80").unwrap();
        assert_eq!(s.bind_address, Some("[::1]".to_string()));
    }

    #[test]
    fn test_parse_wildcard_bind_address() {
        let s = ForwardSpec::parse("*:8080:localhost:80").unwrap();
        assert_eq!(s.bind_address, Some("*".to_string()));
    }

    #[test]
    fn test_get_bind_addresses() {
        // Default (no gateway): two loopback sockets.
        let s = ForwardSpec::parse("8080:localhost:80").unwrap();
        assert_eq!(s.get_bind_addresses(false), vec!["127.0.0.1", "[::1]"]);

        // Gateway flag: single dual-stack [::] socket.
        assert_eq!(s.get_bind_addresses(true), vec!["[::]"]);

        // Wildcard explicit bind address: single dual-stack [::] socket.
        let s = ForwardSpec::parse("*:8080:localhost:80").unwrap();
        assert_eq!(s.get_bind_addresses(false), vec!["[::]"]);

        // 0.0.0.0 explicit bind: single dual-stack [::] socket.
        let s = ForwardSpec::parse("0.0.0.0:8080:localhost:80").unwrap();
        assert_eq!(s.get_bind_addresses(false), vec!["[::]"]);

        // Specific IP explicit bind: returned as-is.
        let s = ForwardSpec::parse("192.168.1.50:8080:localhost:80").unwrap();
        assert_eq!(s.get_bind_addresses(false), vec!["192.168.1.50"]);
    }

    #[test]
    fn test_parse_missing_remote_port() {
        assert!(ForwardSpec::parse("8080:localhost").is_err());
    }

    #[test]
    fn test_parse_bad_local_port() {
        assert!(ForwardSpec::parse("notaport:localhost:80").is_err());
    }

    #[test]
    fn test_parse_bad_remote_port() {
        assert!(ForwardSpec::parse("8080:localhost:notaport").is_err());
    }

    #[test]
    fn test_parse_empty_host() {
        assert!(ForwardSpec::parse("8080::80").is_err());
    }

    #[test]
    fn test_display() {
        let s = ForwardSpec::parse("8080:localhost:80").unwrap();
        assert_eq!(s.to_string(), "8080:localhost:80/tcp");
        let s = ForwardSpec::parse("53:dns.internal:53/udp").unwrap();
        assert_eq!(s.to_string(), "53:dns.internal:53/udp");
        let s = ForwardSpec::parse("127.0.0.1:8080:localhost:80").unwrap();
        assert_eq!(s.to_string(), "127.0.0.1:8080:localhost:80/tcp");
    }

    #[tokio::test]
    async fn test_resolve_udp_target_localhost() {
        // localhost always resolves to a loopback address; the routing probe must
        // succeed and return either ::1 (IPv6 preferred) or 127.0.0.1 (IPv4 fallback).
        let addr = super::resolve_udp_target("localhost:53", AddrPref::Auto)
            .await
            .expect("localhost must resolve");
        assert_eq!(addr.port(), 53);
        assert!(addr.ip().is_loopback(), "expected loopback, got {addr}");
    }

    #[tokio::test]
    async fn test_resolve_udp_target_prefers_ipv6() {
        // On any system with an IPv6 loopback (virtually universal), ::1 should be
        // chosen over 127.0.0.1 because IPv6 is tried first.
        let addr = super::resolve_udp_target("localhost:53", AddrPref::Auto).await;
        if let Some(a) = addr {
            // If the system has IPv6 routing, the result must be IPv6.
            // If not (IPv6 disabled), IPv4 fallback is acceptable.
            let has_ipv6_routing = std::net::UdpSocket::bind("[::]:0")
                .and_then(|s| s.connect("::1:1"))
                .is_ok();
            if has_ipv6_routing {
                assert!(
                    a.is_ipv6(),
                    "expected IPv6 result on IPv6-capable system, got {a}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_resolve_udp_target_explicit_ipv4() {
        // An explicit IPv4 address skips the IPv6 probe and resolves directly.
        let addr = super::resolve_udp_target("127.0.0.1:1234", AddrPref::Auto)
            .await
            .expect("explicit IPv4 loopback must resolve");
        assert!(addr.is_ipv4());
        assert_eq!(addr.port(), 1234);
    }

    #[tokio::test]
    async fn test_resolve_udp_target_unresolvable() {
        let addr =
            super::resolve_udp_target("this.hostname.does.not.exist.invalid:53", AddrPref::Auto)
                .await;
        assert!(addr.is_none(), "unresolvable host must return None");
    }

    #[tokio::test]
    async fn test_resolve_udp_target_ipv4_preference_overrides_default() {
        // `-4` must beat the IPv6-first default on a name that has both.
        let addr = super::resolve_udp_target("localhost:53", AddrPref::Ipv4)
            .await
            .expect("localhost must resolve");
        assert!(addr.is_ipv4(), "expected IPv4 under -4, got {addr}");
    }

    #[tokio::test]
    async fn test_resolve_udp_target_preference_falls_back_to_other_family() {
        // Preference, not restriction: an IPv4-only target under `-6` still resolves.
        let addr = super::resolve_udp_target("127.0.0.1:53", AddrPref::Ipv6)
            .await
            .expect("explicit IPv4 target must still resolve under -6");
        assert!(addr.is_ipv4());
    }

    #[tokio::test]
    async fn test_connect_tcp_preferred_reaches_an_ipv4_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

        let stream = super::connect_tcp_preferred(&format!("127.0.0.1:{port}"), AddrPref::Ipv4)
            .await
            .expect("connect to a live IPv4 listener must succeed");
        assert!(stream.peer_addr().unwrap().is_ipv4());
        accept.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_connect_tcp_preferred_falls_back_across_families() {
        // Listener on IPv4 loopback only, connected via a name that resolves to
        // both families with `-6`: the IPv6 candidate must fail and the IPv4 one
        // must still be tried, since the flag is a preference.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

        let stream = super::connect_tcp_preferred(&format!("localhost:{port}"), AddrPref::Ipv6)
            .await
            .expect("IPv6 preference must fall back to the IPv4 listener");
        assert!(stream.peer_addr().unwrap().is_ipv4());
        accept.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_connect_tcp_preferred_unresolvable_host_errors() {
        let r =
            super::connect_tcp_preferred("this.hostname.does.not.exist.invalid:80", AddrPref::Auto)
                .await;
        assert!(r.is_err(), "unresolvable host must be an error");
    }

    #[test]
    fn test_split_ignoring_brackets_plain() {
        assert_eq!(split_ignoring_brackets("a:b:c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_ignoring_brackets_ipv6_host() {
        // Colons inside brackets must not be treated as separators.
        let parts = split_ignoring_brackets("8080:[::1]:80");
        assert_eq!(parts, vec!["8080", "[::1]", "80"]);
    }

    #[test]
    fn test_split_ignoring_brackets_bind_and_ipv6() {
        let parts = split_ignoring_brackets("[::1]:9090:[::1]:80");
        assert_eq!(parts, vec!["[::1]", "9090", "[::1]", "80"]);
    }

    #[test]
    fn test_split_ignoring_brackets_no_colon() {
        assert_eq!(split_ignoring_brackets("8080"), vec!["8080"]);
    }

    #[test]
    fn test_split_ignoring_brackets_empty() {
        assert_eq!(split_ignoring_brackets(""), vec![""]);
    }

    #[test]
    fn test_split_ignoring_brackets_trailing_colon() {
        // A trailing colon produces an empty final segment.
        let parts = split_ignoring_brackets("a:b:");
        assert_eq!(parts, vec!["a", "b", ""]);
    }

    #[test]
    fn test_parse_display() {
        let d = X11Display::parse(":0").unwrap();
        assert_eq!(d, X11Display::Unix(0));
        assert_eq!(d.display_num(), 0);

        let d = X11Display::parse("unix:10.0").unwrap();
        assert_eq!(d, X11Display::Unix(10));
        assert_eq!(d.display_num(), 10);

        let d = X11Display::parse("localhost:10.0").unwrap();
        assert_eq!(d, X11Display::Tcp("localhost".to_string(), 6010));
        assert_eq!(d.display_num(), 10);

        let d = X11Display::parse("/tmp/launch-123/org.xquartz:0").unwrap();
        assert_eq!(
            d,
            X11Display::Path("/tmp/launch-123/org.xquartz:0".to_string())
        );
        assert_eq!(d.display_num(), 0);
    }
}

/// Representation of a parsed X11 DISPLAY target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X11Display {
    /// Local Unix socket (e.g. `:0` or `unix:0`), carrying display number.
    Unix(u16),
    /// Local explicit Unix socket path (common on macOS / launchd, e.g. `/tmp/launch-XXX/org.xquartz:0`).
    Path(String),
    /// TCP host and port (e.g. `localhost:10.0` -> `localhost`, port `6010`).
    Tcp(String, u16),
}

impl X11Display {
    /// Parse a DISPLAY environment string.
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.starts_with('/') {
            return Ok(Self::Path(s.to_string()));
        }
        let parts = split_ignoring_brackets(s);
        if parts.len() < 2 {
            return Err(format!("invalid DISPLAY '{}'", s));
        }
        let host = &parts[0];
        let rest = &parts[1];
        let display_num_str = rest.split('.').next().unwrap_or(rest);
        let display_num = display_num_str
            .parse::<u16>()
            .map_err(|_| format!("invalid display number '{}'", display_num_str))?;

        if host.is_empty() || host == "unix" {
            Ok(Self::Unix(display_num))
        } else {
            Ok(Self::Tcp(host.clone(), 6000 + display_num))
        }
    }

    /// Extract display number (offset from 6000).
    pub fn display_num(&self) -> u16 {
        match self {
            Self::Unix(n) => *n,
            Self::Path(p) => {
                if let Some(pos) = p.rfind(':') {
                    let rest = &p[pos + 1..];
                    rest.split('.')
                        .next()
                        .unwrap_or(rest)
                        .parse::<u16>()
                        .unwrap_or(0)
                } else {
                    0
                }
            }
            Self::Tcp(_, port) => {
                if *port >= 6000 {
                    port - 6000
                } else {
                    0
                }
            }
        }
    }
}

/// Retrieve the X11 auth protocol and key (cookie) for the specified display string.
///
/// Runs `xauth list` and matches the parsed display number or falls back to
/// the first available `MIT-MAGIC-COOKIE-1` entry.
pub fn get_xauth_cookie(display_str: &str) -> Result<(String, Vec<u8>), String> {
    let display = X11Display::parse(display_str)?;
    let target_num = display.display_num();

    let output = std::process::Command::new("xauth")
        .arg("list")
        .output()
        .map_err(|e| format!("failed to execute xauth: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "xauth failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let entry = fields[0];
        let proto = fields[1];
        let cookie_hex = fields[2];

        if let Some(pos) = entry.rfind(':') {
            let rest = &entry[pos + 1..];
            let display_part = rest
                .split('/')
                .next()
                .unwrap_or(rest)
                .split('.')
                .next()
                .unwrap_or(rest);
            if let Ok(num) = display_part.parse::<u16>()
                && num == target_num
                && let Some(cookie) = hex_decode(cookie_hex)
            {
                return Ok((proto.to_string(), cookie));
            }
        }
    }

    // Fallback: first MIT-MAGIC-COOKIE-1 entry
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 3
            && fields[1] == "MIT-MAGIC-COOKIE-1"
            && let Some(cookie) = hex_decode(fields[2])
        {
            return Ok((fields[1].to_string(), cookie));
        }
    }

    Err(format!("no xauth cookie found for display {}", display_str))
}

/// Reusable encoder for the UDP forwarding hot path.
///
/// Both forwarding loops used to build a fresh [`Envelope`](crate::protocol::Envelope) per
/// datagram and hand it to `quic::write_msg`, which cost **four heap allocations and two stream
/// writes for every datagram**:
///
/// 1. `src.ip().to_string()` — formatting an address that is already in hand;
/// 2. `buf[..n].to_vec()` — copying the payload out of the receive buffer;
/// 3. `encode_to_vec()` — allocating the protobuf body and copying the payload again;
/// 4. `write_all(&len)` then `write_all(&bytes)` — two separate writes for one message.
///
/// This keeps all of it: the framed output buffer, the payload buffer inside the reused
/// `UdpDatagram`, and the formatted peer string. The last one is cached on the address it was
/// produced from, because a forwarded port overwhelmingly carries traffic from **one** peer (a
/// DNS resolver, a game client, a syslog sender), so the format runs once per peer rather than
/// once per datagram. A changing peer degrades to the old cost rather than misbehaving.
///
/// **The wire format is byte-for-byte unchanged.** This is purely how the same bytes get built,
/// so an old peer on either side is unaffected — there is no negotiation and nothing to detect.
pub struct UdpFrameEncoder {
    /// Reused message. Its `data` and `peer_addr` keep their allocations between datagrams.
    dgram: crate::protocol::UdpDatagram,
    /// The address `dgram.peer_addr` currently holds the text for, so a repeat peer skips the
    /// formatting entirely. `None` until the first datagram.
    cached_ip: Option<IpAddr>,
    /// Framed output: `[4-byte big-endian length][protobuf body]`, built in one buffer so the
    /// message costs a single `write_all` instead of two.
    out: Vec<u8>,
}

impl Default for UdpFrameEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl UdpFrameEncoder {
    /// A new encoder with buffers sized for one maximum-size UDP datagram, so the steady state
    /// performs no reallocation at all.
    pub fn new() -> Self {
        Self {
            dgram: crate::protocol::UdpDatagram {
                peer_addr: String::new(),
                peer_port: 0,
                data: Vec::with_capacity(65535),
            },
            cached_ip: None,
            // 64 KiB payload plus protobuf and length-prefix overhead.
            out: Vec::with_capacity(65600),
        }
    }

    /// Frame one datagram, returning `[len][body]` ready for a single `write_all`.
    ///
    /// The returned slice borrows the encoder's internal buffer and is valid until the next
    /// call, which is what makes the steady state allocation-free.
    pub fn frame(&mut self, peer: SocketAddr, payload: &[u8]) -> &[u8] {
        let ip = peer.ip();
        if self.cached_ip != Some(ip) {
            // `to_string` allocates; writing through `fmt::Write` into the existing String
            // reuses the buffer we already hold.
            use std::fmt::Write as _;
            self.dgram.peer_addr.clear();
            let _ = write!(self.dgram.peer_addr, "{ip}");
            self.cached_ip = Some(ip);
        }
        self.dgram.peer_port = peer.port() as u32;
        self.dgram.data.clear();
        self.dgram.data.extend_from_slice(payload);

        // `Payload` owns its message, so encode the `UdpDatagram` field by hand rather than
        // moving `self.dgram` into an `Envelope` and losing the reused allocations. Tag 12 is
        // `Payload::UdpDatagram`; see `protocol::Payload`, where the numbers are pinned.
        let body_len = prost::encoding::message::encoded_len(12, &self.dgram);

        self.out.clear();
        self.out.extend_from_slice(&(body_len as u32).to_be_bytes());
        prost::encoding::message::encode(12, &self.dgram, &mut self.out);
        debug_assert_eq!(self.out.len(), 4 + body_len);
        &self.out
    }
}

/// Turn a [`UdpDatagram`](crate::protocol::UdpDatagram)'s `peer_addr` + `peer_port` into a
/// [`SocketAddr`], without going through a formatted string.
///
/// The obvious way to use those two fields is `format!("{peer_addr}:{peer_port}")` and hand the
/// string to `send_to`, and that is what both forwarding loops used to do. It costs an
/// allocation and a format on every datagram, and then `send_to` has to parse the string
/// straight back into the `SocketAddr` the sender already had — so the round trip through text
/// is pure overhead on the hottest path in the forwarder.
///
/// **The string form is not broken for IPv6, which is worth stating because it looks as though
/// it should be.** `format!("{}:{}", "::1", 5000)` yields the unbracketed `::1:5000`, and the
/// canonical spelling is `[::1]:5000` — but `to_socket_addrs` splits at the *last* colon, so
/// `::1:5000` and `2001:db8::1:5000` both resolve correctly. Checked rather than assumed; the
/// motivation here is cost, not a latent bug.
///
/// Taking the typed route is still better than a string that happens to parse: it cannot be
/// mis-assembled by a future edit, and it makes the two rejections below explicit.
///
/// Returns `None` when `peer_addr` is not a valid IP literal or the port is out of range; the
/// caller drops the datagram, which is the correct response to an unroutable peer.
pub fn datagram_peer_addr(peer_addr: &str, peer_port: u32) -> Option<SocketAddr> {
    // A port of 0 is not a usable destination, and `as u16` would silently truncate anything
    // above 65535 into a plausible-looking port -- exactly the kind of quiet wrong answer this
    // codebase keeps recording. Reject both.
    let port = u16::try_from(peer_port).ok()?;
    if port == 0 {
        return None;
    }
    let ip: IpAddr = peer_addr.parse().ok()?;
    Some(SocketAddr::new(ip, port))
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod udp_fastpath_tests {
    use super::*;
    use crate::protocol::{Envelope, Payload, UdpDatagram};
    use prost::Message;

    /// Frame a datagram the way the forwarding loops did before v0.9.2.
    fn old_framed(peer: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let env = Envelope {
            payload: Some(Payload::UdpDatagram(UdpDatagram {
                peer_addr: peer.ip().to_string(),
                peer_port: peer.port() as u32,
                data: payload.to_vec(),
            })),
        };
        let body = env.encode_to_vec();
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// **The load-bearing test of the whole optimisation.**
    ///
    /// `UdpFrameEncoder` exists only to build the same bytes more cheaply. If it ever emits
    /// something different, that is not a faster encoder — it is an undetected wire-format
    /// change between an updated and a non-updated peer, which no version negotiation would
    /// catch because nothing announces it.
    #[test]
    fn encoder_is_byte_identical_to_the_old_inline_path() {
        let mut enc = UdpFrameEncoder::new();
        let cases: Vec<(SocketAddr, Vec<u8>)> = vec![
            ("192.168.1.50:54321".parse().unwrap(), vec![0xAB; 1400]),
            ("127.0.0.1:53".parse().unwrap(), vec![0x00; 1]),
            // Empty payload: a real thing on the wire (a zero-length UDP datagram is legal).
            ("10.0.0.1:1".parse().unwrap(), Vec::new()),
            ("[::1]:5000".parse().unwrap(), vec![0xFF; 9000]),
            (
                "[2001:db8::dead:beef]:65535".parse().unwrap(),
                vec![0x7F; 64],
            ),
            // Largest payload a UDP datagram can carry over IPv4.
            ("172.16.0.9:9999".parse().unwrap(), vec![0x42; 65507]),
        ];
        for (peer, payload) in &cases {
            assert_eq!(
                enc.frame(*peer, payload),
                old_framed(*peer, payload).as_slice(),
                "framing differs for {peer} with a {}-byte payload",
                payload.len()
            );
        }
    }

    /// The peer string is cached, so the cache must not outlive the peer it was built for.
    /// Alternating senders on one forwarded port is the case that would expose a stale cache,
    /// and it is exactly what a busy DNS or game forward looks like.
    #[test]
    fn encoder_handles_alternating_peers() {
        let mut enc = UdpFrameEncoder::new();
        let a: SocketAddr = "1.2.3.4:10".parse().unwrap();
        let b: SocketAddr = "[fe80::1]:20".parse().unwrap();
        for _ in 0..4 {
            assert_eq!(enc.frame(a, b"first"), old_framed(a, b"first").as_slice());
            assert_eq!(enc.frame(b, b"second"), old_framed(b, b"second").as_slice());
        }
    }

    /// Same address, different port: the cache keys on the IP, so the port must still be
    /// re-encoded every time rather than being carried over with the cached string.
    #[test]
    fn encoder_updates_port_when_only_the_port_changes() {
        let mut enc = UdpFrameEncoder::new();
        let p1: SocketAddr = "192.0.2.7:1000".parse().unwrap();
        let p2: SocketAddr = "192.0.2.7:2000".parse().unwrap();
        assert_eq!(enc.frame(p1, b"x"), old_framed(p1, b"x").as_slice());
        assert_eq!(enc.frame(p2, b"x"), old_framed(p2, b"x").as_slice());
    }

    /// What the encoder emits must survive the decoder the far side actually uses.
    #[test]
    fn encoder_output_round_trips_through_the_decoder() {
        let mut enc = UdpFrameEncoder::new();
        let peer: SocketAddr = "[2001:db8::1]:4433".parse().unwrap();
        let payload = vec![0x5Au8; 1400];
        let framed = enc.frame(peer, &payload);

        let len = u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize;
        assert_eq!(
            len,
            framed.len() - 4,
            "length prefix disagrees with the body"
        );

        let env = Envelope::decode(&framed[4..]).expect("body must decode");
        let Some(Payload::UdpDatagram(dg)) = env.payload else {
            panic!("decoded to the wrong payload variant");
        };
        assert_eq!(dg.data, payload);
        assert_eq!(datagram_peer_addr(&dg.peer_addr, dg.peer_port), Some(peer));
    }

    #[test]
    fn datagram_peer_addr_accepts_both_families() {
        assert_eq!(
            datagram_peer_addr("127.0.0.1", 53),
            Some("127.0.0.1:53".parse().unwrap())
        );
        // Bare IPv6, no brackets -- which is how it travels on the wire.
        assert_eq!(
            datagram_peer_addr("::1", 5000),
            Some("[::1]:5000".parse().unwrap())
        );
    }

    /// Port 0 is not a destination, and a port above 65535 must be dropped rather than
    /// truncated. `dg.peer_port as u16` -- what the server used to do -- turns 65536 into 0 and
    /// 65537 into 1, quietly sending the datagram somewhere plausible and wrong.
    #[test]
    fn datagram_peer_addr_rejects_unusable_ports() {
        assert_eq!(datagram_peer_addr("127.0.0.1", 0), None, "port 0");
        assert_eq!(
            datagram_peer_addr("127.0.0.1", 65536),
            None,
            "just over u16"
        );
        assert_eq!(
            datagram_peer_addr("127.0.0.1", 4_294_967_295),
            None,
            "u32::MAX"
        );
        assert_eq!(65536u32 as u16, 0, "the truncation this guards against");
    }

    #[test]
    fn datagram_peer_addr_rejects_non_addresses() {
        assert_eq!(datagram_peer_addr("", 53), None);
        assert_eq!(datagram_peer_addr("not-an-ip", 53), None);
        // A host name is not accepted: this field carries a literal, and resolving here would
        // put a DNS lookup on the per-datagram path.
        assert_eq!(datagram_peer_addr("localhost", 53), None);
        // Already-bracketed text is not what the wire carries, and must not be accepted as a
        // bare IP literal.
        assert_eq!(datagram_peer_addr("[::1]", 53), None);
    }
}
