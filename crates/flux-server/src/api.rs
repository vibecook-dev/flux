//! The `/api/v1` routes of `flux_core::api` (browse, libraries, tailnet) and
//! the router that ties every module together.

use std::path::Path;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use flux_catalog::read;
use flux_core::api::{
    CreateLibrary, FolderPage, FolderParams, Home, Id, ItemDetail, ItemSummary, ItemsParams, Library,
    MarkWatched, Page, ScanStatus, ServerInfo, TailnetConnect, TailnetInfo, UpdateInfo, UpdateSettings,
};
use serde::Deserialize;

use crate::config::{tailnet_choice_path, tailnet_https_path, tailnet_https_word, TailnetChoice};
use crate::error::{ApiError, ApiResult};
use crate::scan::{Priority, ScanMode};
use crate::state::{AppState, Caller};
use crate::{agent, art, events, guard, media, play, web};

/// The whole HTTP surface, behind the cross-site guard (`guard`: Host
/// allowlist, `X-Flux-Request` on unsafe `/api/` calls), which also covers
/// the media routes and the web UI fallback.
pub fn router(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/server", get(server_info))
        .route("/api/v1/admin/media-tools", get(media_tools))
        .route("/api/v1/admin/update", get(update_info))
        .route("/api/v1/admin/update/check", post(update_check))
        .route("/api/v1/admin/update/settings", put(update_settings))
        .route("/api/v1/admin/update/apply", post(update_apply))
        .route("/api/v1/admin/restart", post(restart))
        .route("/api/v1/libraries", get(list_libraries).post(create_library))
        .route("/api/v1/libraries/{id}/scan", post(scan_library))
        .route("/api/v1/libraries/{id}/items", get(library_items))
        .route("/api/v1/collections/{id}/items", get(collection_items))
        .route("/api/v1/libraries/{id}/folders", get(library_folders))
        .route("/api/v1/home", get(home))
        .route("/api/v1/items/{id}", get(item))
        .route("/api/v1/items/{id}/children", get(children))
        .route("/api/v1/items/{id}/watched", post(mark_watched))
        .route("/api/v1/search", get(search))
        .route("/api/v1/play", post(play::play))
        .route("/api/v1/plan", post(play::plan_preview))
        .route("/api/v1/sessions/{id}/progress", post(play::progress))
        .route("/api/v1/sessions/{id}/diagnostics", get(play::diagnostics))
        .route("/api/v1/tailnet", get(tailnet))
        .route("/api/v1/tailnet/connect", post(tailnet_connect))
        .route("/api/v1/events", get(events::sse))
        .route("/api/v1/art/{id}", get(art::get))
        .route("/api/v1/agent", get(agent::http_manifest))
        .route("/api/v1/agent/skills/{name}", get(agent::http_skill))
        .route("/api/v1/ops/{name}", post(agent::http_op))
        .route("/api/v1/schemas/{name}", get(agent::http_schema))
        .route(
            "/mcp",
            post(agent::mcp::post)
                .get(agent::mcp::get)
                .delete(agent::mcp::delete),
        )
        .route("/media/{id}/{*rest}", get(media::media))
        .fallback(web::fallback)
        // After every route and the fallback, so it wraps them all.
        .layer(axum::middleware::from_fn_with_state(st.clone(), guard::guard))
        .with_state(st)
}

fn id_of(p: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>) -> ApiResult<Id> {
    p.map(|UrlPath(id)| id)
        .map_err(|_| ApiError::not_found("No such item"))
}

/// `GET /api/v1/server`.
async fn server_info(State(st): State<Arc<AppState>>, caller: Caller) -> Json<ServerInfo> {
    Json(ServerInfo {
        name: st.cfg.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        you: caller.identity.clone(),
        path: caller.conn.path.as_str().to_string(),
        tailnet_name: caller
            .conn
            .tailnet_name
            .clone()
            .or_else(|| st.tailnet.info().dns_name),
        metadata: crate::metadata::info(&st.cfg.tmdb_exports),
        https_url: st.tailnet.https_url(),
        web_build: st.web.as_ref().and_then(|w| w.build()).map(str::to_string),
    })
}

