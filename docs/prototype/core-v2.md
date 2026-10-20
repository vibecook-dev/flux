# flux-core v2: the coordinated wire-DTO batch

*2026-10-19.* This lands the "core v2" changes that docs/prototype/integration.md §3 deferred, as one change across flux-core, every producer crate, the web mirror (`web/src/api/types.ts`), the MSW mock and the golden tests.

All numbers below come from a **shared machine** (M1 Max, another project's dev server running), with the 1-minute load average noted. Commands use `CARGO_TARGET_DIR=$FLUX_WORK/target/c-core`, `nice -n 19` and `-j 4`.

## Summary

- **15 items landed**, all with the names the task gave. Deviations are listed per item and collected in the last table. The main ones:
  - `ItemSummary` reads the same in every list now: the catalog no longer retitles episodes on Home or seasons everywhere.
  - `TailnetInfo` keeps three fields the web already used.
  - `CodecNotPackageable` also covers video.
  - `ItemFilter` is implemented in flux-catalog, not just declared.
- **Gates:**
  - Workspace: build, clippy (0 warnings, also `--no-default-features` for flux-net, flux-names and flux-hls), `cargo test --workspace` (354 passed, 0 failed, 4 ignored; 337 before), `cargo fmt --all --check`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`.
  - Web: `pnpm typecheck && pnpm lint && pnpm test` (87 tests, up from 75) `&& pnpm build`.
- **probe-diff with sample aspect ratio compared: 0 mismatches** on features (20/20), seeds (7/7), probe fixtures plus 8 new anamorphic fixtures (24/24, with 2 files routed to the ffprobe fallback) and library-tiny (393/393; 1 rejected by both, as before).

## Per item

### 1. `ItemSummary.parent_title`, `available`, `unwatched_count`

- **Core (api.rs):**
  - `parent_title: Option<String>`: the show for seasons and episodes, the parent work for extras.
  - `available: bool`.
  - `unwatched_count: Option<u32>`: shows and seasons only.
- **Producer: flux-catalog `read.rs`.**
  - `parent_title`: one indexed lookup up the parent chain.
  - `available`: `item.state = 'available'`. The writer already derives item state for leaves, seasons and shows from file and root state, so this costs no extra query.
  - `unwatched_count`: one `count(*)` over the item's episodes, excluding those with `user_state.watched = 1` for the caller. With no user, every episode counts as unwatched.
- **Deviation: one uniform summary.**
  - Before, the catalog gave a season the *show's* title (season name in `subtitle`). On Home it also swapped an episode's title for the show's, with `"S01E03 · Episode title"` in `subtitle`.
  - Now `title` is always the item's own name ("Season 2", "Specials", the episode title). The show is in `parent_title`, and `subtitle` is `"S01E03"` or the season name, as api.rs documents.
  - Why:
    - the web's summary cache paints a detail page from the card that was clicked, so one item must read the same everywhere;
    - the web's season tabs expected season names (they showed the show name four times against the real catalog).
  - Catalog tests updated: `browse.rs` (Home) and `grouping.rs` (season titles).
- **Web:**
  - `useParentTitle` (two requests per card) became `parentTitle(item)`, which reads the field. It is used by Continue-watching cards, the Home hero and the player title.
  - The episode page's "back to show" link uses `parent_title`.
  - The "more from this season" lookup also accepts the catalog's "Specials".

### 2. `Library.roots: Vec<RootStatus { path, available, last_seen }>`

- **Producer: flux-catalog.**
  - `list_libraries` / `library` read the `root` table (`state`, `state_changed_at`, `last_scan_finished_at`); `create_library` returns `available: true, last_seen: None`.
  - **`last_seen`:**
    - **Available root:** the later of the last finished scan and the moment it last came back online.
    - **Unavailable root:** the last finished scan, a lower bound of the last sighting; the schema doesn't keep the come-back time once the root leaves again.
    - **Never scanned:** `None`.
- `CreateLibrary.roots` stays `Vec<String>`.
- **Web:**
  - **Admin:** offline roots come from `RootStatus.available`, still overridden by live `source_availability` events, and show "not responding · last seen 3 days ago". It no longer parses `last_error`.
  - **Library page:** the offline notice is built from `RootStatus`. This also fixed a `[object Object]` that `tsc` can't catch inside a template string.

### 3. `ScanStatus.files_total: Option<u64>`, `dirs_unavailable: u32`

- **Producers:**
  - **flux-catalog:** a library at rest reports `files_total = Some(files_seen)` and `dirs_unavailable = 0`. The catalog doesn't store unread directories.
  - **flux-scan:** the live overlay, documented in the `Scanner::status` contract: `None` until the walk ends; `dirs_unavailable = ScanSummary::unread_dirs.len()`.
- **Web:**
  - Admin's walk bar becomes a true fraction once `files_total` is known, with "1,234 of 5,000".
  - It says "N folders didn't respond during the last scan. Nothing in them was removed."
  - The mock sets `files_total` when its simulated walk finishes.

### 4. `ClientCaps.display_hdr: bool` (`#[serde(default)]`)

- **No deviation in behaviour:** the planner already infers the copy-or-tone-map decision from `VideoCap::hdr`, so it is unchanged, as the task allowed. The semantics are now written down in `plan.rs`:
  - `VideoCap::hdr` lists the formats the client *presents correctly*: on an HDR display, or by tone-mapping itself (Apple platforms, browsers on macOS). The web client already built it that way.
  - `display_hdr` is the display alone (`(dynamic-range: high)`). The server can use it to choose between versions (an SDR version on an SDR display beats a client tone-map) and for diagnostics.
  - Making HDR copy depend on `display_hdr` would video-transcode HDR for every SDR MacBook, which macOS tone-maps fine itself (research/03 §8 fallback chain).
- **Producers:**
  - The web's `detectCaps` sets `display_hdr = matchMedia('(dynamic-range: high)')`.
  - The flux-hls presets set typical values: `false` for desktop browsers and the legacy device, `true` for Apple TV 4K and Android TV.
  - An older client that omits the field deserializes as `false` (tested).

### 5. `PlayResponse.audio: Vec<AudioRef>`

- **Core:** `AudioRef { rendition, track, label, language, codec, channels, default }`, with the fields exactly as specified.
- **Producer:** the new `flux_hls::playlist::audio_refs(asset, plan)`.
  - `rendition` is `AudioKey::path()`: `{track}`, `{track}-aac2` or `{track}-eac36`.
  - `label` is the playlist's `NAME`, deduplicated per codec group exactly as `master_playlist` does it.
  - `codec` and `channels` are what the client receives.
  - `default` marks the planner's first choice.
  - Progressive plans give an empty list.
  - Tested on a real file: every label appears as `NAME="…"` in the master playlist, every rendition URI is present, and the result is empty for direct play.
- **Web mock:** fills `audio` the same way. The player does not use it yet (its audio switch still restarts the session).

### 6. `Identity.tags: Vec<String>`

- **Producer: flux-net.** `RolePolicy::identity_for` copies the WhoIs tags (`PeerFacts::tags`, from Truffle's identity or `tailscale whois`). Localhost identities have none.

### 7. `Reason::CodecNotPackageable { codec }`

- **Producer: the flux-hls planner.** It replaces the `ClientQuirk("TrueHd audio can't be carried in HLS…")` for audio the client decodes but fMP4 can't carry (TrueHD, DTS, Vorbis, PCM, MP2, ALAC).
- **Sentence:** "TrueHD can't be carried in an HLS stream, so flux converts it".
- **Deviation:**
  - It is also used for **video**. Before, a client that decodes VP9 got `VideoCodecUnsupported{Vp9}` ("This device can't decode VP9 video") for VP9-in-MKV on Chrome, which is false; the real cause is that flux's CMAF writer only packages H.264, HEVC and AV1.
  - New planner test, `decodable_but_unpackageable_codecs_say_so`.
  - The web's planner sketch (`plan/predict.ts`) mirrors the rule, with a test.

### 8. Display names in `Reason::explain()`

- **Core:**
  - `Codec::display_name()` plus `Display` ("H.264", "HEVC", "AV1", "VP9", "TrueHD", "E-AC-3", "AC-3", "DTS", "AAC", "Opus", "FLAC", "PGS", "SRT", "MPEG-4 Part 2", …).
  - `Container` ("MKV", "WebM", "MP4", "MOV", "MPEG-TS", "M2TS", "AVI").
  - `HdrFormat` ("SDR", "HDR10", "HDR10+", "HLG", "Dolby Vision").
  - `explain()` formats with `{}` instead of `{:?}`, and `PlaybackPlan::explain()` maps every reason.
- **Web:**
  - `humanizeExplain` and `rustDebugName` are removed. Server sentences are shown as they arrive (stats overlay, item page).
  - `explain.ts` keeps `CODEC_NAMES` / `CONTAINER_NAMES` / `HDR_NAMES`, which golden-rs now emits as a `display` table, and `golden.test.ts` checks them equal to Rust's.
  - **Visible change:** the UI's long codec names follow Rust, so "Dolby Digital (Plus)" became "AC-3"/"E-AC-3", the same as the edge codes already used.

### 9. `ArtRef.attribution`, `generator`

- **Producers:**
  - **flux-meta:**
    - `ArtImage.generator` is filled from `Provenance::model` for generated art (`"provider/model"`, e.g. `openai/gpt-image-1`), and `to_art_ref` passes it through.
    - Frame and local art credit nobody.
    - New `tmdb::attribution::art_ref(url, w, h)` gives provider art credited "TMDB".
  - **flux-catalog** stores both:
    - **migration v2:** `ALTER TABLE art ADD COLUMN attribution/generator`, with an in-place upgrade test from v1;
    - `NewArt { attribution, generator }`;
    - `add_art` writes them, and `read::art_ref` returns them, so lists carry the credit.
- **Web mock:** provider art is credited "TMDB", and generated art has a stand-in model name.

### 10. Tailnet onboarding DTOs

- **Core (exact names from the task):**
  - `TailnetMode { Off, Embedded, System }`.
  - `TailnetState { Off, Starting, NeedsLogin, NeedsApproval, Running, Unavailable, Error }`.
  - `TailnetInfo { mode, state, auth_url, tailnet_name, dns_name, url, owner_login, last_error }`.
  - `Event::Tailnet { info }`.
  - Route docs: `GET /api/v1/tailnet`, and `POST /api/v1/tailnet/connect` with body **`TailnetConnect { mode }`**, a new named type for the body.
- **Deviation (reconciling with the web): `TailnetInfo` also has `approve_url`, `key_expiry` and `system_tailscale`.** The web's Connect page already renders all three: the approve button, the key-expiry warning and the "Tailscale is already running here" hint.
  - `approve_url`: flux-net sets the admin-console machines page (`flux_net::APPROVE_URL`) while approval is pending. That page exists on Tailscale's hosted service only, not on Headscale.
  - `key_expiry`: no producer yet, so it is always `None`. Proposed Truffle change below.
  - `system_tailscale`: `true` in system mode. In the other modes, the server can set it after probing `SystemTailscale::status()`.
- **Name semantics:**
  - `tailnet_name` is the tailnet's DNS suffix (`tail1234.ts.net`).
  - `dns_name` is the server's MagicDNS name.
  - The web used to treat `tailnet_name` as the host. Connect and Admin now use `url` / `dns_name`.
- **Producer: flux-net** (net depends on core, not the other way round):
  - `TailnetStatus::to_api(mode) -> api::TailnetInfo`;
  - `impl From<TailnetMode> for api::TailnetMode`;
  - `TailnetConfig::api_mode()`;
  - `FluxNet::tailnet_info()` and `FluxNet::tailnet_mode()`, which use the configured mode.
  - A bare `From<&TailnetStatus>` can't know the mode while not running (`Starting` and `Failed` happen in both modes), so the mapping takes it.
  - Unit test covers every state.
- **Web:**
  - `TailnetStatus` is renamed to `TailnetInfo`.
  - `connectTailnet(mode)` sends the body.
  - The Connect page also follows `tailnet` SSE events; it still polls every 2 s while waiting, in case the event stream drops.
  - The mock emits the events.

### 11. Dry-run plan: `POST /api/v1/plan` → `PlanPreview { asset_id, plan, explain, label }`

- **Core:** the DTO plus `PlanPreview::new(asset_id, plan)`, which fills `explain` and `label` from the plan.
- **Server wiring is not part of this change.** As integration.md §4.2 says, a preview should plan from the playback index (`HlsEngine::plan`), not the stored probe.
- **Web:** `api.plan(req)` and a mock route; tested. The item page still uses its local sketch until a server exists.

### 12. `POST /api/v1/items/{id}/watched` `{ watched }` → 204

- **Core:** the `MarkWatched` DTO. The route table documents that a session's progress at ≥ 90 % marks the item watched.
- **Catalog:** `Writer::set_watched` (a show or season marks every episode) and the 90 % rule in `put_progress` already existed.
- **Web:** `api.markWatched(id, watched)`; the mock marks leaves and recounts the show (tested).

### 13. `ItemsParams.filter: Option<ItemFilter>`

- **Core:** `Unwatched` (`"unwatched"`), `InProgress` (`"in_progress"`), `Uhd` (`"4k"`), `Hdr` (`"hdr"`), plus a serde test.
- **Beyond the DTO:** flux-catalog implements it, so the server can map it straight through.
  - `ItemsQuery.filter` turns into one SQL condition.
  - Shows and seasons match through their episodes.
  - "Unwatched" and "in progress" are per `user_id`.
  - `4k` matches the 4K/8K badges, and `hdr` the HDR10/HDR10+/HLG/DV badges.
  - `total` counts the filtered set.
  - Tested in `browse.rs::core_v2_summary_fields_root_status_and_filters`.
- **Web:**
  - `libraryItems(…, { filter })`, and the mock serves it (400 on unknown values).
  - The library chips still filter in the browser over the loaded pages. Switching them to the server is a follow-up.

### 14. `Track.sample_aspect_ratio: Option<Rational>`

- **Semantics:**
  - Pixel shape, reduced.
  - `None` means not stated (play as square); `Some(1:1)` means stated square.
- **Producer: flux-probe**, with precedence exactly as ffprobe's `av_guess_sample_aspect_ratio`: the container's value wins, otherwise the codec's. Pinned by fixtures:

  | Source | Where | Notes |
  |---|---|---|
  | Matroska/WebM | `DisplayWidth`/`DisplayHeight`/`DisplayUnit` (+ `StereoMode`) vs the cropped pixel size | Missing display elements default to the pixel size, so an MKV states **1:1 even when the VUI says 32:27**. ffprobe agrees (fixture `anamorphic_vui_only.mkv`). Rounded mkvmerge-style sizes stay unrounded: 853×480 for 720×480 gives **853:720**, as ffprobe prints (FFmpeg reduces with `INT_MAX`; `av_reduce` is ported exactly). |
  | MP4/MOV | `pasp` | Without `pasp`, the H.264/HEVC VUI applies (`anamorphic_vui_only.mp4`, `pasp` renamed to `free`). |
  | H.264 / HEVC | VUI `aspect_ratio_idc` (Table E-1, `Extended_SAR`) | Shared parser. |
  | MPEG-2 | Sequence header aspect code, as FFmpeg's `mpeg_decode_postinit`, including the sequence-display-extension special case | MPEG-1 code 1 only. |
  | AVI | OpenDML `vprp` | Codec-level SAR inside AVI frames (MPEG-4 VOL, in-band SPS) is not read. |

  - `PROBE_VERSION` goes 1 → 2, so a deep rescan re-probes.
  - flux-hls's `IndexTrack` doesn't carry a SAR, so `to_probe_result` gives `None`.
- **Oracle:**
  - `ffprobe::from_json` maps `sample_aspect_ratio`.
  - `probe-diff` compares it, with unstated on either side equal to 1:1; anything else must match exactly.
  - probe-diff now also sees `.mpg`/`.flv` (one shared extension list, item 15) and reports them as `fallback` (flux answers `UnknownFormat`, which is what the scanner sends to ffprobe), not as mismatches.
- **Fixtures:**
  - `crates/flux-probe/tests/gen-fixtures.sh` §15 adds 8: H.264 MKV (DAR display unit + VUI), H.264 MP4 (`pasp` + VUI), VP9 WebM (pixel display size, no VUI), HEVC PAL 16:15 in TS (VUI only), MPEG-2 16:9 in TS (sequence header), MPEG-4 AVI (`vprp`), and the two precedence cases.
  - **They were generated into a scratch copy** (`$FLUX_WORK/tmp/core/probe-fixtures`, the old fixtures cloned with `cp -c`). The rules forbid writing under `media/`, so `media/probe-fixtures` still has the old 18 files. Regenerate there with the script when appropriate; it skips existing files.
  - **Results** (ffprobe 9.0.2; load 3.6–5.0; JSON in `bench/results/flux-core-probe-diff-*.json`):

    | dir | files | ok | mismatched | other |
    |---|---|---|---|---|
    | features | 20 | 20 | 0 | |
    | seeds | 7 | 7 | 0 | |
    | probe fixtures as in `media/` (unchanged) | 18 | 16 | 0 | 2 ffprobe fallback (`.flv`, `.mpg`) |
    | probe fixtures + 8 anamorphic (scratch) | 26 | 24 | 0 | 2 ffprobe fallback (`.flv`, `.mpg`) |
    | library-tiny | 394 | 393 | 0 | 1 rejected by both (`.hidden.mkv`, 4 bytes) |

  - flux reports exactly ffprobe's values on all 8 anamorphic fixtures: 32:27 (×5), 16:15, 853:720, and 1:1 for the VUI-only MKV.
  - Every existing corpus file is 1:1 in ffprobe, so the new fixtures are what exercises this.
- **Other producers:**
  - **flux-catalog:** `synth.rs` maps ffprobe JSON; the test helpers give `None`.
  - **Web:** `types.ts` mirrors the field. `gen-fixtures.mjs` emits 1:1 for the feature clips, and regenerating `fixtures.json` changed nothing else (verified).

### 15. One owner for media extensions

- **Core (`flux_core::media`):** `VIDEO_EXTENSIONS`, `SIDECAR_EXTENSIONS`, `is_video_extension` and `is_sidecar_extension`.
  - **VIDEO:** the union of flux-probe's and flux-names' lists: `mkv mk3d webm mp4 m4v mov qt avi divx ts m2ts mts m2t tp trp mpg mpeg vob wmv asf flv ogv 3gp rmvb`.
  - **SIDECAR:** `srt ass ssa vtt sub idx sup nfo jpg jpeg png webp`.
  - **Excluded, with the reasons in the doc comment:**
    - `iso`: a disc image is a filesystem that needs DVD/Blu-ray navigation, not a stream;
    - `strm`: a text file holding a URL.
- **Thin wrappers:**
  - `flux_probe::MEDIA_EXTENSIONS` is now `flux_core::VIDEO_EXTENSIONS`, and `is_media_path` uses `is_video_extension`.
  - `flux_names::is_video_ext` delegates; `is_video_file` and `split_ext` follow.
  - **Effects:** `.m2t`/`.tp` captures are no longer dropped by flux-names, and `.iso`/`.strm` are no longer "video files".
- `flux-scan/examples/pipeline_smoke.rs` walks `VIDEO_EXTENSIONS ∪ SIDECAR_EXTENSIONS` instead of its own list.

## Web mirror and goldens

- **golden-rs** (`web/scripts/golden-rs`) builds every new field and type literally:
  - enums: `ItemSort`, `ItemFilter`, `TailnetMode`, `TailnetState`;
  - the new reason;
  - samples: `ItemsParams`, `PlanPreview`, `MarkWatched`, `TailnetInfo`, `TailnetConnect`, `Event::Tailnet`;
  - an episode and a show summary, two `RootStatus`, two `AudioRef`;
  - the `display` table.
- **`pnpm golden`** regenerated `golden.json` and `golden.check.ts`. tsc checks every sample against `types.ts`; `golden.test.ts` checks the enum lists (17 now), the display names, the 15 explain sentences and the plan labels.
- **MSW mock** produces every new field and route: summary fields, `RootStatus`, scan fields, art credits, `PlayResponse.audio`, `/plan`, `/items/{id}/watched`, `?filter=`, the `TailnetInfo` + `TailnetConnect` body and `tailnet` SSE events.
- **New web tests:**
  - `client.test.ts`: parent titles, availability, unwatched counts and root status; mark watched; server filter; plan preview; audio renditions; tailnet mode.
  - `predict.test.ts`: TrueHD pass-through is not packageable.
  - `detect.test.ts`: `display_hdr`.

## Deviations at a glance

| # | Task said | Landed | Why |
|---|---|---|---|
| 1 | add `parent_title` | also: `title` is the item's own name everywhere (catalog stopped retitling seasons, and episodes on Home); extras get the parent work | one summary per item (web cache, season tabs) |
| 4 | "HDR copy only if `display_hdr`" | planner unchanged (`VideoCap::hdr` already encodes it); semantics documented | the task's own escape clause; avoids transcoding HDR for every SDR Mac |
| 7 | emit where `ClientQuirk` was used | also replaces `VideoCodecUnsupported` for decodable-but-unpackageable video | the old reason was false |
| 10 | 8 fields | + `approve_url`, `key_expiry`, `system_tailscale`; + `TailnetConnect` body type; mapping is `to_api(mode)` rather than a bare `From` | the web already renders them; the mode isn't knowable from the status alone |
| 13 | DTO | also implemented in flux-catalog `ItemsQuery.filter` | server can pass it straight through |
| 14 | fixtures in the script | generated into a scratch copy, not `media/probe-fixtures` | `media/` is read-only for agents |

## Known gaps

- **flux-server is untouched.** The routes for `/plan`, `/items/{id}/watched`, `/tailnet`, `/tailnet/connect` and the `tailnet` SSE event are documented in `api.rs` and implemented in the mock, not served.
- **`key_expiry` has no producer** (Truffle change proposed below).
- `system_tailscale` is only known in system mode, unless the server probes.
- **SAR gaps:**
  - AVI codec-level SAR (MPEG-4 VOL, in-band H.264 SPS) without `vprp`;
  - MPEG-1 non-square pel-aspect codes;
  - the MP4 `tkhd` matrix-scaled SAR.
  - None of these occur in the corpora.
  - **flux-hls does not write `pasp`** into fMP4 init segments. An MKV whose anamorphism is only in `DisplayWidth` (mkvmerge's usual way) will play stretched over HLS. Owner: flux-hls, which can read `Track::sample_aspect_ratio` from the catalog probe.
- **Web follow-ups (not "trivial"):**
  - audio menu from `PlayResponse.audio`;
  - library chips through `?filter=`;
  - the item page's "Expected on this device" from `POST /plan`;
  - a "mark watched" button.
- **The catalog filter for `4k`/`hdr`** uses `LIKE` on the badges JSON (not indexed); it was not benchmarked at 100k items.

## Re-running

```sh
cd /path/to/flux
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-core
nice -n 19 cargo build --workspace --all-targets -j 4 && nice -n 19 cargo clippy --workspace --all-targets -j 4
nice -n 19 cargo test --workspace -j 4 && cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" nice -n 19 cargo doc --workspace --no-deps -j 4

# probe-diff (writes bench/results/flux-probe-diff-<dir>.json unless --out)
nice -n 19 cargo build --release -p flux-probe --bin probe-diff -j 4
R=$CARGO_TARGET_DIR/release/probe-diff; M=$FLUX_WORK/media
for d in features seeds; do nice -n 19 $R $M/$d; done
nice -n 19 $R $M/library-tiny --frame-hdr never
# anamorphic fixtures: into a scratch copy (or media/probe-fixtures when allowed)
FX=$FLUX_WORK/tmp/core/probe-fixtures
mkdir -p $FX && cp -c -n $M/probe-fixtures/* $FX/ && bash crates/flux-probe/tests/gen-fixtures.sh $FX && nice -n 19 $R $FX

# web
cd web && CARGO_TARGET_DIR=$CARGO_TARGET_DIR pnpm golden && pnpm typecheck && pnpm lint && pnpm test && pnpm build
```
