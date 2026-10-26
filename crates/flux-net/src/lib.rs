//! Networking for flux: serve one `axum::Router` on localhost, the LAN and
//! the tailnet, and tell every handler who is calling.
//!
//! ```text
//!   127.0.0.1:8096 ─┐                       ┌─ Extension<Option<Identity>>
//!   LAN (optional) ─┼─ hyper http1 ─────────┤   (WhoIs → role)
//!   tailnet ────────┘  half_close(true)     └─ Extension<ConnInfo>
//!      ├ embedded: flux's own node via Truffle (listen_tcp, bridge identity)
//!      └ system:   this machine's tailscaled (bind 100.x, `tailscale whois`;
//!                  optionally HTTPS on the same port, see [`tls`])
//! ```
//!
//! - **Localhost** callers get [`RolePolicy::localhost_identity`] (Owner by
//!   default); **LAN** callers get no identity (`None`).
//! - **Tailnet** callers get the identity WhoIs reports, with a role from
//!   tailnet grants or local policy ([`RolePolicy`]).
//! - Every connection is served with hyper `http1::Builder::half_close(true)`
//!   (see [`serve`]); `axum::serve` can't set it.
//! - Connections are admitted within [`ServeLimits`] (per remote IP and in
//!   total), and a response the client stops reading is aborted after the
//!   write timeout ([`limits`]).
//!
//! ```no_run
//! # async fn demo(router: axum::Router) -> Result<(), flux_net::NetError> {
//! use flux_net::{FluxNet, FluxNetConfig, TailnetConfig, SystemConfig};
//! let net = FluxNet::start(
//!     FluxNetConfig { tailnet: TailnetConfig::System(SystemConfig::default()), ..Default::default() },
//!     router,
//! ).await?;
//! let mut status = net.tailnet_status();
//! status.changed().await.ok();
//! println!("{:?}", *status.borrow());
//! net.shutdown(std::time::Duration::from_secs(5)).await;
//! # Ok(()) }
//! ```

pub mod conn;
#[cfg(feature = "embedded")]
pub mod embedded;
pub mod limits;
pub mod roles;
pub mod serve;
pub mod system;
#[cfg(all(test, feature = "embedded"))]
mod test_util;
pub mod tls;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::Duration;

use axum::Router;
use flux_core::api::Identity;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub use conn::{is_tailscale_ip, ConnInfo, NetPath};
#[cfg(feature = "embedded")]
pub use embedded::{resolve_sidecar, EmbeddedConfig};
pub use limits::ServeLimits;
pub use roles::{PeerFacts, RolePolicy, DEFAULT_CAPABILITY};
pub use system::{SystemStatus, SystemTailscale};

/// The default flux port (Jellyfin's, so existing muscle memory works).
pub const DEFAULT_PORT: u16 = 8096;

/// Errors starting or running the network layer.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// A listener could not bind.
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    /// The system tailscaled could not be used.
    #[error("system Tailscale: {0}")]
    System(String),
    /// flux's own Truffle node failed.
    #[error("embedded Tailscale: {0}")]
    Embedded(String),
    /// The Truffle Go sidecar binary is missing.
    #[error("Truffle sidecar not found: {0}")]
    SidecarNotFound(String),
}

/// Where flux listens and how it reaches the tailnet.
#[derive(Debug, Clone)]
pub struct FluxNetConfig {
    /// The local listener; `127.0.0.1:8096` by default.
    pub local_addr: SocketAddr,
    /// An additional LAN listener, e.g. `0.0.0.0:8096` (then `local_addr`
    /// should use another port, or be set to the same wildcard and this left
    /// `None`). LAN callers have no identity.
    pub lan_addr: Option<SocketAddr>,
    /// The tailnet listener.
    pub tailnet: TailnetConfig,
    /// Caller → role mapping.
    pub roles: RolePolicy,
    /// Connection admission and response write timeout, shared by every
    /// listener (tailnet included).
    pub limits: ServeLimits,
}

impl Default for FluxNetConfig {
    fn default() -> Self {
        FluxNetConfig {
            local_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
            lan_addr: None,
            tailnet: TailnetConfig::Off,
            roles: RolePolicy::default(),
            limits: ServeLimits::default(),
        }
    }
}

/// How flux reaches the tailnet.
#[derive(Debug, Clone)]
pub enum TailnetConfig {
    /// No tailnet listener.
    Off,
    /// flux's own node through Truffle ("Connect with Tailscale").
    #[cfg(feature = "embedded")]
    Embedded(EmbeddedConfig),
    /// This machine's tailscaled ("use this machine's Tailscale").
    System(SystemConfig),
}

/// Settings for "use this machine's Tailscale".
#[derive(Debug, Clone)]
pub struct SystemConfig {
    /// Port on the machine's Tailscale addresses.
    pub port: u16,
    /// The `tailscale` CLI; `None` finds it ([`system::find_cli`]).
    pub cli: Option<PathBuf>,
    /// How long a WhoIs answer is reused per caller IP.
    pub whois_ttl: Duration,
    /// How often to look again while tailscaled is not running.
    pub retry_every: Duration,
    /// Also serve HTTPS on the port, with this machine's MagicDNS
    /// certificate from tailscaled ([`tls`]; needs HTTPS certificates
    /// enabled for the tailnet). Plain HTTP keeps working.
    pub https: bool,
}

impl Default for SystemConfig {
    fn default() -> Self {
        SystemConfig {
            port: DEFAULT_PORT,
            cli: None,
            whois_ttl: Duration::from_secs(60),
            retry_every: Duration::from_secs(15),
            https: false,
        }
    }
}

/// First wait before asking tailscaled for the certificate again after a
/// failure; doubled each time up to [`CERT_RETRY_MAX`].
const CERT_RETRY_MIN: Duration = Duration::from_secs(30);
const CERT_RETRY_MAX: Duration = Duration::from_secs(15 * 60);

