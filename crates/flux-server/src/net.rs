//! Serving: the router on flux-net's listeners (never `axum::serve`: hyper
//! with `half_close(true)`), and the tailnet state the API reports.
//!
//! The tailnet mode can change at runtime (`POST /api/v1/tailnet/connect`):
//! `FluxNet::set_tailnet` replaces the tailnet listener alone (its in-flight
//! requests get 2 s to finish); the local and LAN listeners and their
//! connections carry on.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use axum::Router;
use flux_core::api::{self, Event, TailnetInfo};
use flux_net::{
    FluxNet, FluxNetConfig, NetError, RolePolicy, ServeLimits, SystemConfig, TailnetConfig, TailnetStatus,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{Config, TailnetChoice};
use crate::state::AppState;

/// The tailnet state as the API reports it.
#[derive(Default)]
pub struct TailnetView {
    current: RwLock<Option<(watch::Receiver<TailnetStatus>, api::TailnetMode)>>,
    /// A system tailscaled is running on this machine (probed once).
    system_tailscale: AtomicBool,
    control: OnceLock<mpsc::Sender<NetCmd>>,
    /// Bumped whenever another listener's status is attached, so a
    /// replaced listener's follower stops publishing.
    generation: AtomicU64,
}

impl TailnetView {
    /// `GET /api/v1/tailnet`.
    pub fn info(&self) -> TailnetInfo {
        let mut info = match &*self.current.read().unwrap_or_else(|p| p.into_inner()) {
            Some((rx, mode)) => rx.borrow().to_api(*mode),
            None => TailnetStatus::Off.to_api(api::TailnetMode::Off),
        };
        info.system_tailscale |= self.system_tailscale.load(Ordering::Relaxed);
        info
    }

    /// The configured mode.
    pub fn mode(&self) -> api::TailnetMode {
        self.current
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map_or(api::TailnetMode::Off, |(_, m)| *m)
    }

    fn attach(&self, net: &FluxNet) -> (watch::Receiver<TailnetStatus>, u64) {
        let rx = net.tailnet_status();
        let mut current = self.current.write().unwrap_or_else(|p| p.into_inner());
        *current = Some((rx.clone(), net.tailnet_mode()));
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        (rx, generation)
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    /// Asks the listener supervisor to switch modes. `false` when the server
    /// is not serving (tests that drive the router directly).
    pub fn request_mode(&self, choice: TailnetChoice) -> bool {
        self.control
            .get()
            .is_some_and(|tx| tx.try_send(NetCmd::SetMode(choice)).is_ok())
    }

    pub fn set_system_tailscale(&self, running: bool) {
        self.system_tailscale.store(running, Ordering::Relaxed);
    }
}

enum NetCmd {
    SetMode(TailnetChoice),
    Shutdown(Duration, oneshot::Sender<()>),
}

/// The running listeners.
pub struct NetControl {
    tx: mpsc::Sender<NetCmd>,
    pub local_addr: SocketAddr,
    pub lan_addr: Option<SocketAddr>,
}

impl NetControl {
    /// Stops accepting and lets in-flight requests finish for up to `grace`.
    pub async fn shutdown(&self, grace: Duration) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(NetCmd::Shutdown(grace, tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

fn tailnet_config(choice: TailnetChoice, cfg: &Config, port: u16) -> TailnetConfig {
    let port = if port == 0 { flux_net::DEFAULT_PORT } else { port };
    match choice {
        TailnetChoice::Off => TailnetConfig::Off,
        TailnetChoice::Embedded => {
            let mut e = flux_net::EmbeddedConfig::new(&cfg.data_dir, cfg.name.clone());
            e.port = port;
            TailnetConfig::Embedded(e)
        }
        TailnetChoice::System => TailnetConfig::System(SystemConfig {
            port,
            ..SystemConfig::default()
        }),
    }
}

/// Starts flux-net with `router` and a supervisor that can restart it in
/// another tailnet mode. Errors only if the local/LAN listeners can't bind.
pub async fn start(
    cfg: &Config,
    choice: TailnetChoice,
    router: Router,
    st: Arc<AppState>,
) -> Result<NetControl, NetError> {
    let nofile = raise_nofile_limit();
    let limits = serve_limits(nofile);
    tracing::info!(
        ?nofile,
        max_connections = limits.max_connections,
        per_ip = limits.max_connections_per_ip,
        write_timeout = ?limits.write_timeout,
        "connection limits"
    );
    let config = FluxNetConfig {
        local_addr: cfg.local_addr,
        lan_addr: cfg.lan_addr,
        tailnet: tailnet_config(choice, cfg, cfg.local_addr.port()),
        roles: RolePolicy {
            localhost_role: cfg.localhost_role,
            owner_login: cfg.owner_login.clone(),
            ..RolePolicy::default()
        },
        limits,
    };
    let mut net = FluxNet::start(config, router).await?;
    let local_addr = net.local_addr();
    let lan_addr = net.lan_addr();
    follow_tailnet(&st, &net);

    let (tx, mut rx) = mpsc::channel::<NetCmd>(4);
    let _ = st.tailnet.control.set(tx.clone());
    let cfg = cfg.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                NetCmd::SetMode(choice) => {
                    // The tailnet listener serves on the local port.
                    let tailnet = tailnet_config(choice, &cfg, local_addr.port());
                    net.set_tailnet(tailnet, Duration::from_secs(2)).await;
                    tracing::info!(mode = choice.as_str(), "tailnet listener restarted");
                    // Same status channel, new mode.
                    follow_tailnet(&st, &net);
                }
                NetCmd::Shutdown(grace, reply) => {
                    net.shutdown(grace).await;
                    let _ = reply.send(());
                    break;
                }
            }
        }
    });
    Ok(NetControl {
        tx,
        local_addr,
        lan_addr,
    })
}

/// Publishes the listener's tailnet state (and its names, for the Host
/// allowlist) and forwards its changes to SSE.
fn follow_tailnet(st: &Arc<AppState>, net: &FluxNet) {
    let (mut rx, generation) = st.tailnet.attach(net);
    allow_tailnet_hosts(st, &rx.borrow_and_update());
    let st = st.clone();
    tokio::spawn(async move {
        // Ends when this listener is replaced or shut down (sender dropped).
        while rx.changed().await.is_ok() {
            if !st.tailnet.is_current(generation) {
                break;
            }
            allow_tailnet_hosts(&st, &rx.borrow_and_update());
            st.events.send(Event::Tailnet {
                info: st.tailnet.info(),
            });
        }
    });
}

/// The running tailnet's MagicDNS name and addresses are hosts this server
/// answers to; none while it isn't running.
fn allow_tailnet_hosts(st: &AppState, status: &TailnetStatus) {
    let (names, ips): (Vec<String>, Vec<IpAddr>) = match status {
        TailnetStatus::Running(info) => (
            info.dns_name.iter().cloned().collect(),
            info.addrs.iter().map(SocketAddr::ip).collect(),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    st.hosts.set_tailnet(&names, &ips);
}

/// Connection limits that fit the process's open-files limit (`nofile`):
/// room for the catalog, media files, ffmpeg pipes and loopback's own budget.
pub fn serve_limits(nofile: Option<u64>) -> ServeLimits {
    let mut limits = ServeLimits::default();
    if let Some(n) = nofile {
        let per_ip = (n / 8).clamp(4, limits.max_connections_per_ip as u64);
        let total = n.saturating_sub(128 + 2 * per_ip).max(16);
        limits.max_connections_per_ip = per_ip as usize;
        limits.max_connections = limits.max_connections.min(total as usize);
    }
    limits
}

/// Raises the soft open-files limit to the hard limit (on macOS also capped
/// by `kern.maxfilesperproc`; launchd starts processes at 256) and returns
/// the soft limit now in effect.
#[cfg(unix)]
#[allow(unsafe_code, clippy::unnecessary_cast)]
#[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
pub fn raise_nofile_limit() -> Option<u64> {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit/sysctlbyname only read and write the
    // values we pass by pointer, which live on this stack frame.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 {
            return None;
        }
        // Plenty for a home server, and accepted by every kernel.
        let mut want = rl.rlim_max.min(65_536);
        #[cfg(target_os = "macos")]
        {
            let mut max: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>();
            let rc = libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&mut max as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            );
            if rc == 0 && max > 0 {
                want = want.min(max as libc::rlim_t);
            }
        }
        if want > rl.rlim_cur {
            let raised = libc::rlimit {
                rlim_cur: want,
                rlim_max: rl.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                return Some(want as u64);
            }
        }
        Some(rl.rlim_cur as u64)
    }
}

