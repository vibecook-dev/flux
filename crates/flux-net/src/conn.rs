//! Per-connection facts attached to every request as an axum extension.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// How a caller reached the server. Its [`as_str`](NetPath::as_str) form is
/// what `flux_core::api::ServerInfo::path` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetPath {
    /// Same machine, over the loopback interface.
    Localhost,
    /// The local network, without Tailscale.
    Lan,
    /// Tailnet, with a direct WireGuard path to the peer.
    TailnetDirect,
    /// Tailnet, through a DERP or peer relay (expect lower throughput).
    TailnetRelay,
    /// Tailnet, path unknown. Embedded mode can only tell direct from relay
    /// for peers that are themselves flux/Truffle nodes; a browser on a
    /// phone running the Tailscale app lands here.
    Tailnet,
}

impl NetPath {
    /// The wire string: `"localhost"`, `"lan"`, `"tailnet-direct"`,
    /// `"tailnet-relay"` or `"tailnet"`.
    pub fn as_str(self) -> &'static str {
        match self {
            NetPath::Localhost => "localhost",
            NetPath::Lan => "lan",
            NetPath::TailnetDirect => "tailnet-direct",
            NetPath::TailnetRelay => "tailnet-relay",
            NetPath::Tailnet => "tailnet",
        }
    }

    /// True for any of the tailnet paths.
    pub fn is_tailnet(self) -> bool {
        matches!(
            self,
            NetPath::TailnetDirect | NetPath::TailnetRelay | NetPath::Tailnet
        )
    }

    /// Map Tailscale's connection description: `"direct"`, `"relay:fra"`,
    /// `"peer-relay:…"`. Anything else is [`NetPath::Tailnet`].
    pub fn from_connection_type(connection_type: &str) -> NetPath {
        let ct = connection_type.trim();
        if ct.eq_ignore_ascii_case("direct") {
            NetPath::TailnetDirect
        } else if ct.starts_with("relay") || ct.starts_with("peer-relay") || ct.starts_with("derp") {
            NetPath::TailnetRelay
        } else {
            NetPath::Tailnet
        }
    }
}

impl fmt::Display for NetPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Facts about the connection a request arrived on, inserted into every
/// request's extensions next to `Option<flux_core::api::Identity>`.
///
/// ```text
/// async fn server_info(
///     Extension(you): Extension<Option<Identity>>,
///     Extension(conn): Extension<ConnInfo>,
/// ) -> Json<ServerInfo> { … conn.path.as_str() … conn.tailnet_name … }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ConnInfo {
    /// How the caller reached us.
    pub path: NetPath,
    /// The caller's address as seen by the listener (a tailnet 100.x address
    /// for tailnet callers, also in embedded mode).
    pub remote_addr: Option<SocketAddr>,
    /// This server's MagicDNS name when the caller came over the tailnet,
    /// e.g. `flux.tail1234.ts.net` (no trailing dot).
    pub tailnet_name: Option<String>,
    /// Raw grant values of the flux app capability (see
    /// [`RolePolicy::capability`](crate::RolePolicy::capability)) for this
    /// caller, for permissions beyond the role (libraries, rating caps…).
    /// Empty when none are granted or the caller is not on the tailnet.
    pub grants: Vec<serde_json::Value>,
}

impl ConnInfo {
    pub(crate) fn local(remote: SocketAddr, path: NetPath) -> ConnInfo {
        ConnInfo {
            path,
            remote_addr: Some(remote),
            tailnet_name: None,
            grants: Vec::new(),
        }
    }
}

/// Tailscale's address ranges: CGNAT 100.64.0.0/10 and the ULA
/// fd7a:115c:a1e0::/48.
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Unwrap IPv4-mapped IPv6 (`::ffff:a.b.c.d`) so a dual-stack listener
/// classifies v4 callers like a v4 one.
pub(crate) fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_types_map_to_paths() {
        assert_eq!(NetPath::from_connection_type("direct"), NetPath::TailnetDirect);
        assert_eq!(NetPath::from_connection_type("relay:fra"), NetPath::TailnetRelay);
        assert_eq!(
            NetPath::from_connection_type("peer-relay:1.2.3.4:5"),
            NetPath::TailnetRelay
        );
        assert_eq!(NetPath::from_connection_type(""), NetPath::Tailnet);
        assert_eq!(NetPath::TailnetRelay.as_str(), "tailnet-relay");
        assert!(NetPath::Tailnet.is_tailnet() && !NetPath::Lan.is_tailnet());
    }

    #[test]
    fn tailscale_ranges() {
        assert!(is_tailscale_ip("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_ip("100.127.255.254".parse().unwrap()));
        assert!(!is_tailscale_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale_ip("100.63.255.255".parse().unwrap()));
        assert!(!is_tailscale_ip("192.168.1.10".parse().unwrap()));
        assert!(is_tailscale_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailscale_ip("fd00::1".parse().unwrap()));
        assert_eq!(
            canonical_ip("::ffff:100.64.0.1".parse().unwrap()),
            "100.64.0.1".parse::<IpAddr>().unwrap()
        );
    }
}