/// Which Tailscale serves the tailnet listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailnetMode {
    Embedded,
    System,
}

impl From<TailnetMode> for flux_core::api::TailnetMode {
    fn from(m: TailnetMode) -> Self {
        match m {
            TailnetMode::Embedded => flux_core::api::TailnetMode::Embedded,
            TailnetMode::System => flux_core::api::TailnetMode::System,
        }
    }
}

impl TailnetConfig {
    /// The configured mode, as the API reports it.
    pub fn api_mode(&self) -> flux_core::api::TailnetMode {
        match self {
            TailnetConfig::Off => flux_core::api::TailnetMode::Off,
            #[cfg(feature = "embedded")]
            TailnetConfig::Embedded(_) => flux_core::api::TailnetMode::Embedded,
            TailnetConfig::System(_) => flux_core::api::TailnetMode::System,
        }
    }
}

/// Tailscale's admin-console page where a device waiting for approval is
/// approved ([`TailnetStatus::NeedsApproval`]). Tailscale SaaS only; a
/// Headscale control server has no such page (the embedded node learns
/// which one it talks to from Truffle's `ControlServerKnown`).
pub const APPROVE_URL: &str = "https://login.tailscale.com/admin/machines";

/// The tailnet listener once it is up.
#[derive(Debug, Clone, PartialEq)]
pub struct TailnetInfo {
    pub mode: TailnetMode,
    /// The server's MagicDNS name, e.g. `truffle-flux-mac-mini.tail1234.ts.net`.
    pub dns_name: Option<String>,
    /// Tailnet addresses being served.
    pub addrs: Vec<SocketAddr>,
    /// Served over HTTPS: embedded mode with `tls` (HTTPS only), or system
    /// mode with `https` once the certificate is loaded (HTTP too).
    pub tls: bool,
    /// Why HTTPS was asked for but isn't serving (yet), e.g. the tailnet
    /// issues no certificates.
    pub tls_error: Option<String>,
    /// The login that owns the node (the flux Owner by default).
    pub owner_login: Option<String>,
    /// When the node's key expires (Unix ms); `None` when expiry is
    /// disabled for it, or unknown (system mode, a pre-10 Truffle sidecar).
    pub key_expiry: Option<i64>,
    /// The custom control server the node logs in to (Headscale), by URL;
    /// `None` for Tailscale's hosted service or when unknown.
    pub control_server: Option<String>,
}

impl TailnetInfo {
    /// The URL to show in the UI: MagicDNS name if known, else the first address.
    pub fn url(&self) -> Option<String> {
        let scheme = if self.tls { "https" } else { "http" };
        let port = self.addrs.first().map(SocketAddr::port)?;
        let host = match &self.dns_name {
            Some(name) => name.clone(),
            None => match self.addrs.first()?.ip() {
                IpAddr::V6(v6) => format!("[{v6}]"),
                v4 => v4.to_string(),
            },
        };
        let default_port = if self.tls { 443 } else { 80 };
        Some(if port == default_port {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{port}")
        })
    }
}

/// State of the tailnet listener, for the server UI ("Connect with Tailscale").
#[derive(Debug, Clone, PartialEq)]
pub enum TailnetStatus {
    /// No tailnet configured.
    Off,
    /// Starting the node / probing tailscaled.
    Starting,
    /// Show this URL (and a QR code of it) so the owner can log flux in.
    /// Replaced by a newer one if Tailscale rotates it.
    NeedsLogin { auth_url: String },
    /// The tailnet requires device approval: "approve flux in the admin
    /// console". `approve_url` is that console's Machines page ([`APPROVE_URL`])
    /// on Tailscale's hosted service; `None` on a custom control server
    /// (Headscale), which has none.
    NeedsApproval { approve_url: Option<String> },
    /// Serving on the tailnet.
    Running(TailnetInfo),
    /// System mode: tailscaled is missing, stopped or logged out; flux keeps
    /// checking every `retry_every`.
    Unavailable { reason: String },
    /// Gave up; the error says why.
    Failed { error: String },
}

impl TailnetStatus {
    /// The wire DTO (`GET /api/v1/tailnet`, `Event::Tailnet`) for a listener
    /// configured in `mode` (a running node reports its own mode).
    ///
    /// - `tailnet_name` is the MagicDNS suffix of `dns_name`
    ///   (`flux.tail1234.ts.net` → `tail1234.ts.net`).
    /// - `approve_url` is the status's while approval is pending: [`APPROVE_URL`]
    ///   on Tailscale's hosted service, none on Headscale.
    /// - `system_tailscale` is `true` in system mode; in the other modes it is
    ///   `false` here, and the server may set it after probing
    ///   ([`SystemTailscale::status`]).
    /// - `key_expiry` is the running node's (embedded mode, from Truffle);
    ///   `tailscale status` isn't read for it in system mode.
    pub fn to_api(&self, mode: flux_core::api::TailnetMode) -> flux_core::api::TailnetInfo {
        use flux_core::api::{TailnetInfo as Info, TailnetMode as Mode, TailnetState as State};
        let mut info = Info {
            mode,
            state: State::Off,
            auth_url: None,
            approve_url: None,
            tailnet_name: None,
            dns_name: None,
            url: None,
            owner_login: None,
            key_expiry: None,
            system_tailscale: mode == Mode::System,
            last_error: None,
            https: false,
            https_error: None,
        };
        match self {
            TailnetStatus::Off => {}
            TailnetStatus::Starting => info.state = State::Starting,
            TailnetStatus::NeedsLogin { auth_url } => {
                info.state = State::NeedsLogin;
                info.auth_url = Some(auth_url.clone());
            }
            TailnetStatus::NeedsApproval { approve_url } => {
                info.state = State::NeedsApproval;
                info.approve_url = approve_url.clone();
            }
            TailnetStatus::Running(t) => {
                info.state = State::Running;
                info.mode = t.mode.into();
                info.system_tailscale |= t.mode == TailnetMode::System;
                info.url = t.url();
                // Both Tailscales report the name without the trailing dot.
                info.tailnet_name = t
                    .dns_name
                    .as_deref()
                    .and_then(|n| n.split_once('.'))
                    .map(|(_, suffix)| suffix.to_string())
                    .filter(|s| s.contains('.'));
                info.dns_name = t.dns_name.clone();
                info.owner_login = t.owner_login.clone();
                info.key_expiry = t.key_expiry;
                info.https = t.tls;
                info.https_error = t.tls_error.clone();
            }
            TailnetStatus::Unavailable { reason } => {
                info.state = State::Unavailable;
                info.last_error = Some(reason.clone());
            }
            TailnetStatus::Failed { error } => {
                info.state = State::Error;
                info.last_error = Some(error.clone());
            }
        }
        info
    }
}

