# flux architecture (prototype)

flux is an open-source, Rust media server for personal movie and TV libraries. It aims to beat Plex and Jellyfin on the things users feel:

- **Scan speed**, and staying responsive while scanning.
- **Time to first frame and seek latency.**
- **Footprint.**
- **Zero-config private remote access:** sign in with Tailscale, no ports, no passwords.

The research behind every decision here is in `research/01..09`. This document is the contract the prototype crates are built against.

## Principles

1. **Avoid work instead of doing it faster.**
   - Read container indexes rather than decoding.
   - Never spawn ffmpeg to remux.
   - Never re-probe unchanged files.
   - Never write the DB when nothing changed.
2. **Every playback segment is a pure function** of `(asset, recipe, index)`. A seek is a `GET`: no process restart, no shared state.
3. **Explainable decisions.**
   - The playback planner returns `Reason`s.
   - The name parser returns `evidence`.
   - The matcher returns a confidence and evidence.
4. **Bounded everything.** Bounded queues, per-device I/O admission, explicit caches, cancellation tokens.
5. **Never mistake "unreachable" for "deleted".** An asleep NAS makes items *unavailable*, never removed.
6. **AI never blocks scan or playback, and never sees TMDB content** (TMDB API terms §1.C; research/05). That includes agents: what flux shows an agent leaves TMDB's text and images out (docs/AGENT-NATIVE.md).

## Crates

```
crates/
  flux-core     shared plain-data types (media facts, name guesses, plans, API DTOs)
  flux-probe    sans-IO header probe: MKV/WebM, MP4/MOV, MPEG-TS/M2TS, AVI; HDR SEI; ffprobe oracle
  flux-names    filename/folder parser: canonical fast path → hunch (guessit port) → post-rules
  flux-catalog  SQLite (rusqlite, WAL): schema, single batched writer, read queries
  flux-scan     orchestrator: Mille walk → diff vs catalog → fingerprint → probe → parse → write
  flux-hls      playback planner + playback index + stateless fMP4/CMAF segmenter + HLS playlists
                + ffmpeg-backed audio/video transcode segments
  flux-meta     TMDB client + offline TMDB export index, matcher, AI hypothesis trait,
                artwork: frame posters (no AI) and opt-in genAI posters
  flux-net      Truffle node (embedded Tailscale), tailnet + LAN listeners, WhoIs identity, hyper serving, tailnet HTTPS
  flux-server   the binary: axum routes (flux_core::api), sessions, jobs, SSE, static web UI,
                agent operations (flux_core::agent) over HTTP and MCP
  flux-cli      `flux`, the command line: a client of a running server (the agent operations, MCP over stdio)
web/            React 19 + TypeScript + Vite web client (hls.js), talks flux_core::api
tools/          corpus generators and benchmark harnesses (Jellyfin baselines)
```

Dependencies point strictly downward, with no cycles:

- `server → {scan, hls, meta, net, catalog}`
- `scan → {catalog, probe, names}`: the orchestrator needs the catalog's snapshot and writer functions, not just its types (docs/prototype/integration.md §5.1).
- `hls → probe` (allowed, to reuse `flux_probe::ffprobe`)
- every crate → `core`. `catalog`, `meta` and `net` depend on `core` only.
- `cli` depends on `core` only: it talks to a running server over HTTP.

## Data flow

```
             ┌──────────── scan (flux-scan) ─────────────────────────────────────────┐
 roots ───►  │ Mille walk_scan (getattrlistbulk on macOS, statx on Linux)           │
             │   → ScanEntry{name, kind, size, mtime_ns, btime_ns, dev, ino}         │
             │ diff vs catalog (dev, ino, size, mtime) ── unchanged → nothing        │
             │ new/changed → fingerprint (xxh3-128 of first+last 64 KiB, oshash)    │
             │            → flux-probe (2–4 range reads, in-process)                 │
             │            → flux-names (path + siblings + library kind)              │
             │ catalog writer: batched txns (≤1000 rows or ≤50 ms)                   │
             └───────────────────────────────────────────────────────────────────────┘
                     │ items browsable immediately; metadata/art fill in later
                     ▼
 flux-meta jobs: match (TMDB search + duration/year scoring; AI hypothesis for hard names)
                 → artwork (provider art, else frame poster, genAI only if opted in)

 play:  POST /api/v1/play {item, caps} → flux-hls planner → PlaybackPlan{reasons}
        → direct: /media/{asset}/file (Range)
        → HLS:    /media/{plan}/master.m3u8 → v/init.mp4, v/{n}.m4s, a/{t}/{n}.m4s, s/{t}.vtt
                  each segment = read source byte range [kf_n, kf_n+1) → moof+mdat
```

## Key invariants per crate

### flux-probe

