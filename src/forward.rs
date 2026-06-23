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
use crate::protocol::ForwardProto;

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
/// preferring IPv6 when the kernel has a route to it.
///
/// We prefer IPv6 to be consistent with modern Happy-Eyeballs behaviour.
/// For each candidate address (IPv6 first, then IPv4) we probe routing by
/// binding an ephemeral UDP socket and calling `connect()` on it — this
/// checks the routing table without sending any packets.  The first address
/// whose routing probe succeeds is returned.
pub async fn resolve_udp_target(addr_str: &str) -> Option<std::net::SocketAddr> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr_str).await.ok()?.collect();

    // IPv6 candidates first, then IPv4.
    let ordered = addrs
        .iter()
        .filter(|a| a.is_ipv6())
        .copied()
        .chain(addrs.iter().filter(|a| a.is_ipv4()).copied());

    for addr in ordered {
        let bind_str = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        if let Ok(sock) = std::net::UdpSocket::bind(bind_str)
            && sock.connect(addr).is_ok()
        {
            return Some(addr);
        }
    }
    None
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
        let addr = super::resolve_udp_target("localhost:53")
            .await
            .expect("localhost must resolve");
        assert_eq!(addr.port(), 53);
        assert!(addr.ip().is_loopback(), "expected loopback, got {addr}");
    }

    #[tokio::test]
    async fn test_resolve_udp_target_prefers_ipv6() {
        // On any system with an IPv6 loopback (virtually universal), ::1 should be
        // chosen over 127.0.0.1 because IPv6 is tried first.
        let addr = super::resolve_udp_target("localhost:53").await;
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
        let addr = super::resolve_udp_target("127.0.0.1:1234")
            .await
            .expect("explicit IPv4 loopback must resolve");
        assert!(addr.is_ipv4());
        assert_eq!(addr.port(), 1234);
    }

    #[tokio::test]
    async fn test_resolve_udp_target_unresolvable() {
        let addr = super::resolve_udp_target("this.hostname.does.not.exist.invalid:53").await;
        assert!(addr.is_none(), "unresolvable host must return None");
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

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
