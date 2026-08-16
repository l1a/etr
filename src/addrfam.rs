// SPDX-License-Identifier: GPL-3.0-or-later
//! Address-family preference (`-4` / `-6`) and the resolution helpers built on it.
//!
//! Both binaries accept `-4`/`--prefer-ipv4` and `-6`/`--prefer-ipv6`.  The flags
//! are a **preference, not a restriction**: candidates of the requested family are
//! tried first and the other family is still used if the preferred one is absent
//! or unroutable.  Nothing here ever fails a connection purely because the
//! requested family was unavailable — that is the difference between these flags
//! and `ssh -4`/`-6`, which are hard restrictions.
//!
//! [`AddrPref::Auto`] deliberately means *"whatever the call site did before this
//! module existed"* rather than a single global default, because the two call
//! sites differed: the QUIC connect address took the resolver's first answer,
//! while UDP forward targets have preferred IPv6 since v0.4.x.  Each caller passes
//! its own fallback via [`AddrPref::or`], so an unflagged run is unchanged.

use std::net::SocketAddr;

/// Which IP version to try first when a name resolves to both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AddrPref {
    /// No explicit preference — the caller's own default order applies.
    #[default]
    Auto,
    /// Try IPv4 addresses first, fall back to IPv6.
    Ipv4,
    /// Try IPv6 addresses first, fall back to IPv4.
    Ipv6,
}

impl AddrPref {
    /// Build a preference from the two mutually exclusive CLI flags.
    ///
    /// `clap` rejects both being set at once, so the combination is unreachable
    /// through the CLI; if it is ever constructed directly, IPv6 wins (it is the
    /// order the rest of the codebase already defaults to).
    pub fn from_flags(prefer_ipv4: bool, prefer_ipv6: bool) -> Self {
        match (prefer_ipv4, prefer_ipv6) {
            (_, true) => AddrPref::Ipv6,
            (true, false) => AddrPref::Ipv4,
            (false, false) => AddrPref::Auto,
        }
    }

    /// Parse the `address_family` config value.
    ///
    /// Accepts `"ipv4"`/`"v4"`/`"4"`/`"inet"` and the IPv6 equivalents, case
    /// insensitively.  Anything else — including `"auto"` and an empty value —
    /// is [`AddrPref::Auto`], so a typo degrades to today's behaviour instead of
    /// silently selecting the wrong family.
    pub fn from_config(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("ipv4" | "v4" | "4" | "inet") => AddrPref::Ipv4,
            Some("ipv6" | "v6" | "6" | "inet6") => AddrPref::Ipv6,
            _ => AddrPref::Auto,
        }
    }

    /// Return `self` unless it is [`AddrPref::Auto`], in which case return
    /// `fallback`.  Used by call sites that have their own historical default
    /// order to preserve.
    pub fn or(self, fallback: AddrPref) -> Self {
        match self {
            AddrPref::Auto => fallback,
            other => other,
        }
    }

    /// The `ssh(1)` flag matching this preference, if any.
    ///
    /// `ssh -4`/`-6` are *restrictions*, so the client only passes this on once
    /// it has confirmed the target actually has an address of that family —
    /// otherwise a preference would turn into a bootstrap failure.
    pub fn ssh_flag(self) -> Option<&'static str> {
        match self {
            AddrPref::Auto => None,
            AddrPref::Ipv4 => Some("-4"),
            AddrPref::Ipv6 => Some("-6"),
        }
    }

    /// The value carried by the `ETRPREFER:` SSH-bootstrap line, if any.
    pub fn wire(self) -> Option<&'static str> {
        match self {
            AddrPref::Auto => None,
            AddrPref::Ipv4 => Some("4"),
            AddrPref::Ipv6 => Some("6"),
        }
    }

    /// Parse an `ETRPREFER:` payload sent by the client.  Unknown values are
    /// [`AddrPref::Auto`] so a newer client can add values without breaking an
    /// older server.
    pub fn from_wire(value: &str) -> Self {
        match value.trim() {
            "4" => AddrPref::Ipv4,
            "6" => AddrPref::Ipv6,
            _ => AddrPref::Auto,
        }
    }

    /// Human-readable name, for `-v` logging.
    pub fn as_str(self) -> &'static str {
        match self {
            AddrPref::Auto => "auto",
            AddrPref::Ipv4 => "ipv4",
            AddrPref::Ipv6 => "ipv6",
        }
    }

    /// Does `addr` belong to the preferred family?  Always false for
    /// [`AddrPref::Auto`], which has no preferred family.
    pub fn matches(self, addr: &SocketAddr) -> bool {
        match self {
            AddrPref::Auto => false,
            AddrPref::Ipv4 => addr.is_ipv4(),
            AddrPref::Ipv6 => addr.is_ipv6(),
        }
    }
}