/// `GET /api/v1/admin/update` (owner/admin): flux's version, the newest it
/// may update to, and how this install updates (`update`).
async fn update_info(State(st): State<Arc<AppState>>, caller: Caller) -> ApiResult<Json<UpdateInfo>> {
    caller.require_admin()?;
    Ok(Json(st.updater.info()))
}

/// `POST /api/v1/admin/update/check` (owner/admin): asks GitHub now.
async fn update_check(State(st): State<Arc<AppState>>, caller: Caller) -> ApiResult<Json<UpdateInfo>> {
    caller.require_admin()?;
    Ok(Json(st.updater.check().await))
}

/// `PUT /api/v1/admin/update/settings` (owner/admin): the daily check on or
/// off, remembered in the data dir (409 when the server's configuration
/// sets it).
async fn update_settings(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    body: Result<Json<UpdateSettings>, JsonRejection>,
) -> ApiResult<Json<UpdateInfo>> {
    caller.require_admin()?;
    let Json(UpdateSettings { check }) = body?;
    st.updater.set_check(check).map(Json).map_err(ApiError::conflict)
}

/// `POST /api/v1/admin/update/apply` (owner/admin): updates to the newest
/// release and restarts as it (202; progress as `update` events). 409 when
/// this install can't update itself or an update is running.
async fn update_apply(
    State(st): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<(StatusCode, Json<UpdateInfo>)> {
    caller.require_admin()?;
    st.updater
        .apply()
        .map(|info| (StatusCode::ACCEPTED, Json(info)))
        .map_err(ApiError::conflict)
}

/// `POST /api/v1/admin/restart` (owner/admin): stops gracefully and starts
/// again as the binary now on disk (202). `flux-server update` asks this
/// after putting a new version in place.
async fn restart(State(st): State<Arc<AppState>>, caller: Caller) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    let r = st
        .updater
        .restart_in_place("asked")
        .ok_or_else(|| ApiError::unavailable("flux doesn't know where its binary is"))?;
    tracing::info!(
        by = caller
            .identity
            .as_ref()
            .map(|i| i.login_name.as_str())
            .unwrap_or("?"),
        "restart asked"
    );
    st.updater.ask_restart(r);
    Ok(StatusCode::ACCEPTED)
}

/// `GET /api/v1/admin/media-tools` (owner/admin): what `flux-server
/// --check` prints, as JSON: ffmpeg/ffprobe paths and versions, the video
/// encoder transcodes use, hardware acceleration, tone mapping and what is
/// missing (docs/ENCODERS.md). The encoder detection is the engine's
/// (cached); ffprobe is asked for its version.
async fn media_tools(
    State(st): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<crate::doctor::ToolReport>> {
    caller.require_admin()?;
    let caps = st.engine.video_caps().await;
    let ffprobe = st.cfg.ffprobe.clone();
    let report = tokio::task::spawn_blocking(move || crate::doctor::ToolReport::from_caps(&caps, &ffprobe))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(report))
}

/// The live scan status and per-root `last_seen` (this process's scanner)
/// over the catalog's.
fn with_live_status(st: &AppState, mut lib: Library) -> Library {
    if let Some(s) = st.scanner.status(lib.id) {
        lib.scan = s;
    }
    st.scanner.overlay_roots(&mut lib);
    lib
}

