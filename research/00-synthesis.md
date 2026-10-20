# flux research synthesis

*2026-10-18. This document summarises reports 01–09 in this folder, which hold the evidence, sources and file:line references. Numbers marked "measured" come from this machine: M1 Max, macOS, with media on a USB-attached SSD.*

*The reports cite `initial-discussion.md`, the design conversation that started the project; it is not published. Paths under `$FLUX_WORK` are the scratch directory described in [docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md#environment).*

## The bet in one paragraph

Between 2025 and 2026, Plex put **remote playback of your own files** behind a paywall: the Remote Watch Pass and Plex Pass. It also raised Lifetime to $749.99 and suffered a credential breach. Jellyfin is the open-source default (about 61% of self-hosters), but it has a performance reputation problem:
- 10.11 shipped scan regressions of 6–200×.
- 12.0 launched with out-of-memory bugs.
- It needs a reverse proxy for remote access.

"Jellyfin, but in Rust" is already being attempted (ferrofin, remux), so **flux should not position on Rust**. flux should position on three things users feel:

1. **Sign in with Tailscale and your family streams from anywhere.** No ports, no paywall, no passwords.
2. **Instant.** Scans that don't hurt, playback that starts and seeks immediately.
3. **Lean and calm.** Tens of MB idle, no ads, no "Discover" feed.

## Baselines we measured (Jellyfin 12.2, same machine, same corpus)

| Metric | Jellyfin 12.2, 2026-10-18 (loaded machine) | Jellyfin 12.2, 2026-10-20 re-measurement (benchmarks.md) | flux target |
|---|---|---|---|
| Cold scan, 16.7k-file library (14.1k items), metadata fetch off | **1,442 s (24 min)** | 1,047–1,063 s (2 runs) | ≥ 10× faster |
| Unchanged rescan | **19.6 s** | 18.8–20.8 s | ≤ 1 s, with 0 file opens and 0 DB writes |
| Rescan after adding 50 files | 22.6 s | 20.9–22.4 s | ≤ 5 s from new file to browsable, via the watcher |
| Startup to usable API | **17.3 s** | first start 6.0–7.6 s; restart 4.7–5.2 s (authenticated API; public endpoint 1.4 s) | ≤ 0.5 s |
| Memory: idle / peak during scan | **319 MB / 520 MB** | 344 MiB after start, 476–483 MiB after a full scan / 539–549 MiB | ≤ 50 MB / ≤ 300 MB |
| Time to first HLS segment (MKV remux) | **0.19–0.91 s** | 0.18–0.62 s (per-file medians; 0.40 s with a TrueHD audio transcode; 0.29 s for the first play after a start) | ≤ 0.1 s server-side |
| Cold seek (new segment far ahead) | **0.32–0.84 s** | 0.46–0.76 s in a 2 h file; 1.9–2.1 s for a 42 MB 4K segment | ≤ 50 ms server-side |

The research agents were running at the same time as the 2026-10-18 Jellyfin runs, so the machine was loaded. The targets were set against those numbers and stay as they are.

*2026-10-20:* the comparison was re-run with a revised, same-for-both harness (Jellyfin on loopback only, no image extraction, true medians, rotated playback order) at load 3–11. Every current flux-vs-Jellyfin figure should cite docs/prototype/benchmarks.md. Linux/Docker runs are still to do.

## What the research says flux should do

### 1. Scan engine (report 02, 07): avoid work instead of doing it fast

- **Jellyfin spawns one `ffprobe` per file.** The spawn alone costs 17–28 ms, and a realistic probe takes 30–116 ms (measured). In-process header parsing costs **0.01–0.11 ms** per file, about **300–3,000× less**.
  - Jellyfin's probe settings also read **34.6 MB of a 5-minute TS file**, against 64 KB for the same content in MKV.
- **flux-probe:** our own small sans-IO parser for MKV, MP4, TS and AVI. It needs 2–4 range reads per file, so it works over SMB or Truffle too. ffprobe is kept only as a fallback and as a correctness oracle.
  - The HDR10 metadata in typical encodes lives only in the first frame's SEI, so flux parses it there; it never decodes the frame.
- **Enumeration:** macOS `getattrlistbulk` is about 1.5× faster single-threaded and about 5× faster in parallel than `readdir`+`stat`. Its bigger win is that it returns inode and birthtime for free, and those give stable identity.
- **Zero-touch rescans:**
  - Diff the walk against SQLite on `(dev, inode, size, mtime)` in memory (about 30 ms per 100k files).
  - Unchanged files are not opened, and nothing is written.
  - macOS FSEvents can replay changes made while flux was stopped (verified). The `notify` crate can't, so flux needs its own FSEvents layer.
- **SQLite done right:** one writer, batched transactions (110–270k rows/s, against 6–13k with autocommit), and WAL readers at p99 ≤ 1 ms during bulk writes.

### 2. Playback engine (report 03, 08): every segment is a pure function

- **Jellyfin kills and restarts `ffmpeg` on any backward seek or any jump of more than 24 s.** It also serves segment N only after N+1 exists, polling every 100 ms (verified in source).
- **flux builds a keyframe index once**, from MKV Cues or the MP4 `moov` box, at scan time. It then serves **any** HLS fMP4 segment as a pure function of `(file, index, n)`: read one GOP's bytes, then write `moof`+`mdat`.
  - A seek is just an HTTP GET. There is no process, no restart and no race.
  - A throwaway prototype measured **0.05–0.09% of one core per 1080p stream**, 0 decode errors, and AVPlayer exact seeks in 62–67 ms.
- **The fact check found limits in that prototype.** It mislabelled AV1, and Cues-only boundaries gave 10 s segments. The production version must handle av01, Opus, E-AC-3 (`dec3`), FLAC, and sparse Cues.
- **When the audio is unplayable** (TrueHD, DTS, or AC-3 on Chrome), flux copies the video and transcodes only the audio, cached by recipe. Full video transcodes use keyframe-aligned VideoToolbox/VAAPI segments.
- **An explainable planner** tells the user *why* a stream is being converted.

### 3. Networking (report 04, 06): your Tailscale login *is* your flux account

- **Bring your own Tailscale account.** The server and each client run an embedded node and show a login URL (a QR code on TVs). Each household lives in its own free tailnet: 6 users, unlimited personal devices. Never ship shared keys; that would be a security hole and a breach of the ToS.
- **Truffle already provides most of this:** `listen_tcp` with **unforgeable per-connection WhoIs identity**, the login-URL callback, peers and egress.
- **Missing pieces:**
  - App capabilities (`CapMap`), which let tailnet policy grant roles such as admin, viewer or kid.
  - Tags.
  - Service announcement.
- **A verified Truffle bug cuts long streams.** The sidecar reaps idle connections **per direction**, so a single long download (progressive direct play) gets half-closed after 10 minutes, and hyper's default then aborts the response. The fix is small: track idle time at the connection level, and serve with `half_close(true)`.
- **Throughput is mostly fine, and the CPU cost lives in the sidecar, not the Rust–Go bridge.**
  - On macOS, the tsnet userspace stack costs about **62 ms CPU per MB** (no GSO/GRO), so roughly 1 core per UHD stream.
  - Linux gets GSO/GRO, so NAS servers should be much cheaper. **This is unmeasured and is the top gap.**
  - DERP relays are slow; peer relays have been GA since Feb 2026 and are free. flux must detect the path and cap bitrate on relays.
- **Clients:**
  - Apple TV, Android TV and Fire TV have official Tailscale apps.
  - iOS and Android can embed Tailscale in-process, but **AVPlayer ignores in-app proxies**, so embedded clients need a loopback HTTP gateway.
  - Roku, Samsung and LG TVs can't join a tailnet. They get LAN-only access in v1.
- **Official `tailscale-rs` exists** (0.6, audited) but lacks MagicDNS, certs, app capabilities and mobile support, and has 64 KiB TCP windows. Track it; don't build on it yet.

### 4. Metadata and AI (report 05)

- **TMDB is the primary provider.** Ship a project key, allow bring-your-own keys, and cache for no more than 6 months.
- **Legal tripwire, verified verbatim:** TMDB's API terms prohibit using TMDB content "in connection with … an AI based Application". flux must **keep TMDB data out of every LLM path**, and **ask TMDB in writing** before shipping AI matching. The fact check notes the clause may apply to the whole product.
- **AI forms hypotheses; deterministic code decides.**
  - An LLM turns messy local evidence (path, siblings, probe facts, OCR) into a structured guess.
  - Code then queries the providers and scores the candidates on title, year, **probed runtime** and **audio language**.
  - Jellyfin's scorer uses only title and year.
  - The cloud cost is about $0.0003–0.0005 per hard item.
- **Posters for titles without art:**
  - The default is a **frame poster**: best keyframe plus deterministic typography. It costs $0 and carries no legal risk.
  - genAI posters are opt-in, text-free key art with the title typeset by flux, and are **labelled and provenance-tagged**: C2PA, IPTC `trainedAlgorithmicMedia`, and the EU AI Act Art. 50.
- **AI with the best value per watt:**
  1. Intro/credits detection by cross-episode audio fingerprinting.
  2. Thumbnail selection.
  3. On-device subtitles (Whisper or Parakeet).
  4. Semantic search with embeddings.

## Dogfooding plan: Truffle and Mille

**Truffle (P0):**
1. Connection-level idle reaping, which fixes the stream-cut bug.
2. `CapMap`, tags and user id in WhoIs, so roles come from tailnet policy.
3. A headless-friendly start: re-emit a changed login URL and make the start timeout configurable.
4. Fix the bridge's head-of-line lock.

**Truffle (P1):**
- Sidecar crash supervision.
- Service announcement that works with pretty hostnames.
- Linux performance numbers.
- An iOS loopback gateway for AVPlayer.
- Media-shaped benchmark workloads.

**Mille (P0):**
1. A media-grade scan record (`dev`, `ino`, birthtime, ns mtime) with a streaming visitor and filter pushdown.
2. Fix the O(n²) event coalescer: 10k events take 7.4 s and 40k take 86 s.
3. Explicit "unread" results, with separate first-response and stall deadlines, so a sleeping NAS is never read as "everything deleted".

**Mille (P1):**
- A `getattrlistbulk` fast path: 2.6–4.6× faster than Mille today.
- FSEvents resume.
- A NAPI-free runtime crate.

## Decisions for the maintainer

*Update 2026-10-20: the licence (MIT), Jellyfin-API compatibility (not planned) and the FFmpeg build (separate process; jellyfin-ffmpeg in the Docker image) are now decided; see [docs/DECISIONS.md](../docs/DECISIONS.md). The list below is kept as written.*

1. **License.** The research recommends `MIT OR Apache-2.0` for engine crates (so they can flow back into Truffle and Mille), **AGPL-3.0** for the server (the Immich precedent), MPL-2.0 for clients, and a DCO with no CLA. If NAS-vendor bundling matters more than reciprocity, use Apache-2.0 everywhere. Decide before the first outside contribution.
2. **TMDB.** Email TMDB for written clarification on the AI clause, or keep AI fully isolated from TMDB data.
3. **Client strategy for v1:**
   - Our own web client, plus Apple TV.
   - Apple TV can either rely on the official Tailscale tvOS app (two apps) or embed Tailscale via a tvOS spike (one app, unproven).
   - A clean-room Jellyfin-API compatibility layer would let Infuse, Swiftfin and Findroid work on day one. This needs a design for mapping their password login onto Tailscale identity, which is an open gap.
4. **FFmpeg build.** An LGPL build has no x264/x265 software encoders but keeps distribution simple. A GPL build (like jellyfin-ffmpeg) is fine as a separate process, with obligations attached only to that binary.

## Biggest risks and open gaps (report 09)

- **Everything was measured on macOS.** Linux NAS/Docker (N100, Pi 5, SMB, ZFS), cold or spun-down HDDs, and Linux tsnet CPU per Gbps are all unmeasured.
- **The remux engine needs real-world MKV pathologies:** sparse or missing Cues, in-band parameter sets, Dolby Vision P7/P8, header stripping, VFR.
- **Home upload bandwidth** (often 10–40 Mb/s) is the real remote bottleneck. flux needs a remote bitrate ladder driven by path classification.
- **The security model for friends from other tailnets** is undesigned: does WhoIs/CapMap work across tailnets, and how are libraries restricted? The folder-picker RPC also needs path-traversal rules.
- **Tailscale platform and policy risk.** Fallbacks are iroh 1.x or LAN pairing; Headscale loses certs and sharing.
- **No Plex baseline yet.** Plex needs an account claim to run.

## The five claims to prove publicly (report 01 §11)

1. **Scan:** a 10k-file library is browsable in under a minute, rescans are free, and API p95 stays ≤ 50 ms during a scan.
2. **Playback:** TTFF p50 ≤ 300 ms on LAN and ≤ 800 ms over a direct tailnet path; seek p95 ≤ 500 ms.
3. **Footprint:** idle ≤ 50 MiB, scan peak ≤ 300 MiB, cold start ≤ 200 ms.
4. **Remote:** 0 open ports; an invitee goes from link to playing in ≤ 3 min; ≥ 90% of sessions on direct paths.
5. **Metadata:** ≥ 98% correct auto-match on a hard-names set, and 100% of items have art, with AI art labelled.
