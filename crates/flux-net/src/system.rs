//! "Use this machine's Tailscale": the system tailscaled.
//!
//! flux binds its tailnet listener to the machine's own Tailscale address and
//! asks tailscaled who each caller is. Both go through the `tailscale` CLI
//! (`status --json`, `whois --json`), which already knows how to reach
//! LocalAPI on every platform: the Unix socket on Linux, the macOS app's
//! authenticated localhost port, Windows named pipes. A per-IP TTL cache with
//! single-flight lookups keeps it to one process spawn per caller per TTL
//! (~50 ms measured on macOS); speaking LocalAPI directly would cut that to
//! about a millisecond and is a follow-up.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::conn::{canonical_ip, NetPath};
use crate::roles::PeerFacts;
use crate::NetError;

/// The CLI inside the macOS app bundle. It matches the running daemon's
/// version, unlike an older Homebrew CLI that prints a mismatch warning.
const MACOS_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

/// Bound on any one CLI call; tailscaled answers in milliseconds.
const CLI_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on `tailscale cert`: a new certificate takes a Let's Encrypt DNS
/// challenge, usually under a minute.
const CERT_TIMEOUT: Duration = Duration::from_secs(180);

/// Age after which a `status --json` snapshot is refreshed (in the
/// background: classification keeps using the previous one meanwhile).
const STATUS_TTL: Duration = Duration::from_secs(10);

/// Bound on cached WhoIs entries (a reset, not LRU: entries are tiny and a
/// reset costs one extra lookup per live caller).
const MAX_WHOIS_ENTRIES: usize = 4096;

/// Locate the `tailscale` CLI: an explicit path, `FLUX_TAILSCALE_CLI`, the
/// macOS app bundle, then `tailscale` on `PATH`.
pub fn find_cli(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = std::env::var_os("FLUX_TAILSCALE_CLI") {
        return PathBuf::from(path);
    }
    if cfg!(target_os = "macos") && Path::new(MACOS_APP_CLI).exists() {
        return PathBuf::from(MACOS_APP_CLI);
    }
    PathBuf::from("tailscale")
}

/// The parts of `tailscale status --json` flux uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemStatus {
    /// `Running`, `NeedsLogin`, `Stopped`, …
    pub backend_state: String,
    /// This machine's tailnet addresses (IPv4 first).
    pub self_ips: Vec<IpAddr>,
    /// This machine's MagicDNS name, no trailing dot.
    pub dns_name: Option<String>,
    /// The login that owns this machine's node (the flux Owner by default).
    pub owner_login: Option<String>,
    /// The login URL when the daemon waits for one.
    pub auth_url: Option<String>,
    /// Per-peer path, by every tailnet IP of each peer.
    pub peer_paths: HashMap<IpAddr, NetPath>,
    /// Names this machine can get an HTTPS certificate for (`tailscale
    /// cert`); empty when HTTPS certificates are off for the tailnet.
    pub cert_domains: Vec<String>,
}

impl SystemStatus {
    /// True when tailscaled is up and logged in.
    pub fn is_running(&self) -> bool {
        self.backend_state == "Running"
    }

    /// Path to the peer that owns `ip`. Our own address is direct (a
    /// same-host request over the tailnet interface); an unknown address
    /// is [`NetPath::Tailnet`].
    pub fn path_to(&self, ip: IpAddr) -> NetPath {
        let ip = canonical_ip(ip);
        if self.self_ips.contains(&ip) {
            return NetPath::TailnetDirect;
        }
        self.peer_paths.get(&ip).copied().unwrap_or(NetPath::Tailnet)
    }
}