/// `GET /api/v1/libraries`.
///
/// The live scan status is read **before** the counts. A scan reports
/// itself finished only after its last batch committed, so a response that
/// says "finished" always carries the final counts. (Read the other way
/// round, a scan finishing in between gave "finished" with the previous
/// counts: 298 of 300 shows in the Jellyfin head-to-head benchmark.)
///
/// The counts come from the catalog's count cache, which the writer
/// invalidates after each commit and before the scan that made it can
/// report itself finished, so the order above still holds.
async fn list_libraries(State(st): State<Arc<AppState>>) -> ApiResult<Json<Vec<Library>>> {
    let scanner = st.scanner.clone();
    let counts = st.catalog.file_counts().clone();
    let mut libs = st
        .catalog
        .read(move |c| {
            let ids: Vec<Id> = c
                .prepare_cached("SELECT id FROM library")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let live: Vec<(Id, ScanStatus)> = ids
                .into_iter()
                .filter_map(|id| scanner.status(id).map(|s| (id, s)))
                .collect();
            // Every catalog read below starts after the status snapshot.
            let mut libs = read::list_libraries_cached(c, &counts)?;
            for lib in &mut libs {
                if let Some((_, s)) = live.iter().find(|(id, _)| *id == lib.id) {
                    lib.scan = s.clone();
                }
            }
            Ok(libs)
        })
        .await?;
    for lib in &mut libs {
        st.scanner.overlay_roots(lib);
    }
    Ok(Json(libs))
}

/// `POST /api/v1/libraries` (owner/admin): creates the library and starts
/// scanning and watching it.
async fn create_library(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    body: Result<Json<CreateLibrary>, JsonRejection>,
) -> ApiResult<Json<Library>> {
    caller.require_admin()?;
    let Json(req) = body?;
    if req.name.trim().is_empty() {
        return Err(ApiError::bad_request("A library needs a name"));
    }
    if req.roots.is_empty() {
        return Err(ApiError::bad_request("A library needs at least one folder"));
    }
    if let Some(bad) = req.roots.iter().find(|r| !Path::new(r).is_absolute()) {
        return Err(ApiError::bad_request(format!("{bad:?} is not an absolute path")));
    }
    crate::roots::check_new_roots(&st.catalog, &req.roots).await?;
    let lib = st.catalog.writer().create_library(req).await?;
    let _ = start_scan(&st, lib.id, Priority::Interactive);
    Ok(Json(with_live_status(&st, lib)))
}

/// Quick scan + watch (unless `--no-watch`); logs (and returns) why it
/// couldn't start.
pub fn start_scan(st: &AppState, library_id: Id, priority: Priority) -> Result<(), String> {
    let r = st.scanner.scan_library(library_id, ScanMode::Quick, priority);
    match &r {
        Ok(()) if st.cfg.watch => st.scanner.watch(library_id),
        Ok(()) => {}
        Err(e) => tracing::info!(library_id, "not scanning: {e}"),
    }
    r
}

/// `POST /api/v1/libraries/{id}/scan` (owner/admin).
async fn scan_library(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    id: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>,
) -> ApiResult<Json<ScanStatus>> {
    caller.require_admin()?;
    let id = id_of(id)?;
    let lib = st
        .catalog
        .library(id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("No library {id}")))?;
    start_scan(&st, id, Priority::Interactive).map_err(ApiError::unavailable)?;
    Ok(Json(with_live_status(&st, lib).scan))
}

/// `GET /api/v1/libraries/{id}/items?cursor=&limit=&sort=&filter=`.
async fn library_items(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    id: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>,
    params: Result<Query<ItemsParams>, QueryRejection>,
) -> ApiResult<Json<Page<ItemSummary>>> {
    let id = id_of(id)?;
    let Query(p) = params?;
    // All six sorts have keyset cursors in the catalog (`read::ItemSort` is
    // the core type); a cursor from another sort is rejected there (400).
    let sort = p.sort.unwrap_or_default();
    let exists = st.catalog.read(move |c| read::library(c, id)).await?.is_some();
    if !exists {
        return Err(ApiError::not_found(format!("No library {id}")));
    }
    let user = st.user_id(&caller).await?;
    let q = read::ItemsQuery {
        library_id: Some(id),
        sort,
        cursor: p.cursor,
        limit: p.limit.unwrap_or(100),
        user_id: Some(user),
        filter: p.filter,
        ..Default::default()
    };
    Ok(Json(st.catalog.read(move |c| read::items_page(c, &q)).await?))
}

