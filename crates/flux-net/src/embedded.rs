//! Embedded Tailscale: flux's own tailnet node through Truffle.
//!
//! flux serves its API and media directly from Rust on a Truffle
//! `listen_tcp_opts` listener (research/06 §9.1), not through the sidecar's
//! HTTP proxy: every accepted connection carries an unforgeable WhoIs
//! identity from the sidecar's bridge header, and hyper keeps full control
//! of HTTP semantics (Range, half-close, upgrades).

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use truffle_core::network::tailscale::TailscaleProvider;
use truffle_core::network::{ControlServer, KeyExpiry, ListenOpts, NetworkStatusEvent};
use truffle_core::transport::RawListener;
use truffle_core::Node;

use crate::conn::{canonical_ip, ConnInfo, NetPath};
use crate::roles::PeerFacts;
use crate::{NetError, Scope, Shared, TailnetInfo, TailnetMode, TailnetStatus};

/// Truffle app id: peers with hostnames `truffle-flux-*` are flux nodes.
pub const APP_ID: &str = "flux";

/// Default idle reap for bridged connections. Truffle's 600 s default cut
/// long single-response streams before the flux/dogfood fix; six hours
/// covers a paused movie without leaking dead connections forever.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

/// Settings for flux's own tailnet node.
#[derive(Debug, Clone)]
pub struct EmbeddedConfig {
    /// Where the node keeps its keys and device id; a durable path under the
    /// server's data dir (losing it means a new device on the tailnet).
    pub state_dir: PathBuf,
    /// Human device name, e.g. "Mac mini". The tailnet hostname becomes
    /// `truffle-flux-<slug>`, the convention flux clients use to find
    /// servers with `peers()` until Truffle has service announcement
    /// (research/06 §9.1 #2).
    pub device_name: String,
    /// A pretty hostname (`flux` → `https://flux.<tailnet>.ts.net`) instead
    /// of the convention. Such a node drops out of other flux nodes'
    /// `peers()` (research/06 §4.3), so it is opt-in.
    pub hostname: Option<String>,
    /// The Go sidecar binary; `None` resolves it with [`resolve_sidecar`].
    pub sidecar_path: Option<PathBuf>,
    /// Tailnet port to serve on.
    pub port: u16,
    /// Terminate TLS with the tailnet's MagicDNS certificate (needs MagicDNS
    /// and HTTPS enabled on the tailnet). Native apps don't need it.
    pub tls: bool,
    /// Idle reap for bridged connections (both directions quiet).
    pub idle_timeout: Duration,
    /// How long to wait for the login; `None` waits until the owner opens
    /// the URL (headless first run).
    pub start_timeout: Option<Duration>,
    /// Pre-authorised key for unattended installs (Docker). Never logged.
    pub auth_key: Option<String>,
}

impl EmbeddedConfig {
    /// Defaults for a server whose data lives in `data_dir`.
    pub fn new(data_dir: &Path, device_name: impl Into<String>) -> Self {
        EmbeddedConfig {
            state_dir: data_dir.join("tailscale"),
            device_name: device_name.into(),
            hostname: None,
            sidecar_path: None,
            port: 8096,
            tls: false,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            start_timeout: None,
            auth_key: None,
        }
    }
}

/// The sidecar's file name next to flux-server: `sidecar-slim`, with the
/// platform's executable suffix (`sidecar-slim.exe` on Windows).
pub fn sidecar_file_name() -> String {
    format!("sidecar-slim{}", std::env::consts::EXE_SUFFIX)
}

/// Where a package puts the sidecar, from flux-server's folder `exe_dir`:
/// beside it, or in `../libexec/flux/`.
pub fn sidecar_places(exe_dir: &Path) -> [PathBuf; 2] {
    [
        exe_dir.join(sidecar_file_name()),
        exe_dir.join("../libexec/flux").join(sidecar_file_name()),
    ]
}