// ── `tailscale status --json` (a subset of ipnstate.Status) ─────────────────

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawStatus {
    #[serde(default)]
    backend_state: String,
    #[serde(default, rename = "AuthURL")]
    auth_url: String,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Vec<IpAddr>,
    #[serde(default, rename = "Self")]
    self_: Option<RawPeer>,
    #[serde(default)]
    peer: HashMap<String, RawPeer>,
    #[serde(default)]
    user: HashMap<String, RawUser>,
    #[serde(default)]
    cert_domains: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawPeer {
    #[serde(default, rename = "DNSName")]
    dns_name: String,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Vec<IpAddr>,
    #[serde(default, rename = "UserID")]
    user_id: i64,
    /// Direct UDP endpoint in use; empty when traffic goes via a relay.
    #[serde(default)]
    cur_addr: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawUser {
    #[serde(default)]
    login_name: String,
}

fn non_empty(s: String) -> Option<String> {
    let s = s.trim_end_matches('.').to_string();
    (!s.is_empty()).then_some(s)
}

/// Parse `tailscale status --json` output.
pub fn parse_status(json: &[u8]) -> Result<SystemStatus, NetError> {
    let raw: RawStatus =
        serde_json::from_slice(json).map_err(|e| NetError::System(format!("status JSON: {e}")))?;
    let mut self_ips = raw.tailscale_ips;
    self_ips.sort_by_key(|ip| ip.is_ipv6());
    let (dns_name, owner_login) = match raw.self_ {
        Some(me) => {
            let owner = raw
                .user
                .get(&me.user_id.to_string())
                .map(|u| u.login_name.clone());
            (non_empty(me.dns_name), owner.and_then(non_empty))
        }
        None => (None, None),
    };
    let mut peer_paths = HashMap::new();
    for peer in raw.peer.into_values() {
        // A peer with a direct UDP endpoint in use is direct; otherwise its
        // traffic goes through a peer relay or DERP. (Its home DERP region,
        // `Relay`, is set either way and says nothing about the path.)
        let path = if peer.cur_addr.is_empty() {
            NetPath::TailnetRelay
        } else {
            NetPath::TailnetDirect
        };
        for ip in peer.tailscale_ips {
            peer_paths.insert(ip, path);
        }
    }
    Ok(SystemStatus {
        backend_state: raw.backend_state,
        self_ips,
        dns_name,
        owner_login,
        auth_url: non_empty(raw.auth_url),
        peer_paths,
        cert_domains: raw
            .cert_domains
            .unwrap_or_default()
            .into_iter()
            .filter_map(non_empty)
            .collect(),
    })
}

/// The domain to get a certificate for: this machine's MagicDNS name, if
/// the tailnet issues certificates for it.
pub fn cert_domain(status: &SystemStatus) -> Result<String, String> {
    if status.cert_domains.is_empty() {
        return Err(
            "HTTPS certificates are off for this tailnet. Turn them on in the Tailscale admin \
                    console (DNS, HTTPS Certificates), and flux will pick them up."
                .into(),
        );
    }
    match &status.dns_name {
        Some(name) if status.cert_domains.iter().any(|d| d.eq_ignore_ascii_case(name)) => Ok(name.clone()),
        _ => Ok(status.cert_domains[0].clone()),
    }
}

// ── `tailscale whois --json` (apitype.WhoIsResponse) ─────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawWhois {
    #[serde(default)]
    node: Option<RawNode>,
    #[serde(default)]
    user_profile: Option<RawProfile>,
    #[serde(default)]
    cap_map: Option<HashMap<String, Vec<serde_json::Value>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawNode {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "StableID")]
    stable_id: String,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawProfile {
    #[serde(default, rename = "ID")]
    id: i64,
    #[serde(default)]
    login_name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default, rename = "ProfilePicURL")]
    profile_pic_url: String,
}

/// Parse `tailscale whois --json` output into [`PeerFacts`].
pub fn parse_whois(json: &[u8]) -> Result<PeerFacts, NetError> {
    let raw: RawWhois =
        serde_json::from_slice(json).map_err(|e| NetError::System(format!("whois JSON: {e}")))?;
    let mut facts = PeerFacts::default();
    if let Some(node) = raw.node {
        facts.node_name = non_empty(node.name);
        facts.node_id = non_empty(node.stable_id);
        facts.tags = node.tags.unwrap_or_default();
    }
    if let Some(profile) = raw.user_profile {
        facts.user_id = (profile.id != 0).then_some(profile.id);
        facts.login_name = non_empty(profile.login_name);
        facts.display_name = non_empty(profile.display_name);
        facts.profile_pic_url = non_empty(profile.profile_pic_url);
    }
    facts.cap_map = raw.cap_map.unwrap_or_default();
    Ok(facts)
}

