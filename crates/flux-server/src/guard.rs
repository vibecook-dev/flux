//! Cross-site defences (review SEC-1, SEC-7): which host names flux answers
//! to, and the header every state-changing API call must carry. The web
//! client follows the same contract (`web/src/api/client.ts`):
//!
//! 1. **Host allowlist**, on every listener, for every request. The `Host`
//!    header (and an absolute-form request target's authority) must name,
//!    with any port:
//!    - `localhost`, `127.0.0.1` or `[::1]` (any loopback address);
//!    - this machine's host name, or `<hostname>.local`;
//!    - an IP address of one of this machine's interfaces;
//!    - the tailnet's MagicDNS name (or its first label) or a tailnet
//!      address, while the tailnet is on;
//!    - an entry of `--allow-host` / `FLUX_ALLOW_HOSTS` (comma-separated).
//!
//!    Anything else is answered `421 Misdirected Request` with a plain-text
//!    body before any handler runs. This defeats DNS rebinding: a page on
//!    `attacker.example` that re-points its name at 127.0.0.1 is same-origin
//!    with itself, but its requests still say `Host: attacker.example`. So
//!    the localhost Owner identity only reaches handlers through requests
//!    that passed this check.
//! 2. **Unsafe methods** (`POST`, `PUT`, `PATCH`, `DELETE`) under `/api/`,
//!    and **every** request under `/api/v1/admin/`, need the header
//!    `X-Flux-Request: 1` (else 403). An `Origin` header, if present, must
//!    name an allowed host as `scheme://host[:port]` (else 403). flux never
//!    sends `Access-Control-Allow-*`, so a page on another origin can't add
//!    the header (that needs a CORS preflight flux never grants), and a
//!    body-less form or `no-cors` POST doesn't carry it. Admin `GET`s are
//!    covered too because some act on the machine (the folder browser reads
//!    any folder it is given): an `<img>` on another site must not make the
//!    Owner's browser do that (a `\\host\share` path would hand Windows'
//!    credentials to that host).
//! 3. `GET`s of media, HLS, SSE and art need nothing extra (`<video>`,
//!    hls.js segment loads and `EventSource` can't set headers).
//! 4. `/mcp` (MCP, for agents): a `POST` needs `Content-Type:
//!    application/json` (or the header of rule 2), and a present `Origin`
//!    must be allowed. A cross-site page can send neither without a CORS
//!    preflight.
//!
//! A request without any `Host` (HTTP/1.0 tools; never a browser) passes
//! (1): DNS rebinding needs a browser, and browsers always send one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;

/// The header (and value `1`) every unsafe `/api/` request must carry
/// (shared with clients through flux-core).
pub use flux_core::api::REQUEST_HEADER;

/// How often, at most, the interface addresses are re-read when a request
/// names an address that isn't (yet) known.
const INTERFACE_REFRESH: Duration = Duration::from_secs(5);

/// The host names and addresses this server answers to (rule 1 above).
pub struct HostAllowlist {
    /// Lowercase names without a trailing dot.
    names: Vec<String>,
    /// IP literals from `--allow-host`.
    ips: Vec<IpAddr>,
    interfaces: RwLock<Interfaces>,
    tailnet: RwLock<Tailnet>,
}

struct Interfaces {
    ips: Vec<IpAddr>,
    read_at: Instant,
}

#[derive(Default)]
struct Tailnet {
    names: Vec<String>,
    ips: Vec<IpAddr>,
}

/// A parsed host: an IP literal or a lowercase name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    Ip(IpAddr),
    Name(String),
}

