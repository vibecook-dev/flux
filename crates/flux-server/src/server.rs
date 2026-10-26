//! Startup and shutdown.
//!
//! ```text
//! data dir → catalog (migrate) → libraries from the config → HlsEngine
//!   → scanner → web UI → router → flux-net listeners        = "API ready"
//! then, in the background: watch every library (the first watch catches up
//!   with a Quick scan at Scheduled priority), background jobs (after a
//!   delay), the video encoder detection, tailscaled probe.
//! ```

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use flux_catalog::{read, Catalog, CatalogConfig, CatalogError};
use flux_core::api::{CreateLibrary, Event};
use flux_hls::{EngineConfig, HlsEngine};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::art::ArtStore;
use crate::config::{persisted_tailnet, persisted_tailnet_https, Config, LibrarySpec};
use crate::events::EventBus;
use crate::guard::HostAllowlist;
use crate::net::{NetControl, TailnetView};
use crate::play::PlayState;
use crate::scan::ScanService;
use crate::state::{Activity, AppState, Users};
use crate::web::WebUi;
use crate::{api, jobs, net, scan};

/// A running server.
pub struct Server {
    pub state: Arc<AppState>,
    pub local_addr: SocketAddr,
    pub lan_addr: Option<SocketAddr>,
    /// From `t0` (process start) until the listeners accepted connections.
    pub ready_ms: f64,
    net: NetControl,
    tasks: Vec<JoinHandle<()>>,
}

/// Builds the shared state without listening (router tests drive
/// [`crate::api::router`] directly). `scanner` overrides the configured one.
pub async fn build_state(
    cfg: Config,
    t0: Instant,
    scanner: Option<Arc<dyn ScanService>>,
) -> anyhow::Result<(Arc<AppState>, Option<broadcast::Receiver<Event>>)> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating the data dir {}", cfg.data_dir.display()))?;
    let db = cfg.catalog_path();
    let catalog = tokio::task::spawn_blocking(move || Catalog::open(&db, CatalogConfig::default()))
        .await?
        .with_context(|| format!("opening the catalog {}", cfg.catalog_path().display()))?;
    let catalog = Arc::new(catalog);
    ensure_libraries(&catalog, &cfg.libraries).await?;

    let engine = HlsEngine::new(EngineConfig {
        // The catalog caches playback indexes (asset.play_index).
        index_cache_dir: None,
        ffmpeg: cfg.ffmpeg.clone(),
        ffprobe: cfg.ffprobe.clone(),
        video_encoder: cfg.video_encoder,
        vaapi_device: cfg.vaapi_device.clone(),
        max_video_transcodes: cfg.max_video_transcodes,
        ..EngineConfig::default()
    });
    let (scanner, scan_events) = match scanner {
        Some(s) => {
            let rx = s.subscribe();
            (s, rx)
        }
        None => scan::start(catalog.clone(), cfg.scan, cfg.ffprobe.clone()),
    };
    let web = cfg.web_dir.as_deref().and_then(WebUi::load);
    match (&cfg.web_dir, &web) {
        (Some(dir), None) => {
            tracing::warn!(dir = %dir.display(), "no built web client there (index.html missing)")
        }
        (None, _) => tracing::info!("no web client configured (--web-dir); serving the API only"),
        _ => {}
    }
    let state = Arc::new(AppState {
        play: PlayState::new(&cfg.data_dir, engine.config().asset_cache_bytes),
        art: Arc::new(ArtStore::new(&cfg.data_dir)),
        catalog,
        scanner,
        engine,
        events: EventBus::default(),
        web,
        users: Users::default(),
        tailnet: TailnetView::default(),
        hosts: HostAllowlist::new(&crate::config::host_name(), &cfg.allow_hosts),
        activity: Activity::default(),
        mcp: Default::default(),
        started: t0,
        shutdown: CancellationToken::new(),
        cfg,
    });
    Ok((state, scan_events))
}

/// Starts a server (see the module docs); `t0` is when the process started.
pub async fn start_at(cfg: Config, t0: Instant) -> anyhow::Result<Server> {
    start_with(cfg, t0, None).await
}