/// Find the Truffle Go sidecar: `explicit`, then `TRUFFLE_SIDECAR_PATH`,
/// then [`sidecar_file_name`] next to the running executable (how a release
/// archive ships it), then in `../libexec/flux/` from it (the .deb, the
/// .rpm and Homebrew: `/usr/libexec/flux/sidecar-slim`, kept out of the
/// `bin` folder on PATH). `scripts/package/fetch-sidecar.sh` downloads the
/// one that matches truffle-core (docs/RELEASING.md, "The Truffle sidecar").
pub fn resolve_sidecar(explicit: Option<&Path>) -> Result<PathBuf, NetError> {
    let mut tried = Vec::new();
    // The real binary's directory, also when it was started through a
    // symlink (macOS reports the symlink's path).
    let exe_dir = std::env::current_exe()
        .ok()
        .map(|exe| exe.canonicalize().unwrap_or(exe))
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let candidates = explicit
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::env::var_os("TRUFFLE_SIDECAR_PATH").map(PathBuf::from))
        .chain(exe_dir.iter().flat_map(|dir| sidecar_places(dir)));
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
        tried.push(candidate.display().to_string());
    }
    Err(NetError::SidecarNotFound(format!(
        "tried {}; release packages ship it next to flux-server as {}. \
         Otherwise set TRUFFLE_SIDECAR_PATH to the tsnet-sidecar asset for this platform \
         from the Truffle release that matches truffle-core \
         (https://github.com/vibecook-dev/truffle/releases), or to a sidecar-slim built \
         with `go build` in Truffle's packages/sidecar-slim",
        if tried.is_empty() {
            "nothing".to_string()
        } else {
            tried.join(", ")
        },
        sidecar_file_name()
    )))
}

/// Looks up the network path to a tailnet IP.
pub(crate) type PathLookup =
    Arc<dyn Fn(IpAddr) -> Pin<Box<dyn Future<Output = NetPath> + Send>> + Send + Sync>;

/// A path lookup backed by the node's peer list. Only flux/Truffle peers are
/// listed (Truffle filters peers by app), so other callers — a browser on a
/// phone with the Tailscale app — come out as [`NetPath::Tailnet`].
fn node_path_lookup(node: Arc<Node<TailscaleProvider>>) -> PathLookup {
    Arc::new(move |ip: IpAddr| {
        let node = node.clone();
        Box::pin(async move {
            node.peers()
                .await
                .into_iter()
                .find(|p| canonical_ip(p.ip) == ip)
                .map_or(NetPath::Tailnet, |p| {
                    NetPath::from_connection_type(&p.connection_type)
                })
        })
    })
}

/// Start the node, report login/approval through the status channel, then
/// serve the tailnet listener until `scope` stops (shutdown, or a tailnet
/// switch). Never returns an error: every failure ends up as
/// [`TailnetStatus::Failed`].
pub(crate) async fn run(config: EmbeddedConfig, shared: Arc<Shared>, scope: Scope) {
    if let Err(e) = run_inner(config, &shared, &scope).await {
        tracing::error!(error = %e, "embedded tailnet failed");
        shared.set_status(TailnetStatus::Failed { error: e.to_string() });
    }
}