/// Reorder resolved addresses so the preferred family comes first.
///
/// The sort is **stable**: the resolver's relative order is preserved within
/// each family, and [`AddrPref::Auto`] returns the input order untouched — which
/// is what keeps an unflagged run byte-identical to previous releases.
pub fn order_by_family(addrs: &[SocketAddr], pref: AddrPref) -> Vec<SocketAddr> {
    match pref {
        AddrPref::Auto => addrs.to_vec(),
        _ => addrs
            .iter()
            .filter(|a| pref.matches(a))
            .chain(addrs.iter().filter(|a| !pref.matches(a)))
            .copied()
            .collect(),
    }
}

/// Return the first address the kernel has a route to.
///
/// Probes by binding an ephemeral UDP socket and calling `connect()` on it,
/// which consults the routing table **without sending a packet**.  Returns
/// `None` when no candidate is routable.
///
/// Note what this does and does not catch: a host with no IPv6 route at all
/// fails here instantly, which is the case worth avoiding.  A route that exists
/// but blackholes traffic still probes as reachable — routing tables cannot tell
/// you about the far end.
pub fn first_routable(addrs: &[SocketAddr]) -> Option<SocketAddr> {
    for addr in addrs {
        let bind_str = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        if let Ok(sock) = std::net::UdpSocket::bind(bind_str)
            && sock.connect(*addr).is_ok()
        {
            return Some(*addr);
        }
    }
    None
}

/// Resolve `"host:port"` and pick one address, honouring `pref`.
///
/// Preferred-family candidates are tried first, and the first one the kernel can
/// route to wins.  If nothing is routable the first candidate in preference
/// order is returned anyway, so the caller reports a real connection error
/// rather than a misleading "could not resolve".
///
/// Returns `None` only when the name does not resolve at all.
pub async fn resolve_preferred(host_port: &str, pref: AddrPref) -> Option<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(host_port).await.ok()?.collect();
    let ordered = order_by_family(&addrs, pref);
    first_routable(&ordered).or_else(|| ordered.first().copied())
}