// ── The client ──────────────────────────────────────────────────────────────

type WhoisCell = Arc<OnceCell<Option<PeerFacts>>>;

/// The system tailscaled, through its CLI, with cached lookups.
#[derive(Debug)]
pub struct SystemTailscale {
    cli: PathBuf,
    whois_ttl: Duration,
    whois_cache: Mutex<HashMap<IpAddr, (Instant, WhoisCell)>>,
    status_cache: Mutex<Option<(Instant, Arc<SystemStatus>)>>,
    status_refreshing: AtomicBool,
}

impl SystemTailscale {
    /// A client for the CLI at `cli` caching WhoIs answers for `whois_ttl`.
    pub fn new(cli: PathBuf, whois_ttl: Duration) -> Self {
        SystemTailscale {
            cli,
            whois_ttl,
            whois_cache: Mutex::new(HashMap::new()),
            status_cache: Mutex::new(None),
            status_refreshing: AtomicBool::new(false),
        }
    }

    /// The CLI this client runs.
    pub fn cli(&self) -> &Path {
        &self.cli
    }

    async fn run(&self, args: &[&str]) -> Result<std::process::Output, NetError> {
        self.run_for(args, CLI_TIMEOUT).await
    }

    async fn run_for(&self, args: &[&str], timeout: Duration) -> Result<std::process::Output, NetError> {
        let child = Command::from(flux_core::tool::command(&self.cli))
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        tokio::time::timeout(timeout, child)
            .await
            .map_err(|_| NetError::System(format!("`{} {}` timed out", self.cli.display(), args.join(" "))))?
            .map_err(|e| NetError::System(format!("cannot run {}: {e}", self.cli.display())))
    }