/// `GET /api/v1/collections/{id}/items?cursor=&limit=&sort=&filter=`: a
/// collection's titles across libraries, paged and sorted as a library's
/// (a collection someone else keeps for themselves is not found).
async fn collection_items(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    id: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>,
    params: Result<Query<ItemsParams>, QueryRejection>,
) -> ApiResult<Json<Page<ItemSummary>>> {
    let id = id_of(id)?;
    let Query(p) = params?;
    let user = st.user_id(&caller).await?;
    let mine = caller.identity.is_some().then_some(user);
    let row = st
        .catalog
        .read(move |c| flux_catalog::collections::get(c, id))
        .await?
        .filter(|r| flux_catalog::collections::visible(r, mine))
        .ok_or_else(|| ApiError::not_found(format!("No collection {id}")))?;
    let q = read::ItemsQuery {
        sort: p.sort.unwrap_or_default(),
        cursor: p.cursor,
        limit: p.limit.unwrap_or(100),
        user_id: Some(user),
        filter: p.filter,
        collection: Some(row.id),
        ..Default::default()
    };
    Ok(Json(st.catalog.read(move |c| read::items_page(c, &q)).await?))
}

/// `GET /api/v1/libraries/{id}/folders?folder=&cursor=&limit=&sort=`: one
/// folder of a videos library; 400 for a library of another kind.
async fn library_folders(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    id: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>,
    params: Result<Query<FolderParams>, QueryRejection>,
) -> ApiResult<Json<FolderPage>> {
    let id = id_of(id)?;
    let Query(p) = params?;
    let user = st.user_id(&caller).await?;
    let q = read::FolderQuery {
        library_id: id,
        folder: p.folder.unwrap_or_default(),
        sort: p.sort.unwrap_or_default(),
        cursor: p.cursor,
        limit: p.limit.unwrap_or(100),
        user_id: Some(user),
    };
    st.catalog
        .read(move |c| read::folder_page(c, &q))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("No such folder"))
}

/// `GET /api/v1/home`.
async fn home(State(st): State<Arc<AppState>>, caller: Caller) -> ApiResult<Json<Home>> {
    let user = st.user_id(&caller).await?;
    Ok(Json(
        st.catalog.read(move |c| read::home(c, Some(user), 20)).await?,
    ))
}

/// `GET /api/v1/items/{id}`.
async fn item(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    id: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>,
) -> ApiResult<Json<ItemDetail>> {
    let id = id_of(id)?;
    let user = st.user_id(&caller).await?;
    let detail = st
        .catalog
        .read(move |c| read::item_detail(c, id, Some(user)))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("No item {id}")))?;
    if lacks_art(&detail.summary) {
        bump_art(&st, id);
    }
    Ok(Json(detail))
}

/// The art a detail page shows is missing: a movie's or show's poster or
/// backdrop, a season's or episode's image (an episode's thumb, else its
/// show's poster).
fn lacks_art(s: &ItemSummary) -> bool {
    use flux_core::api::ItemKind;
    match s.kind {
        ItemKind::Movie | ItemKind::Show => s.poster.is_none() || s.backdrop.is_none(),
        _ => s.poster.is_none(),
    }
}

/// Someone is looking at an item without art: its work's art job (and the
/// match it waits for) runs next instead of in queue order. Fire and forget.
fn bump_art(st: &Arc<AppState>, item_id: Id) {
    let st = st.clone();
    tokio::spawn(async move {
        match st.catalog.writer().bump_art(item_id).await {
            Ok(true) => st.activity.wake_jobs(),
            Ok(false) => {}
            Err(e) => tracing::debug!(item_id, error = %e, "bump_art"),
        }
    });
}