/// [`start_at`] with a scanner of the caller's (tests).
pub async fn start_with(
    cfg: Config,
    t0: Instant,
    scanner: Option<Arc<dyn ScanService>>,
) -> anyhow::Result<Server> {
    let (state, scan_events) = build_state(cfg.clone(), t0, scanner).await?;
    let choice = if cfg.tailnet_explicit {
        cfg.tailnet
    } else {
        persisted_tailnet(&cfg.data_dir).unwrap_or(cfg.tailnet)
    };
    let https = if cfg.tailnet_https_explicit {
        cfg.tailnet_https
    } else {
        persisted_tailnet_https(&cfg.data_dir).unwrap_or(cfg.tailnet_https)
    };
    let mut tasks = Vec::new();
    if let Some(rx) = scan_events {
        tasks.push(state.events.forward(rx));
    }
    let router = api::router(state.clone());
    let net = net::start(&cfg, choice, https, router, state.clone())
        .await
        .context("starting the listeners")?;
    let ready_ms = t0.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(
        url = %format!("http://{}", net.local_addr),
        lan = ?net.lan_addr,
        tailnet = choice.as_str(),
        tailnet_https = https,
        ready_ms = format!("{ready_ms:.1}"),
        "flux is ready"
    );
    crate::metadata::log_startup(&cfg.tmdb_exports);
    write_server_file(&cfg.data_dir, net.local_addr);

    // Background work, after the API is up.
    {
        let st = state.clone();
        tasks.push(tokio::spawn(async move {
            let s = st.clone();
            let _ = tokio::task::spawn_blocking(move || s.play.prune_plan_files()).await;
            // Items an upgrade by an earlier build left with no file (a
            // re-parse moved them all elsewhere), hidden ever since: they
            // go, with their images. Their users' state went with the files.
            match st.catalog.writer().purge_empty_items().await {
                Ok(ids) if !ids.is_empty() => {
                    tracing::info!(
                        items = ids.len(),
                        "deleted items an earlier upgrade left without files"
                    )
                }
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "purge_empty_items"),
            }
            if st.cfg.scan {
                // watch() catches up with a Quick scan (Scheduled priority)
                // the first time, then follows changes. Without watching,
                // the startup Quick scan runs on its own.
                //
                // After an upgrade (files parsed by an older flux-names or
                // probed by an older flux-probe) the startup scan is a Deep
                // one: it also re-parses those files (no I/O) and re-probes
                // them, which a Quick scan never does for unchanged files.
                // The watcher starts first and without its catch-up: the
                // Deep scan covers that, and a catch-up joining it once it
                // had started ran a second pass that counted every file
                // twice.
                let libs = st.catalog.read(read::list_libraries).await.unwrap_or_default();
                for lib in libs {
                    let id = lib.id;
                    let outdated = st
                        .catalog
                        .read(move |c| read::outdated_files(c, id))
                        .await
                        .unwrap_or(0);
                    let mode = if outdated > 0 {
                        tracing::info!(
                            library_id = id,
                            files = outdated,
                            "files cataloged by an older flux: bringing them up to date"
                        );
                        Some(scan::ScanMode::Deep)
                    } else if st.cfg.watch {
                        None
                    } else {
                        Some(scan::ScanMode::Quick)
                    };
                    if st.cfg.watch {
                        match mode {
                            Some(_) => st.scanner.watch_without_catch_up(id),
                            None => st.scanner.watch(id),
                        }
                    }
                    if let Some(mode) = mode {
                        if let Err(e) = st.scanner.scan_library(id, mode, scan::Priority::Scheduled) {
                            tracing::info!(library_id = id, "not scanning: {e}");
                        }
                    }
                }
            }
        }));
    }
    if cfg.jobs {
        tasks.push(jobs::spawn(state.clone()));
    }
    // Idle playback gives the caches' memory back.
    tasks.push(tokio::spawn(crate::play::trim_when_idle(
        state.clone(),
        cfg.cache_idle,
    )));
    if cfg.detect_encoder_at_start {
        // Which H.264 encoder video transcodes use (logged), so the first
        // transcode doesn't wait for the probes and test encodes.
        let engine = state.engine.clone();
        tasks.push(tokio::spawn(async move {
            engine.video_caps().await;
        }));
    }
    if cfg.probe_system_tailscale {
        net::probe_system_tailscale(state.clone());
    }
    Ok(Server {
        local_addr: net.local_addr,
        lan_addr: net.lan_addr,
        ready_ms,
        net,
        tasks,
        state,
    })
}