impl HostAllowlist {
    /// The allowlist for a machine called `host_name` (as `gethostname`
    /// reports it, `.local` or not), plus `extra` (`--allow-host` entries:
    /// names or IP literals, an optional port is ignored).
    pub fn new(host_name: &str, extra: &[String]) -> HostAllowlist {
        let mut names = vec!["localhost".to_string()];
        let mut ips = Vec::new();
        let host = normalize_name(host_name);
        let host = host.strip_suffix(".local").unwrap_or(&host).to_string();
        if !host.is_empty() {
            names.push(format!("{host}.local"));
            // A fully qualified host name also answers to its first label.
            if let Some((short, _)) = host.split_once('.') {
                names.push(short.to_string());
                names.push(format!("{short}.local"));
            }
            names.push(host);
        }
        for entry in extra {
            match parse_host(entry) {
                Some(Host::Ip(ip)) => ips.push(ip),
                Some(Host::Name(n)) => names.push(n),
                None => tracing::warn!(entry, "--allow-host: not a host name or address; ignored"),
            }
        }
        names.sort();
        names.dedup();
        HostAllowlist {
            names,
            ips,
            interfaces: RwLock::new(Interfaces {
                ips: interface_ips(),
                read_at: Instant::now(),
            }),
            tailnet: RwLock::new(Tailnet::default()),
        }
    }

    /// The tailnet's MagicDNS name(s) and addresses while it runs; empty
    /// lists when it stops.
    pub fn set_tailnet(&self, dns_names: &[String], ips: &[IpAddr]) {
        let mut names = Vec::new();
        for n in dns_names {
            let n = normalize_name(n);
            if n.is_empty() {
                continue;
            }
            // MagicDNS search domains make the first label work too.
            if let Some((short, _)) = n.split_once('.') {
                names.push(short.to_string());
            }
            names.push(n);
        }
        let mut t = self.tailnet.write().unwrap_or_else(|p| p.into_inner());
        t.names = names;
        t.ips = ips.iter().copied().map(canonical).collect();
    }

    /// Whether a `Host` header value (or an authority, `host[:port]`)
    /// names this server.
    pub fn allows(&self, authority: &str) -> bool {
        match parse_host(authority) {
            Some(Host::Name(n)) => self.allows_name(&n),
            Some(Host::Ip(ip)) => self.allows_ip(ip),
            None => false,
        }
    }

    /// Whether an `Origin` header value (`scheme://host[:port]`) names this
    /// server. `null` and anything with a path never do.
    pub fn allows_origin(&self, origin: &str) -> bool {
        let Some((scheme, authority)) = origin.trim().split_once("://") else {
            return false;
        };
        !scheme.is_empty() && !authority.contains('/') && self.allows(authority)
    }

    fn allows_name(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
            || self
                .tailnet
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .names
                .iter()
                .any(|n| n == name)
    }

    fn allows_ip(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        if ip.is_loopback() || self.ips.contains(&ip) {
            return true;
        }
        if self
            .tailnet
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .ips
            .contains(&ip)
        {
            return true;
        }
        {
            let i = self.interfaces.read().unwrap_or_else(|p| p.into_inner());
            if i.ips.contains(&ip) {
                return true;
            }
            if i.read_at.elapsed() < INTERFACE_REFRESH {
                return false;
            }
        }
        // Addresses change (DHCP, Wi-Fi, VPN): look again, at most every few seconds.
        let fresh = interface_ips();
        let found = fresh.contains(&ip);
        *self.interfaces.write().unwrap_or_else(|p| p.into_inner()) = Interfaces {
            ips: fresh,
            read_at: Instant::now(),
        };
        found
    }

