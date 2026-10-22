# flux web client prototype

`web/` is the browser client for flux. It is React 19.3, TypeScript 6.0, Vite 8 (Rolldown), hls.js 1.7.3, and no UI framework. It covers:

- onboarding (connect with Tailscale, invite people);
- home, library grid, detail pages for films, shows and episodes;
- the player, with an explainable stats overlay;
- libraries and scanning.

It runs fully against an in-browser **mock backend** (MSW). The mock is built from the real test-library manifest and the feature clips' probe facts, so every screen works without flux-server. The same build talks to a real server when `VITE_FLUX_API` is set.

Quality gates (all green): `pnpm typecheck` · `pnpm lint` (ESLint 10 + typescript-eslint type-checked + React Compiler hook rules, zero warnings) · `pnpm test` (75 vitest tests) · `pnpm build`.

## Design

The direction comes from one brief: *calm, cinematic, poster-first; the library is the product* (research/01 §8.2.7). Taken literally: **the interface is a dark room and the films are the only light in it.**

- **No brand accent colour.** Chrome is neutral (near-black room, warm "screen white" text). The one call to action is a lit screen: a white Play button in dark mode, ink in light. I dropped an amber accent from the first plan because amber/yellow is Plex's colour.
- **Signature: bias lighting.** Hover or focus a poster, or open a detail page, and the room behind the content washes with that artwork's colour, like the light a TV throws on a wall.
  - The colour is the blurhash DC term, so computing it is free.
  - It transitions through a registered `@property --ambient` set on a single fixed layer (`inherits: false`), so the 900 ms fade never restyles the document. See `src/app/ambient.ts`.
- **Type.**
  - Archivo variable, used *condensed* (`font-stretch` 66–74 %, weight 700–780), for titles. These are film-title-card shapes, and long titles like *The Lord of the Rings: The Fellowship of the Ring* still fit.
  - Atkinson Hyperlegible Next for body text: legible at 10 feet and good for accessibility.
  - Atkinson Hyperlegible Mono for **edge codes**: technical facts set like the print along a film's edge (`2160P · HEVC MAIN 10 · HDR10 · E-AC-3 5.1`).
  - All fonts are self-hosted via `@fontsource-variable`. A private media server should make no third-party font requests. Latin subsets are 90 + 34 + 18 kB woff2, loaded on demand.
- **Colour only where it means something.** Playback paths get semantic colours, used for the plan pill, the "expected playback" dot and scan states:
  - phosphor green: direct play;
  - blue: repackaged;
  - amber: converted;
  - tally red: transcode or offline.
- **Focus is illumination.** Cards lift toward the light, with a white focus ring for keyboards and remotes. Arrow keys work everywhere:
  - the virtual grid uses a roving tabindex with Page/Home/End;
  - home shelves use geometric up/down between shelves (`src/components/spatial.ts`);
  - the player has YouTube-style shortcuts, with `?` for help.
- **Explainability is a first-class surface, not a debug page.**
  - "Expected on this device" on the detail page.
  - "Matched as" with a confidence meter, the evidence list, provider ids and TMDB attribution.
  - Art provenance: `Frame` (no AI) and `AI art` labels on posters and in the detail credits.
  - A files panel showing every probed track and how many range reads the probe took.
  - The player's stats overlay (below).
- **Calm copy.** Sentence case, plain verbs. Errors say what happened and what to do; empty states invite an action. Numbered steps appear only where order is real (onboarding).

The light theme is real, not inverted: "house lights up", a cool grey room, and the ambient wash at lower strength. The theme follows the system by default and can be overridden from the sidebar. The resolved theme is set before first paint by an inline script in `index.html`.

## Screens

Screenshots are in `$FLUX_WORK/bench/web-screens/`, named `<screen>-<dark|light>-<desktop|phone>.png`: 12 screens × 4 = 48 files. Regenerate them with `pnpm screens`.

