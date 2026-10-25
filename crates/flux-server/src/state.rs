//! Shared server state and the per-request caller.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use flux_catalog::Catalog;
use flux_core::api::{Id, Identity, Role};
use flux_hls::HlsEngine;
use flux_net::{ConnInfo, NetPath};
use tokio_util::sync::CancellationToken;

use crate::art::ArtStore;
use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::events::EventBus;
use crate::guard::HostAllowlist;
use crate::net::TailnetView;
use crate::play::PlayState;
use crate::scan::ScanService;
use crate::web::WebUi;

/// Everything handlers share (`State<Arc<AppState>>`).
pub struct AppState {
    pub cfg: Config,
    pub catalog: Arc<Catalog>,
    pub scanner: Arc<dyn ScanService>,
    pub engine: HlsEngine,
    pub events: EventBus,
    pub play: PlayState,
    pub art: Arc<ArtStore>,
    pub web: Option<WebUi>,
    pub users: Users,
    pub tailnet: TailnetView,
    /// The host names this server answers to (`guard`).
    pub hosts: HostAllowlist,
    pub activity: Activity,
    /// MCP sessions (`/mcp`): which agent each client is.
    pub mcp: crate::agent::mcp::Sessions,
    pub started: Instant,
    /// Cancelled on shutdown: ends SSE streams and background loops.
    pub shutdown: CancellationToken,
}

impl AppState {
    /// The catalog user for a caller: tailnet/localhost identities by login;
    /// anonymous callers (LAN, unknown tailnet peers) share one guest user
    /// so a living-room TV still gets resume points.
    pub async fn user_id(&self, caller: &Caller) -> ApiResult<Id> {
        let who = caller.identity.clone().unwrap_or_else(anonymous_identity);
        self.users.resolve(&self.catalog, who).await
    }
}

/// The identity anonymous callers share.
pub fn anonymous_identity() -> Identity {
    Identity {
        login_name: "anonymous".into(),
        display_name: "Guest".into(),
        profile_pic_url: None,
        node_name: None,
        role: Role::Guest,
        tags: Vec::new(),
    }
}

/// Caches tailnet login → catalog user id, so `ensure_user` (a writer
/// round trip) runs once per login (again when the name or role changes).
#[derive(Default)]
pub struct Users {
    map: Mutex<HashMap<String, (Id, String, Role)>>,
}

impl Users {
    pub async fn resolve(&self, catalog: &Catalog, who: Identity) -> ApiResult<Id> {
        if let Some((id, name, role)) = self.lock().get(&who.login_name) {
            if *name == who.display_name && *role == who.role {
                return Ok(*id);
            }
        }
        let (login, name, role) = (who.login_name.clone(), who.display_name.clone(), who.role);
        let id = catalog.writer().ensure_user(who).await?;
        self.lock().insert(login, (id, name, role));
        Ok(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Id, String, Role)>> {
        self.map.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Recent playback activity, so background jobs back off while someone
/// watches, and a nudge for the job loop when a job was moved up.
#[derive(Default)]
pub struct Activity {
    /// Unix ms of the last media request.
    last_media_ms: AtomicI64,
    jobs: tokio::sync::Notify,
}

impl Activity {
    /// A job was moved up (`Writer::bump_art`): the idle job loop looks
    /// again now instead of at its next poll.
    pub fn wake_jobs(&self) {
        self.jobs.notify_one();
    }

    /// Resolves after [`Activity::wake_jobs`] (or at once if a wake-up is
    /// pending).
    pub async fn jobs_woken(&self) {
        self.jobs.notified().await;
    }

    pub fn touch_media(&self) {
        self.last_media_ms
            .store(flux_catalog::now_ms(), Ordering::Relaxed);
    }

    /// Unix ms of the last media request (0: none yet).
    pub fn last_media_ms(&self) -> i64 {
        self.last_media_ms.load(Ordering::Relaxed)
    }

    /// Milliseconds since the last media request (`i64::MAX` if none).
    pub fn media_idle_ms(&self) -> i64 {
        match self.last_media_ms.load(Ordering::Relaxed) {
            0 => i64::MAX,
            t => flux_catalog::now_ms().saturating_sub(t),
        }
    }
}

/// Who is calling and how they connected (flux-net attaches both to every
/// request). Requests that didn't come through flux-net (router tests) are
/// anonymous LAN callers.
#[derive(Debug, Clone)]
pub struct Caller {
    pub identity: Option<Identity>,
    pub conn: ConnInfo,
}

impl Caller {
    /// Owners and admins may add libraries, start scans and change remote access.
    pub fn is_admin(&self) -> bool {
        matches!(
            self.identity.as_ref().map(|i| i.role),
            Some(Role::Owner | Role::Admin)
        )
    }

    pub fn require_admin(&self) -> ApiResult<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(ApiError::forbidden(
                "Only the server's owner or an admin can do that",
            ))
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let identity = parts.extensions.get::<Option<Identity>>().cloned().flatten();
        let conn = parts.extensions.get::<ConnInfo>().cloned().unwrap_or(ConnInfo {
            path: NetPath::Lan,
            remote_addr: None,
            tailnet_name: None,
            grants: Vec::new(),
        });
        Ok(Caller { identity, conn })
    }
}