#[cfg(not(unix))]
pub fn raise_nofile_limit() -> Option<u64> {
    None
}

/// Looks once (read-only `tailscale status --json`) whether this machine
/// runs tailscaled, for `TailnetInfo::system_tailscale`.
pub fn probe_system_tailscale(st: Arc<AppState>) {
    tokio::spawn(async move {
        let client =
            flux_net::SystemTailscale::new(flux_net::system::find_cli(None), Duration::from_secs(60));
        let running = matches!(
            tokio::time::timeout(Duration::from_secs(5), client.status()).await,
            Ok(Ok(s)) if s.is_running()
        );
        st.tailnet.set_system_tailscale(running);
        tracing::debug!(running, "system tailscaled probe");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_fit_the_open_files_limit() {
        let roomy = serve_limits(Some(65_536));
        assert_eq!(roomy, ServeLimits::default());
        // launchd's default soft limit, if it can't be raised.
        let tight = serve_limits(Some(256));
        assert_eq!((tight.max_connections, tight.max_connections_per_ip), (64, 32));
        assert!(tight.max_connections + 2 * tight.max_connections_per_ip + 64 <= 256);
        assert_eq!(serve_limits(None), ServeLimits::default());
        // Never below a usable floor.
        let tiny = serve_limits(Some(64));
        assert!(tiny.max_connections >= 16 && tiny.max_connections_per_ip >= 4);
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // rlim_t is u64 on some targets only
    fn the_open_files_limit_is_raised_not_lowered() {
        #[cfg(unix)]
        {
            let mut rl = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            #[allow(unsafe_code)]
            // SAFETY: getrlimit writes into the struct we own.
            let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
            assert_eq!(rc, 0);
            let now = raise_nofile_limit().expect("getrlimit works");
            assert!(now >= rl.rlim_cur.min(65_536) as u64, "{now} < {}", rl.rlim_cur);
        }
    }
}