async fn run_inner(config: EmbeddedConfig, shared: &Arc<Shared>, scope: &Scope) -> Result<(), NetError> {
    let embedded = |e: truffle_core::NodeError| NetError::Embedded(e.to_string());
    let sidecar = resolve_sidecar(config.sidecar_path.as_deref())?;
    std::fs::create_dir_all(&config.state_dir)
        .map_err(|e| NetError::Embedded(format!("state dir {}: {e}", config.state_dir.display())))?;
    let state_dir = config.state_dir.to_string_lossy().into_owned();

    // Which control server the node talks to decides whether "approve this
    // device" can link to Tailscale's admin console. Protocol-10 sidecars
    // report it (`ControlServerKnown`, before any approval request); for
    // older ones, tsnet's own rule: TS_CONTROL_URL, else Tailscale's.
    let control = Arc::new(Mutex::new(ControlServer::from_url(
        &std::env::var("TS_CONTROL_URL").unwrap_or_default(),
    )));
    let on_status = shared.clone();
    let on_control = control.clone();
    let mut builder = Node::<TailscaleProvider>::builder()
        .app_id(APP_ID)
        .map_err(embedded)?
        .device_name(&config.device_name)
        .sidecar_path(sidecar)
        .state_dir(&state_dir)
        .idle_timeout_secs(config.idle_timeout.as_secs().max(1))
        .start_timeout(config.start_timeout)
        .on_network_status(move |event| on_network_event(&on_status, &on_control, event));
    if let Some(hostname) = &config.hostname {
        builder = builder.hostname(hostname.as_str()).map_err(embedded)?;
    }
    if let Some(key) = &config.auth_key {
        builder = builder.auth_key(key);
    }

    shared.set_status(TailnetStatus::Starting);
    let on_auth = shared.clone();
    let build = builder.build_with_auth_handler(move |auth_url| {
        tracing::info!(%auth_url, "open this URL to connect flux to Tailscale");
        on_auth.set_status(TailnetStatus::NeedsLogin { auth_url });
    });
    let node = tokio::select! {
        node = build => Arc::new(node.map_err(embedded)?),
        // Dropping the build future kills the sidecar it spawned.
        _ = scope.token.cancelled() => return Ok(()),
    };

    let info = node.local_info();
    if let Some(login) = &info.login_name {
        shared.learn_owner(login);
    }
    let listener = match node
        .listen_tcp_opts(config.port, ListenOpts { tls: config.tls })
        .await
    {
        Ok(listener) => listener,
        Err(e) => {
            node.stop().await;
            return Err(embedded(e));
        }
    };
    let dns_name = info
        .dns_name
        .as_deref()
        .map(|n| n.trim_end_matches('.').to_string());
    let addrs = info
        .ip
        .map(|ip| vec![SocketAddr::new(ip, listener.port)])
        .unwrap_or_default();
    // The getters are filled before build() returns; the hook's events may
    // land before or after this, so both paths write the same fields.
    let control_server = node.control_server().unwrap_or_else(|| lock(&control).clone());
    shared.set_status(TailnetStatus::Running(TailnetInfo {
        mode: TailnetMode::Embedded,
        dns_name: dns_name.clone(),
        addrs,
        tls: config.tls,
        tls_error: None,
        owner_login: shared.owner_login(),
        key_expiry: node.key_expiry().and_then(KeyExpiry::unix_millis),
        control_server: custom_url(&control_server),
    }));
    tracing::info!(dns_name = ?dns_name, port = listener.port, "flux is on the tailnet (embedded)");

    serve_listener(
        listener,
        shared.clone(),
        node_path_lookup(node.clone()),
        dns_name,
        scope.clone(),
    )
    .await;
    node.stop().await;
    Ok(())
}

fn lock(m: &Mutex<ControlServer>) -> std::sync::MutexGuard<'_, ControlServer> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The URL of a custom control server (Headscale); `None` for Tailscale's.
fn custom_url(c: &ControlServer) -> Option<String> {
    (!c.is_tailscale()).then(|| c.url().to_string())
}

/// What a Truffle status event changes in flux's tailnet status. One
/// forwarder task delivers events in order, so `ControlServerKnown` is
/// always seen before the `NeedsApproval` it qualifies.
pub(crate) fn on_network_event(shared: &Shared, control: &Mutex<ControlServer>, event: NetworkStatusEvent) {
    match event {
        NetworkStatusEvent::ControlServerKnown { control_server } => {
            let url = custom_url(&control_server);
            *lock(control) = control_server;
            shared.update_running(|t| t.control_server = url);
        }
        NetworkStatusEvent::NeedsApproval => shared.set_status(TailnetStatus::NeedsApproval {
            approve_url: lock(control).machines_admin_url().map(str::to_owned),
        }),
        NetworkStatusEvent::LoginKnown { login_name } => shared.learn_owner(&login_name),
        NetworkStatusEvent::KeyExpiryKnown { key_expiry } => {
            shared.update_running(|t| t.key_expiry = key_expiry.unix_millis())
        }
        NetworkStatusEvent::SidecarExited { exit_code } => {
            let how = match exit_code {
                Some(code) => format!("with code {code}"),
                None => "on a signal".to_string(),
            };
            tracing::error!(?exit_code, "the Tailscale sidecar exited");
            shared.set_status(TailnetStatus::Failed {
                error: format!(
                    "The Tailscale sidecar exited {how}, so flux is off the tailnet. \
                     Turn remote access off and on again, or restart flux, to reconnect."
                ),
            });
        }
        _ => {}
    }
}