- Sans-IO state machine: `Prober::feed(offset, &[u8]) -> Step::{Need{offset,len}, Done(ProbeResult)}`.
- Drivers: a local `pread` driver with a 64 KiB page cache. The same parser must work over HTTP range reads or Truffle later.
- Sniff magic bytes, never trust extensions.
- Skip Matroska Cues and MP4 sample tables during scanning (load them at play time).
- HDR static metadata comes from the container when present. Otherwise set `needs_frame_sei` and read the first keyframe's SEI (parse, don't decode).
- Fallback: `ffprobe` (subprocess) for unknown containers, and as the differential-testing oracle.

### flux-hls

- **Playback index**:
  - MKV: SeekHead → Cues → keyframe `(pts, cluster offset)`, plus CodecPrivate.
  - MP4: `moov` sample tables (counts bounded by the file size before any allocation).
  - MPEG-TS/M2TS with H.264/HEVC: one sequential packet scan (`ts`), remuxed in process.
  - Anything else (ASF/WMV, FLV, RealMedia, MPEG-PS, AVI, rejected TS, an MP4 or Matroska file flux's own readers refuse, which is then named `unknown` so it is never copied): one short, bounded ffprobe run (streams and the first 600 packets: the real frame rate and GOP length), plus where FLV/ASF packets end. No packet scan, so playback starts at once; ffmpeg paths only.
- **Segment plan:** cut at keyframes at or after the 6 s target. Index-less files: a fixed 6 s grid on the frame grid of the rate their video is converted to; each video run seeks ffmpeg by time (a margin early: 1.5 GOPs for MPEG-PS/TS, whose seeks land on any packet), converts to that constant rate and pads past the video's end, so every piece has a known frame count.
- **fMP4 writer:** init `ftyp+moov(mvex)`; media `styp+moof+mdat`.
- **Timestamps:**
  - DTS synthesized from sorted PTS.
  - A global offset D = reorder depth × frame duration so that CTS ≥ 0.
  - An exact rational timescale.
- Codec configuration passes through: avcC, hvcC, av1C, esds, dOps. Synthesize dac3/dec3.
- Demuxed audio renditions; always offer stereo AAC.
- Audio transcode via ffmpeg, per segment and stateless (byte-identical however requested).
- Video transcode via ffmpeg, keyframe-aligned and cacheable; frame counts from the index's real frame times. Its bytes are equivalent, not identical, across encoder runs, so it is served with weak ETags (`HlsEngine::is_deterministic`).
- Sources hold no file descriptor while idle (closed after 30 s, or at once when the file is deleted or replaced).

### flux-catalog

- One writer thread, many read connections (`query_only`).
- `WAL`, `synchronous=NORMAL`.
- Domain model: Library, Root, File (location), Asset (probed media), Item (movie / show / season / episode), UserState.
- File identity: `(root, rel_path)` plus `(dev, ino)` and `btime` for rename/move detection.
- Where each value came from (`item_field`): a provider, a person or an agent. A value a person or an agent set is locked: the matcher and the scanner's re-parse never overwrite it, and a merge carries it to the item that stays (`fields`).
- Every edit through the agent operations is a changeset (`changes`): applied or proposed, journaled field by field, undoable while the fields still hold what it set.
- A file's fix (`file_fix`, `fixes`) corrects what its name says: every placement goes by the stored guess with the fix applied, so a fix holds through rescans and re-parses. A pin (`of`) follows its work through merges.
- Metadata files beside the videos (`.flux.json`, `.nfo`: `meta_file`, `metafiles`) are read once per change and rank between agents and providers; a value one no longer gives goes back to flux.
- Collections (`collections`) are picked titles and/or a rule; a rule never counts a value the reader may not see.

### flux-scan

- Progressive availability: an item is browsable right after probe and parse.
- A zero-change rescan opens zero files and writes zero WAL frames.
- Per-device I/O semaphore. Priorities: play-triggered > watcher > scheduled > deep work.
- Symlinks: links inside the library's roots are aliases and never followed; links leaving them are followed once per target, and a new file reached that way is cataloged only if it probes as media (docs/prototype/scan.md "Symlinks").
- Unreachable is never deleted, below the root too: a dangling link or an unmounted sub-volume is *unread*.
- Failed probes are retried with backoff; a request that joins a running scan gets one follow-up pass.

### flux-net

- Serve with hyper `http1::Builder::new().half_close(true)`. This is required to survive Truffle's per-direction idle reap (research/06 §3.4).
- WhoIs identity is attached to each request (`Extension<Option<Identity>>`).
- Connections are admitted per remote IP and in total (loopback has its own budget); over a limit a connection gets an immediate 503. A response that makes no write progress for 60 s is aborted.
- HTTPS on the tailnet (system mode): rustls with tailscaled's MagicDNS certificate (`tailscale cert`), on the same port as plain HTTP. The first byte of a connection (a TLS handshake record is `0x16`) picks the protocol. TLS ends in flux, so WhoIs still sees the caller's own address. The certificate is fetched again every 12 hours and swapped in for new handshakes; a replaced tailnet listener takes its TLS along.

### flux-server

- Clients browse from the catalog only, never from the filesystem.
- Scan progress is pushed over SSE.
- One router layer guards every listener: a `Host` that isn't one of the server's names gets 421 (DNS rebinding); an unsafe `/api/` call needs `X-Flux-Request: 1` and no foreign `Origin`, else 403 (CSRF).
- A file is served only from inside its library, or when it is exactly the file the scan recorded behind a link that leaves it.
- Everything a caller can create is bounded: plans (on disk, LRU), sessions, event streams.
- Agent operations (`agent`, docs/AGENT-NATIVE.md) are defined once (name, JSON Schemas, who may call them) and served at `/api/v1/ops/{name}`, over MCP at `/mcp`, and by the `flux` command line. A caller that names an agent never receives TMDB-sourced text or images, nor what flux read from `.nfo` files (`agent::policy`): principle 6 holds for agents too.
- Files move only by a plan a person applies (`agent::files`): steps stay inside their library folder, are checked again right before they run, and use the platform's no-replace rename (never replacing, never copying across drives, never through a link); the catalog follows and items keep their ids.

## Dogfooding: Truffle and Mille

flux dogfoods two libraries by the same maintainer: [Mille](https://github.com/vibecook-dev/mille) (file walking and watching) and [Truffle](https://github.com/vibecook-dev/truffle) (embedded Tailscale). The changes flux needed were developed on `flux/dogfood` branches of each and are upstreamed from there. flux consumes Mille as a git dependency on `vibecook-dev/mille`, and Truffle's `truffle-core` 0.7.20 (the release with these changes) from crates.io. To build against local checkouts, use the untracked `.cargo/config.toml` `[patch]` setup in [CONTRIBUTING.md](../CONTRIBUTING.md).

- **Mille.** Changes:
  - A media-grade scan record (`ScanEntry` with dev, ino, btime, ns mtime) and a streaming `walk_scan` visitor with filter pushdown.
  - A macOS `getattrlistbulk` fast path.
  - A fix for the O(n²) `coalesce_events`.
  - Explicit `unread` results and HDD-friendly deadlines.
- **Truffle.** Changes:
  - Connection-level idle reaping, instead of per direction.
  - `CapMap` and tags in WhoIs.
  - Re-emitting a changed auth URL, with a configurable start timeout.
  - A fix for the bridge head-of-line lock.

Phase D (`flux/dogfood-d`, fast-forwarded into `flux/dogfood`) added, in Mille, order-independent Smart links, a debounced watcher that keeps deletions, stateful coalescing, FSEvents `sinceWhen` resume and a store-free snapshot writer; in Truffle, key expiry, control-server and sidecar-exit events (protocol 10) and a cheaper bridge copy loop (docs/prototype/mille-changes.md, truffle-changes.md).

## Environment

The prototype docs and tools refer to two directories by environment variable:

- **`$FLUX_WORK`** is a scratch directory with the test corpus; the maintainer's lived on an external SSD. Benchmark results, build output for the harnesses, temp files and the corpus go under it. A file named `$FLUX_WORK/QUIET` pauses the benchmark and end-to-end harnesses before each measured phase (the prototype was developed on a shared machine).
- **`$FLUX_TEST_MEDIA`** is the test corpus, `$FLUX_WORK/media` in the docs. Tests and tools that need media read it; see [CONTRIBUTING.md](../CONTRIBUTING.md#media-corpus-tests).

Paths such as `$FLUX_WORK/wt/<name>` in `docs/prototype/` and `research/` are the git worktrees and scratch trees those reports were written in; they are kept as a record and are not needed to build flux.

- **Machine:** the prototype was developed and measured on an M1 Max, 64 GB, macOS.
- **Test media:**
  - `$FLUX_TEST_MEDIA/features/`: feature clips, made by `tools/corpus/gen-features.sh`. `manifest.json` holds ffprobe facts for each clip.
  - `$FLUX_TEST_MEDIA/library/`: 16.7k-file messy library, made by `tools/corpus/gen-library.py`. `library.manifest.jsonl` holds the ground truth.
  - `$FLUX_TEST_MEDIA/library-tiny/`: 424-file version of the library.
  - `$FLUX_TEST_MEDIA/playback-research/`: 10-minute files from the playback research.
- **Baselines:** Jellyfin 12.2 baseline harnesses are in `tools/bench/`; results are in `$FLUX_WORK/bench/results/`.
