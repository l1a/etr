// SPDX-License-Identifier: GPL-3.0-or-later
//! Local port-forwarding spec parsing.
//!
//! Accepts the same syntax as `ssh -L`:
//! ```text
//! local_port:remote_host:remote_port[/tcp|/udp]
//! ```
//!
//! Examples:
//! ```text
//! 8080:localhost:80          TCP (default)
//! 8080:localhost:80/tcp      explicit TCP
//! 5353:192.168.1.1:53/udp   UDP
//! 5432:[::1]:5432            IPv6 remote host, TCP
//! ```
use crate::protocol::ForwardProto;

/// A parsed `-L` forwarding specification.
#[derive(Debug, Clone)]
pub struct ForwardSpec {
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub proto: ForwardProto,
}

impl ForwardSpec {
    /// Parse a `-L` spec string.
    ///
    /// Format: `local_port:remote_host:remote_port[/tcp|/udp]`
    ///
    /// The remote host may be an IPv6 address in brackets (`[::1]`).
    /// Splitting uses the first `:` for `local_port` and the last `:` for
    /// `remote_port`, so IPv6 bracket notation works without quoting.
    pub fn parse(s: &str) -> Result<Self, String> {
        // Strip optional /tcp or /udp suffix.
        let (rest, proto) = if let Some(base) = s.strip_suffix("/udp") {
            (base, ForwardProto::Udp)
        } else {
            (s.strip_suffix("/tcp").unwrap_or(s), ForwardProto::Tcp)
        };

        // Split local_port off the front.
        let first_colon = rest.find(':').ok_or_else(|| {
            format!("invalid spec '{s}': expected local_port:remote_host:remote_port")
        })?;
        let local_port_str = &rest[..first_colon];
        let after_local = &rest[first_colon + 1..];

        // Split remote_port off the back (last ':').
        let last_colon = after_local.rfind(':').ok_or_else(|| {
            format!("invalid spec '{s}': expected local_port:remote_host:remote_port")
        })?;
        let remote_host = after_local[..last_colon].to_string();
        let remote_port_str = &after_local[last_colon + 1..];

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
            local_port,
            remote_host,
            remote_port,
            proto,
        })
    }
}

impl std::fmt::Display for ForwardSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let proto = match self.proto {
            ForwardProto::Tcp => "tcp",
            ForwardProto::Udp => "udp",
        };
        write!(
            f,
            "{}:{}:{}/{}",
            self.local_port, self.remote_host, self.remote_port, proto
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_tcp() {
        let s = ForwardSpec::parse("8080:localhost:80").unwrap();
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
    }
}