/// [`start_at`] with the clock starting now.
pub async fn start(cfg: Config) -> anyhow::Result<Server> {
    start_at(cfg, Instant::now()).await
}

impl Server {
    /// Graceful shutdown: stop accepting, let in-flight requests finish for
    /// up to `grace`, stop scans/jobs/transcodes, commit and close the catalog.
    pub async fn shutdown(self, grace: Duration) {
        let Server {
            state, net, tasks, ..
        } = self;
        state.shutdown.cancel();
        remove_server_file(&state.cfg.data_dir);
        net.shutdown(grace).await;
        let scanner = state.scanner.clone();
        let _ = tokio::task::spawn_blocking(move || scanner.shutdown(grace)).await;
        state.engine.shutdown();
        for t in tasks {
            t.abort();
        }
        let w = state.catalog.writer().clone();
        if let Err(e) = w.flush().await {
            tracing::debug!(error = %e, "catalog flush at shutdown");
        }
        if let Err(e) = w.shutdown().await {
            tracing::debug!(error = %e, "catalog writer shutdown");
        }
        tracing::info!("flux stopped");
    }
}

/// The file in the data dir that tells the `flux` command line where this
/// server listens (`server.json`): the local address, the process and the
/// version. A stale one (a server that died) is harmless: the CLI tries
/// the address, then the default.
pub const SERVER_FILE: &str = "server.json";

fn write_server_file(data_dir: &Path, local: SocketAddr) {
    let host = match local.ip() {
        ip if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        ip => ip.to_string(),
    };
    let body = serde_json::json!({
        "url": format!("http://{host}:{}", local.port()),
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    let path = data_dir.join(SERVER_FILE);
    let tmp = path.with_extension("json.part");
    let ok = std::fs::write(&tmp, body.to_string()).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = ok {
        tracing::debug!(error = %e, "could not write {}", path.display());
    }
}

fn remove_server_file(data_dir: &Path) {
    let path = data_dir.join(SERVER_FILE);
    // Only our own: another server may have started on this data dir since.
    let ours = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["pid"] == std::process::id());
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

/// Creates the configured libraries that don't exist yet (matched by name).
/// A folder that another library already has (or one inside or around it)
/// stops the start with the reason, rather than cataloging its files twice.
async fn ensure_libraries(catalog: &Catalog, specs: &[LibrarySpec]) -> anyhow::Result<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let existing = catalog.read(read::list_libraries).await?;
    for s in specs {
        if existing.iter().any(|l| l.name == s.name) {
            continue;
        }
        if !Path::new(&s.path).exists() {
            tracing::warn!(library = %s.name, path = %s.path, "library folder not found; it will show as unavailable");
        }
        let created = match crate::roots::check_new_roots(catalog, std::slice::from_ref(&s.path)).await {
            Ok(()) => {
                catalog
                    .writer()
                    .create_library(CreateLibrary {
                        name: s.name.clone(),
                        kind: s.kind,
                        roots: vec![s.path.clone()],
                    })
                    .await
            }
            Err(e) => Err(e),
        };
        let lib = created.map_err(|e| match e {
            e @ CatalogError::RootInOtherLibrary { .. } => anyhow::anyhow!(
                "library {:?}: {e} Change or remove its --library flag or config entry.",
                s.name
            ),
            e => e.into(),
        })?;
        tracing::info!(library = %lib.name, id = lib.id, "library created");
    }
    Ok(())
}