/// Does `host_port` resolve to at least one address of the preferred family?
///
/// Used before handing `-4`/`-6` to `ssh`, which treats them as restrictions:
/// passing one for a family the host does not have would turn a preference into
/// a bootstrap failure.  A name that does not resolve locally (an `ssh_config`
/// `Host` alias, say) answers `false`, so ssh is left to its own resolution.
pub async fn family_available(host_port: &str, pref: AddrPref) -> bool {
    if pref == AddrPref::Auto {
        return false;
    }
    match tokio::net::lookup_host(host_port).await {
        Ok(mut addrs) => addrs.any(|a| pref.matches(&a)),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn from_flags_maps_each_combination() {
        assert_eq!(AddrPref::from_flags(false, false), AddrPref::Auto);
        assert_eq!(AddrPref::from_flags(true, false), AddrPref::Ipv4);
        assert_eq!(AddrPref::from_flags(false, true), AddrPref::Ipv6);
        // clap rejects both; if constructed directly, IPv6 wins deterministically.
        assert_eq!(AddrPref::from_flags(true, true), AddrPref::Ipv6);
    }

    #[test]
    fn from_config_accepts_aliases_and_falls_back_to_auto() {
        assert_eq!(AddrPref::from_config(Some("ipv4")), AddrPref::Ipv4);
        assert_eq!(AddrPref::from_config(Some(" V4 ")), AddrPref::Ipv4);
        assert_eq!(AddrPref::from_config(Some("inet6")), AddrPref::Ipv6);
        assert_eq!(AddrPref::from_config(Some("IPv6")), AddrPref::Ipv6);
        assert_eq!(AddrPref::from_config(Some("auto")), AddrPref::Auto);
        // A typo must degrade to today's behaviour, not to the wrong family.
        assert_eq!(AddrPref::from_config(Some("ipv46")), AddrPref::Auto);
        assert_eq!(AddrPref::from_config(None), AddrPref::Auto);
    }

    #[test]
    fn or_only_substitutes_for_auto() {
        assert_eq!(AddrPref::Auto.or(AddrPref::Ipv6), AddrPref::Ipv6);
        assert_eq!(AddrPref::Ipv4.or(AddrPref::Ipv6), AddrPref::Ipv4);
        assert_eq!(AddrPref::Ipv6.or(AddrPref::Ipv4), AddrPref::Ipv6);
    }

    #[test]
    fn wire_round_trips_and_ignores_unknown_values() {
        assert_eq!(AddrPref::Auto.wire(), None);
        assert_eq!(
            AddrPref::from_wire(AddrPref::Ipv4.wire().unwrap()),
            AddrPref::Ipv4
        );
        assert_eq!(
            AddrPref::from_wire(AddrPref::Ipv6.wire().unwrap()),
            AddrPref::Ipv6
        );
        // Forward compatibility: an unknown value must not select a family.
        assert_eq!(AddrPref::from_wire("46"), AddrPref::Auto);
        assert_eq!(AddrPref::from_wire(""), AddrPref::Auto);
    }

    #[test]
    fn ssh_flag_matches_the_preference() {
        assert_eq!(AddrPref::Auto.ssh_flag(), None);
        assert_eq!(AddrPref::Ipv4.ssh_flag(), Some("-4"));
        assert_eq!(AddrPref::Ipv6.ssh_flag(), Some("-6"));
    }

    #[test]
    fn order_by_family_puts_the_preferred_family_first() {
        let addrs = vec![v4("[::1]:22"), v4("127.0.0.1:22"), v4("[fe80::1]:22")];

        let v4_first = order_by_family(&addrs, AddrPref::Ipv4);
        assert!(v4_first[0].is_ipv4());
        assert_eq!(v4_first.len(), 3);

        let v6_first = order_by_family(&addrs, AddrPref::Ipv6);
        assert!(v6_first[0].is_ipv6());
        assert!(v6_first[2].is_ipv4());
    }

    #[test]
    fn order_by_family_is_stable_within_a_family() {
        let addrs = vec![v4("[::1]:22"), v4("[fe80::1]:22"), v4("127.0.0.1:22")];
        let ordered = order_by_family(&addrs, AddrPref::Ipv6);
        // Relative order of the two IPv6 entries must be the resolver's.
        assert_eq!(ordered[0], addrs[0]);
        assert_eq!(ordered[1], addrs[1]);
    }

    #[test]
    fn order_by_family_auto_is_identity() {
        let addrs = vec![v4("[::1]:22"), v4("127.0.0.1:22")];
        assert_eq!(order_by_family(&addrs, AddrPref::Auto), addrs);
    }

    #[test]
    fn order_by_family_handles_single_family_and_empty_input() {
        let only_v4 = vec![v4("127.0.0.1:22"), v4("10.0.0.1:22")];
        assert_eq!(order_by_family(&only_v4, AddrPref::Ipv6), only_v4);
        assert!(order_by_family(&[], AddrPref::Ipv4).is_empty());
    }

    #[test]
    fn first_routable_finds_loopback_and_rejects_nothing_to_probe() {
        let addrs = vec![v4("127.0.0.1:9")];
        assert_eq!(first_routable(&addrs), Some(v4("127.0.0.1:9")));
        assert_eq!(first_routable(&[]), None);
    }

    #[tokio::test]
    async fn resolve_preferred_honours_ipv4() {
        let addr = super::resolve_preferred("localhost:22", AddrPref::Ipv4).await;
        // Every supported platform resolves localhost to at least 127.0.0.1.
        let addr = addr.expect("localhost must resolve");
        assert!(addr.is_ipv4(), "expected IPv4 for -4, got {addr}");
    }

    #[tokio::test]
    async fn resolve_preferred_returns_none_for_unresolvable_host() {
        let addr =
            super::resolve_preferred("this.hostname.does.not.exist.invalid:22", AddrPref::Ipv6)
                .await;
        assert!(addr.is_none());
    }

    #[tokio::test]
    async fn family_available_is_false_for_auto_and_for_unresolvable_hosts() {
        assert!(!super::family_available("localhost:22", AddrPref::Auto).await);
        assert!(
            !super::family_available("this.hostname.does.not.exist.invalid:22", AddrPref::Ipv4)
                .await
        );
        assert!(super::family_available("127.0.0.1:22", AddrPref::Ipv4).await);
        assert!(!super::family_available("127.0.0.1:22", AddrPref::Ipv6).await);
    }
}