    /// Checks one request against rules 1 and 2 (see the module docs).
    #[allow(clippy::result_large_err)]
    pub fn check<B>(&self, req: &axum::http::Request<B>) -> Result<(), Response> {
        for value in req.headers().get_all(header::HOST) {
            match value.to_str() {
                Ok(host) if self.allows(host) => {}
                Ok(host) => return Err(misdirected(host)),
                Err(_) => return Err(misdirected("?")),
            }
        }
        if let Some(authority) = req.uri().authority() {
            if !self.allows(authority.as_str()) {
                return Err(misdirected(authority.as_str()));
            }
        }
        let unsafe_method = matches!(
            *req.method(),
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        );
        if unsafe_method && req.uri().path() == "/mcp" {
            // MCP clients send JSON-RPC as application/json, which a page
            // on another site can't send without a preflight flux never
            // grants (the X-Flux-Request header works too).
            let json = req
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(';').next())
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"));
            let marked = req
                .headers()
                .get(REQUEST_HEADER)
                .is_some_and(|v| v.as_bytes().trim_ascii() == b"1");
            if *req.method() == Method::POST && !json && !marked {
                return Err(ApiError::forbidden(
                    "MCP requests are JSON: send Content-Type: application/json",
                )
                .into_response());
            }
            if let Some(origin) = req.headers().get(header::ORIGIN) {
                if !origin.to_str().is_ok_and(|o| self.allows_origin(o)) {
                    return Err(
                        ApiError::forbidden("flux doesn't accept changes from pages on other sites")
                            .into_response(),
                    );
                }
            }
        }
        let admin = req.uri().path().starts_with("/api/v1/admin/");
        if (unsafe_method || admin) && req.uri().path().starts_with("/api/") {
            let marked = req
                .headers()
                .get(REQUEST_HEADER)
                .is_some_and(|v| v.as_bytes().trim_ascii() == b"1");
            if !marked {
                return Err(ApiError::forbidden(
                    "This request needs the header X-Flux-Request: 1 (flux's cross-site request guard)",
                )
                .into_response());
            }
            if let Some(origin) = req.headers().get(header::ORIGIN) {
                if !origin.to_str().is_ok_and(|o| self.allows_origin(o)) {
                    return Err(
                        ApiError::forbidden("flux doesn't accept changes from pages on other sites")
                            .into_response(),
                    );
                }
            }
        }
        Ok(())
    }
}

/// The router layer that applies [`HostAllowlist::check`] to every request.
pub async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    match st.hosts.check(&req) {
        Ok(()) => next.run(req).await,
        Err(refused) => refused,
    }
}

fn misdirected(host: &str) -> Response {
    // Echo a short, printable form of the name so the owner can fix it.
    let shown: String = host.chars().filter(|c| c.is_ascii_graphic()).take(100).collect();
    ApiError::new(
        StatusCode::MISDIRECTED_REQUEST,
        format!(
            "This flux server doesn't answer to the host name \"{shown}\". Open it as localhost, \
             by this machine's name or address, or allow the name with --allow-host."
        ),
    )
    .into_response()
}

fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

fn is_port(p: &str) -> bool {
    p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())
}

/// `host[:port]` → the host: an IP literal (`[v6]`, zone ids dropped) or a
/// lowercase name without a trailing dot. `None` if it isn't one.
fn parse_host(value: &str) -> Option<Host> {
    let v = value.trim();
    if v.is_empty() || v.contains(['@', '/', '?', '#', ' ', '\\']) {
        return None;
    }
    if let Some(rest) = v.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }
        let addr = inner.split('%').next()?;
        return addr.parse::<Ipv6Addr>().ok().map(|a| Host::Ip(IpAddr::V6(a)));
    }
    let host = match v.split_once(':') {
        Some((h, port)) if is_port(port) => h,
        Some(_) => return None,
        None => v,
    };
    let host = normalize_name(host);
    if host.is_empty() {
        return None;
    }
    Some(match host.parse::<Ipv4Addr>() {
        Ok(ip) => Host::Ip(IpAddr::V4(ip)),
        Err(_) => Host::Name(host),
    })
}