    /// The HTTPS certificate and key for `domain`, as PEM (`tailscale cert
    /// --cert-file - --key-file -`). tailscaled keeps the certificate and
    /// renews it before it expires; the first one takes a DNS challenge.
    pub async fn cert(&self, domain: &str) -> Result<Vec<u8>, NetError> {
        let out = self
            .run_for(
                &["cert", "--cert-file", "-", "--key-file", "-", domain],
                CERT_TIMEOUT,
            )
            .await?;
        if out.status.success() && !out.stdout.is_empty() {
            return Ok(out.stdout);
        }
        Err(NetError::System(format!(
            "tailscale cert {domain} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }

    /// A fresh `tailscale status --json`.
    pub async fn status(&self) -> Result<SystemStatus, NetError> {
        let out = self.run(&["status", "--json"]).await?;
        // `status` exits non-zero when stopped/logged out but still prints
        // JSON; only a missing body is an error.
        if out.stdout.is_empty() {
            return Err(NetError::System(format!(
                "tailscale status failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        parse_status(&out.stdout)
    }

    /// Seed the snapshot [`cached_status`](Self::cached_status) serves.
    pub fn prime_status(&self, status: SystemStatus) {
        if let Ok(mut slot) = self.status_cache.lock() {
            *slot = Some((Instant::now(), Arc::new(status)));
        }
    }

    /// The latest `status` snapshot, for path classification per connection.
    /// Stale-while-revalidate: once a snapshot exists it is returned at once
    /// and, if older than 10 s, refreshed by one background task, so a new
    /// connection never waits on the CLI for its path. Only the very first
    /// call (no snapshot yet) runs the CLI inline.
    pub async fn cached_status(self: &Arc<Self>) -> Result<Arc<SystemStatus>, NetError> {
        let current = self.status_cache.lock().ok().and_then(|slot| slot.clone());
        match current {
            Some((at, status)) => {
                if at.elapsed() >= STATUS_TTL && !self.status_refreshing.swap(true, Ordering::AcqRel) {
                    let this = self.clone();
                    tokio::spawn(async move {
                        match this.status().await {
                            Ok(fresh) => this.prime_status(fresh),
                            Err(e) => tracing::debug!(error = %e, "status refresh failed"),
                        }
                        this.status_refreshing.store(false, Ordering::Release);
                    });
                }
                Ok(status)
            }
            None => {
                let status = self.status().await?;
                self.prime_status(status);
                self.status_cache
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|(_, s)| s.clone()))
                    .ok_or_else(|| NetError::System("status cache poisoned".into()))
            }
        }
    }

    /// Who owns tailnet address `ip`. `Ok(None)` means tailscaled knows no
    /// such peer (anonymous). Answers, including "nobody", are cached for
    /// the TTL and concurrent lookups for one IP share a single CLI call;
    /// failures are not cached, so a retry works once the daemon recovers.
    pub async fn whois(&self, ip: IpAddr) -> Result<Option<PeerFacts>, NetError> {
        let ip = canonical_ip(ip);
        let cell = {
            let mut cache = self
                .whois_cache
                .lock()
                .map_err(|_| NetError::System("whois cache poisoned".into()))?;
            let fresh = cache.get(&ip).filter(|(at, _)| at.elapsed() < self.whois_ttl);
            match fresh {
                Some((_, cell)) => cell.clone(),
                None => {
                    if cache.len() >= MAX_WHOIS_ENTRIES {
                        cache.clear();
                    }
                    let cell = WhoisCell::default();
                    cache.insert(ip, (Instant::now(), cell.clone()));
                    cell
                }
            }
        };
        let result = cell.get_or_try_init(|| self.whois_uncached(ip)).await.cloned();
        if result.is_err() {
            // Forget the failed slot so the next caller retries.
            if let Ok(mut cache) = self.whois_cache.lock() {
                if cache.get(&ip).is_some_and(|(_, c)| Arc::ptr_eq(c, &cell)) {
                    cache.remove(&ip);
                }
            }
        }
        result
    }

    async fn whois_uncached(&self, ip: IpAddr) -> Result<Option<PeerFacts>, NetError> {
        let out = self.run(&["whois", "--json", &ip.to_string()]).await?;
        if out.status.success() {
            return parse_whois(&out.stdout).map(Some);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("not found") || stderr.contains("no match") {
            Ok(None)
        } else {
            Err(NetError::System(format!(
                "tailscale whois {ip} failed: {}",
                stderr.trim()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "Version": "1.104.1", "BackendState": "Running", "AuthURL": "",
      "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.64.0.1"],
      "Self": {"ID": "nSELF", "HostName": "mac", "DNSName": "mac.tail1234.ts.net.",
               "TailscaleIPs": ["100.64.0.1"], "UserID": 1},
      "Peer": {
        "nodekey:a": {"DNSName": "tv.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.5", "fd7a:115c:a1e0::5"],
                      "CurAddr": "192.168.1.20:41641", "Relay": "fra", "UserID": 2},
        "nodekey:b": {"DNSName": "phone.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.6"],
                      "CurAddr": "", "Relay": "fra", "PeerRelay": "", "UserID": 2}
      },
      "User": {"1": {"ID": 1, "LoginName": "owner@example.com"},
               "2": {"ID": 2, "LoginName": "friend@example.com"}},
      "CertDomains": ["mac.tail1234.ts.net"]
    }"#;

    #[test]
    fn status_yields_addresses_owner_and_paths() {
        let s = parse_status(STATUS.as_bytes()).unwrap();
        assert!(s.is_running());
        assert_eq!(
            s.self_ips[0],
            "100.64.0.1".parse::<IpAddr>().unwrap(),
            "IPv4 first"
        );
        assert_eq!(s.dns_name.as_deref(), Some("mac.tail1234.ts.net"));
        assert_eq!(s.owner_login.as_deref(), Some("owner@example.com"));
        assert_eq!(s.auth_url, None);
        assert_eq!(s.path_to("100.64.0.5".parse().unwrap()), NetPath::TailnetDirect);
        assert_eq!(
            s.path_to("fd7a:115c:a1e0::5".parse().unwrap()),
            NetPath::TailnetDirect
        );
        assert_eq!(s.path_to("100.64.0.6".parse().unwrap()), NetPath::TailnetRelay);
        assert_eq!(s.path_to("100.64.0.1".parse().unwrap()), NetPath::TailnetDirect);
        assert_eq!(s.path_to("100.64.9.9".parse().unwrap()), NetPath::Tailnet);
    }

    #[test]
    fn certificates_are_for_this_machines_name() {
        let s = parse_status(STATUS.as_bytes()).unwrap();
        assert_eq!(s.cert_domains, vec!["mac.tail1234.ts.net"]);
        assert_eq!(cert_domain(&s).unwrap(), "mac.tail1234.ts.net");
        // HTTPS certificates off for the tailnet: `CertDomains` is null.
        let off = parse_status(
            br#"{"BackendState":"Running","Self":{"DNSName":"mac.ts.net."},"CertDomains":null}"#,
        )
        .unwrap();
        assert!(off.cert_domains.is_empty());
        assert!(cert_domain(&off).unwrap_err().contains("admin console"));
    }

    #[test]
    fn logged_out_status_reports_the_login_url() {
        let s = parse_status(br#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/x"}"#)
            .unwrap();
        assert!(!s.is_running());
        assert_eq!(s.auth_url.as_deref(), Some("https://login.tailscale.com/a/x"));
        assert!(s.self_ips.is_empty());
    }

    #[test]
    fn whois_maps_profile_tags_and_caps() {
        let json = br#"{
          "Node": {"Name": "tv.tail1234.ts.net.", "StableID": "nTV", "Tags": ["tag:tv"]},
          "UserProfile": {"ID": 99, "LoginName": "tagged-devices", "DisplayName": "Tagged Devices"},
          "CapMap": {"vibecook.dev/cap/flux": [{"role": "kid"}]}
        }"#;
        let f = parse_whois(json).unwrap();
        assert_eq!(f.node_name.as_deref(), Some("tv.tail1234.ts.net"));
        assert_eq!(f.node_id.as_deref(), Some("nTV"));
        assert_eq!(f.tags, vec!["tag:tv"]);
        assert_eq!(f.user_id, Some(99));
        assert_eq!(f.grants("vibecook.dev/cap/flux")[0]["role"], "kid");

        let plain = parse_whois(
            br#"{"Node":{"Name":"mac.ts.net.","StableID":"n1"},"UserProfile":{"ID":1,"LoginName":"a@b","DisplayName":"A"},"CapMap":null}"#,
        )
        .unwrap();
        assert!(plain.cap_map.is_empty() && plain.tags.is_empty());
        assert_eq!(plain.login_name.as_deref(), Some("a@b"));
    }

    #[test]
    fn explicit_cli_wins() {
        assert_eq!(find_cli(Some(Path::new("/opt/ts"))), PathBuf::from("/opt/ts"));
    }

    /// A CLI stand-in that counts its invocations, to check caching and
    /// single-flight without a tailscaled.
    #[cfg(unix)]
    #[tokio::test]
    async fn whois_is_cached_single_flight_and_misses_are_anonymous() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("flux-net-fakecli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls");
        let cli = dir.join("tailscale");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\nsleep 0.2\ncase \"$3\" in\n  100.64.0.5) echo '{{\"Node\":{{\"Name\":\"tv.ts.net.\"}},\"UserProfile\":{{\"ID\":7,\"LoginName\":\"a@b\"}}}}' ;;\n  *) echo 'peer not found' >&2; exit 1 ;;\nesac\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_file(&log);

        let ts = Arc::new(SystemTailscale::new(cli, Duration::from_secs(60)));
        let ip: IpAddr = "100.64.0.5".parse().unwrap();
        let lookups: Vec<_> = (0..8)
            .map(|_| {
                let ts = ts.clone();
                tokio::spawn(async move { ts.whois(ip).await })
            })
            .collect();
        for l in lookups {
            let facts = l.await.unwrap().unwrap().expect("known peer");
            assert_eq!(facts.login_name.as_deref(), Some("a@b"));
        }
        assert!(ts.whois(ip).await.unwrap().is_some(), "cached");
        assert_eq!(
            ts.whois("100.64.9.9".parse().unwrap()).await.unwrap(),
            None,
            "miss = anonymous"
        );
        assert_eq!(ts.whois("100.64.9.9".parse().unwrap()).await.unwrap(), None);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls.lines().count(),
            2,
            "one CLI call per IP within the TTL: {calls}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
