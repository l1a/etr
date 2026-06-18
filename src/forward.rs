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
}