/// A group of listener and connection tasks that stop together (`token`)
/// and are awaited together (`tracker`).
#[derive(Clone)]
pub(crate) struct Scope {
    pub(crate) token: CancellationToken,
    pub(crate) tracker: TaskTracker,
}

impl Scope {
    /// A scope that also stops when `parent` does.
    fn child_of(parent: &CancellationToken) -> Scope {
        Scope {
            token: parent.child_token(),
            tracker: TaskTracker::new(),
        }
    }
}

/// State shared by all listeners and connection tasks.
pub(crate) struct Shared {
    router: Router,
    policy: RwLock<RolePolicy>,
    /// The owner login was configured explicitly and must not be replaced by
    /// what the tailnet reports.
    owner_fixed: bool,
    status: watch::Sender<TailnetStatus>,
    /// Stops everything (and, as their parent, the tailnet scopes).
    shutdown: CancellationToken,
    /// The local and LAN listeners and their connections.
    tracker: TaskTracker,
    /// The tailnet listener and its connections: a scope of its own, so
    /// [`FluxNet::set_tailnet`] can replace it alone.
    tailnet: Mutex<Scope>,
    /// Set once system mode is running: classifies LAN-listener connections
    /// that arrive on our Tailscale address (wildcard binds).
    system: RwLock<Option<SystemBinding>>,
    /// Open connections per remote IP and in total.
    admission: Arc<limits::Admission>,
    /// See [`ServeLimits::write_timeout`].
    write_timeout: Option<Duration>,
    /// Bumped whenever the tailnet listener is replaced: a task of the old
    /// one (a certificate fetch in flight) can't change the new one's state.
    tailnet_gen: AtomicU64,
    /// HTTPS for new connections (system mode), for the tailnet generation
    /// that set it up.
    tls: RwLock<Option<(u64, TlsAcceptor)>>,
}

#[derive(Clone)]
struct SystemBinding {
    client: Arc<SystemTailscale>,
    self_ips: Vec<IpAddr>,
    dns_name: Option<String>,
}

impl Shared {
    /// A bare `Shared` for unit tests of listeners.
    #[cfg(all(test, feature = "embedded"))]
    pub(crate) fn for_test(router: Router, policy: RolePolicy) -> Arc<Shared> {
        let shutdown = CancellationToken::new();
        Arc::new(Shared {
            router,
            owner_fixed: policy.owner_login.is_some(),
            policy: RwLock::new(policy),
            status: watch::channel(TailnetStatus::Off).0,
            tailnet: Mutex::new(Scope::child_of(&shutdown)),
            shutdown,
            tracker: TaskTracker::new(),
            system: RwLock::new(None),
            admission: limits::Admission::new(ServeLimits::default()),
            write_timeout: ServeLimits::default().write_timeout,
            tailnet_gen: AtomicU64::new(0),
            tls: RwLock::new(None),
        })
    }

    /// The local/LAN scope.
    fn main_scope(&self) -> Scope {
        Scope {
            token: self.shutdown.clone(),
            tracker: self.tracker.clone(),
        }
    }

    /// The current tailnet scope.
    pub(crate) fn tailnet_scope(&self) -> Scope {
        self.tailnet.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn policy(&self) -> RwLockReadGuard<'_, RolePolicy> {
        // A poisoned lock only means a panic elsewhere mid-update of a plain
        // value; the data is still a valid policy.
        self.policy.read().unwrap_or_else(|p| p.into_inner())
    }

    fn owner_login(&self) -> Option<String> {
        self.policy().owner_login.clone()
    }

    /// Record the node owner's login reported by Tailscale, unless an owner
    /// was configured explicitly.
    fn learn_owner(&self, login: &str) {
        if self.owner_fixed || login.is_empty() {
            return;
        }
        let mut policy = self.policy.write().unwrap_or_else(|p| p.into_inner());
        if policy.owner_login.as_deref() != Some(login) {
            tracing::info!(
                owner = login,
                "flux owner is the tailnet login that owns this node"
            );
            policy.owner_login = Some(login.to_string());
        }
    }

    fn set_status(&self, status: TailnetStatus) {
        self.status.send_replace(status);
    }

    /// Changes the running tailnet's info in place (no-op in other states);
    /// subscribers hear of it only if something changed.
    #[cfg_attr(not(feature = "embedded"), allow(dead_code))]
    fn update_running(&self, f: impl FnOnce(&mut TailnetInfo)) {
        self.status.send_if_modified(|s| match s {
            TailnetStatus::Running(t) => {
                let before = t.clone();
                f(t);
                *t != before
            }
            _ => false,
        });
    }

    /// The current tailnet listener's generation.
    fn tailnet_gen(&self) -> u64 {
        self.tailnet_gen.load(Ordering::SeqCst)
    }

