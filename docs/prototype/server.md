# flux-server prototype

*2026-10-19. Crate: `crates/flux-server` (binary `flux-server` + library). Branch `phase-c/server`.*

> **After the Phase C merge** ([merge.md](merge.md)): the flux-scan stub, the 400s for four sorts, the in-process art scan and the JSON play index described below are gone. The server runs the real scanner, passes all six sorts through, executes the catalog's durable job queue, and stores flux-hls's binary index in the catalog.

flux-server wires the other crates into the HTTP API of `flux_core::api`:

```text
flux-net listeners (127.0.0.1:8096, optional LAN, tailnet; hyper http1 half_close — never axum::serve)
  └─ axum Router (src/api.rs)
       /api/v1/…            browse + libraries (flux-catalog), scans (flux-scan), play/plan/sessions (flux-hls),
                            events (SSE), tailnet (flux-net), art (flux-meta output)
       /media/{plan}/…      HLS from flux-hls (HlsResource::parse → HlsEngine::serve)
       /media/{asset}/file  the original file with Range / If-Range / ETag
       everything else      web/dist (SPA fallback, strict CSP)
background: startup Quick scans (Scheduled priority) + watchers, jobs (matching, frame posters),
            tailnet status → SSE, a read-only tailscaled probe
```

All numbers below come from a **shared machine** (M1 Max; another project's dev server and three other agents were running; load average 5.9–14.8, recorded per run). Everything heavy ran at `nice -n 19`, including the server under test.

## Summary

- **Every route of `api.rs` is served**, including core v2: `/plan`, `/items/{id}/watched`, `/tailnet`, `/tailnet/connect`, sorts (`title`, `-added`; 400 for the four the catalog lacks today) and filters.
- **`POST /play` plans from the playback index** (integration §4.2): the catalog-cached index with its path rewritten to the file's current location, else a fresh index that is stored back. Plan ids are deterministic and plans are persisted, so **a restart or a file rename doesn't break a playing session** (tested).
- **Through the real binary, flux-server is as fast as flux-hls's own dev server**: same files, sequence and machine state as `tools/bench/jellyfin_playback_bench.py`, 5 runs, medians:

  | file | plan | `POST /play` | TTFS | TTFS + audio | cold seeks (video / audio) | Jellyfin TTFS | Jellyfin seeks |
  |---|---|---|---|---|---|---|---|
  | h264_multi_audio_subs.mkv | Audio conversion | 1.8 ms | **2.4 ms** | 3.4 ms | 30 s: 1.7 / 0.7 ms | 189 ms | 131 ms |
  | h264_truehd71.mkv | Audio conversion | 0.8 ms | **3.3 ms** | 97 ms | — | 395 ms | — |
  | hevc_main10_hdr10_eac3.mkv | Audio conversion | 1.0 ms | **5.6 ms** | 119 ms | — | 199 ms | — |
  | hevc_4k_60mbps.mkv (41 MB seg 0) | Audio conversion | 2.5 ms | **14.9 ms** | 115 ms | 30 s: 15.5 / 84 ms | 617 ms | 2,027 ms |
  | long_2h_360p.mkv | Direct stream | 1.5 ms | **0.8 ms** | 1.3 ms | 600/3600/5100/30 s: 0.2–0.3 / 0.2–0.3 ms | 195 ms | 487–698 ms far, 17 ms back to 30 s |

  *Audit, 2026-10-20:* the Jellyfin columns are now benchmarks.md §5.4's medians (2026-10-20, n = 3, rotated order, warm-up play excluded). That is a different session from the flux columns. They first quoted the loaded 2026-10-18 single run (`jellyfin-playback-20261018-224518.json`): 327/521/304/913/191 ms TTFS and 318/840/12–701 ms seeks. In that run, h264_multi was the first play after Jellyfin started, so it carried about +100 ms of PlaybackInfo and +100 ms of TTFS (BENCH-6).

  `hls-serve` (flux-hls's dev server) measured by `hls-bench` minutes earlier under the same conditions: TTFS 1.0–14.2 ms, +audio 1.7–117 ms, seeks 0.4–15.7 ms. So the server adds no measurable overhead. TTFS+audio is dominated by the TrueHD/E-AC-3 → AAC transcode of the first audio segment (~85–115 ms), which `POST /play` starts early (`warm`).
- **Startup → API ready: 15 ms** (median of 5, measured from spawn to the first `GET /api/v1/server` 200), 10 ms on a restart; the server's own clock says 6.5 ms / 1.5 ms. **Idle RSS: 13 MB**; 80 MB after the playback benchmark (two transcodes in flight, engine caches). On the 14.7k-file catalog from `pipeline_smoke`: startup 10.7 ms, idle RSS 12.9 MB, `GET /libraries` 5.3 ms, `/home` 4.4 ms, a 100-item page 4.6 ms, search 0.2 ms (p50).
- **The web client works against the real server.** Headless Chrome on `web/dist` built with `VITE_FLUX_API=/`: **0 CSP violations, 0 console errors, 0 HTTP errors** on Home, Library, Admin, three item pages and three playbacks; the app's own TTFF (click → first frame): **TrueHD MKV over HLS 598 ms, 4K HDR10 HEVC 372 ms, direct-play MP4 318 ms**.
- **Gates** (in this worktree): `cargo build --workspace --all-targets` ok; `cargo clippy --workspace --all-targets` 0 warnings; `cargo test --workspace` **375 passed, 0 failed, 5 ignored** (354 + 21 new; 1 new ignored: needs flux-scan); `cargo fmt --all --check` clean; `RUSTDOCFLAGS="-D warnings" cargo doc -p flux-server --no-deps` clean.

## What's in the crate

| module | what |
|---|---|
| `config.rs` | clap CLI + env (`FLUX_*`) + optional TOML; `Config::for_tests` |
| `server.rs` | startup (`build_state`, `start_with`), background work, graceful shutdown |
| `api.rs` | the router; browse, libraries, scans, watched, search, tailnet routes |
| `play.rs` | `POST /play`, `POST /plan`, sessions, plan registry (LRU + plan files), prepared-asset LRU |
| `media.rs` | `/media/{plan}/…` (HLS) and `/media/{asset}/file` (Range) |
| `events.rs` | event bus + SSE |
| `jobs.rs` | background matching and artwork |
| `art.rs` | `/api/v1/art/{id}?w=` resize + disk cache |
| `web.rs` | static UI, SPA fallback, CSP, gzip |
| `net.rs` | flux-net start, tailnet view, mode-switch supervisor |
| `scan.rs` | `ScanService` over `flux_scan::Scanner`; disabled stand-in |
| `state.rs` | `AppState`, the `Caller` extractor (identity + `ConnInfo`), user cache |
| `examples/server_bench.rs` | the benchmark above |
| `examples/seed_catalog.rs` | fills a data dir's catalog without flux-scan (names + probe → writer) |

## Design decisions

### Callers, users and roles

- Handlers take a `Caller` extractor that reads flux-net's `Extension<Option<Identity>>` and `Extension<ConnInfo>` (requests that didn't come through flux-net, i.e. router tests, are anonymous LAN callers).
- `ServerInfo.path` is `conn.path.as_str()`; `tailnet_name` is the connection's MagicDNS name, else the running tailnet's.
- Identity → `Writer::ensure_user` → user id, cached per login (re-run only when the display name or role changes). **Anonymous callers (LAN) share one `anonymous` guest user**, so a living-room TV still gets resume points; their state is separate from the owner's (tested).
- Adding libraries, starting scans and switching remote access need Owner/Admin (403 otherwise). Browsing and playing are open to every caller that can reach a listener.

### `POST /play` and `POST /plan`

1. **Pick the asset:** the requested one (400 if it isn't a version of the item), else the best available (`item_assets` order) — and an SDR version of the same edition when `caps.display_hdr` is false and one exists. Show/season → 400 "pick an episode"; unreachable root → **409**.
2. **Prepare from the index:** in-memory LRU (32 assets, file re-checked every 10 s) → catalog `asset_play_index` (version-checked, `source.path` rewritten to `asset_location().path`; size + mtime still guard the content) → else `PlaybackIndex::build_with_fallback` (ffprobe for TS/AVI), stored with `put_asset_index` in the background. Concurrent prepares of one asset share one build. I build the index directly instead of `HlsEngine::prepare` because the engine's per-path map is unbounded.
3. **Plan:** `HlsEngine::plan(asset, caps, PlanPrefs { audio_track, subtitle_track, .. })`. Files the engine can't index (fragmented MP4, QuickTime AC-3, no video) fall back to the stored probe **only if that gives direct play**; otherwise 501 with the reason.
4. **Plan id** = `{asset}-{xxh3(index version, file size + mtime, plan)}`: the same request maps to the same URLs (immutable caching works across sessions), a changed file gets new ids, and an old id answers **410** ("start again") rather than serving bytes from a different file. Plans are kept in an LRU (512) and written to `<data>/plans/<id>.json` (pruned after 14 days), so after a restart `/media/{id}/…` reloads the plan and the index from the catalog.
5. **Warm + prefetch:** `engine.warm(asset, plan, n)` for the segment covering `start_ms`; `PlayResponse.prefetch` lists the video init + segment n, the default audio rendition's init + segment n + playlist, and the video playlist. `PlayResponse.audio` is `flux_hls::playlist::audio_refs`; subtitles are WebVTT sidecars at `/media/{id}/s/{track}.vtt` (also for direct play).
6. **Resume:** `start_ms` defaults to the caller's stored position (a finished viewing stores 0; a rewatch in progress resumes, as Continue watching lists it).
7. **Session id** = `{item}.{plan_id}.{nonce}`: a session the server forgot (restart) is rebuilt from its id on the next progress report (tested).

`POST /plan` runs steps 1–3 without a session or warm-up (and leaves the prepared asset cached, so a following Play is instant).

### Media delivery

- **HLS:** `HlsResource::parse` → `HlsEngine::serve` → `Content-Length` = `SegmentBody::len`, the engine's `Cache-Control` (playlists 10 s, init/segments/VTT immutable), a strong `ETag` (`"{plan}/{path}"`, 304 on `If-None-Match`) and `X-Flux-Ms` (production time). Bodies stream with `SegmentBody::into_stream(1 MiB)`.
- **Direct file:** single `Range` (`a-b`, `a-`, `-n`; multi-range → 200), 416 with `Content-Range: bytes */len`, `If-Range` (ETag or date), `If-None-Match` → 304, `ETag` = size + mtime, `Last-Modified`, `Accept-Ranges`. The body is 1 MiB positioned reads on the blocking pool with one read ahead (`buffered(2)`): ≤ ~3 MiB per stream and no blocking thread parked per stream. Loopback: **4.6–4.8 GB/s from the page cache, 405 MB/s cold from the SSD, 0.34 ms server CPU per MB** (`flux-server-file-throughput.json`).
- **Diagnostics:** per plan (HLS) or per asset (direct): segments served, bytes, p50/p95 of the last 256 production times; plus the client's last `ProgressReport` and the connection path. Sessions sharing a plan share its counters.

### Events (SSE)

One broadcast bus (1024 events). Sources: `Scanner::subscribe()` (forwarded), every tailnet status change (`Event::Tailnet` with `TailnetInfo`), and the job executor (`ItemsChanged.updated` when art or metadata lands). The stream opens with a `: flux events` comment, sends `: keep-alive` every 15 s, turns lag into a `: lagged N` comment, and **ends on shutdown** so graceful shutdown doesn't wait for SSE clients (tested: shutdown < 4 s with a stream open; median 10 ms in the benchmark).

### Background jobs

- **Never in the way:** one title at a time, 2 parallel ffmpeg decodes per title, **ffmpeg/ffprobe through a `nice -n 19` wrapper** (`<data>/bin/ffmpeg-low`), the export index built on a background-QoS thread, and the loop **pauses while anything was streamed in the last 30 s or a library is scanning**. It starts 10 s after startup and wakes on `ItemsChanged.added`.
- **Matching** claims `JOB_MATCH` jobs from the catalog's durable `job` table (`claim_jobs`/`finish_job`, retry after 1 h, 3 attempts). Candidates: TMDB (`TMDB_READ_TOKEN`/`TMDB_API_KEY`: search → details for the top 3 → matcher; on Accept, title/overview/genres/ids and provider poster/backdrop art rows credited "TMDB"); else the offline TMDB ID exports if present (`--tmdb-exports`, default `<data>/tmdb-exports`; the index is built lazily and dropped after 10 min idle); else jobs stay queued. Evidence names the best candidate and its signals.
- **Artwork:** movies/shows without a poster get a frame poster + backdrop (`flux_meta::art::frame_posters`, facts from the stored probe), then episodes without a thumbnail get one. JPEGs go to `<data>/art/`, rows via `add_art` (source `frame`, blurhash, size). *(Merged: this now runs from the catalog's durable job queue, one art job per movie or show; see [merge.md](merge.md).)*
- Measured (`flux-server-jobs.json`): on library-tiny + features (61 top-level items), **51 titles got frame art in the first ~80 s** (p50 0.50 s per title, max 3.1 s); the export index took 10.1 s to build at background priority; all 40 movies were scored (all *Review*: the export has no year or runtime, as flux-meta reported). RSS with the export index loaded: 108 MB.

### Artwork endpoint

`GET /api/v1/art/{id}?w=` rounds `w` up to the web client's buckets (160 … 2560; larger → original), resizes once (CatmullRom, JPEG q82, on the blocking pool, single-flight per key) into `<data>/art-cache/`, and serves with `Cache-Control: public, max-age=31536000, immutable` and `ETag "art-{id}-{w}"` (art rows are never rewritten). Provider URLs are fetched once into the cache.

### Web UI

`--web-dir` (default `./web/dist` or next to the binary). `index.html` is served with `Cache-Control: no-cache` and a CSP whose `script-src` lists the SHA-256 of each inline script, computed when the UI loads (so a rebuilt `index.html` stays covered): `default-src 'self'; script-src 'self' 'sha256-…'; style-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; font-src 'self'; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'`. `/assets/*` are immutable; text assets are gzipped once in memory; unknown paths get the shell, a missing hashed asset is a real 404, `/api` and `/media` never get the shell. Without a built client, `/` explains how to build it.

### Scanning

`ScanService` wraps `flux_scan::Scanner` (status overlay on `GET /libraries` when this process scanned the library). *(Merged: the server now runs the real flux-scan Scanner; the stub detection is gone, see [merge.md](merge.md).)* `--no-scan` does the same on purpose. Until then, `examples/seed_catalog.rs` fills a catalog.

### Remote access

`GET /api/v1/tailnet` maps flux-net's status (`TailnetStatus::to_api(mode)`) plus a once-at-startup read-only `tailscale status` probe for `system_tailscale`. `POST /api/v1/tailnet/connect` (owner/admin) restarts the listeners in the new mode on the same addresses (in-flight requests get 2 s; players retry) and remembers the choice in `<data>/tailnet-mode` (a `--tailnet`/`FLUX_TAILNET` setting wins). flux-net fixes the mode at start, hence the restart; see proposed changes. **Every run here used tailnet off**; the embedded/system paths are compiled, not exercised.

### Shutdown

SIGINT/SIGTERM → cancel the shutdown token (ends SSE, stops jobs) → `FluxNet::shutdown(5 s)` → `Scanner::shutdown` → `HlsEngine::shutdown` (kills transcodes) → catalog flush + writer shutdown.

## Tests (`cargo test -p flux-server`: 22 run, 1 ignored)

Real media under `$FLUX_WORK/media` (tests skip if absent); every data dir is a temp dir under `$FLUX_WORK/tmp/server/tests`. Catalogs are seeded the way flux-scan will fill them (flux-names + flux-probe → `Writer::apply`, as `pipeline_smoke` does).

| test | covers |
|---|---|
| `router::browse_routes_over_library_tiny` | server info (owner vs anonymous LAN), libraries, keyset paging covers the library exactly once, `-added` + `4k` filter, 400 for `year`/bogus sorts, 404s, home, movie detail with probes, show → seasons → episodes, search, mark watched/unwatched, separate guest state |
| `router::admin_routes_auth_tailnet_and_errors` | 403 for anonymous admin actions, library validation, 503 when scanning is off, tailnet info and connect auth, unknown routes, bad play bodies, unknown sessions, the no-UI page |
| `router::web_ui_spa_csp_and_caching` | CSP hash of the inline script, SPA shell, immutable assets, gzip, 304, 404 for missing assets and API paths |
| `play::play_truehd_mkv_over_hls_with_sessions` | plan from the index (TrueHD → AAC, `explain`), deterministic URLs, prefetch, master → variant → init → seg 0, audio init + segment (ffmpeg), ETag 304, 404s, `/plan`, progress → continue watching → resume, diagnostics, index stored in the catalog |
| `play::direct_play_with_ranges` | progressive plan → `/file`; `bytes=0-99`, suffix, multi-chunk range, 416, HEAD, `If-Range` by ETag and by date, 304, bytes counted in diagnostics |
| `play::streams_survive_restarts_and_file_moves` | a new server over the same data dir serves the old plan's segment byte-identically and rebuilds the old session; after rename + `FileMove` the cached index is reused (not rebuilt), the plan id is unchanged; unavailable root → 409 for play and file |
| `socket::real_socket_play_range_sse_and_shutdown` | flux-net on 127.0.0.1:0: localhost owner, SSE opens and receives `scan_progress`, TrueHD MKV master → variant → init → seg 0 + audio, Range on `/file`, graceful shutdown with SSE open |
| `socket::startup_scan_with_flux_scan` | `#[ignore = "needs flux-scan impl"]`: startup scan of library-tiny reports over SSE and fills the catalog |
| `jobs::frame_posters_become_art_rows_and_resize` | the job loop makes a frame poster + backdrop for a clip, the summary carries them, `/art/{id}?w=300` → 320-px JPEG, 304, original, 404 |
| unit (13) | config precedence + TOML, library specs, ranges, ETags, plan-id determinism, stats, LRU, art buckets/resize, CSP extraction, path traversal, the nice wrapper |
| `examples/probe_vs_index` | unchanged (integration check) |

## Benchmarks (re-run)

```sh
cd $FLUX_WORK/wt/flux-server
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-server TMPDIR=$FLUX_WORK/tmp/server/tmp
nice -n 19 cargo build --release -p flux-server --bin flux-server --example server_bench --example seed_catalog -j 4
nice -n 19 $CARGO_TARGET_DIR/release/examples/server_bench --runs 5
#   → bench/results/flux-server-playback-<ts>.json (5 runs, medians; startup, restart, RSS; 14.7k-file catalog browse)
# Same-conditions baseline: flux-hls's dev server
nice -n 19 cargo build --release -p flux-hls --bin hls-serve --bin hls-bench -j 4
nice -n 19 $CARGO_TARGET_DIR/release/hls-bench --serve-bin $CARGO_TARGET_DIR/release/hls-serve --runs 5 --no-sweep --out /tmp/x
```

Results in `$FLUX_WORK/bench/results/`:

- `flux-server-playback-20261019-094039.json`: the table above (load 10.9 → 9.1).
- `flux-server-hls-serve-baseline.json`: `hls-bench` against `hls-serve` just before (load 9.6 → 9.2).
- `flux-server-web-check.json` + screenshots in `$FLUX_WORK/bench/server-screens/` (load 5.9–7.5).
- `flux-server-file-throughput.json` (load 13.4).
- `flux-server-jobs.json` (load 7.9–8.4).

| startup (5 runs, median) | value |
|---|---|
| spawn → `GET /api/v1/server` 200, fresh copy of the catalog | 15.0 ms (run 1: 277 ms, the first open of a freshly cloned database under load) |
| same, restart on the same data dir | 10.3 ms |
| server's own `ready_ms` (main → listening), fresh / restart | 6.5 / 1.5 ms |
| idle RSS | 13.0 MB |
| RSS after the playback runs | 79.9 MB |
| SIGTERM → exit | 9.8 ms |

The very first launch of a newly built binary took ~1 s in a trial run, probably macOS assessing a new executable on first exec; later launches of the same binary took 15–20 ms.

## Run guide

```sh
# 1. Build (release) and the web client (served same-origin; dist is git-ignored)
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-server
nice -n 19 cargo build --release -p flux-server --bin flux-server --example seed_catalog -j 4
cd /path/to/flux/web && VITE_FLUX_API=/ pnpm build     # → web/dist
#   (what I ran, to keep web/ untouched: VITE_FLUX_API=/ ./node_modules/.bin/vite build \
#    --outDir $FLUX_WORK/tmp/server/web-dist --emptyOutDir)

# 2a. With flux-scan merged: libraries are created on first run and scanned at startup
$CARGO_TARGET_DIR/release/flux-server \
  --data-dir $FLUX_WORK/tmp/server/demo \
  --library Movies:movies:$FLUX_WORK/media/library-tiny/Movies \
  --library TV:shows:$FLUX_WORK/media/library-tiny/TV \
  --web-dir /path/to/flux/web/dist
# 2b. On this branch (flux-scan is a stub): seed the catalog, then run without scanning
$CARGO_TARGET_DIR/release/examples/seed_catalog --data-dir $FLUX_WORK/tmp/server/demo \
  Movies:movies:$FLUX_WORK/media/library-tiny/Movies \
  TV:shows:$FLUX_WORK/media/library-tiny/TV \
  Features:movies:$FLUX_WORK/media/features
$CARGO_TARGET_DIR/release/flux-server --data-dir $FLUX_WORK/tmp/server/demo --no-scan \
  --web-dir /path/to/flux/web/dist \
  --tmdb-exports $FLUX_WORK/cache/tmdb-exports
# → open http://127.0.0.1:8096 ; Ctrl-C (or SIGTERM) stops it gracefully
```

| flag | env | default |
|---|---|---|
| `--config FILE` | `FLUX_CONFIG` | — (TOML: the long flags with `_`, plus `[[library]] {name, kind, path}`, `scan`, `jobs`) |
| `--data-dir DIR` | `FLUX_DATA_DIR` | `~/Library/Application Support/flux` (macOS), `$XDG_DATA_HOME/flux` / `~/.local/share/flux` |
| `--library NAME:KIND:PATH` (repeatable) | `FLUX_LIBRARIES` (`;`-separated) | — (created when no library has that name) |
| `--port` / `--bind` | `FLUX_PORT` / `FLUX_BIND` | 8096 / 127.0.0.1 (callers there are the Owner) |
| `--lan ADDR:PORT` | `FLUX_LAN` | off (LAN callers are anonymous) |
| `--tailnet off\|embedded\|system` | `FLUX_TAILNET` | off (or the mode last chosen in the UI) |
| `--web-dir DIR` | `FLUX_WEB_DIR` | `./web/dist`, then next to the binary |
| `--name` | `FLUX_NAME` | host name |
| `--no-scan` / `--no-jobs` | `FLUX_NO_SCAN` / `FLUX_NO_JOBS` | scanning and jobs on |
| `--no-watch` | `FLUX_NO_WATCH` | watching on (Phase D) |
| `--allow-host HOST` (repeatable) | `FLUX_ALLOW_HOSTS` (`,`-separated) | — (localhost, this machine's names and addresses, the tailnet's name; Phase D) |
| `--localhost-role owner\|admin\|viewer\|kid\|guest\|none` | `FLUX_LOCALHOST_ROLE` | owner (Phase D) |
| `--owner-login LOGIN` | `FLUX_OWNER_LOGIN` | learned from the tailnet (Phase D) |
| `--ffmpeg` / `--ffprobe` | `FLUX_FFMPEG` / `FLUX_FFPROBE` | `PATH`, then Homebrew |
| `--tmdb-exports DIR` | `FLUX_TMDB_EXPORTS` | `<data>/tmdb-exports` |
| `--log FILTER` | `FLUX_LOG`, `RUST_LOG` | `info` (stderr) |
| — | `TMDB_READ_TOKEN` / `TMDB_API_KEY` | online matching off |

Data dir layout: `catalog.db` (+ WAL), `plans/`, `art/`, `art-cache/`, `bin/` (nice wrappers), `tailnet-mode`, `tailscale/` (embedded node state).

Tests: `nice -n 19 cargo test -p flux-server -j 4` (≈ 15 s after the build; needs ffmpeg for the TrueHD and poster tests).

## Known gaps

- **flux-scan is a stub here**, so startup scans, watchers, scan SSE and live scan status are wired but unexercised (one ignored test). The scan → SSE path is tested with a stand-in scanner.
- **Not run on a tailnet** (rules): embedded/system modes and the mode switch are compiled, not exercised. The switch restarts all listeners (2 s grace) because flux-net fixes the mode at start.
- **Online TMDB matching is untested** (no keys; it compiles against flux-meta's client). TMDB text is stored in the catalog without a ≤ 6-month refresh (ToS §1.C) — needs a periodic re-fetch.
- **Roles beyond admin checks are not enforced**: Kid/Guest see everything; grants (`ConnInfo.grants`) are ignored; LAN callers can browse and stream (as flux-net's design says, the server decides; pairing is future work).
- **Diagnostics are per plan, not per session**: two sessions with the same plan share segment counters (segment URLs are relative and carry no session).
- **No Range on HLS segments**, no HTTP/2, no compression for JSON responses (only the UI's static text assets are gzipped).
- **Jobs:** art via an in-process scan (no durable art jobs yet); an item whose art fails isn't retried until the next start; Review matches don't store the candidate id (only evidence text); the export index holds ~60 MB while matching runs (dropped after 10 min idle).
- **Shows, seasons** can't be played directly (400: pick an episode); there is no "next up".
- **Bench caveats:** shared machine at load 9–11; OS page cache warm (no `purge`); the server ran at nice 19; the first run of each batch includes the first open of a freshly cloned catalog.

## Proposed changes in other crates

1. **flux-net:** `FluxNet::set_tailnet(TailnetConfig)` (or start/stop of the tailnet listener alone), so switching remote access doesn't drop local and LAN connections. *(Done in Phase D.)*
2. **flux-catalog:**
   - the other four sorts (`-title`, `year`, `-year`, `added`) with keyset cursors (the server answers 400 today);
   - an art-jobs API (enqueue "needs art" for new items + claim), replacing `jobs::items_needing_art`'s SQL;
   - store Review candidates (a match-decision row with candidate ids) so "Matched as" can offer one-click confirmation;
   - expose `item.library_id` in a read helper (the job executor queries it with SQL) and `ScanRecord`-free root info for the API.
3. **flux-hls:**
   - bounded asset/subtitle caches so the server can call `prepare` (with its de-duplication) instead of building indexes itself;
   - a small in-memory cache for the video segment `warm` produces (today it only warms the page cache, so the first video segment is produced twice);
   - the ffmpeg fallback for unindexable files (fragmented MP4, QuickTime AC-3, AVI) and a binary index encoding (12 MB JSON for long MP4s in SQLite).
4. **flux-meta:** `Tools::nice`/priority option (the server wraps ffmpeg in a `nice -n 19` script today); `MatchQuery::from_probe` for audio languages (the server calls `from_guess(&NameGuess::unknown(), probe)`); frame candidates from the playback index's keyframes (as meta.md proposes).
5. **flux-scan:** a cheap "is any library scanning" accessor (the job executor polls `status` per library).
6. **flux-core:** document that error bodies are plain text and that anonymous callers share a guest user; consider `ServerInfo.scanning_available` so the UI can hide "Scan now" when scanning is off.

## Phase D: security and robustness fixes

> **After the merge** ([fixes.md](fixes.md)): serving outside the library is no longer refused outright. flux-scan supports links that leave the roots, so a file outside them is served when it is exactly the file the scan recorded (inode, size, mtime); a re-pointed link still gets 404. The server also passes its ffprobe to the scanner, maps overlapping roots to 400, reads cached library counts with live `last_seen`, bumps an item's art job when its page lacks art, streams WebVTT sidecars, sends weak ETags for video transcodes and ends `main` with `rt.shutdown_timeout`.

*Branch `phase-d/server` (from main 1fbcb63). Findings: `review/findings-d1.md` SEC-1…SEC-7. Crates touched: flux-server and flux-net only (flux-net's part is in [net.md](net.md#phase-d-admission-write-timeout-set_tailnet)).*

### The cross-site contract (SEC-1, SEC-7)

`src/guard.rs`, one router layer over every route, the media routes and the web UI fallback, so it covers every listener (local, LAN, tailnet) and runs before any handler:

1. **Host allowlist, every request.** `Host` (and an absolute-form target's authority) must name, with any port: `localhost`, `127.0.0.1`, `[::1]` (any loopback address); this machine's host name and `<hostname>.local` (plus the first label of a dotted host name); an address of one of this machine's interfaces (`getifaddrs`, re-read at most every 5 s when an unknown address shows up); the tailnet's MagicDNS name, its first label and its addresses while the tailnet runs (fed from flux-net's status); entries of `--allow-host` / `FLUX_ALLOW_HOSTS` (comma-separated). Anything else: **421 Misdirected Request**, plain text. A request with no `Host` at all (HTTP/1.0 tools; never a browser) passes.
2. **Unsafe methods under `/api/`** (`POST`/`PUT`/`PATCH`/`DELETE`) need `X-Flux-Request: 1`, else **403**. An `Origin` header, if present, must be `scheme://host[:port]` naming an allowed host, else **403** (`null` is refused). flux sends no `Access-Control-Allow-*`, so a page on another origin can't add the header, and a body-less `no-cors` POST (the SEC-7 scan trigger) doesn't carry it.
3. The web client sends `X-Flux-Request: 1` on every `/api/` fetch; `GET`s of media, HLS, SSE and art need nothing.
4. The localhost Owner identity therefore only reaches handlers through requests that passed (1); roles are checked after the guard as before.

Also from the verifier's SEC-1 notes: `--localhost-role owner|admin|viewer|kid|guest|none` (`FLUX_LOCALHOST_ROLE`, TOML `localhost_role`) makes net.md's multi-user mitigation configurable, and `--owner-login` (`FLUX_OWNER_LOGIN`) pins the owner so `tailnet/connect` can't hand ownership to whoever opens the login URL.

### The other findings

| finding | fix | where |
|---|---|---|
| SEC-2 symlink escapes the root | Files are served only from inside their library: `play::location` resolves the path and requires it under one of the library's roots (both canonicalized, so a symlinked root or `/tmp`→`/private/tmp` still works) for play, plan and HLS; `/media/{asset}/file` opens first, then checks that the opened file (dev, ino) is the one the resolved, contained path names, so re-pointing a link or swapping a folder between check and open can't slip another file through. Outside → 404. Links that stay inside the library keep working. Scan-time policy is flux-scan's. | `play.rs` `locate`/`location`/`contained`/`open_contained`, `media.rs` |
| SEC-3 connection/SSE exhaustion | flux-net admission (512 total non-loopback, 64 per IP, loopback its own budget, 503 at once beyond); the soft `RLIMIT_NOFILE` is raised to the hard limit (macOS: also `kern.maxfilesperproc`), and the limits shrink to fit what's left (`net::serve_limits`); SSE streams: at most 32 in total (503 beyond) and 8 per caller address, where a ninth ends that caller's *oldest* stream (most likely a closed tab not yet noticed, see below) instead of being refused. | flux-net `limits.rs`, `net.rs`, `events.rs` |
| SEC-4 unbounded `<data>/plans` | The caller's `max_bitrate` and video `max_width`/`max_height` are rounded down to standard rungs before planning (`quantize_caps`), so one file has a small fixed set of plan ids (an identical recipe reuses its plan and file); at most 4,096 plan files (≈16 MiB), least recently used deleted first; a used plan's mtime is refreshed hourly, so the 14-day startup prune now removes plans *unused* for 14 days (it removed plans *created* 14 days ago, including ones in use); `.tmp` leftovers are pruned. | `play.rs` `PlanFiles` |
| SEC-5 unbounded session map | At most 1,024 sessions, oldest first, O(1) per insert (`HashMap` + creation-order `VecDeque`; idle-for-12-h sessions at the front go too). `GET …/diagnostics` of an unknown id answers from a transient session and stores nothing; `POST …/progress` still rebuilds a forgotten session (restarts). | `play.rs` `Sessions` |
| SEC-6 stalled readers | flux-net's write timeout: a response that makes no write progress for 60 s is aborted, freeing its buffers (also covers HLS and SSE). | flux-net `limits::WriteTimeout` |

### Also

- **`--no-watch`** (`FLUX_NO_WATCH`, TOML `watch = false`): no filesystem watchers; libraries are still Quick-scanned at startup (Scheduled priority — watching used to start that scan) and by "Scan now".
- **`GET /api/v1/libraries` reads the live scan status before the counts** (inside one catalog read: library ids, then every scanner status, then the counts). A scan reports itself finished only after its last batch commits, so a response that says "finished" now always carries the final counts (the head-to-head benchmark saw "finished" with 298 of 300 shows).
- **Error bodies are plain text** (`text/plain; charset=utf-8`, `no-store`), documented in `error.rs`: handler errors, the guard's 421/403, the SSE cap's 503 and flux-net's 503. Only bare protocol answers (405, a missing static file's 404, 416) have no body.
- **Switching remote access no longer restarts every listener:** `POST /tailnet/connect` calls `FluxNet::set_tailnet`, which replaces the tailnet listener alone (2 s grace for its requests); local and LAN streams carry on.
- `examples/server_bench.rs` sends `X-Flux-Request: 1`.

### Tests (Phase D)

Each fix's regression tests fail without it: with all the fixes disabled in one batch (a mutation run), the 17 tests that exercise them through the router, a socket or flux-net all failed, and they pass with the fixes restored.

| test | covers |
|---|---|
| `guard::rebinding_hosts_get_421_before_any_handler` | SEC-1, the reviewer's curl: a forged Host gets 421 (plain text, no `owner`) on `/server`, library creation (nothing created), scan, tailnet connect, events, media, art and the web UI, and in an absolute-form target; localhost, 127.0.0.1, `[::1]`, the host name, `<host>.local` and an interface address get 200 as Owner; tailnet names only while set; `--allow-host` |
| `guard::cross_site_writes_are_refused` | SEC-7, the reviewer's request exactly (body-less POST `/scan`, foreign `Origin`, `Sec-Fetch-Site: cross-site`) → 403; foreign or `null` Origin with the header → 403; no header → 403 on every unsafe `/api/` route and method; the web client's request → 200; roles still apply (anonymous and Viewer → 403) |
| `guard::error_bodies_are_plain_text` | 400/403/404/421 bodies are `text/plain`, not JSON |
| `guard::event_streams_are_capped_per_caller_and_in_total` | SEC-3 server side: 8 per caller (a ninth ends the oldest, whose body then completes), 32 in total (503), slots freed on drop |
| `guard::libraries_read_the_live_status_before_the_counts` | a scanner that commits one more item when asked for its status: the "finished" response counts it (the benchmark race) |
| `play::files_outside_the_library_are_not_served` | SEC-2: after the scan, a file becomes a link to a stand-in key outside, and a folder becomes a link outside: 404 for owner and anonymous, with or without Range, and for `/play`; a link inside the library and a symlinked root keep working |
| `play::varied_caps_do_not_mint_a_plan_per_request` | SEC-4, the verifier's play_flood: 60 plays with varied `max_bitrate` → ≤ 2 plans and exactly that many files |
| `play::forged_session_ids_create_no_sessions` | SEC-5, the verifier's session_flood: 500 forged diagnostics ids → 200, no new session; a forgotten session's progress still rebuilds it |
| `socket::real_socket_refuses_foreign_hosts` | SEC-1 on the real listener (raw HTTP/1.1) |
| `socket::no_watch_scans_at_startup_without_watching` | `--no-watch`: startup Quick scan at Scheduled priority, "Scan now" works, nothing watched; with watching, startup watches |
| unit (11) | Host parsing, the allowlist, Origins, request checks; config defaults and flags; fd-limit sizing and raising; session cap (oldest first, O(1)); plan-file LRU, mtime refresh and startup prune (`.tmp`, 20-day-old removed, 13-day-old kept and refreshed on use); caps rungs |

Gate in this worktree (`CARGO_TARGET_DIR=…/target/r-security`, everything at `nice -n 19 -j 3`): `cargo build --workspace --all-targets` ok; `cargo clippy --workspace --all-targets` 0 warnings (flux-net also with `--no-default-features`); `cargo test --workspace` **480 passed, 0 failed, 4 ignored**; `cargo fmt --all --check` clean; `RUSTDOCFLAGS="-D warnings" cargo doc -p flux-net -p flux-server --no-deps` clean. Existing tests now send what the web client sends: `common::call`/`get`/`post` add `Host: 127.0.0.1:8096` and `X-Flux-Request: 1` (use `call_raw` for exact headers); the socket tests' reqwest client sets the header.

**End to end** (release binary on 127.0.0.1:18521, tailnet off, `--ffmpeg/--ffprobe /nonexistent` as the verifier did, under a hard `ulimit -n 256` — the worst case, since flux can't raise it — so the limits were 64 total / 32 per IP; the reviewers' scripts ported in `tmp/d-server/e2e/scripts/e2e.py`, results in `bench/results/flux-server-phase-d-security-e2e.json`; load average 30–80):

| repro | before (review) | now |
|---|---|---|
| SEC-1 `GET /server`, `POST /libraries` with `Host: rebind.attacker.example` | 200 Owner, library created | 421, 421; real Host → Owner |
| SEC-7 body-less `POST /scan` with a foreign Origin | 200, scan started | 403; with `X-Flux-Request: 1` → 200 |
| SEC-2 `GET /media/{leak}/file` (symlink to a stand-in key) | 200 + the key | 404, no bytes; the clip's Range still 206 |
| SEC-4 300 × `POST /play`, `max_bitrate` 100000…100299 | 300 plan files | 300 × 200, 1 plan, 1 file |
| SEC-5 20,000 forged `GET …/diagnostics` | +9 MB RSS, growing per-request time | all 200, RSS 26.9 → 27.0 MB |
| SEC-3 245 SSE connections from one client | EMFILE in accept, every client reset, outage outlasting the attack ~21 s | 31 streams, 214 answered 503 at once, 54 server fds, no EMFILE; a new request succeeds at once after the flood closes |
| SEC-6 100 sockets with a 4 KiB receive buffer requesting the clip and never reading | +208 MB held as long as they stay connected | 32 admitted (+48 MB), the rest 503; the stalled ones dropped by the 60 s write timeout (server fds 79 → 24 by 75 s; RSS stays at its high-water mark because the allocator keeps freed pages for reuse) |

With launchd's soft limit (`ulimit -S -n 256`, hard unlimited) flux raised `nofile` to 65,536 and kept the defaults (512 / 64).

### Known gaps (Phase D)

- **Strict containment breaks libraries that link outside their roots on purpose** (rclone/debrid mounts, *arr layouts), as the verifier warned; such files now 404. Fix: an opt-in list of allowed external roots, shared with flux-scan's scan-time policy (proposed below).
- **HLS re-checks containment when the prepared asset is (re)loaded**, not per segment: a link re-pointed while an asset is cached is caught at the next 10 s file re-check only if size/mtime changed. `/media/{asset}/file` checks every request.
- **Interface addresses**: macOS reports link-local IPv6 with the scope in the address bytes, so `http://[fe80::…%en0]:8096` doesn't match; use the name or a global address.
- **SSE streams of vanished clients** are noticed only when a keep-alive write fails (≤ ~30 s; `half_close(true)` ignores the client's FIN). Per caller this is handled (a new stream ends the oldest); for the total of 32, slots of vanished clients count until then.
- **The per-IP budget is shared by everyone behind one address** (NAT, a reverse proxy on the same host); `--allow-host` doesn't change that.
- Session nonces are not authenticated (an HMAC would let forged ids 404 without even a transient session); the cap bounds them.

### Proposed changes elsewhere (Phase D)

1. **web** (the contract above): send `X-Flux-Request: 1` on every `fetch` to `/api/` (`web/src/api/client.ts`); the MSW mock handlers may assert it. Without it every POST (play, progress, watched, libraries, scan, tailnet) gets 403.
2. **tools/bench**: `flux_scan_bench.py` `FluxApi.req` (lines 45–46, used by `flux_playback_bench.py` too) must always send `X-Flux-Request: 1`, e.g. `headers={"X-Flux-Request": "1", **({"Content-Type": "application/json"} if data else {})}`; its POST `/libraries`, `/scan` and `/play` get 403 otherwise. Clients that set `Host` themselves must use an allowed name (`127.0.0.1:PORT` is).
3. **flux-core** `api` docs: error bodies are plain text (never JSON); 421 = Host not allowed, 403 = missing `X-Flux-Request` / foreign `Origin` / role; consider `pub const REQUEST_HEADER: &str = "X-Flux-Request"` in `flux_core::api` so server, web types and tools share it.
4. **flux-scan / flux-catalog** (SEC-2): if the scan-time symlink policy gets an opt-in for targets outside the roots (rclone/debrid libraries), store it per library and expose it next to the roots (`scan_support::RootInfo` or `read::library`) so `play::contained` can accept the same targets; until then serving is strictly in-root.
5. **flux-catalog** (SEC-2, optional hardening): expose the scanned `(dev, ino, size, mtime_ns)` in `AssetLocation` so `/media/{asset}/file` can also refuse a file swapped in place since the scan.
6. **flux-hls** (SEC-4, optional): quantize `ClientCaps` numbers in the planner itself (`plan`, `plan_from_probe`), so every caller gets the bounded plan set; flux-server's `quantize_caps` can then go.
7. **ARCHITECTURE.md** flux-net invariants: "connections are admitted per IP and in total; a response that stops making write progress is aborted; the Host allowlist and `X-Flux-Request` contract are enforced by flux-server's router for every listener."

## Phase E: memory

*2026-10-20, branch `phase-e/memory` (from main f938c2c). Crates touched: flux-server (the allocator, `src/alloc.rs`, `main.rs`, `Cargo.toml`), plus small changes in flux-scan, flux-catalog and flux-hls. Shared machine as always: load average 3.4–9.2 during the runs (recorded in every result file), everything at `nice -n 19`, cargo at `-j 3`, servers on 127.0.0.1 with the tailnet off.*

**Result:** a cold scan of the full library now leaves the idle server at **45 MB phys footprint** (85 MB RSS; was 100 / 141 MB; target ≤ 50, met by footprint, not by RSS). The playback burst no longer adds ~95 MB of footprint (48 vs 195 MB), and a restart idles at **8 MB footprint** (51 MB RSS; was 23 / 70). Peak RSS went **down**: 117 vs 141 MB during the cold scan, 137 vs 230 MB over the first process's whole life. Scan, rescan and playback times are unchanged within noise.

### What changed

- **jemalloc is flux-server's global allocator** (cargo feature `jemalloc`, on by default; `--no-default-features` builds with the system allocator; compiled out on MSVC). The library crates stay allocator-agnostic. `src/alloc.rs`:
  - **SQLite allocates through jemalloc too** (`sqlite3_config(SQLITE_CONFIG_MALLOC)`, first thing in `main`, before SQLite's first use). Otherwise its memory stays in libmalloc: builds that moved only Rust's allocations elsewhere still held 36–43 MB of `MALLOC_SMALL` after a cold scan, and routing SQLite took that to 0.5 MB. Most of it is the 64 MiB bulk-scan page cache, freed at `end_bulk` and kept resident.
  - **Settings** (jemalloc's `malloc_conf` symbol; `_RJEM_MALLOC_CONF` overrides them): `narenas:1,tcache_max:1024,lg_tcache_nslots_mul:-2` (one arena; small per-thread caches), `dirty_decay_ms:1000` (unused dirty pages go back after 1 s), `muzzy_decay_ms:0` (no lazy `MADV_FREE` stage, which macOS keeps counting in the footprint).
  - **A decay thread**: jemalloc's background purging thread exists only on Linux (on macOS: `<jemalloc>: option background_thread currently supports pthread only`), so a small thread calls `arena.<all>.decay` every 5 s. On Linux `background_thread:true` is set instead and no thread is started (not measured here).
  - **A release hook**: `flux_catalog::release_free_memory()` (end of a scan job, end of a bulk write, after a large fingerprint batch) and the new `release_thread_memory()` now also run what the binary registered with `flux_catalog::set_release_hook`: flush the calling thread's cache, purge every arena.
- **flux-catalog**: `set_release_hook`, `release_thread_memory` (the hook alone). Without a hook nothing changes.
- **flux-scan**:
  - A scan worker that ran tasks and then finds no work for 1 s calls `release_thread_memory()` once (`pool.rs`; test `idle_workers_release_memory_once`).
  - **The background fingerprint pass reads without a page cache** (`fingerprint_direct`): two `pread`s into one 64 KiB stack buffer, hashed as they come. Through a fresh `PageCache` each file cost 2 × (64 KiB span + 4 × 16 KiB page copies + 64 KiB copy-out) = 384 KiB in 12 allocations. Same values, at every size class and with short reads (test `direct_matches_cached`).
  - The fingerprint queue's key set (~1 MB of table after a cold scan) is dropped once empty.
- **flux-hls**: a segment produced ahead by `warm` leaves the warm cache once the request it was made for has been served. It used to stay until LRU eviction, up to `warm_cache_bytes` (64 MiB); a 4K segment 0 is ~45 MB. A repeat request streams from the source, as any segment does. With jemalloc this is worth ~60 MB after a playback burst; with libmalloc it doesn't show, because libmalloc keeps the freed large blocks anyway (86 MB of `MALLOC_LARGE`).

### Before / after

`mem_bench.py` (below): a release flux-server on an APFS clone of `media/library` (Movies + TV, 14.7k media files) plus a third library with clones of the 5 playback feature files. Cold start on an empty data dir → scans done → background fingerprints done → 15 s idle → `footprint`; 3 unchanged rescans of each library (server clock); a playback burst of 5 × (`POST /play` with `X-Flux-Request: 1` + the TTFS walk + cold seeks, `tools/bench/hls_walk.py`) → 15 s idle; stop; restart on the same data dir → startup scans → 15 s idle. Peak RSS/footprint sampled every 50 ms (libproc) and from `wait4`. 3 interleaved rounds (the order rotates), medians, load 3.8–5.9: `bench/results/flux-e-memory-final-20261020-025739.json`; restart peaks from `restart_probe.py` (exact: rusage lifetime peak footprint and `wait4`, 3 rounds): `flux-e-memory-restart-final-20261020-030548.json`; summary with every run: `flux-e-memory-summary-20261020-030649.json`.

| | base (main f938c2c) | this branch, system allocator | **this branch (jemalloc, default)** |
|---|---|---|---|
| Idle after the cold scan: footprint / RSS | 100 / 141 MB | 100 / 140 MB | **45 / 85 MB** |
| Idle after the playback burst: footprint / RSS | 195 / 217 MB | 197 / 215 MB | **48 / 64 MB** |
| Restart on the full catalog, idle: footprint / RSS | 23 / 70 MB | 23 / 65 MB | **8.3 / 51 MB** |
| Peak RSS (footprint), cold scan + fingerprint pass | 141.5 (101.6) MB | 141.0 (101.3) MB | **117.1 (92.9) MB** |
| Peak RSS, playback burst | 228 MB | 226 MB | **104 MB** |
| Peak RSS, first process's whole life (`wait4`) | 230 MB | 227 MB | **137 MB** |
| Peak footprint / RSS, restart (exact) | 24.9 / 71.2 MB | 24.4 / 70.6 MB | 25.5 / 68.0 MB |
| Cold scan of the 3 libraries (server clock) | 2.22 s [2.12–2.23] | 1.34 s [1.33–2.07] | 1.34 s [1.33–2.40] |
| Background fingerprints done (from API ready) | 8.70 s | 7.56 s | 7.57 s |
| Unchanged rescan, Movies / TV | 45 / 33 ms | 44 / 29 ms | 43 / 28 ms |
| Restart scans | 138 ms | 138 ms | 135 ms |
| Playback: Σ (play + TTFS) of 5 files / Σ cold seeks | 219 / 41 ms | 219 / 42 ms | 222 / 43 ms |
| Binary, stripped | 23.69 MB | 23.69 MB | 24.22 MB (+530 KB) |

- Cold-scan wall and the fingerprint pass vary by up to ~1 s between runs here (page cache, load): in the earlier A/B set (`flux-e-memory-ab-20261020-022728.json`, 3 rounds) base, system-allocator branch and jemalloc took 2.43 / 2.50 / 2.44 s and 8.90 / 9.00 / 8.91 s. The faster medians above are not claimed as a win; there is no regression in either set.
- The first run of a session pays cold caches: base's first run in the A/B set had TTFS Σ 505 ms and seeks Σ 182 ms; every build's other runs are 190–230 / 38–46 ms.
- RSS includes clean file-backed pages (SQLite's mmap); the footprint is what macOS charges the process.

### Allocators evaluated

One round each unless noted (`flux-e-memory-screen{,2,3,4}-*.json`, `-ab-`, `-tcache-`): idle footprint after the cold scan / after the playback burst / after a restart, and peak RSS during the cold scan, MB.

| build | cold idle | after play | restart | peak RSS |
|---|---|---|---|---|
| system (libmalloc) | 98 | 195 | 23 | 136 |
| mimalloc 3.3.2 (Rust only), default `purge_delay` | 123 | 267 | 31 | 186 |
| mimalloc 2.3.2 (Rust only) | 124 | 205 | 30 | 190 |
| mimalloc 3 + SQLite + `mi_collect(true)` every 2 s | 71 | 166 | 11 | 200 |
| mimalloc 3 + SQLite + release hooks + collect on tokio park + every 5 s (3 rounds) | 61 | 198 | 11 | 195 |
| jemalloc 5.3.1 defaults (Rust only) | 106 | 186 | 32 | 153 |
| jemalloc + SQLite + purge every 2 s | 62 | 129 | 17 | 157 |
| … + `narenas:2` | 57 | 122 | 11 | 150 |
| … + warm-cache release + hooks, decay every 5 s (3 rounds) | 54 | 69 | 14 | 139 |
| … + `tcache:false` (2 rounds) | 43 | 49 | 10 | 115 |
| **… + `narenas:1,tcache_max:1024,lg_tcache_nslots_mul:-2` (final, 3 rounds)** | **45** | **48** | **8.3** | **117** |

- **mimalloc** returns freed memory per thread: pages belong to the thread that allocated them and are only purged when that thread collects. A scan leaves its garbage spread over 10 worker threads (and playback over tokio's threads); `mi_collect` from a timer thread reaches only arena-level free pages. Even with collection on every idle worker and tokio park, it idled at 61 MB, and its peak RSS was ~40 % higher (195 vs 138 MB cold scan, 288 vs 229 MB whole process). Its binary cost is smaller (+134 KB unstripped, jemalloc +724 KB), its speed the same. Rejected.
- **jemalloc**'s arenas are shared, so one `purge`/`decay` call returns every freed page, whichever thread freed it. What remained per thread was the thread cache: with the defaults (32 KiB `tcache_max`, 2 × slab slots per bin) ~30 threads held ~10 MB of freed blocks, visible as +12 MB on the restart peak (38 vs 25 MB) and +9 MB at idle. The small caches remove that; `tcache:false` removes a little more but made the TV rescan ~10 % slower (32.5 vs 29 ms), the small caches did not. On the `narenas:2` build (56 / 62 / 14 MB), dropping the decay thread gave 65 / 74 / 33 MB and dropping the release hooks 57 / 70 / 12 MB: the thread does most of the work; the hooks make it immediate at the end of a scan.
- jemalloc's own stats at idle after a cold scan (`narenas:2` build, `tikv-jemalloc-sys/stats`): 37.8 MB allocated, 55.4 MB active, 4.3 MB metadata, 2.9 MB in thread caches, 0 dirty.

### What the memory was

The base server at idle after a cold scan (100 MB footprint), with `MallocStackLogging=lite`: `heap` finds **36.7 MB live in 60k blocks and 55.7 MB free but dirty** (61 % fragmentation) in libmalloc. `malloc_history -callTree` on the live blocks: 33.7 MB were allocated on scan workers, **24 MB of them under `flux_names::fallback::guess` → hunch** (its rule sets' compiled regexes in `LazyLock`s, 5.7 MB from `RuleSet::from_toml` alone, plus per-thread regex caches) and ~4 MB in flux-names' own regexes. These are kept for the next parse; a restart that parses nothing idles at 8 MB, so ~25–30 MB of today's 45 MB is the name parser.

### Scan-time churn

`malloc_history -allEvents` (`MallocStackLoggingNoCompact=1`, logs written to the SSD with `MallocStackLoggingDirectory`) over a cold scan of the same three libraries plus the background fingerprint pass, aggregated by the innermost flux/hunch frame (`churn.py`), system-allocator builds:

| | allocations | bytes allocated |
|---|---|---|
| base | 8.03 M | 14.97 GB |
| this branch | 7.78 M | **9.26 GB (−38 %)** |

The fingerprint pass was the biggest single item that flux-scan controls (5.7 GB: 384 KiB of buffers per file); it is gone. What is left, by size:

1. **flux-probe `PageCache::read` + `Store::insert`: ~3.2 GB** (14.7k probes): each read allocates the span, copies it into per-page `Vec`s, then copies the requested range out again; the store copies once more. Plus 2.75 GB / 462k allocations of `Vec` growth inside flux-probe.
2. **SQLite: 2.72 GB in 1.32 M allocations** (~90 per file): statement execution and rusqlite binding text/blobs as `SQLITE_TRANSIENT` (a copy per bind).
3. **flux-names: ~2.4 M small allocations** (~160 per file: `stem` anchors and `episode_part` captures, `vocab::classify`, tokens).
4. hunch's regex compilation: ~80 MB, once per process.

Items 1–3 are outside this branch's crates; proposals below. They cost CPU, not idle memory, now that the allocator returns freed pages.

### Tests (Phase E)

| test | what |
|---|---|
| `flux-server` `alloc::jemalloc::tests::sqlite_allocates_from_jemalloc` | `SQLITE_CONFIG_MALLOC` succeeds before SQLite's first use; 5,000 rows round-trip; `sqlite3_msize` of a SQLite allocation is jemalloc's `malloc_usable_size` |
| `flux-scan` `pool::tests::idle_workers_release_memory_once` | each worker that ran a task calls the release hook once after 1 s idle, and not again while idle |
| `flux-scan` `fingerprint::tests::direct_matches_cached` | the direct read gives the cached path's fp128 and oshash for sizes 0 … 1,000,000 around both 64 KiB edges, also with 1,000-byte short reads |
| `flux-hls` `engine::tests::warm_keeps_the_segments_it_produced` | now also: once served, the warm cache is empty |

Gate in this worktree (`CARGO_TARGET_DIR=…/target/c-e2e`, `nice -n 19`, `-j 3`): `cargo build --workspace --all-targets` ok; `cargo clippy --workspace --all-targets` 0 warnings (flux-server also with `--no-default-features`); `cargo test --workspace` **606 passed, 0 failed, 4 ignored**; `cargo fmt --all --check` clean.

### Known gaps (Phase E)

- **Linux is not measured.** There jemalloc runs its own background thread (`background_thread:true`) and the decay thread is not started; glibc's `malloc_trim` still runs for anything left on the system allocator.
- **One arena** serves every thread. Five sequential plays and a 10-worker scan showed no cost; many concurrent streams were not tested. `_RJEM_MALLOC_CONF=narenas:4` (or any other option) overrides it without a rebuild.
- The decay thread wakes every 5 s for the life of the process (a `mallctl` call; no measurable CPU).
- The idle number is now mostly the name parser's compiled regexes (above); flux-server can't free statics inside hunch.
- MSVC builds use the system allocator (tikv-jemallocator doesn't support it).

### Proposed changes elsewhere (Phase E)

1. **flux-names / hunch** (~25–30 MB of the 45 MB idle): compile only the rule sets a parse needs, or build them ASCII-only (`(?-u)`) where the patterns allow (Unicode classes inflate the NFAs); consider dropping per-thread regex caches by parsing on fewer threads. ~160 allocations per parsed file could also shrink (`stem` anchors, `episode_part` captures).
2. **flux-probe** (~6 GB of the remaining 9.3 GB churn): read a missing span straight into the page buffers (one allocation per page, no span buffer), return borrowed slices (`Cow`) when a read falls inside one page, and let `Store::insert` take ownership instead of copying.
3. **flux-catalog**: bind large text/blob values as `SQLITE_STATIC` where the value outlives the step (rusqlite's default is a copy per bind); `find_by_dev_ino` formats its SQL string on every call before `prepare_cached`.
4. **flux-hls**: a warmed segment for a play that never starts stays until LRU eviction (up to 64 MiB); a short TTL would bound it in time too.

### How to run

```sh
# Harness (not committed; uses tools/bench/benchlib.py and hls_walk.py): $FLUX_WORK/tmp/e-memory/
#   mem_bench.py (cold scan / rescans / playback burst / restart, NAME=BINARY[@ENV=..;..] interleaved),
#   restart_probe.py (exact restart peaks), churn.py (malloc_history aggregation), summarize.py
cd $FLUX_WORK/wt/e-memory
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-e2e TMPDIR=$FLUX_WORK/tmp/e-memory/tmp
nice -n 19 cargo build -j 3 --release -p flux-server                          # jemalloc (default)
nice -n 19 cargo build -j 3 --release -p flux-server --no-default-features    # system allocator
E=$FLUX_WORK/tmp/e-memory
nice -n 19 python3 -I $E/mem_bench.py --tag final --rounds 3 base=$E/bin/flux-server-base jemalloc=$E/bin/flux-server-je4
# jemalloc options at run time, e.g.: _RJEM_MALLOC_CONF=narenas:4,stats_print:true
# Churn: MallocStackLoggingDirectory=$E/tmp/msl MallocStackLoggingNoCompact=1 flux-server …  (system-allocator build), then
#   malloc_history <pid> -allEvents | python3 -I $E/churn.py out.json
```