| Screen | Route | What's there |
|---|---|---|
| Home | `/` | Hero "Continue watching" with backdrop, remaining time and Resume. A Tailscale banner appears until remote access is set up. Shelves: Continue watching (wide cards with progress; episode cards resolve their show name), Recently added, one per library. No promoted content. |
| Library | `/library/:id` | Library name in display type. A sticky toolbar holds an instant filter (accent- and article-insensitive), chips (Unwatched, In progress, 4K, HDR) and server-side sort. The virtualised poster grid is 3 columns on phones. A–Z jump rail. A notice explains offline roots. |
| Library 10k | `/library/3` | The "Film Archive": 10,000 generated titles, one root "asleep", to prove the grid. |
| Item: film | `/item/:id` | Backdrop and poster; condensed title; edge-code quality chips; Resume / From the start. **Expected on this device** gives the plan label, plain sentence and first reason. Version/edition picker (e.g. Spirited Away cd1/cd2, Fight Club Director's Cut). Audio/subtitle pickers. **Matched as** panel and files/tracks panel. |
| Item: show | `/item/:showId` | Play next episode; season tabs; episode rows with frame stills (labelled), progress, quality and a direct play button. A season URL redirects into the show with that season open. |
| Item: episode | `/item/:epId` | Link back to the show, then the same picker, plan and panels as a film, plus more from the season. |
| Player | `/play/:id` | Full-bleed video; quiet auto-hiding controls; scrubber with buffered range; audio and subtitle menus; fullscreen; shortcut help; stats overlay (`i`); error and end cards. |
| Connect | `/connect` | Three real steps: connect, keep signed in, invite. "Connect with Tailscale" leads to sign-in (QR plus link plus copy, polling "Waiting for you to sign in…") and then Connected (MagicDNS URL with QR, key-expiry warning with deep link). |
| Invite | `/connect` (connected) | "Someone in another home" (node share) or "Someone in your household" (tailnet invite), plus role (viewer/kid/guest/admin) and an optional note. Produces a link and QR with expiry; lists invites. |
| Admin | `/admin` | Server facts (path, identity, remote access); a live/reconnecting SSE indicator. Per library: roots with online / not responding, live scan readout (files found, read, added, updated, files/s, time). A zero-change rescan shows "nothing changed, no files opened". Add library dialog. |
| Search | `/` or `⌘K` anywhere | Instant search overlay (`<dialog>`), with ↑/↓/Enter and hover prefetch. |
| Focus | `/library/1` | Keyboard focus moved by arrow keys through the virtual grid (TV/remote check). |

## Architecture (web/src)

```
api/        types.ts (flux-core mirror), client.ts (typed fetch), events.ts (shared EventSource),
            query.ts (tiny cache: dedupe, prefetch, stale-time, useSyncExternalStore),
            queries.ts, library-list.ts (progressive paging), explain.ts, tracks.ts, format.ts,
            __golden__/ (generated from flux-core)
caps/       detect.ts (ClientCaps), engine.ts (hls.js vs native vs progressive), useCaps.ts
plan/       predict.ts: client sketch of the planner (mock "server" + "expected playback" hint)
player/     start.ts (one-round-trip play), engine.ts, metrics.ts (TTFF/seek), StatsOverlay.tsx
app/        router.ts + Link.tsx (≈150 lines, route preload on intent), routes.ts, App.tsx (shell),
            theme.ts, ambient.ts
components/ Art (blurhash + srcset + provenance labels), Cards, Row, VirtualGrid, SearchOverlay,
            QrCode, Icon
routes/     Home, Library, Item, Player, Connect, Admin (each its own chunk)
mock/       db.ts (catalog), handlers.ts (MSW), art.ts (stand-in posters + real blurhash),
            media.ts, curated.ts, fixtures.json (generated), browser.ts (boot)
```

### API layer: exact mirror of flux-core

- `src/api/types.ts` hand-mirrors `media.rs`, `names.rs`, `plan.rs` and `api.rs`:
  - `Option<T>` → `T | null`, always present;
  - snake_case enums as `const` arrays;
  - internally tagged `VideoOp`/`AudioOp`/`SubtitleOp` (`op`), `Reason` (`reason`) and `Event` (`type`);
  - flattened `ItemDetail`;
  - `external_ids` as `[string, string][]`.
- **The mirror is verified against the Rust code itself.** `web/scripts/golden-rs` is a 300-line cargo program, its own workspace, that depends on `crates/flux-core` and serializes:
  - every enum variant, inside an exhaustive `match`, so a new variant fails to compile;
  - every `Reason` with its `explain()` sentence;
  - plans with their `label()`;
  - full sample payloads.
- `pnpm golden` writes `src/api/__golden__/golden.json` and `golden.check.ts`:
  - `golden.check.ts` declares every sample as a `satisfies ItemDetail` (etc.) object literal, so `tsc` fails on missing or extra fields or wrong enum strings;
  - `golden.test.ts` checks enum lists, every `Reason::explain` sentence (including Rust's ties-to-even `{:.0}`) and every `label()`.
- Two serde subtleties this caught:
  - `AudioPlan` nests the tag: `{"track":1,"op":{"op":"copy"}}`;
  - `Container::WebM` serializes as `"web_m"`, and `DoviP7To81` as `"dovi_p7_to81"`.

### Typed client: `createFluxClient({ baseUrl, fetch?, ready? })`

| Method | Route |
|---|---|
| `server()` | `GET /api/v1/server` |
| `libraries()` | `GET /api/v1/libraries` |
| `createLibrary(CreateLibrary)` | `POST /api/v1/libraries` |
| `scanLibrary(id)` | `POST /api/v1/libraries/{id}/scan` |
| `libraryItems(id, {cursor, limit, sort}, signal?)` | `GET /api/v1/libraries/{id}/items` |
| `libraryFolders(id, {folder, cursor, limit, sort}, signal?)` | `GET /api/v1/libraries/{id}/folders` (a videos library's folder view) |
| `home()` | `GET /api/v1/home` |
| `item(id)` | `GET /api/v1/items/{id}` |
| `children(id)` | `GET /api/v1/items/{id}/children` |
| `search(q, signal?)` | `GET /api/v1/search?q=` |
| `play(PlayRequest)` | `POST /api/v1/play` |
| `reportProgress(sessionId, ProgressReport, keepalive?)` | `POST /api/v1/sessions/{id}/progress` (204) |
| `diagnostics(sessionId)` | `GET /api/v1/sessions/{id}/diagnostics` |
| `eventsUrl()` + `subscribeEvents(cb)` / `subscribeEventsState(cb)` | `GET /api/v1/events` (one shared, ref-counted EventSource) |
| `artUrl(ref, w)`, `artSrcSet(ref, cssPx)` | `GET /api/v1/art/{id}?w=` (fixed width buckets 160…2560 so URLs cache across screens) |
| `tailnet()`, `connectTailnet()`, `invites()`, `createInvite(CreateInvite)` | proposed routes (below) |

- Non-2xx responses throw `FluxApiError {status, path, message}` with the server's text.
- `resolveUrl()` resolves server-relative media URLs (`/media/...`) against `VITE_FLUX_API`.

### Client capabilities: `getClientCaps()`

`detectCaps(probe)` builds `ClientCaps` from the following probes (research/03 §2):

- `MediaSource.isTypeSupported`, or `ManagedMediaSource` on Safari;
- `canPlayType`, for progressive containers and native HLS;
- `mediaCapabilities.decodingInfo`, for 4K and for HDR via `transferFunction` pq/hlg and `hdrMetadataType` smpteSt2086 / smpteSt2094-40;
- `matchMedia('(dynamic-range: high)')`.

Codecs checked:

- **Video:** H.264 High at levels 4.1 and 5.1, HEVC Main and Main 10, AV1 8- and 10-bit, VP9 profiles 0 and 2.
- **Dolby Vision:** `dvh1.08`, `dvh1.05`.
- **Audio:** AAC, AC-3, E-AC-3, Opus, FLAC, MP3, ALAC, TrueHD, DTS.

Design notes:

- HDR formats are reported when the decoder supports them *and* either the display is HDR or the platform tone-maps itself (macOS/iOS). Otherwise the server would transcode for every SDR laptop. A `display_hdr` field is proposed below.
- Opus is capped at stereo on Apple WebKit.
- Detection runs once at idle after boot, so Play never waits for it.
- The logic sits behind a `MediaProbe` interface and is unit-tested with Chrome-on-Mac, Safari and Firefox-on-Linux fakes.

**Engine choice:**

| Case | Engine |
|---|---|
| Progressive file | `<video src>` |
| Apple WebKit (Safari, every iOS browser) | native HLS (hls.js is not even downloaded) |
| Everything else | **hls.js forced over MSE**, even on Chrome 142+, which ships native HLS (research/03 §2.1, Mux incident) |

hls.js settings for TTFF:

- `startPosition` = resume point, so there is no seek after start;
- `testBandwidth: false`;
- `startFragPrefetch`;
- `progressive` for chunked CMAF;
- `maxMaxBufferLength: 60`, so a paused remote viewer doesn't pull the whole film;
- one `recoverMediaError()` before failing.

### One-round-trip play and TTFF techniques (research/03 §11)

- **Hover or focus on any Play button** preloads the player route chunk and hls.js, and warms caps.
- **The Play click** (`player/start.ts`) starts `POST /play` *before* navigating, in parallel with the code download. When the plan arrives it immediately `fetch`es the server's `prefetch` URLs (init + first segment; immutable caching makes hls.js hit the HTTP cache).
- **The player** takes the in-flight request and measures TTFF from the click.
- **Cards and search results** prefetch item detail (and a show's seasons) on hover or focus. The detail page paints its header instantly from the clicked card's summary (optimistic navigation). Route changes run in a transition, so the old screen stays until the next one's code is ready.

### Measurement and reporting

`PlaybackMetrics`:

- TTFF = click → first `requestVideoFrameCallback` `presentationTime`;
- seek latency = `seeking` → first rVFC after `seeked`;
- stalls = `waiting` after the first frame, outside seeks;
- dropped frames from `getVideoPlaybackQuality()`;
- falls back to `playing`/`seeked` plus rAF when rVFC is missing, and says so ("approximate timings").

Reported via `POST /sessions/{id}/progress`:

- as soon as TTFF is known;
- every 10 s while playing;
- on pause, seek and end;
- on unmount or page hide, with `keepalive`.

### Stats overlay (`i`)

- The plan pill (`PlaybackPlan::label()` mirror, coloured by path).
- Every reason as a sentence. The server's `explain` is shown with Rust Debug names prettified ("TrueHd" → "TrueHD"); a core change is proposed below.
- Network path: Local network / Tailscale · direct / relayed (DERP) / This computer.
- Player engine and version.
- Video, audio and subtitles **in → out** (e.g. `E-AC-3 5.1 → AAC 5.1 · 640 kb/s`, `HEVC HDR10 → copy`).
- Source track; first frame; last seek with p50 over the session; buffer ahead; dropped/total frames.
- Picture size; hls.js throughput estimate.
- Server segments, bytes, p50/p95 segment time (from `/diagnostics`, polled every 4 s while open); estimated CPU cost.

### Library grid at 10k

`VirtualGrid`:

- window-scrolled; rows absolutely positioned via `transform`;
- range recomputed at most once per frame and re-rendered only when the visible row range changes;
- 2 rows of overscan;
- **fast-fling mode** above 4 px/ms: cards show only their blurhash and skip image requests until scrolling slows;
- WAI-ARIA `grid` with `aria-rowcount` / `aria-rowindex` on the virtual rows;
- roving tabindex that survives the active cell scrolling out of the DOM.

`LibraryList` loads a first page of 120 so the grid paints immediately, then pages of 1,000 in the background. `Page.total` sizes the scrollbar from the start, and unloaded cells are skeletons. SSE `items_changed` refreshes in place, keeping old items on screen until the new first page lands.

### Mock backend

- **Catalog** (`src/mock/`, a lazy chunk):
  - 2,998 films from `library.manifest.jsonl`, with each file a version, edition or part;
  - 300 shows and ~11k episodes;
  - a 10,000-title Film Archive whose NAS root is unavailable (its items stay browsable; Play returns 409 with an explanation).
- **Assets** reuse the **real ffprobe facts of the feature clips**, matched by container family and enriched with realistic language and title tags. Showcase titles are pinned to clips that exercise each plan:
  - Dune: Part Two: MPEG-2/AC-3 TS → Transcode;
  - Blade Runner 2049: 4K HDR10 HEVC + E-AC-3 → Audio conversion;
  - Princess Mononoke: TrueHD 7.1;
  - Spirited Away: multi-audio, SRT + ASS, two parts;
  - Past Lives: AV1/Opus.
- **"Matched as" evidence** comes from each path's real naming style (`Title (Year)/` fast path, scene names, loose files). Some ambiguous items carry an AI-assisted hypothesis, labelled as such.
- **Loglines** for about 35 well-known titles were written for the mock; none are copied from a provider.
- **Stand-in artwork.** No real posters exist, and a prototype must not ship TMDB images. The mock renders SVG duotone compositions with typeset titles (dusk, ridges, monolith, eclipse, projector beam, …) and computes a **real blurhash** from each by point-sampling its layers. Art sources are mixed, so the frame and AI labels are visible.
- **Plans** come from `plan/predict.ts` (the decision order of plan.rs / research/03 §12.2) with the mirrored `explain` strings.
- **Playback bytes:**
  - `pnpm mock-media` stream-copies the feature clips into fMP4 HLS (473 MB on the SSD, 5 s);
  - a small Vite dev/preview middleware serves them, and the raw clips, with HTTP Range and immutable caching;
  - without them, the mock plays the public Mux test stream and says so in `explain`.
- **Other routes:**
  - scans are simulated over SSE: an unchanged rescan walks everything and opens ~0.2 % of files; a new library adds items progressively;
  - WebVTT sidecars return real cues;
  - tailnet onboarding and invites are mocked, with a "Simulate sign-in" button.

## How to run

```sh
cd web && pnpm install

# Mock (no server): everything above, including playback of local test media
pnpm dev                         # http://localhost:5173
pnpm mock-media                  # once: local HLS renditions for playback

# Real flux-server
VITE_FLUX_API=http://127.0.0.1:8096 pnpm dev   # dev: /api and /media proxied (no CORS)
VITE_FLUX_API=/ pnpm build                     # production: flux-server serves web/dist same-origin;
                                               # the mock chunk is constant-folded away (verified)
```

Mock scenario switches (remembered per tab): `?mock_tailnet=running`, `?mock_path=tailnet-direct`, `?mock_latency=0`.

## Tests

`pnpm test` runs 75 tests in about 0.4 s.

| Suite | Covers |
|---|---|
| `api/golden.test.ts` | 13 enum lists; 14 `Reason::explain` sentences; 4 plan labels; flattening; `null` vs omitted |
| `api/client.test.ts` | The real client against the MSW handlers in Node (`msw/node`). Server and libraries; paging all 10,000 archive items through opaque cursors (unique ids, total); sort order; flattened detail with probed tracks; show → seasons → episodes; accent folding in search. Play → plan and explain → progress → diagnostics round trip; 409 for offline files; `FluxApiError` on 404; create + scan library; tailnet connect and invites. |
| `caps/detect.test.ts` | Chrome/M1 (HEVC + AV1 hardware, no AC-3, macOS tone-mapping); Safari (E-AC-3, DV 8 and 5, stereo Opus); Firefox/Linux (no HEVC); 1080p-only decoders; engine choice (hls.js forced on Chrome/Edge, native on Safari and iOS Chrome); UA parsing |
| `plan/predict.test.ts` | Plans for the real feature clips: MP4 direct; MKV repackaged; TrueHD → AAC 5.1; HDR10 + E-AC-3; SDR-only tone-map transcode; MPEG-2 transcode without a false "lossless" claim; audio track choice; WebVTT sidecars; PGS burn-in; bitrate cap |
| `api/format.test.ts` | Formatting; explain humanizing; SSE parsing; blurhash DC colour (exact, from a flat encoded image); median |

## Results

Measured with Google Chrome (headless) via `playwright-core`, against `vite preview` of the production build with the mock backend. The machine was **heavily loaded by the other agents (load average 23–72)**; all scripts are re-runnable.

### Bundle (`pnpm sizes` → `bench/results/flux-web-bundle.json`)

| | gzip |
|---|---|
| **Initial load** (entry JS + shared icon chunk + CSS) | **84.6 kB** (79.9 kB JS + 4.6 kB CSS; 73.6 kB brotli). React + ReactDOM are ~60 kB of it. |
| Route chunks | Home 2.5 · Library 3.6 · Item 5.1 · Player 7.3 · Connect 11.1 (QR encoder) · Admin 3.4 kB |
| hls.js 1.7.3 | 173 kB, loaded only on Play intent, and never on Safari |
| Mock backend (MSW + fixtures) | 242 kB, mock builds only; absent from `VITE_FLUX_API=/` builds |

### Library grid, 10,000 items (`pnpm bench:grid` → `bench/results/flux-web-grid-scroll.json`)

The harness scrolls the Film Archive with 120 fast wheel flings (≈72,000 px), 120 slow wheel steps back, and 25 keyboard PageDowns. Values are medians of 3 runs.

| Metric | Value |
|---|---|
| Frame time p50 / p95 / p99 | **16.7 / 16.7 / 16.8 ms** (60 fps held) |
| Janky frames (>25 ms) | 2 of ~806 |
| Long tasks / long animation frames during scroll | **0 / 0**. The first, cold run had 3 long tasks (658 ms total: JIT and cache warm-up). |
| Cards in DOM | **54** (580 DOM nodes total) for 1,667 grid rows |
| JS heap | 32 MB with all 10k summaries loaded |
| Keyboard | 25 PageDowns land on item 1,326, and focus follows |
| Cold load to Home | first shelf in DOM at 623 ms; LCP 540 ms (includes MSW worker boot) |

### Playback: click → first frame, and seek → first frame (`pnpm bench:playback` → `bench/results/flux-web-playback.json`)

Each case ran 5 times in fresh contexts, with 6 seeks per run (30 seeks). The app's own `PlaybackMetrics` and the harness's independent rVFC probe **agree to the millisecond**.

| Case | TTFF p50 / p95 | Seek p50 / p95 |
|---|---|---|
| Direct play MP4 (H.264/AAC, faststart) | 326 / 547 ms | 96 / 114 ms |
| Direct play MKV (Chrome progressive Matroska) | 336 / 338 ms | 97 / 120 ms |
| HLS fMP4 via hls.js, 4K HEVC Main 10 HDR10 | 473 / 479 ms | 114 / 147 ms |
| HLS fMP4 via hls.js, 2 h H.264 | 349 / 350 ms | 96 / 114 ms |

These numbers measure the *client pipeline*: route code, POST /play, prefetch, hls.js/MSE and decode. The bytes come from local files through Vite and the MSW service worker, not from flux-server. For context, Jellyfin 12.2 on this machine measured 0.19–0.91 s to the first *segment* and 0.32–0.84 s per seek (2026-10-18 baseline; the 2026-10-20 same-harness figures in benchmarks.md are 0.18–0.62 s and 0.46–0.76 s). Those are server-side, so this is not an apples-to-apples comparison. The research targets (research/01 §11 claim 2: TTFF p50 ≤ 300 ms on LAN; research/03 §13: seek p95 ≤ 500 ms) look reachable: the direct-play cases sit within ~30 ms of 300 ms even under load. Re-run against flux-server with `BASE=http://<server>`.

## Proposed core changes (flux-core `api.rs` / `plan.rs`)

> **Update:** items 1, 2 (sort, filter), 3, 5, 6, 7, 8, 9 and 10 (path, attribution, mark watched, `files_total`) landed in core v2 ([core-v2.md](core-v2.md)); the client and mock use them. Still open: invites (4), a letter index for A–Z jumps (2).

1. **`ItemSummary.parent_title: Option<String>`** (the show name for episodes and seasons). Today "Continue watching" needs two extra requests per episode card (`useParentTitle`).
2. **Library items:** pin `sort` values (`title|-title|year|-year|added|-added`, used here). Add `filter=` (`unwatched`, `in_progress`, `4k`, `hdr`, `genre`), and an offset or letter index (`GET /libraries/{id}/index` → first offset per letter). Then filtering and A–Z jumps don't need every page. `Page.total` should always be set.
3. **Tailnet onboarding:** `GET /api/v1/tailnet` and `POST /api/v1/tailnet/connect` → `TailnetStatus { state: off|starting|needs_login|needs_approval|running|unavailable|error, auth_url, approve_url, tailnet_name, owner_login, key_expiry, system_tailscale, last_error }`. This mirrors flux-net's `TailnetStatus` (see `web/src/api/types.ts`). Better still, a `tailnet_status` SSE event instead of the client polling every 2 s.
4. **Invites:** `GET/POST /api/v1/invites`, `CreateInvite { kind: share|household, role, note }` → `Invite { id, kind, role, note, url, created_at, expires_at, accepted }` (research/04 §5.4, §9 step 9).
5. **`Library.roots`** should become `Vec<RootStatus { path, available, last_seen }>`. Then availability doesn't depend on catching `source_availability` events or parsing `last_error`.
6. **`Reason::explain()` should use display names** (HEVC, TrueHD, E-AC-3, Dolby Vision, MKV) instead of `{:?}` (`Hevc`, `TrueHd`, `DolbyVision`, `Matroska`). The client prettifies them meanwhile (`humanizeExplain`).
7. **`ClientCaps.display_hdr: bool`.** The planner needs it (research/03 §8); the client currently folds it into `VideoCap.hdr`.
8. **`POST /api/v1/plan`** (a dry run: caps → `PlaybackPlan` + explain, no session). Then "Expected on this device" uses the server planner instead of the client sketch.
9. **Audio renditions in `PlayResponse`** (`audio: Vec<AudioRef { track, label, language }>`, mapping to HLS renditions). The player could then switch audio inside a session; today it requests a new session at the current position.
10. Smaller items:
    - `ServerInfo.path`: add `"tailnet"`, or share an enum with `flux_net::NetPath`. The client already accepts it.
    - `ArtRef.attribution` (provider name), for credits.
    - A "mark watched" route.
    - `ScanStatus.files_total` (estimate), for a true progress fraction.

## Known gaps

> **Update:** the client has now been run end to end against a release flux-server on the real corpus, and the bugs that turned up are fixed. See [e2e.md](e2e.md): Play TTFF 324 → 51 ms (lazy-route suspension), server-side plan preview, mark watched, and caps for HEVC level 5.2.

- **Not yet run against a real flux-server**; it wasn't serving the API during this work. The wire format is proven against flux-core by the golden tests, and the client against MSW. The Vite proxy and `VITE_FLUX_API=/` build paths exist but are untested end to end.
- **Mock playback bytes are stand-ins.** A "Transcode" plan plays a pre-made H.264 HLS file. The mock planner is a client sketch of plan.rs, not flux-hls.
- **Not built:**
  - Audio switching restarts the stream (new session at the current position).
  - Subtitles are WebVTT `<track>` sidecars only; there is no ASS/PGS client rendering (JASSUB/libbitsub not bundled), so ASS is shown as plain WebVTT.
  - No trickplay thumbnails, skip-intro, chapters UI, "mark watched", profile switching or settings page beyond the theme.
  - "Fix match…" is disabled (no API).
- **Browser coverage:**
  - Only Chrome was driven end to end.
  - The Safari, iOS and Firefox paths (native HLS, ManagedMediaSource, caps) are unit-tested with fake probes only.
  - No real TV browser was tried. Arrow navigation is DOM-geometry based, not a full spatial-navigation engine.
- **hls.js `progressive: true`** chunked streaming is untested with real chunked CMAF.
- **Benchmark caveats:** measured under heavy load; mock TTFF includes the MSW service-worker round trip (~1–3 ms per request).
- **Minor:**
  - `window.__fluxPlayback` (read-only metrics for the benchmark) is always exposed.
  - A strict CSP on flux-server needs the hash of the inline theme script.
  - `@property` colour transitions need Firefox 128+ / Safari 16.4+; older browsers just switch the ambient colour instantly.
- **Mock data:** the TMDB ids in `mock/curated.ts` are from memory and unverified; mock loglines and overviews are placeholders. `src/mock/fixtures.json` (547 kB, generated) is checked in so the mock works without the SSD; regenerate it with `pnpm fixtures`.
- **`web/scripts/golden-rs`** is a separate cargo workspace (target dir on the SSD). The lead may prefer to move the golden emission into a flux-core test.

## Files

All new, under `web/`:

- **Config:** `package.json`, `pnpm-workspace.yaml` (pnpm 11 `allowBuilds`), `.npmrc` (store on the SSD), `vite.config.ts`, `vitest.config.ts`, `eslint.config.js`, `tsconfig*.json`, `index.html`, `public/` (favicon, `mockServiceWorker.js`), `README.md`.
- **Source:** `src/**`, as in the architecture listing above.
- **Scripts:** `scripts/` holds:
  - `gen-fixtures.mjs`, `gen-golden.mjs`, `golden-rs/`, `make-mock-media.sh`, `mock-media-plugin.ts`;
  - `screens.mjs`, `bench-grid.mjs`, `bench-playback.mjs`, `bundle-sizes.mjs`.

Large outputs live on the SSD:

- `$FLUX_WORK/web-mock-media/` (mock HLS);
- `$FLUX_WORK/bench/web-screens/`;
- `$FLUX_WORK/bench/results/flux-web-{bundle,grid-scroll,playback}.json`;
- the pnpm store under `$FLUX_WORK/cache/pnpm-store`.

## Phase D: review fixes (web)

> **Merged.** The server half of the contract is on `main` (phase-d/server: the Host allowlist, `X-Flux-Request`, 421/403); flux-core now exports the header name (`api::REQUEST_HEADER`), and `tools/bench` sends it too. PB-6's server half landed with phase-d/hls-perf (`warm()` produces only the default audio). No CSP change was needed (ASS stays WebVTT). See [fixes.md](fixes.md).

Six changes, committed on `main` under `web/` only (`f82ff0b`, `8be3b57`, `e6eb073`, `35b4a17`, `19ab208`, `baa81bb`). Nothing changes flux-core, and the golden tests pass unchanged.

### 1. CSRF header and request-guard errors (SEC-1, SEC-7, web half)

The contract the server and the web client share:

1. **Host allowlist** on every listener and every request. Allowed:
   - `localhost`, `127.0.0.1` and `[::1]` (any port);
   - the machine's hostname and `<hostname>.local`;
   - IP literals of the server's own interfaces;
   - the tailnet DNS name and IPs, when the tailnet is on;
   - entries from `--allow-host` / `FLUX_ALLOW_HOSTS`.

   Any other Host gets `421 Misdirected Request` with a plain-text body. This blocks DNS rebinding.
2. **Unsafe methods** (POST/PUT/PATCH/DELETE) under `/api/` need `X-Flux-Request: 1`, or the server answers 403. If an `Origin` header is present, it must be one of the allowed hosts, or the server answers 403. flux never sends `Access-Control-Allow-*`, so a page on another origin can't add the header.
3. **The web client** sends `X-Flux-Request: 1` on every fetch to `/api/`, GETs included. GETs for media, HLS, SSE and art need nothing extra.
4. **Localhost is Owner** only if the Host passed check 1.

What changed in the web client:

- **`api/client.ts`**
  - Every request carries `x-flux-request: 1`.
  - Headers are built after `...init`, so a caller's `init` can't drop the header.
  - `apiErrorMessage()` turns the guard answers into instructions:
    - 421 names the host the page used and the ways out: localhost, the server's own name or IP, its Tailscale name, or `--allow-host`.
    - A 403 whose text mentions the header, Origin or cross-site check says the request didn't come from flux's own page.
    - Any other 403 reads "Not allowed: …", e.g. a role check.
  - `guardRefusal(err)` lets screens tell "refused" from "unreachable":
    - Home says "flux refused this page" plus the reason, where it used to say "Can't reach the server".
    - Library shows the same reason.
    - Admin's "Scan now" shows its error. It used to fail silently, as an unhandled rejection.
- **The MSW mock enforces the header** on every `/api/` call, GETs too. That is stricter than flux, so a code path that forgets the header fails in tests. `/events` (EventSource) and `/art` (`<img>`) are exempt. The mock also refuses a foreign `Origin` on unsafe methods.
- **Scripts:** `scripts/e2e-lib.mjs` `api()`, the direct `fetch` POST in `e2e-product.mjs` and the GETs in `e2e-bench.mjs` send the header.
- **`config.ts`** now says that a cross-origin production build can't work by design: flux sends no CORS headers. The Vite dev proxy still works: `changeOrigin` rewrites Host to the target, and an `Origin` of `http://localhost:5173` passes rule 2 because localhost is allowed on any port.
- **Tests** (`api/client.test.ts`, "CSRF guard"):
  - the client sends the header on every GET and POST;
  - the mock refuses an unsafe call without it, and lets the same call through with it;
  - a foreign Origin is refused;
  - a client that loses the header gets a 403 whose message says why;
  - 421 names the host and `--allow-host`;
  - a role 403 reads as one.

  With the old client, every mock POST in the suite fails with 403.
- **Callers outside `web/` that still need the header:** `tools/bench/flux_scan_bench.py` and `tools/bench/flux_playback_bench.py` POST `/api/v1/libraries`, `/scan` and `/play` without it, so they will get 403 once the server enforces rule 2.

### 2. In-session audio switching (e2e P2 #4, and the client side of PB-6)

Before, every audio change was a new `POST /play`. That meant a new session, a new plan, and the server warming segment *n* of every audio rendition again (PB-6). `PlayResponse.audio` already lists the session's renditions, so the player now switches between them in place. The decision is a pure function, `player/audio.ts` `planAudioSwitch()`:

| Situation | What happens |
|---|---|
| HLS session with a rendition of the chosen track | **In session.** hls.js: `setAudioOption(track)`, which takes a track from any audio group. flux-hls makes one group per codec, and hls.js changes variant when the group changes. Safari: enable the matching `video.audioTracks` entry, matched by NAME, else by language. |
| Direct play (progressive) | **New session.** The browser plays the file's default track, and a track it can't decode needs HLS. The audio menu says so: "This file is playing directly, so another track restarts the stream from here (converted if this browser can't play it)." |
| The session has no rendition of the track | **New session.** The menu marks the item "restarts the stream". |
| hls.js on a FLAC or Opus SourceBuffer, target in another codec | **New session**, marked "restarts the stream (codec change)". hls.js 1.7.3 misses the `changeType`: it knows `fLaC`/`Opus`, Chrome reports `flac`/`opus`. The next append fails and playback stalls (hls.md C.1 #3). A same-codec target stays in session, and AAC → Opus/FLAC stays in session. |

Other details:

- If the engine can't find the rendition (`selectAudio` returns false), the player falls back to a new session.
- A switch counts as done on hls.js `AUDIO_TRACK_SWITCHED`, which fires once the new track's audio is buffered. That is also when it is timed.
- The stats overlay marks the playing rendition and shows "Audio switch: N ms · in session / new stream".
- `window.__fluxPlayback.audio()` gives the harnesses `{rendition, switches[]}`.
- **Mock:**
  - `/play` and `/plan` list every audio track as a rendition, the chosen one first, as flux-hls does (`addOtherAudioTracks`, `PlanPrefs::all_audio_tracks`).
  - `ONLY=multi pnpm mock-media` builds a demuxed variant of `h264_multi_audio_subs.mkv` with one master per starting rendition: `v/`, `a/1/` (English AAC, copied) and `a/2-aac6/` (Japanese AC-3 → AAC 5.1, as the mock planner plans it). Without it, the mock falls back to the muxed stand-in, and a switch falls back to a new session.
- **Tests:**
  - `player/audio.test.ts`: every row of the table; rendition keys as flux-hls names them; hls.js track lookup by URL (`a/1/` is not matched by `a/12/`) and by NAME; native lookup by label and by language.
  - `api/client.test.ts`: the mock `/play` for the Japanese track lists `["2-aac6","1"]`, and switching back to English is in session.
  - `mock/media.test.ts`: direct play uses the clip, Japanese uses `master-2-aac6.m3u8`, and a rendition that isn't on disk falls back.
- **Measurement:** `pnpm bench:audio` (`scripts/bench-audio-switch.mjs`) writes `bench/results/flux-web-audio-switch.json`. The e2e walk (`e2e-product.mjs`) now also switches back after its restart and records `switch_back_in_session` (same session, mode, ms). The results are under "Measured" below.

### 3. Version picker names (e2e P2 #7)

`api/tracks.ts` `versionNames()` keeps the short name, the edition or the resolution, when it is unique. When two versions would share a name, both get the full name:

- `2160p · HEVC · HDR10 · E-AC-3 · MKV` vs `2160p · HEVC · HDR10 · AAC · MP4` (the e2e case);
- a colliding edition keeps its name in front;
- files whose facts are identical get the differing token of their file names (`… · cd1`, `… · cd2`), else a number.

The picker's facts line now also shows the container, and the file path is the tooltip. Tests are in `api/tracks.test.ts`, including a pass over every multi-version title in the mock catalog.

### 4. Episode pages without art (e2e P2 #9)

An episode's art is a wide still, shown as the backdrop, so the 260 px poster column was always empty on episode pages. The layout is now one column unless the page has a poster (`hasPosterColumn(kind)`, `.layout[data-poster]`); the phone breakpoint also overrides the attribute rule. Test: `components/card-utils.test.ts`.

### 5. First-scan counters (e2e P2 #6)

`items_added` counts shows, seasons and episodes. `items_updated` counts parents touched again by later batches. So the TV library's first scan read "12,658 added, 465 updated" for 10,949 files.

The copy now counts files and titles separately, with no core change:

- **First scan** (every file seen was read and nothing removed): "Last scan … · 10,949 files read · 300 shows added". "films" / "shows" / "titles" follows the library kind.
- **Rescan:** "3,811 files checked, 12 read · 10 added, 2 updated". Only non-zero changes are listed. "nothing changed in the library" when files were read but nothing changed; "nothing changed, no files opened" only when no file was read.
- **Live readout:** Files found, Files read, and the library's Films/Shows/Titles count, instead of item-level Added/Updated. `files_seen` counts only files whose fate is decided, so a file being read also counts as found; the readout never shows more files read than found.

Tests are in `routes/admin-state.test.ts`, including the e2e TV numbers.

### 6. ASS subtitles: evaluated, WebVTT kept

flux serves text subtitles only as WebVTT (`flux-hls subtitle.rs`, ASS override blocks stripped). The client never sees the original ASS script: not its `[V4+ Styles]` header (the Matroska CodecPrivate), not its event fields, not the fonts attached to the MKV. No client library can restore styling from WebVTT, so styled ASS needs a server route first. The candidates, as of October 2026:

| | JASSUB 2.5.23 (libass → WASM) | ASS.js (`assjs` 0.1.10, DOM/CSS) |
|---|---|---|
| Size | 4.9 MB unpacked, 56 files, 4 dependencies (worker + SIMD and non-SIMD WASM + default font) | 262 kB unpacked including sources and maps; one `dist/ass.js`; no runtime dependencies |
| Maintained | yes | yes (0.1.7 → 0.1.10, May–Aug 2026) |
| Offline | yes, but online font lookup "loads 50+ KB of code" | yes; the browser does font fallback |
| CSP | needs `script-src … 'wasm-unsafe-eval'`. `worker-src 'self' blob:` is already there. It wants COOP/COEP for threads; without them it runs single-threaded | its `addGlobalStyle()` injects a `<style id="ASS-global-style">`, which `style-src 'self'` blocks. It only creates the element if no element with that id exists, so shipping its global CSS as `<link id="ASS-global-style" rel="stylesheet" href="…">` from `'self'` avoids any CSP change. The rest is CSSOM and Web Animations, which CSP allows |
| Fidelity | libass, exact | "almost full" ASS (`\clip` via clip-path, `\t` via registerProperty); not pixel-identical to VSFilter |
| Licence | LGPL-2.1+ and others | MIT |

**Recommendation:** ASS.js, once the server offers the original track. The client side is then about 40 lines: lazy-load ASS.js when an ASS track is selected, overlay it on the `<video>`, and keep WebVTT as the fallback. No dependency was added in this phase, because there is nothing for it to render yet.

The server-side proposals are in "Proposed core changes" below.

### Findings status (web)

| ID | Status | Regression test |
|---|---|---|
| SEC-1 (web half: contract §3) | fixed in web; the server half is the server agent's | `api/client.test.ts` › CSRF guard |
| SEC-7 (web half) | fixed in web; same | same |
| PB-6 (client half: audio switches no longer re-POST /play) | partly: the warm-every-rendition fix is flux-hls's | `player/audio.test.ts`, `api/client.test.ts` › renditions |
| e2e P2 #4 audio switching | fixed | `player/audio.test.ts` |
| e2e P2 #5 ASS rendering | deferred (needs a raw ASS route; evaluation above) | — |
| e2e P2 #6 scan counters | fixed (copy) | `routes/admin-state.test.ts` |
| e2e P2 #7 version names | fixed | `api/tracks.test.ts` |
| e2e P2 #9 episode poster column | fixed | `components/card-utils.test.ts` |

### Proposed core changes (for the merge agent)

1. **flux-hls + flux-server: the original ASS track.**
   - A route `GET /media/{plan}/s/{t}.ass`: the CodecPrivate header plus `Dialogue:` lines rebuilt from the blocks (`ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text` plus timestamps).
   - Its attachments: `GET /media/{asset}/attachments/{n}`.
   - `SubtitleRef.ass_url: Option<String>`.

   The web client then uses ASS.js with no CSP change.
2. **flux-hls `subtitle.rs`: keep what WebVTT can carry**, with no client dependency:
   - `{\i1}…{\i0}` → `<i>`, and likewise `\b` and `\u`;
   - `\an7-9`, or a style with Alignment 7–9 → cue settings `line:0` (`\an1-3`/`\an7-9` also give `align:start/end`), so signs sit at the top instead of on top of dialogue.
3. **`tools/bench/flux_*_bench.py`:** send `X-Flux-Request: 1` on their POSTs (see §1).

### Gates

All four gates were run in `web/`, at nice 19:

- `pnpm typecheck`: clean.
- `pnpm lint`: 0 warnings.
- `pnpm test`: **130 tests in 11 files, all passing**. Phase C had 75.
- `pnpm build`: passes for the mock build and for `VITE_FLUX_API=/`. The production build contains no MSW chunk, and `x-flux-request` is in the shared client chunk.

The new tests fail without the fixes. With the header removed from `client.ts`, 18 of the 24 client tests fail on the mock's 403.

Production chunk sizes, gzip:

| Chunk | Size |
|---|---|
| Player (audio switching included) | 8.7 kB |
| Item | 6.8 kB |
| Admin | 4.2 kB |
| tracks | 2.5 kB |
| Entry | 74.5 kB |

No dependency was added. The Rust gate (`cargo fmt --check`, build, clippy, test, workspace-wide) was run in the same tree, though nothing outside `web/` changed. All of it passes (target `$FLUX_WORK/target/d-web`): `fmt --check` is clean; `build --workspace --all-targets` passes; `clippy --workspace --all-targets -- -D warnings` gives 0 warnings; `test --workspace` passes 451 tests, fails 0 and ignores 4.

### Measured

**Audio switch latency in the dev mock** (`pnpm bench:audio`, results in `bench/results/flux-web-audio-switch.json`):

- Setup: Chrome headless, `vite preview` of the mock build on 127.0.0.1, 3 runs × 10 switches.
- Load average was 21 / 13 / 9 at the time: the machine was shared.

| Switch | Time | p50 | p95 | n |
|---|---|---|---|---|
| Direct play (English) → Japanese: **new session** | click → first frame | 173 ms | 182 ms | 3 |
| English ↔ Japanese inside the HLS session: **in session** | click → hls.js `AUDIO_TRACK_SWITCHED` | **19 ms** | **25 ms** | 30 |

The harness checked every in-session switch:

- the session id stayed the same;
- there were **0** `POST /play` requests;
- segments of the target rendition (`a/1/`, `a/2-aac6/`) loaded after the click;
- playback kept going: `currentTime` advanced 1.2 s per 1.2 s.

The mock serves local files, so this measures the client pipeline, not flux-server. Against a real server, an in-session switch to a transcoded rendition also pays the first AAC segment from ffmpeg, about 100 ms cold (e2e.md TTFS + audio). That is still well under the ~170 ms of a new session, which pays the same segment plus `/play`, a new MediaSource and a new first frame.

The e2e walk now records `switch_back_in_session` on a real server, but it was not run in this phase: it needs a release build and the whole library walk.

**UI checked in Chrome** against the mock build:

- the direct-play audio menu shows the footnote;
- after the restart, the HLS menu has no "restarts" hints, because both tracks switch in place;
- the stats overlay shows "AAC Stereo → copy · playing" and "Audio switch 33 ms · in session";
- the episode page has no poster column;
- adding a library ends with "Last scan now in 0.8 s · 180 files read · 180 films added".