    /// [`update_running`](Self::update_running), only while the tailnet
    /// listener of generation `gen` is the current one.
    fn update_running_for(&self, gen: u64, f: impl FnOnce(&mut TailnetInfo)) {
        self.status.send_if_modified(|s| match s {
            TailnetStatus::Running(t) if self.tailnet_gen() == gen => {
                let before = t.clone();
                f(t);
                *t != before
            }
            _ => false,
        });
    }

    /// Serve HTTPS with `acceptor` from now on, if the tailnet listener of
    /// generation `gen` is still the current one.
    fn set_tls(&self, gen: u64, acceptor: TlsAcceptor) -> bool {
        let mut slot = self.tls.write().unwrap_or_else(|p| p.into_inner());
        if self.tailnet_gen() != gen {
            return false;
        }
        *slot = Some((gen, acceptor));
        true
    }

    /// HTTPS for a new connection, while the current tailnet listener serves it.
    fn tls_acceptor(&self) -> Option<TlsAcceptor> {
        let gen = self.tailnet_gen();
        self.tls
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .filter(|(g, _)| *g == gen)
            .map(|(_, a)| a.clone())
    }

    fn system_binding(&self) -> Option<SystemBinding> {
        self.system.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Identity and connection facts for a TCP connection accepted on a
    /// local, LAN or system-tailnet listener.
    async fn classify_tcp(
        &self,
        local: Option<SocketAddr>,
        remote: SocketAddr,
    ) -> (Option<Identity>, ConnInfo) {
        let remote_ip = conn::canonical_ip(remote.ip());
        if remote_ip.is_loopback() {
            return (
                self.policy().localhost_identity(),
                ConnInfo::local(remote, NetPath::Localhost),
            );
        }
        // A tailnet connection through the system tailscaled arrives on one
        // of our Tailscale addresses from a Tailscale address. (Replies to a
        // spoofed 100.x source would route into the tailnet, so a LAN host
        // cannot complete such a handshake.)
        let local_ip = local.map(|l| conn::canonical_ip(l.ip()));
        if let Some(sys) = self.system_binding() {
            if is_tailscale_ip(remote_ip) && local_ip.is_some_and(|ip| sys.self_ips.contains(&ip)) {
                return self.classify_system_tailnet(&sys, remote, remote_ip).await;
            }
        }
        (None, ConnInfo::local(remote, NetPath::Lan))
    }

    async fn classify_system_tailnet(
        &self,
        sys: &SystemBinding,
        remote: SocketAddr,
        remote_ip: IpAddr,
    ) -> (Option<Identity>, ConnInfo) {
        let (facts, status) = tokio::join!(sys.client.whois(remote_ip), sys.client.cached_status());
        let path = status.map_or(NetPath::Tailnet, |s| s.path_to(remote_ip));
        let facts = facts.unwrap_or_else(|e| {
            // Fail closed: an unknown caller is anonymous, never the owner.
            tracing::warn!(error = %e, %remote_ip, "whois failed; treating caller as anonymous");
            None
        });
        let policy = self.policy();
        let (identity, grants) = match &facts {
            Some(f) => (
                Some(policy.identity_for(f)),
                f.grants(&policy.capability).to_vec(),
            ),
            None => (None, Vec::new()),
        };
        let conn = ConnInfo {
            path,
            remote_addr: Some(remote),
            tailnet_name: sys.dns_name.clone(),
            grants,
        };
        (identity, conn)
    }
}

/// A running network layer. Call [`shutdown`](FluxNet::shutdown) to wait
/// for in-flight requests; dropping it also stops accepting and asks open
/// connections to finish, but does not wait for them.
pub struct FluxNet {
    local_addr: SocketAddr,
    lan_addr: Option<SocketAddr>,
    status: watch::Receiver<TailnetStatus>,
    mode: flux_core::api::TailnetMode,
    shared: Arc<Shared>,
}

impl FluxNet {
    /// Bind the local (and LAN) listeners, start the tailnet listener in the
    /// background, and serve `router` on all of them. Binding errors of the
    /// local/LAN listeners are returned; tailnet progress and failures are
    /// reported through [`tailnet_status`](Self::tailnet_status), so a
    /// server without Tailscale still starts.
    pub async fn start(config: FluxNetConfig, router: Router) -> Result<FluxNet, NetError> {
        let mode = config.tailnet.api_mode();
        let initial = match config.tailnet {
            TailnetConfig::Off => TailnetStatus::Off,
            _ => TailnetStatus::Starting,
        };
        let (status_tx, status_rx) = watch::channel(initial);
        let shutdown = CancellationToken::new();
        let shared = Arc::new(Shared {
            router,
            owner_fixed: config.roles.owner_login.is_some(),
            policy: RwLock::new(config.roles),
            status: status_tx,
            tailnet: Mutex::new(Scope::child_of(&shutdown)),
            shutdown,
            tracker: TaskTracker::new(),
            system: RwLock::new(None),
            admission: limits::Admission::new(config.limits),
            write_timeout: config.limits.write_timeout,
            tailnet_gen: AtomicU64::new(0),
            tls: RwLock::new(None),
        });

        let local = bind(config.local_addr).await?;
        let local_addr = local.local_addr().map_err(|source| NetError::Bind {
            addr: config.local_addr,
            source,
        })?;
        let lan = match config.lan_addr {
            Some(addr) => Some(bind(addr).await?),
            None => None,
        };
        let lan_addr = lan.as_ref().and_then(|l| l.local_addr().ok());

        let main = shared.main_scope();
        shared
            .tracker
            .spawn(accept_loop(local, shared.clone(), main.clone()));
        if let Some(lan) = lan {
            shared.tracker.spawn(accept_loop(lan, shared.clone(), main));
        }
        spawn_tailnet(&shared, config.tailnet, local_addr, lan_addr);

        Ok(FluxNet {
            local_addr,
            lan_addr,
            status: status_rx,
            mode,
            shared,
        })
    }