/// The addresses of this machine's interfaces (`getifaddrs`).
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn interface_ips() -> Vec<IpAddr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that we only read
    // and then free with freeifaddrs; every pointer is checked for null and
    // each sockaddr is read as the type its family says it is.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let addr = (*cur).ifa_addr;
            if !addr.is_null() {
                match i32::from((*addr).sa_family) {
                    libc::AF_INET => {
                        let sin = &*(addr as *const libc::sockaddr_in);
                        out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(addr as *const libc::sockaddr_in6);
                        out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out.sort();
    out.dedup();
    out
}

/// The addresses of this machine's adapters (`GetAdaptersAddresses`).
#[cfg(windows)]
pub fn interface_ips() -> Vec<IpAddr> {
    crate::win32::interface_ips()
}

#[cfg(not(any(unix, windows)))]
pub fn interface_ips() -> Vec<IpAddr> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_parse_with_and_without_ports() {
        let name = |s: &str| Some(Host::Name(s.into()));
        let ip = |s: &str| Some(Host::Ip(s.parse().unwrap()));
        assert_eq!(parse_host("localhost:8096"), name("localhost"));
        assert_eq!(parse_host("LocalHost."), name("localhost"));
        assert_eq!(parse_host("127.0.0.1"), ip("127.0.0.1"));
        assert_eq!(parse_host("[::1]:8096"), ip("::1"));
        assert_eq!(parse_host("[fe80::1%en0]:80"), ip("fe80::1"));
        assert_eq!(parse_host("host:"), name("host"));
        assert_eq!(parse_host("::1"), None, "IPv6 needs brackets");
        assert_eq!(parse_host("evil.example:80:80"), None);
        assert_eq!(parse_host("user@localhost"), None);
        assert_eq!(parse_host("localhost/x"), None);
        assert_eq!(parse_host("localhost:99999999"), None);
        assert_eq!(parse_host(""), None);
        assert_eq!(parse_host("[::1]x"), None);
    }

    #[test]
    fn the_allowlist_follows_the_contract() {
        let hosts = HostAllowlist::new(
            "Mac-Mini.local",
            &["media.example.com:443".into(), "203.0.113.7".into()],
        );
        for ok in [
            "localhost",
            "localhost:8096",
            "127.0.0.1:8096",
            "127.0.0.2",
            "[::1]:1",
            "[::ffff:127.0.0.1]",
            "mac-mini",
            "MAC-MINI.local:8096",
            "media.example.com",
            "203.0.113.7:8096",
        ] {
            assert!(hosts.allows(ok), "{ok} should be allowed");
        }
        for bad in [
            "evil.example",
            "evil.example:8096",
            "rebind.attacker.example:18421",
            "localhost.evil.example",
            "mac-mini.evil.example",
            "192.0.2.1",
            "0.0.0.0",
            "x",
            "",
        ] {
            assert!(!hosts.allows(bad), "{bad} should be refused");
        }

        // Interface addresses are ours. Windows listed none, so the
        // machine's own LAN address got 421.
        assert!(
            interface_ips().iter().any(|ip| ip.is_loopback()),
            "loopback is an interface: {:?}",
            interface_ips()
        );
        let iface = interface_ips().into_iter().find(|ip| !ip.is_loopback());
        if let Some(ip) = iface {
            let lit = match ip {
                IpAddr::V4(v4) => format!("{v4}:8096"),
                IpAddr::V6(v6) => format!("[{v6}]:8096"),
            };
            assert!(hosts.allows(&lit), "{lit}");
        }

        // The tailnet's names and addresses while it runs.
        assert!(!hosts.allows("flux.tail1234.ts.net"));
        hosts.set_tailnet(
            &["flux.tail1234.ts.net.".into()],
            &["100.64.0.7".parse().unwrap()],
        );
        assert!(hosts.allows("flux.tail1234.ts.net:8096"));
        assert!(hosts.allows("flux"));
        assert!(hosts.allows("100.64.0.7:8096"));
        hosts.set_tailnet(&[], &[]);
        assert!(!hosts.allows("flux.tail1234.ts.net"));
        assert!(!hosts.allows("100.64.0.7"));
    }

    #[test]
    fn origins_name_an_allowed_host() {
        let hosts = HostAllowlist::new("nas", &[]);
        assert!(hosts.allows_origin("http://localhost:5173"));
        assert!(hosts.allows_origin("http://127.0.0.1:8096"));
        assert!(hosts.allows_origin("https://nas.local"));
        assert!(!hosts.allows_origin("https://evil.example"));
        assert!(!hosts.allows_origin("null"));
        assert!(!hosts.allows_origin("http://localhost/path"));
        assert!(!hosts.allows_origin("localhost"));
    }

    fn req(method: Method, uri: &str, headers: &[(&str, &str)]) -> axum::http::Request<()> {
        let mut r = axum::http::Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        r.body(()).unwrap()
    }

    fn status(r: Result<(), Response>) -> u16 {
        r.map_or_else(|resp| resp.status().as_u16(), |()| 200)
    }

    #[test]
    fn requests_are_checked_for_host_marker_and_origin() {
        let hosts = HostAllowlist::new("nas", &[]);
        let local = ("host", "127.0.0.1:8096");
        let marked = (REQUEST_HEADER, "1");
        // Host.
        assert_eq!(
            status(hosts.check(&req(Method::GET, "/api/v1/server", &[local]))),
            200
        );
        assert_eq!(
            status(hosts.check(&req(Method::GET, "/api/v1/server", &[("host", "evil.example")]))),
            421
        );
        assert_eq!(
            status(hosts.check(&req(Method::GET, "http://evil.example/api/v1/server", &[]))),
            421,
            "absolute-form targets are checked too"
        );
        assert_eq!(
            status(hosts.check(&req(Method::GET, "/media/1/file", &[("host", "evil.example")]))),
            421
        );
        assert_eq!(
            status(hosts.check(&req(Method::GET, "/", &[]))),
            200,
            "no Host: not a browser"
        );
        // Unsafe API methods need the marker.
        for m in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                status(hosts.check(&req(m.clone(), "/api/v1/libraries", &[local]))),
                403,
                "{m}"
            );
            assert_eq!(
                status(hosts.check(&req(m.clone(), "/api/v1/libraries", &[local, marked]))),
                200,
                "{m}"
            );
        }
        assert_eq!(
            status(hosts.check(&req(
                Method::POST,
                "/api/v1/libraries",
                &[local, (REQUEST_HEADER, "0")]
            ))),
            403
        );
        // GETs (media, SSE, art) need nothing.
        assert_eq!(
            status(hosts.check(&req(Method::GET, "/api/v1/events", &[local]))),
            200
        );
        // Admin GETs do need it: the folder browser reads the folder it is
        // given, which an <img> on another site must not get to ask for.
        let browse = "/api/v1/admin/browse?path=%5C%5Cevil.example%5Cshare";
        assert_eq!(status(hosts.check(&req(Method::GET, browse, &[local]))), 403);
        assert_eq!(
            status(hosts.check(&req(Method::GET, browse, &[local, marked]))),
            200
        );
        assert_eq!(
            status(hosts.check(&req(
                Method::GET,
                "/api/v1/admin/devices",
                &[local, marked, ("origin", "https://evil.example")]
            ))),
            403
        );
        // A foreign Origin is refused even with the marker; an allowed one passes.
        let evil = ("origin", "https://evil.example");
        assert_eq!(
            status(hosts.check(&req(
                Method::POST,
                "/api/v1/libraries/1/scan",
                &[local, marked, evil]
            ))),
            403
        );
        assert_eq!(
            status(hosts.check(&req(
                Method::POST,
                "/api/v1/libraries/1/scan",
                &[local, marked, ("origin", "http://localhost:8096")]
            ))),
            200
        );
        assert_eq!(
            status(hosts.check(&req(
                Method::POST,
                "/api/v1/libraries/1/scan",
                &[local, marked, ("origin", "null")]
            ))),
            403
        );
    }
}