async fn require_item(st: &AppState, id: Id) -> ApiResult<()> {
    let found = st.catalog.read(move |c| read::item_summary(c, id, None)).await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::not_found(format!("No item {id}")))
}

/// `GET /api/v1/items/{id}/children`.
async fn children(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    id: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>,
) -> ApiResult<Json<Vec<ItemSummary>>> {
    let id = id_of(id)?;
    require_item(&st, id).await?;
    let user = st.user_id(&caller).await?;
    Ok(Json(
        st.catalog
            .read(move |c| read::children(c, id, Some(user)))
            .await?,
    ))
}

/// `POST /api/v1/items/{id}/watched` → 204.
async fn mark_watched(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    id: Result<UrlPath<Id>, axum::extract::rejection::PathRejection>,
    body: Result<Json<MarkWatched>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let id = id_of(id)?;
    let Json(m) = body?;
    require_item(&st, id).await?;
    let user = st.user_id(&caller).await?;
    st.catalog.writer().set_watched(user, id, m.watched).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SearchParams {
    q: String,
    limit: Option<u32>,
}

/// `GET /api/v1/search?q=`.
async fn search(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> ApiResult<Json<Vec<ItemSummary>>> {
    let Query(p) = params?;
    if p.q.trim().is_empty() {
        return Ok(Json(Vec::new()));
    }
    let user = st.user_id(&caller).await?;
    let limit = p.limit.unwrap_or(30).clamp(1, 100);
    Ok(Json(
        st.catalog
            .read(move |c| read::search(c, &p.q, limit, Some(user)))
            .await?,
    ))
}

/// `GET /api/v1/tailnet`.
async fn tailnet(State(st): State<Arc<AppState>>) -> Json<TailnetInfo> {
    Json(st.tailnet.info())
}

/// `POST /api/v1/tailnet/connect` (owner/admin): switch remote access mode,
/// or HTTPS on the tailnet. The tailnet listener restarts in the background
/// (local and LAN connections are untouched); follow the `tailnet` SSE
/// events (or poll) for the login URL, the certificate and the final state.
async fn tailnet_connect(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    body: Result<Json<TailnetConnect>, JsonRejection>,
) -> ApiResult<Json<TailnetInfo>> {
    caller.require_admin()?;
    let Json(TailnetConnect { mode, https }) = body?;
    let https = https.unwrap_or_else(|| st.tailnet.https());
    let mode_changed = mode != st.tailnet.mode();
    let https_changed = https != st.tailnet.https();
    if !mode_changed && !https_changed {
        return Ok(Json(st.tailnet.info()));
    }
    let choice = TailnetChoice::from_api(mode);
    if !st.tailnet.request(choice, https) {
        return Err(ApiError::unavailable(
            "The server can't change its listeners right now",
        ));
    }
    // Remembered for the next start (unless the command line or the
    // environment says otherwise: --tailnet, --tailnet-https).
    let data_dir = &st.cfg.data_dir;
    if mode_changed {
        if let Err(e) = std::fs::write(tailnet_choice_path(data_dir), choice.as_str()) {
            tracing::warn!(error = %e, "could not remember the tailnet mode");
        }
    }
    if https_changed {
        if let Err(e) = std::fs::write(tailnet_https_path(data_dir), tailnet_https_word(https)) {
            tracing::warn!(error = %e, "could not remember HTTPS on the tailnet");
        }
    }
    let mut info = st.tailnet.info();
    info.mode = mode;
    info.https = https;
    info.https_error = None;
    if https_changed {
        // The new listener reports its address once it serves.
        info.url = None;
    }
    info.state = if mode == flux_core::api::TailnetMode::Off {
        flux_core::api::TailnetState::Off
    } else {
        flux_core::api::TailnetState::Starting
    };
    info.auth_url = None;
    info.approve_url = None;
    info.last_error = None;
    Ok(Json(info))
}