    /// The bound local listener address (resolves port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The bound LAN listener address, if any.
    pub fn lan_addr(&self) -> Option<SocketAddr> {
        self.lan_addr
    }

    /// Tailnet state; `changed().await` to follow it (login URL, running…).
    pub fn tailnet_status(&self) -> watch::Receiver<TailnetStatus> {
        self.status.clone()
    }

    /// The tailnet state as the API reports it (`GET /api/v1/tailnet`); for
    /// `Event::Tailnet`, map each [`tailnet_status`](Self::tailnet_status)
    /// change with [`TailnetStatus::to_api`].
    pub fn tailnet_info(&self) -> flux_core::api::TailnetInfo {
        self.status.borrow().to_api(self.mode)
    }

    /// The configured tailnet mode.
    pub fn tailnet_mode(&self) -> flux_core::api::TailnetMode {
        self.mode
    }

    /// The role policy in effect (the owner login fills in once known).
    pub fn role_policy(&self) -> RolePolicy {
        self.shared.policy().clone()
    }

    /// Replace the tailnet listener (another mode, or other settings)
    /// without touching the local and LAN listeners or their connections.
    /// The old tailnet listener stops accepting, its in-flight requests get
    /// up to `grace`, and its node (embedded mode) stops before the new one
    /// starts, so two nodes never share a state dir. Progress is reported
    /// through [`tailnet_status`](Self::tailnet_status) as at start.
    pub async fn set_tailnet(&mut self, tailnet: TailnetConfig, grace: Duration) {
        let shared = &self.shared;
        // From here the old listener's tasks leave the shared state alone,
        // and new connections get no HTTPS until the new listener sets it up.
        shared.tailnet_gen.fetch_add(1, Ordering::SeqCst);
        *shared.tls.write().unwrap_or_else(|p| p.into_inner()) = None;
        let old = std::mem::replace(
            &mut *shared.tailnet.lock().unwrap_or_else(|p| p.into_inner()),
            Scope::child_of(&shared.shutdown),
        );
        old.token.cancel();
        old.tracker.close();
        if tokio::time::timeout(grace, old.tracker.wait()).await.is_err() {
            tracing::warn!(
                ?grace,
                "tailnet connections still open after the switch; abandoning them"
            );
        }
        // Wildcard listeners no longer classify anyone as a tailnet caller.
        *shared.system.write().unwrap_or_else(|p| p.into_inner()) = None;
        self.mode = tailnet.api_mode();
        shared.set_status(match tailnet {
            TailnetConfig::Off => TailnetStatus::Off,
            _ => TailnetStatus::Starting,
        });
        spawn_tailnet(shared, tailnet, self.local_addr, self.lan_addr);
    }

    /// Stop accepting, let in-flight requests finish for up to `grace`, then
    /// stop the tailnet node.
    pub async fn shutdown(self, grace: Duration) {
        self.shared.shutdown.cancel();
        self.shared.tracker.close();
        let tailnet = self.shared.tailnet_scope();
        tailnet.tracker.close();
        let all = async {
            tokio::join!(self.shared.tracker.wait(), tailnet.tracker.wait());
        };
        if tokio::time::timeout(grace, all).await.is_err() {
            tracing::warn!(?grace, "connections still open at shutdown; abandoning them");
        }
    }
}

impl Drop for FluxNet {
    fn drop(&mut self) {
        // Idempotent; after `shutdown` this is a no-op.
        self.shared.shutdown.cancel();
    }
}

/// Start the tailnet listener for `tailnet` in the current tailnet scope.
fn spawn_tailnet(
    shared: &Arc<Shared>,
    tailnet: TailnetConfig,
    local_addr: SocketAddr,
    lan_addr: Option<SocketAddr>,
) {
    let scope = shared.tailnet_scope();
    match tailnet {
        TailnetConfig::Off => {}
        #[cfg(feature = "embedded")]
        TailnetConfig::Embedded(embedded) => {
            scope
                .tracker
                .spawn(embedded::run(embedded, shared.clone(), scope.clone()));
        }
        TailnetConfig::System(system) => {
            let covered = [Some(local_addr), lan_addr].into_iter().flatten().collect();
            let gen = shared.tailnet_gen();
            scope
                .tracker
                .spawn(run_system(system, covered, gen, shared.clone(), scope.clone()));
        }
    }
}

async fn bind(addr: SocketAddr) -> Result<TcpListener, NetError> {
    TcpListener::bind(addr)
        .await
        .map_err(|source| NetError::Bind { addr, source })
}

/// Accept TCP connections until shutdown; admit, classify and serve each
/// one on its own task (classification may run a WhoIs, which must not
/// stall accepts). A connection over the [`ServeLimits`] is answered `503`
/// and closed at once.
async fn accept_loop(listener: TcpListener, shared: Arc<Shared>, scope: Scope) {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = scope.token.cancelled() => break,
        };
        let (stream, remote) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                // EMFILE and friends: back off instead of spinning.
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(permit) = shared.admission.try_admit(remote.ip()) else {
            limits::refuse(stream);
            continue;
        };
        let shared_conn = shared.clone();
        let token = scope.token.clone();
        scope.tracker.spawn(async move {
            // Held until the connection ends.
            let _permit = permit;
            serve_tcp(stream, remote, shared_conn, token).await
        });
    }
}