/// Serve every connection `listener` accepts until shutdown: WhoIs identity
/// from the bridge header → role → `Option<Identity>` extension; path from
/// `paths`. Connections are admitted per tailnet caller IP like the TCP
/// listeners' ([`crate::ServeLimits`]).
pub(crate) async fn serve_listener(
    mut listener: RawListener,
    shared: Arc<Shared>,
    paths: PathLookup,
    tailnet_name: Option<String>,
    scope: Scope,
) {
    loop {
        let incoming = tokio::select! {
            incoming = listener.accept() => incoming,
            _ = scope.token.cancelled() => break,
        };
        let Some(incoming) = incoming else { break };
        let remote: Option<SocketAddr> = incoming.remote_addr.parse().ok();
        // An unparseable peer address shares one bucket.
        let caller = remote.map_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |a| a.ip());
        let Some(permit) = shared.admission.try_admit(caller) else {
            crate::limits::refuse(incoming.stream);
            continue;
        };
        // Nagle off: HLS segments and API replies are latency-bound, and the
        // bridge's loopback hop otherwise delays the tail of each response.
        let _ = incoming.stream.set_nodelay(true);
        let shared_conn = shared.clone();
        let paths = paths.clone();
        let tailnet_name = tailnet_name.clone();
        let token = scope.token.clone();
        scope.tracker.spawn(async move {
            let _permit = permit;
            let path = match remote {
                Some(addr) => paths(canonical_ip(addr.ip())).await,
                None => NetPath::Tailnet,
            };
            let facts = incoming.remote_identity.map(PeerFacts::from);
            let (identity, grants) = {
                let policy = shared_conn.policy();
                match &facts {
                    Some(f) => (
                        Some(policy.identity_for(f)),
                        f.grants(&policy.capability).to_vec(),
                    ),
                    None => (None, Vec::new()),
                }
            };
            let conn = ConnInfo {
                path,
                remote_addr: remote,
                tailnet_name,
                grants,
            };
            crate::serve::serve_connection(
                incoming.stream,
                shared_conn.router.clone(),
                identity,
                conn,
                token,
                shared_conn.write_timeout,
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::RolePolicy;
    use crate::test_util;
    use axum::routing::get;
    use axum::{Extension, Json, Router};
    use flux_core::api::{Identity, Role};
    use truffle_core::network::TailscalePeerIdentity;
    use truffle_core::transport::RawIncoming;

    fn whoami_router() -> Router {
        Router::new().route(
            "/whoami",
            get(|Extension(you): Extension<Option<Identity>>, Extension(conn): Extension<ConnInfo>| async move {
                Json(serde_json::json!({
                    "you": you,
                    "path": conn.path.as_str(),
                    "server": conn.tailnet_name,
                    "remote": conn.remote_addr.map(|a| a.to_string()),
                    "grants": conn.grants,
                }))
            }),
        )
    }

    fn running() -> TailnetStatus {
        TailnetStatus::Running(TailnetInfo {
            mode: TailnetMode::Embedded,
            dns_name: Some("truffle-flux-mac.tail1234.ts.net".into()),
            addrs: vec!["100.64.0.1:8096".parse().unwrap()],
            tls: false,
            tls_error: None,
            owner_login: None,
            key_expiry: None,
            control_server: None,
        })
    }

    /// Truffle's protocol-10 events: the control server decides the approve
    /// link (Headscale has no admin console), the key expiry reaches the
    /// running status and the API, and a sidecar that exits fails the status.
    #[test]
    fn network_events_set_approval_link_key_expiry_and_failure() {
        use flux_core::api::{TailnetMode as Mode, TailnetState as State};
        let shared = Shared::for_test(Router::new(), RolePolicy::default());
        let status = shared.status.subscribe();
        let control = Mutex::new(ControlServer::from_url(""));

        // Tailscale's hosted service: link to its Machines page.
        on_network_event(&shared, &control, NetworkStatusEvent::NeedsApproval);
        assert_eq!(
            *status.borrow(),
            TailnetStatus::NeedsApproval {
                approve_url: Some(crate::APPROVE_URL.into())
            }
        );
        // Headscale: no link.
        on_network_event(
            &shared,
            &control,
            NetworkStatusEvent::ControlServerKnown {
                control_server: ControlServer::from_url("https://hs.example.com"),
            },
        );
        on_network_event(&shared, &control, NetworkStatusEvent::NeedsApproval);
        assert_eq!(
            *status.borrow(),
            TailnetStatus::NeedsApproval { approve_url: None }
        );
        assert_eq!(status.borrow().to_api(Mode::Embedded).approve_url, None);

        // The key expiry only changes a running status.
        let at = std::time::UNIX_EPOCH + Duration::from_millis(1_790_000_000_000);
        on_network_event(
            &shared,
            &control,
            NetworkStatusEvent::KeyExpiryKnown {
                key_expiry: KeyExpiry::At(at),
            },
        );
        assert!(matches!(*status.borrow(), TailnetStatus::NeedsApproval { .. }));
        shared.set_status(running());
        on_network_event(
            &shared,
            &control,
            NetworkStatusEvent::KeyExpiryKnown {
                key_expiry: KeyExpiry::At(at),
            },
        );
        on_network_event(
            &shared,
            &control,
            NetworkStatusEvent::ControlServerKnown {
                control_server: ControlServer::from_url("https://hs.example.com"),
            },
        );
        let api = status.borrow().to_api(Mode::Embedded);
        assert_eq!(
            (api.state, api.key_expiry),
            (State::Running, Some(1_790_000_000_000))
        );
        match &*status.borrow() {
            TailnetStatus::Running(t) => {
                assert_eq!(t.control_server.as_deref(), Some("https://hs.example.com"))
            }
            other => panic!("{other:?}"),
        }
        // Expiry disabled by an admin: none.
        on_network_event(
            &shared,
            &control,
            NetworkStatusEvent::KeyExpiryKnown {
                key_expiry: KeyExpiry::Never,
            },
        );
        assert_eq!(status.borrow().to_api(Mode::Embedded).key_expiry, None);

        // The sidecar died: off the tailnet, and the UI says so.
        on_network_event(
            &shared,
            &control,
            NetworkStatusEvent::SidecarExited { exit_code: Some(2) },
        );
        let api = status.borrow().to_api(Mode::Embedded);
        assert_eq!(api.state, State::Error);
        assert!(api.last_error.unwrap_or_default().contains("code 2"));
    }

    /// A loopback pair standing in for one bridged tailnet connection.
    async fn pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    /// The embedded path end to end minus the node: a Truffle RawListener
    /// delivers a connection whose bridge header carried a WhoIs identity
    /// with a flux capability grant; the handler sees the derived Identity,
    /// the grant values, the path from the lookup and the server's name.
    #[tokio::test]
    async fn tailnet_connections_carry_identity_role_and_path() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let listener = RawListener::new(rx, 8096);
        let policy = RolePolicy {
            owner_login: Some("owner@example.com".into()),
            ..Default::default()
        };
        let shared = Shared::for_test(whoami_router(), policy);
        let paths: PathLookup = Arc::new(|ip: IpAddr| {
            Box::pin(async move {
                if ip == "100.64.0.9".parse::<IpAddr>().unwrap() {
                    NetPath::TailnetRelay
                } else {
                    NetPath::Tailnet
                }
            })
        });
        let task = tokio::spawn(serve_listener(
            listener,
            shared.clone(),
            paths,
            Some("truffle-flux-mac-mini.tail1234.ts.net".into()),
            shared.tailnet_scope(),
        ));

        // 1. A friend whose tailnet grant makes them a kid.
        let (mut client, server) = pair().await;
        let mut identity = TailscalePeerIdentity {
            dns_name: Some("ipad.tail1234.ts.net".into()),
            login_name: Some("friend@example.com".into()),
            display_name: Some("Friend".into()),
            node_id: Some("nIPAD".into()),
            ..Default::default()
        };
        identity.cap_map.insert(
            crate::DEFAULT_CAPABILITY.into(),
            vec![serde_json::json!({"role": "kid", "libraries": ["Cartoons"]})],
        );
        tx.send(RawIncoming {
            stream: server,
            remote_addr: "100.64.0.9:51000".into(),
            remote_identity: Some(identity),
        })
        .await
        .unwrap();
        let resp = test_util::get(&mut client, "/whoami").await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["you"]["login_name"], "friend@example.com");
        assert_eq!(v["you"]["display_name"], "Friend");
        assert_eq!(v["you"]["node_name"], "ipad.tail1234.ts.net");
        assert_eq!(v["you"]["role"], "kid");
        assert_eq!(v["path"], "tailnet-relay");
        assert_eq!(v["server"], "truffle-flux-mac-mini.tail1234.ts.net");
        assert_eq!(v["remote"], "100.64.0.9:51000");
        assert_eq!(v["grants"][0]["libraries"][0], "Cartoons");

        // 2. The owner, no grants: Owner by node ownership.
        let (mut client, server) = pair().await;
        let owner = TailscalePeerIdentity {
            login_name: Some("owner@example.com".into()),
            ..Default::default()
        };
        tx.send(RawIncoming {
            stream: server,
            remote_addr: "100.64.0.2:51001".into(),
            remote_identity: Some(owner),
        })
        .await
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&test_util::get(&mut client, "/whoami").await.body).unwrap();
        assert_eq!(v["you"]["role"], serde_json::to_value(Role::Owner).unwrap());
        assert_eq!(v["path"], "tailnet");

        // 3. No identity in the bridge header (WhoIs failed): anonymous.
        let (mut client, server) = pair().await;
        tx.send(RawIncoming {
            stream: server,
            remote_addr: "100.64.0.3:51002".into(),
            remote_identity: None,
        })
        .await
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&test_util::get(&mut client, "/whoami").await.body).unwrap();
        assert!(v["you"].is_null());

        shared.shutdown.cancel();
        task.await.unwrap();
    }

    #[test]
    fn sidecar_resolution_reports_what_it_tried() {
        let missing = Path::new("/nonexistent/flux/sidecar-slim");
        let err = resolve_sidecar(Some(missing)).unwrap_err().to_string();
        assert!(
            err.contains("/nonexistent/flux/sidecar-slim")
                && err.contains("TRUFFLE_SIDECAR_PATH")
                && err.contains("go build")
                && !err.contains("worktree"),
            "{err}"
        );
        let exe = std::env::current_exe().unwrap();
        assert_eq!(resolve_sidecar(Some(&exe)).unwrap(), exe);
    }

    /// The .deb, .rpm and Homebrew keep the sidecar out of the folder on PATH.
    #[test]
    fn packages_keep_the_sidecar_in_libexec() {
        let d = std::env::temp_dir().join(format!("flux-sidecar-places-{}", std::process::id()));
        let bin = d.join("bin");
        let libexec = d.join("libexec/flux");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&libexec).unwrap();
        std::fs::write(libexec.join(sidecar_file_name()), b"").unwrap();
        let found = sidecar_places(&bin).into_iter().find(|p| p.is_file()).unwrap();
        assert_eq!(
            found.canonicalize().unwrap(),
            libexec.join(sidecar_file_name()).canonicalize().unwrap()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Windows looked for `sidecar-slim` without `.exe`, so a sidecar
    /// shipped next to flux-server.exe was never found.
    #[test]
    fn the_sidecar_next_to_the_binary_has_the_platform_suffix() {
        let name = sidecar_file_name();
        if cfg!(windows) {
            assert_eq!(name, "sidecar-slim.exe");
        } else {
            assert_eq!(name, "sidecar-slim");
        }
        let missing = Path::new("/nonexistent/flux/sidecar");
        let err = resolve_sidecar(Some(missing)).unwrap_err().to_string();
        let exe = std::env::current_exe().unwrap();
        let next_to_exe = exe.canonicalize().unwrap_or(exe).with_file_name(&name);
        if std::env::var_os("TRUFFLE_SIDECAR_PATH").is_none() && !next_to_exe.exists() {
            assert!(err.contains(&next_to_exe.display().to_string()), "{err}");
        }
        assert!(err.contains(&format!("next to flux-server as {name}")), "{err}");
    }

    #[test]
    fn embedded_defaults_suit_a_media_server() {
        let cfg = EmbeddedConfig::new(Path::new("/var/lib/flux"), "Mac mini");
        assert_eq!(cfg.state_dir, Path::new("/var/lib/flux/tailscale"));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(6 * 3600));
        assert_eq!(cfg.start_timeout, None, "headless first run waits for the login");
        assert!(
            cfg.hostname.is_none(),
            "keep the truffle-flux-* discovery convention"
        );
    }
}