async fn serve_tcp(stream: TcpStream, remote: SocketAddr, shared: Arc<Shared>, token: CancellationToken) {
    let _ = stream.set_nodelay(true);
    let local = stream.local_addr().ok();
    let Some(tls) = shared.tls_acceptor() else {
        let (identity, conn) = shared.classify_tcp(local, remote).await;
        serve::serve_connection(
            stream,
            shared.router.clone(),
            identity,
            conn,
            token,
            shared.write_timeout,
        )
        .await;
        return;
    };
    // HTTPS is on: find out what the client speaks while finding out who it is.
    let opening = async {
        tokio::select! {
            o = tls::opening(&stream, serve::HEADER_READ_TIMEOUT) => o,
            _ = token.cancelled() => tls::Opening::Gone,
        }
    };
    let ((identity, conn), opening) = tokio::join!(shared.classify_tcp(local, remote), opening);
    let (router, write_timeout) = (shared.router.clone(), shared.write_timeout);
    match opening {
        tls::Opening::Plain => {
            serve::serve_connection(stream, router, identity, conn, token, write_timeout).await;
        }
        tls::Opening::Tls => {
            let handshake = tokio::time::timeout(tls::HANDSHAKE_TIMEOUT, tls.accept(stream));
            let accepted = tokio::select! {
                a = handshake => a,
                _ = token.cancelled() => return,
            };
            match accepted {
                Ok(Ok(stream)) => {
                    serve::serve_connection(stream, router, identity, conn, token, write_timeout).await;
                }
                Ok(Err(e)) => tracing::debug!(error = %e, %remote, "TLS handshake failed"),
                Err(_) => tracing::debug!(%remote, "TLS handshake timed out"),
            }
        }
        tls::Opening::Gone => {}
    }
}

/// System mode: wait for tailscaled to be running, bind our Tailscale
/// addresses, and serve, until `scope` stops. Connections are classified in
/// [`Shared::classify_tcp`].
async fn run_system(
    config: SystemConfig,
    covered: Vec<SocketAddr>,
    gen: u64,
    shared: Arc<Shared>,
    scope: Scope,
) {
    let client = Arc::new(SystemTailscale::new(
        system::find_cli(config.cli.as_deref()),
        config.whois_ttl,
    ));
    let status = loop {
        match client.status().await {
            Ok(status) if status.is_running() && !status.self_ips.is_empty() => break status,
            Ok(status) => {
                let mut reason = format!("tailscaled is {}", status.backend_state);
                if let Some(url) = &status.auth_url {
                    reason.push_str(&format!("; log in at {url}"));
                }
                shared.set_status(TailnetStatus::Unavailable { reason });
            }
            Err(e) => shared.set_status(TailnetStatus::Unavailable {
                reason: e.to_string(),
            }),
        }
        tokio::select! {
            _ = tokio::time::sleep(config.retry_every) => {}
            _ = scope.token.cancelled() => return,
        }
    };

    if let Some(owner) = &status.owner_login {
        shared.learn_owner(owner);
    }
    client.prime_status(status.clone());
    *shared.system.write().unwrap_or_else(|p| p.into_inner()) = Some(SystemBinding {
        client: client.clone(),
        self_ips: status.self_ips.clone(),
        dns_name: status.dns_name.clone(),
    });

    let mut addrs = Vec::new();
    let mut errors = Vec::new();
    for ip in &status.self_ips {
        let addr = SocketAddr::new(*ip, config.port);
        // A wildcard LAN/local listener on the same port already receives
        // tailnet connections (and binding again would fail): classify there.
        if covered
            .iter()
            .any(|c| c.port() == config.port && c.ip().is_unspecified())
        {
            addrs.push(addr);
            continue;
        }
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                addrs.push(listener.local_addr().unwrap_or(addr));
                scope
                    .tracker
                    .spawn(accept_loop(listener, shared.clone(), scope.clone()));
            }
            Err(e) => errors.push(format!("{addr}: {e}")),
        }
    }
    if addrs.is_empty() {
        shared.set_status(TailnetStatus::Failed {
            error: format!("cannot listen on the Tailscale addresses: {}", errors.join("; ")),
        });
        return;
    }
    if !errors.is_empty() {
        tracing::warn!(?errors, "some Tailscale addresses could not be bound");
    }
    tracing::info!(cli = %client.cli().display(), ?addrs, "flux is on the tailnet (system tailscaled)");
    shared.set_status(TailnetStatus::Running(TailnetInfo {
        mode: TailnetMode::System,
        dns_name: status.dns_name,
        addrs,
        tls: false,
        tls_error: None,
        owner_login: shared.owner_login(),
        key_expiry: None,
        control_server: None,
    }));
    if config.https {
        scope
            .tracker
            .spawn(keep_certificate(client, gen, shared.clone(), scope.clone()));
    }
}

/// System mode with HTTPS: get this machine's certificate from tailscaled,
/// serve it, and ask again every [`tls::REFRESH_EVERY`] (sooner after a
/// failure, backing off to [`CERT_RETRY_MAX`]) until `scope` stops. A
/// renewed certificate is served from the next handshake; when a refresh
/// fails, the one loaded keeps being served.
async fn keep_certificate(client: Arc<SystemTailscale>, gen: u64, shared: Arc<Shared>, scope: Scope) {
    let certs = Arc::new(tls::CertStore::default());
    let acceptor = match tls::server_config(certs.clone()) {
        Ok(config) => TlsAcceptor::from(config),
        Err(e) => {
            shared.update_running_for(gen, |t| t.tls_error = Some(e));
            return;
        }
    };
    let mut serving = false;
    let mut backoff = CERT_RETRY_MIN;
    loop {
        let fetched = tokio::select! {
            f = fetch_certificate(&client) => f,
            _ = scope.token.cancelled() => return,
        };
        let wait = match fetched {
            Ok((domain, key)) => {
                certs.set(key);
                if !serving {
                    serving = shared.set_tls(gen, acceptor.clone());
                    if serving {
                        tracing::info!(%domain, "HTTPS on the tailnet, with tailscaled's certificate");
                    }
                }
                shared.update_running_for(gen, |t| {
                    t.tls = serving;
                    t.tls_error = None;
                });
                backoff = CERT_RETRY_MIN;
                tls::REFRESH_EVERY
            }
            Err(e) => {
                tracing::warn!(error = %e, serving, "HTTPS on the tailnet: no certificate from tailscaled");
                shared.update_running_for(gen, |t| t.tls_error = Some(e));
                let wait = backoff;
                backoff = (backoff * 2).min(CERT_RETRY_MAX);
                wait
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = scope.token.cancelled() => return,
        }
    }
}

/// This machine's certificate from tailscaled, and the name it is for.
async fn fetch_certificate(client: &SystemTailscale) -> Result<(String, rustls::sign::CertifiedKey), String> {
    let status = client.status().await.map_err(|e| e.to_string())?;
    let domain = system::cert_domain(&status)?;
    let pem = client.cert(&domain).await.map_err(|e| e.to_string())?;
    let key = tls::certified_key_from_pem(&pem)?;
    Ok((domain, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Extension;
    use rustls::pki_types::{CertificateDer, ServerName};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers `/path` with how the caller reached the server.
    fn path_router() -> Router {
        Router::new().route(
            "/path",
            get(|Extension(conn): Extension<ConnInfo>| async move { conn.path.as_str() }),
        )
    }

    async fn local_net() -> FluxNet {
        let config = FluxNetConfig {
            local_addr: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        };
        FluxNet::start(config, path_router()).await.unwrap()
    }

    /// The whole response to `GET path` sent over `stream`.
    async fn get_over<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        mut stream: S,
        host: &str,
        path: &str,
    ) -> std::io::Result<String> {
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await?;
        let mut out = Vec::new();
        match stream.read_to_end(&mut out).await {
            // A TLS peer that closes without close_notify still sent it all.
            Err(e) if e.kind() != std::io::ErrorKind::UnexpectedEof => return Err(e),
            _ => {}
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    async fn http_get(addr: SocketAddr, path: &str) -> String {
        let stream = TcpStream::connect(addr).await.unwrap();
        get_over(stream, "localhost", path).await.unwrap()
    }

    /// `GET path` over TLS to `name` at `addr`, trusting only `cert`.
    async fn https_get(
        addr: SocketAddr,
        name: &str,
        cert: &CertificateDer<'static>,
        path: &str,
    ) -> std::io::Result<String> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(tls::provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect(addr).await?;
        let stream = connector
            .connect(ServerName::try_from(name.to_string()).unwrap(), tcp)
            .await?;
        get_over(stream, name, path).await
    }

    fn acceptor_for(pem: &str) -> TlsAcceptor {
        let certs = Arc::new(tls::CertStore::default());
        certs.set(tls::certified_key_from_pem(pem.as_bytes()).unwrap());
        TlsAcceptor::from(tls::server_config(certs).unwrap())
    }

    #[tokio::test]
    async fn https_and_http_share_a_port() {
        let mut net = local_net().await;
        let addr = net.local_addr();
        let (pem, cert) = tls::tests::self_signed("flux.test");
        let acceptor = acceptor_for(&pem);

        // No HTTPS yet: a TLS client gets nowhere; plain HTTP works.
        assert!(https_get(addr, "flux.test", &cert, "/path").await.is_err());
        assert!(http_get(addr, "/path").await.ends_with("localhost"));

        let gen = net.shared.tailnet_gen();
        assert!(net.shared.set_tls(gen, acceptor.clone()));
        // Both on one port, and the caller is classified the same either way.
        let body = https_get(addr, "flux.test", &cert, "/path").await.unwrap();
        assert!(
            body.starts_with("HTTP/1.1 200") && body.ends_with("localhost"),
            "{body}"
        );
        assert!(http_get(addr, "/path").await.ends_with("localhost"));

        // A broken handshake costs only its own connection.
        let mut junk = TcpStream::connect(addr).await.unwrap();
        junk.write_all(&[0x16, 0x03, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5])
            .await
            .unwrap();
        let _ = junk.read_to_end(&mut Vec::new()).await;
        assert!(https_get(addr, "flux.test", &cert, "/path").await.is_ok());

        // Replacing the tailnet listener takes its HTTPS along, and a task of
        // the old one can't put it back.
        net.set_tailnet(TailnetConfig::Off, Duration::from_millis(100))
            .await;
        assert!(https_get(addr, "flux.test", &cert, "/path").await.is_err());
        assert!(!net.shared.set_tls(gen, acceptor));
        assert!(http_get(addr, "/path").await.ends_with("localhost"));
        net.shutdown(Duration::from_secs(1)).await;
    }

    /// A `tailscale` stand-in: `status --json` prints `status`, `cert … <domain>`
    /// prints `pem` for flux.tail1234.ts.net and fails for any other name.
    #[cfg(unix)]
    fn fake_cli(dir: &std::path::Path, status: &str, pem: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("status.json"), status).unwrap();
        std::fs::write(dir.join("cert.pem"), pem).unwrap();
        let cli = dir.join("tailscale");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  status) cat {d}/status.json ;;\n  cert) [ \"$6\" = flux.tail1234.ts.net ] || {{ echo \"bad domain $6\" >&2; exit 3; }}; cat {d}/cert.pem ;;\n  *) exit 2 ;;\nesac\n",
                d = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        cli
    }

    #[cfg(unix)]
    const RUNNING: &str = r#"{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],
        "Self":{"DNSName":"flux.tail1234.ts.net.","TailscaleIPs":["100.64.0.1"],"UserID":1},
        "User":{"1":{"LoginName":"owner@example.com"}},"CertDomains":["flux.tail1234.ts.net"]}"#;

    /// Runs the certificate task against `cli` until the running tailnet's
    /// info satisfies `done`; returns that info.
    #[cfg(unix)]
    async fn certificate_task(
        net: &FluxNet,
        cli: PathBuf,
        done: impl Fn(&TailnetInfo) -> bool,
    ) -> TailnetInfo {
        net.shared.set_status(TailnetStatus::Running(TailnetInfo {
            mode: TailnetMode::System,
            dns_name: Some("flux.tail1234.ts.net".into()),
            addrs: vec![net.local_addr()],
            tls: false,
            tls_error: None,
            owner_login: None,
            key_expiry: None,
            control_server: None,
        }));
        let scope = net.shared.tailnet_scope();
        let client = Arc::new(SystemTailscale::new(cli, Duration::from_secs(60)));
        let task = keep_certificate(
            client,
            net.shared.tailnet_gen(),
            net.shared.clone(),
            scope.clone(),
        );
        scope.tracker.spawn(task);
        let mut rx = net.tailnet_status();
        let wait = async {
            loop {
                if let TailnetStatus::Running(t) = &*rx.borrow_and_update() {
                    if done(t) {
                        return t.clone();
                    }
                }
                rx.changed().await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn without_tailnet_certificates_https_says_how_to_turn_them_on() {
        let dir = std::env::temp_dir().join(format!("flux-net-nocert-{}", std::process::id()));
        let off = RUNNING.replace(
            r#""CertDomains":["flux.tail1234.ts.net"]"#,
            r#""CertDomains":null"#,
        );
        let cli = fake_cli(&dir, &off, "");
        let net = local_net().await;

        let info = certificate_task(&net, cli, |t| t.tls_error.is_some()).await;
        assert!(!info.tls);
        assert!(info.tls_error.unwrap().contains("admin console"));
        assert!(net.shared.tls_acceptor().is_none());
        assert!(net.tailnet_info().url.unwrap().starts_with("http://"));
        net.shutdown(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tailnet_url_prefers_the_magicdns_name() {
        let mut info = TailnetInfo {
            mode: TailnetMode::System,
            dns_name: Some("mac.tail1234.ts.net".into()),
            addrs: vec!["100.64.0.1:8096".parse().unwrap()],
            tls: false,
            tls_error: None,
            owner_login: None,
            key_expiry: None,
            control_server: None,
        };
        assert_eq!(info.url().as_deref(), Some("http://mac.tail1234.ts.net:8096"));
        info.tls = true;
        info.addrs = vec!["100.64.0.1:443".parse().unwrap()];
        assert_eq!(info.url().as_deref(), Some("https://mac.tail1234.ts.net"));
        info.dns_name = None;
        info.addrs = vec!["[fd7a:115c:a1e0::1]:8096".parse().unwrap()];
        assert_eq!(info.url().as_deref(), Some("https://[fd7a:115c:a1e0::1]:8096"));
    }

    #[test]
    fn tailnet_status_maps_to_the_api_dto() {
        use flux_core::api::{TailnetMode as Mode, TailnetState as State};
        let off = TailnetStatus::Off.to_api(Mode::Off);
        assert_eq!(
            (off.mode, off.state, off.system_tailscale),
            (Mode::Off, State::Off, false)
        );

        let login = TailnetStatus::NeedsLogin {
            auth_url: "https://login.tailscale.com/a/abc".into(),
        }
        .to_api(Mode::Embedded);
        assert_eq!(login.state, State::NeedsLogin);
        assert_eq!(
            login.auth_url.as_deref(),
            Some("https://login.tailscale.com/a/abc")
        );

        let approve = TailnetStatus::NeedsApproval {
            approve_url: Some(APPROVE_URL.into()),
        }
        .to_api(Mode::Embedded);
        assert_eq!(approve.state, State::NeedsApproval);
        assert_eq!(approve.approve_url.as_deref(), Some(APPROVE_URL));
        // Headscale has no admin console to send the owner to.
        let headscale = TailnetStatus::NeedsApproval { approve_url: None }.to_api(Mode::Embedded);
        assert_eq!(headscale.state, State::NeedsApproval);
        assert_eq!(headscale.approve_url, None);

        let running = TailnetStatus::Running(TailnetInfo {
            mode: TailnetMode::System,
            dns_name: Some("mac.tail1234.ts.net".into()),
            addrs: vec!["100.64.0.1:8096".parse().unwrap()],
            tls: false,
            tls_error: Some("HTTPS certificates are off for this tailnet.".into()),
            owner_login: Some("james@example.com".into()),
            key_expiry: Some(1_790_000_000_000),
            control_server: None,
        })
        .to_api(Mode::System);
        assert_eq!(running.state, State::Running);
        assert_eq!(running.dns_name.as_deref(), Some("mac.tail1234.ts.net"));
        assert_eq!(running.tailnet_name.as_deref(), Some("tail1234.ts.net"));
        assert_eq!(running.url.as_deref(), Some("http://mac.tail1234.ts.net:8096"));
        assert_eq!(running.owner_login.as_deref(), Some("james@example.com"));
        assert_eq!(running.key_expiry, Some(1_790_000_000_000));
        assert!(running.system_tailscale);
        assert!(!running.https);
        assert_eq!(
            running.https_error.as_deref(),
            Some("HTTPS certificates are off for this tailnet.")
        );

        let failed = TailnetStatus::Failed { error: "boom".into() }.to_api(Mode::Embedded);
        assert_eq!(
            (failed.state, failed.last_error.as_deref()),
            (State::Error, Some("boom"))
        );
        let unavailable = TailnetStatus::Unavailable {
            reason: "tailscaled is Stopped".into(),
        }
        .to_api(Mode::System);
        assert_eq!(unavailable.state, State::Unavailable);
        assert_eq!(TailnetConfig::Off.api_mode(), Mode::Off);
    }
}
