# 09 — Adversarial fact-check and gap analysis of reports 01–08

Date: 2026-10-18. Scope: the most decision-relevant claims from the eight flux research reports, checked against primary sources (vendor pages, specs, GitHub/crates.io APIs) and local code (Truffle `d6f6664`, Mille `c8160d5`, Jellyfin `08c2ba6`, tailscale-rs `0d41a6f`, tailscale.com `v1.104.1` fetched from GitHub raw). I also re-ran three local measurements: the walk benchmark, the FSEvents replay, and the in-process remux probe on two original and three new files.

Verdict scale: **confirmed** means I found the primary source or code and it says this. **partially true** means the core is right but a number, date or framing is wrong or overstated. **refuted** means the source says otherwise. **unverifiable** means only secondary sources exist.

Machine state during my re-runs: load average about 8–12, down from 64–140 during the original runs. All runs were warm-cache, on the USB SSD. My outputs went to `$FLUX_WORK/factcheck09/` and were deleted afterwards.

---

## 1. Headline corrections (read these first)

1. **03's "AV1 → av1C copied verbatim, AVPlayer and ffmpeg accepted [V]" is refuted for the probe that actually exists.** `playback-remux-probe/src/main.rs:584-585` maps only `V_MPEGH/ISO/HEVC` to `hvc1`. Every other codec falls through to `avc1/avcC`. On `media/features/av1_opus.mkv` the output is labelled `h264 (avc1)`. jellyfin-ffmpeg 8.1.3 then logs 56 "Error splitting the input into NAL units", and mp4-atom fails with `UnknownVersion(129)`.
   - The H.264/HEVC results do reproduce. HEVC10 open-GOP used 0.048% of a core, and both HEVC files decoded with 0 errors.
   - So the "remux is solved" claim covers H.264/HEVC + AAC only. AV1, Opus, E-AC-3 (no CodecPrivate) and DV are untested.
2. **The remux probe's segments follow the Cues, not the 6 s target.** `synth_hevc10_opengop_10min.mkv` has 60 cues in 600 s, so the probe planned 60 segments of **9.4–10.0 s** against a 6 s target. Apple's HLS authoring spec says 6 s.
   - Real libraries mix mkvmerge files (a cue per video keyframe) and ffmpeg-muxed files (a cue per cluster, often sparser).
   - flux therefore needs a fallback keyframe index (block-header scan) when Cues are sparse. That costs 3.7–6.4 s per 466 MB per report 03, which moves work to scan time.
3. **Report 04's DERP "per-client rate limit (2.2 Mbps observed)" conflates two things.**
   - `derp/derpserver/derpserver.go:648-703` does have a configurable `PerClientRateLimitBytesPerSec`. A value of 0 means unlimited, and the production values are unpublished.
   - The 2.2 Mbps figure comes from a single iperf3 TCP stream from **Delhi to Minnesota** (Tailscale blog, 2026-01-26), with a 31.1 Mbps internet baseline. That is a long-haul relay path, so it is not evidence of the rate limit.
   - The "cap bitrate on DERP" design is still right, but nobody has measured DERP throughput on a same-continent path.
4. **The tailscale-rs announcement date is wrong in 01.** The first public preview was **2026-04-15**: blog post `tailscale-rs-rust-tsnet-library-preview`, CHANGELOG v0.2.0. The 2026-08-31 post 01 cites does call it "experimental (pre-alpha)". But 0.6.0 (2026-09-17) completed a third-party crypto audit and dropped the `TS_RS_EXPERIMENT` gate, so "pre-alpha" understates where it is (CHANGELOG.md:16-39). 04 has this right.
5. **"Dim stalled since 2021" (01) is refuted.** The GitHub API shows the last commit on 2025-09-21 (a crate split) and substantive work until 2023-12. 08's "dormant since 2025-09" is accurate.
6. **ferrofin is real but tiny, and it targets the old API.** It has 40 stars and was created 2026-08-12. The "412/412 operations" are against the **Jellyfin 10.11.8** contract, not 12.x, which removed `/emby` routes. Its benchmarks ran on a Ryzen 9 9950X3D in 8-core containers. "The Rust-Jellyfin niche is already taken" (01) overstates it: the niche is *occupied*, not *won*.
7. **Getattrlistbulk's single-thread gain is smaller at low load.** My re-run at load about 10 on the same 80,800-file / 6,402-dir tree, medians of 5:

   | Method | Median |
   |---|---|
   | std read_dir+lstat, 1 thread | 388 ms |
   | getattrlistbulk, 1 thread | 265 ms (**1.46×**, vs 02's 1.96×) |
   | std, rayon parallel | 150 ms |
   | getattrlistbulk, parallel | 81 ms (**1.84×** vs parallel std, vs 02's 1.6×; **4.8×** vs sequential std, vs 5.1×) |

   The direction holds, and the parallel case is the one that matters. Expect about 1.5× single-thread, not 2×.
8. **Licensing recommendations conflict (01 vs 08).** 01 says to default to an **LGPL** FFmpeg build. 08 says to "start from jellyfin-ffmpeg 8.1.x", which is built `--enable-gpl --enable-version3`, so it is **GPLv3**. Both work out of process. But an LGPL build has **no libx264/libx265**, so software H.264 transcoding on a NAS without a usable GPU has no good encoder. Nobody costed that.
9. **TMDB clause scope is wider than "keep TMDB data out of the LLM path".** The verbatim text, downloaded and grepped:
   - §1.A reserves "the right … to use the TMDB APIs or TMDB Content in connection with, or for training, a machine learning or artificial intelligence based Application".
   - §1.C prohibits "Use the TMDB APIs or TMDB Content in connection with, including for training, a machine learning (ML) or artificial intelligence (AI) based Application."
   - The object is the *Application*. If flux markets AI metadata as a headline feature, TMDB could deem flux as a whole an "AI based Application", even if no TMDB byte reaches a model. 05 lists this as open question (c). 01's mitigation ("design the LLM path to exclude TMDB content") may not be enough. Get TMDB's answer in writing before AI ships.

---

## 2. Claim-by-claim verification

| # | Claim (report) | Verdict | Evidence / correction |
|---|---|---|---|
| 1 | Plex remote video paywall: smart TVs and consoles from 2026-03-23; third-party clients such as Infuse from late Sep 2026; Firecore suggests Tailscale (01) | **confirmed** | forums.plex.tv/t/…/937015 (posted 2026-03-11, effective 2026-03-23); community.firecore.com/t/…/60825 (posted 2026-09-21, "later this week", "solutions like Tailscale or a more traditional VPN") |
| 2 | Lifetime Plex Pass $749.99 from 2026-07-01 (01) | **confirmed** | forums.plex.tv/t/new-lifetime-plex-pass-pricing/938910 (announced 2026-05-18) |
| 3 | Remote Watch Pass "raised" $1.99 → $2.99 on 2026-06-01 (01) | **partially true** | plex.tv/plans: $1.99/$19.99 was an *introductory* price for cycles before 2026-06-01, and $2.99/$29.99 is the "regular list price". The lifetime post says RWP pricing "will remain unchanged". The effect on users is the same. |
| 4 | TMDB ToS bans use "in connection with … AI based Application"; 6-month cache cap; last updated 2023-10-20 (01, 05) | **confirmed** (scope wider, see §1.9) | themoviedb.org/api-terms-of-use, downloaded: §1.A, §1.C, §2.A (LLM/chatbot = commercial) |
| 5 | Jellyfin 10.11 scan regression 2m30s → 29m53s (#15070); photos 26 s → 86 min → 3m06s (#14386) (01, 02) | **confirmed** | GitHub API. Note that "3m06s" was measured on a **12.0 master build** before release. #15070 is closed. |
| 6 | Jellyfin 12.0 on 2026-09-07; .NET 10, FFmpeg 8.1, /emby removed; 12.1 on 09-15, 12.2 on 10-05 (01, 03) | **confirmed** | jellyfin.org/posts/jellyfin-release-12.0 (2026-09-07); GitHub release v12.0 at 2026-09-08T01:38Z, which explains 03's "09-08". The blog post does not mention libbitsub, so 03's libbitsub claim rests on the GitHub release notes only. |
| 7 | ferrofin: 6.7× less idle memory, 15.8× faster cold start vs 12.0-rc7, all 412 operations (01, 08) | **partially true** | The README numbers match (self-reported, 9950X3D). The 412 operations are against the 10.11.8 contract; 40 stars. No scan, time-to-first-frame or seek numbers, which confirms flux's open lane. |
| 8 | Dim stalled since 2021 (01) | **refuted** | Last commit 2025-09-21; real activity until 2023-12 (GitHub API) |
| 9 | selfh.st 2025: Jellyfin 60.9%, Plex 29.9% of 4,081 (01) | **confirmed** | selfhst/cdn 2025-results.json: 2,487 and 1,219 responses. This is a self-selected self-hoster audience and does not represent Plex's mass market. |
| 10 | Tailscale Personal: 6 users, unlimited user devices, 50 tagged resources at +$1 each, non-commercial; Standard $8, Premium $18 (04) | **confirmed** | tailscale.com/pricing |
| 11 | Tailscale ToS (2026-08-25) §2.3 forbids reselling and "benchmarking or competitive analysis" (04) | **confirmed** | tailscale.com/terms, downloaded. 04 correctly warns not to publish Tailscale comparisons. |
| 12 | tailscale-rs 0.6.1, BSD-3, 64 KiB TCP buffers hard-coded (≈26 Mbps at 20 ms RTT), NAT traversal unfinished (04) | **confirmed** | `crates/ts_runtime/src/lib.rs:119` `tcp_buffer_size: 64 * 1024`; README:71; crates.io `tailscale` 0.6.1 BSD-3-Clause. The arithmetic checks: 64 KiB / 20 ms ≈ 26.2 Mb/s. |
| 13 | tailscale-rs announced 2026-08-31, pre-alpha (01) | **partially true / wrong date** | Preview blog 2026-04-15; v0.2.0 2026-04-15; the 08-31 post does say "pre-alpha" |
| 14 | tsnet gVisor buffers: RX 8 MiB / TX 6 MiB max (04, 06) | **confirmed** | tailscale v1.104.1 `wgengine/netstack/netstack_tcpbuf_default.go` |
| 15 | tsnet `ListenTLS` sets no ALPN, so Truffle TLS is HTTP/1.1 only (03, 06) | **confirmed** | `tsnet/tsnet.go:1429-1431` (`&tls.Config{GetCertificate: s.getCert}` only); Truffle sidecar calls it at `main.go:1497,2244`. RFC 023's "h2 free" (`023-http-serving.md:387`) applies only to the Go-engine path. |
| 16 | DERP per-client rate-limited; 2.2 Mbps vs peer relay 27.5 Mbps (04) | **partially true** | See §1.3. Peer relays GA on 2026-02-18 on all plans: **confirmed** (tailscale.com/blog/peer-relays-ga). |
| 17 | Node sharing: tagged machines can't be shared; reusable invite 1,000 uses; 30-day expiry; quarantine (04) | **confirmed** | tailscale.com/kb/1084/sharing. Also, a shared machine "cannot be … accessed by tagged machines on another tailnet", so friends' clients must be user-owned too. |
| 18 | Apple TV 4K (4th gen, A19): AV1, DV 8.1 and 5, HDR10+, HLG; no TrueHD/DTS (03) | **confirmed** | apple.com/apple-tv-4k/specs. Announced 2026-10-13 per secondary press; **ships 2026-11-13**, so almost no users have it yet. |
| 19 | Samsung 2026: no DTS; MSE ≤ FHD (03) | **confirmed** | developer.samsung.com 2026 video specs |
| 20 | Roku: no muxed CMAF; AV1 DASH-only; DTS pass-through (03) | **confirmed** | developer.roku.com/dev/docs/media |
| 21 | Chrome native HLS default in 142 with bugs; force MSE (03) | **confirmed, with nuance** | status.mux.com: the rollout began in **Chrome 141** (141.0.7390.123+ affected); 142 is default-on without the fix; the fix is expected in 143. |
| 22 | Firefox 145 adds MKV; Chrome needs 145+ for MKV (03) | **confirmed, with caveat** | Firefox 145 notes (2025-11-11). Jellyfin docs say "MKV support in Firefox is currently **disabled in Jellyfin**, as there are significant issues", so don't plan on Firefox MKV direct play. |
| 23 | Jellyfin restarts ffmpeg on a backward seek or a jump of more than 24/SegmentLength segments, and serves segment N only after N+1 exists, polling every 100 ms (03) | **confirmed** | `DynamicHlsController.cs:1472-1494` (gap check), `:1953-1977` (`Task.Delay(100)`) |
| 24 | Jellyfin probes with `-analyzeduration 200M -probesize 1G … -show_frames -only_first_vframe`, one ffprobe per file (02) | **confirmed** | `ConfigurationOptions.cs:18-19`; `MediaEncoder.cs:434-470` (GetExtraArguments), `:513-519`. `-only_first_vframe` is used only when the prober supports it (jellyfin-ffmpeg); chapters only when `extractChapters`. |
| 25 | Jellyfin item ID = MD5(type.FullName + lowercased path) (02) | **confirmed** | `LibraryManager.cs:797-818`. Lower-casing applies when `EnableCaseSensitiveItemIds` is false, which is the default. |
| 26 | Jellyfin TMDB scorer 8/4/2 + hard-coded key (05) | **confirmed** | `TmdbUtils.cs:35,42-44,209-254` |
| 27 | In-process MKV→CMAF remux: ≈0.08% of a core, 0 decode errors, AV1 `av1C` passes through (03) | **partially true** | Reproduced for HEVC: index 29 ms, 0.048% of a core, p99 per-segment CPU 1.9 ms. On a new 4K 60 Mb/s HEVC file: 0.48% of a core, decodes clean. **Refuted for AV1** (§1.1). Segment length follows Cues (§1.2). The probe's peak memory was **947 MB** for a 466 MB file, so it is not a model for server memory use. |
| 28 | getattrlistbulk 1.96× single-thread / 5.1× parallel (02); 2.6× vs Mille (07) | **partially true** | Re-run gives 1.46× / 4.8× (§1.7). I did not re-run the comparison against Mille. |
| 29 | FSEvents replays changes made while no watcher ran, via a persisted sinceWhen, in ≈30 ms (02) | **confirmed** | Re-ran `fsev`: 4 events, HistoryDone after 26.6 ms. **Caveat:** historical flags are *cumulative per path*: a file created, modified then deleted came back as `Created|Removed|Modified`. Replay says *which paths to re-stat*, not what happened in order. Persistence across reboot and external-volume remount is still untested. |
| 30 | notify (incl. main 9.0.0-rc.6) hard-codes `kFSEventStreamEventIdSinceNow` (02, 07) | **confirmed** | notify main `src/fsevent.rs:395`; the `since_when` field has no public setter |
| 31 | Truffle bridge idle timeout runs per direction (default 600 s) against its own "BOTH directions" comment; hyper `half_close` defaults to false (06) | **confirmed** (code) | `main.go:58-61` vs `idleCopy` at `:2875-2891`; hyper-1.12.0 `server/conn/http1.rs:260-268` ("Default is `false`"). I did not re-run 06's end-to-end repro. |
| 32 | Truffle discovery admits only `truffle-{appId}-*` hostnames, so a custom-hostname server is invisible to `peers()` (06) | **confirmed** | `provider.rs:299-306,747-750`. The tradeoff is documented at `node.rs:1810-1822`. |
| 33 | Sidecar crash leads to Stopped with no restart; 300 s auth timeout; tags hard-coded None (06) | **confirmed** | `provider.rs:689-700`, `:877`; `node.rs:2066` |
| 34 | Bridge holds the `incoming_channels` lock across `send().await`; 256 cap applies to handshakes, "silently dropped" (06) | **mostly confirmed** | `bridge.rs:124-147,206-215`: the permit is released after handoff, so the cap counts handshakes. Drops are logged at `warn`, so not fully silent. |
| 35 | Truffle Swift: macOS 14 / iOS 18.1 only; TailscaleKit linked on iOS only (01, 06) | **confirmed** | `Package.swift:38-41,67` |
| 36 | Truffle same-host throughput 330–550 Mb/s; the sidecars use ≈6 logical CPUs (03, 04, 06, 08) | **confirmed** | `benchmark/README.md:615-620,727-731`; release builds per README:42-44. The range 08 quotes (350–500) is narrower but consistent. |
| 37 | Truffle QUIC: `max_concurrent_uni_streams(0)`, fixed ALPN (08) | **confirmed** | `quic.rs:142,146,172,181` |
| 38 | Mille: WalkedEntry lacks dev/ino/btime; RealFs stub; O(n²) coalesce; notify 6.1.1; watch_runtime has 0 napi references (07) | **confirmed** | `walker.rs:77-96`; `fs.rs:47-86`; `watcher.rs:605-608` (a linear `find` per slot); Cargo.lock; `grep -c napi` = 0 |
| 39 | FFmpeg 9.0 shipped 2026-08-04; ffmpeg-next maintenance-only, ≤ FFmpeg 8.0; rsmpeg last release 2025-08 (08) | **confirmed, with nuance** | crates.io: ffmpeg-next **crate 9.0.0** (2026-08-05) still supports FFmpeg 3.4–8.0, which doesn't even include jellyfin-ffmpeg's 8.1. But **ffmpeg-the-third 6.0.0+ffmpeg-9.0** (2026-08-09) is current, so "bindings lag upstream" is true only of ffmpeg-next and rsmpeg. |
| 40 | MoQ transport draft-22 (2026-10-01), still a WG document, IESG milestone Dec 2026 (08) | **confirmed** | datatracker.ietf.org |
| 41 | Claude Haiku 5.5 at $0.10/$0.50 per MTok for prompts ≤100K (05) | **confirmed** | claude.com/pricing |
| 42 | FLUX.2-klein-4B is Apache-2.0 (05) | **confirmed** | HF API: license `apache-2.0`, created 2026-01-14, not gated |
| 43 | TheTVDB: free under $50k revenue with attribution; $1k to $250k; $10k to $1M (05) | **confirmed** | thetvdb.com/api-information. That page doesn't mention subscriber PINs; that part rests on 05's other sources. |
| 44 | EU AI Act Art. 50 applies from 2026-08-02, with grace to 2026-12-02 for marking (05) | **unverifiable (secondary only)** | Several compliance blogs agree on the dates. They conflict on whether the Digital Omnibus has entered into force. Check the Official Journal before any commercial claim. |
| 45 | Swiftfin tvOS on the App Store with v1.5, 2026-07-16 (01) | **confirmed** | Swiftfin discussion #1294 ("Last Updated 2026-07-16"); release 1.5 at 2026-07-15T04:55Z; now 1.6.1 |
| 46 | Tailscale tvOS app: tvOS 17+, "must be a home hub" (01) | **partially true** | Home hub is a listed prerequisite so the Apple TV stays reachable while asleep. That matters when the Apple TV is the *server* or router; it matters less when it is a client. |

---

## 3. Contradictions between reports

1. **How the server's data path should work (04 vs 06), which matters for P0.** 04 says to put the sidecar's `proxy:add` in front of flux's Axum server on 127.0.0.1. 06 says to serve directly from Rust via `node.listen_tcp`/`listen_tcp_opts(tls)` with raw hyper, and notes the idle-reap bug *doesn't* affect the proxy path. The proxy path, however, gives only forgeable-on-loopback `Tailscale-User-*` headers, while listen_tcp gives token-authenticated identity but hits the half-close bug. Pick one deliberately.
   - **My read:** use listen_tcp with `half_close(true)` and a long idle timeout now, and fix Truffle. The proxy path is acceptable only if flux binds a unique loopback port and checks that the connecting process is the sidecar.
2. **HTTP/2 (08 vs 03/06).** 08's P0 says to deliver over "HTTP/1.1 + HTTP/2". Over Truffle's TLS listeners only HTTP/1.1 is possible without the ALPN patch 06 lists as P2 (§2 #15). HTTP/2 works only on LAN-direct or system-Tailscale paths.
3. **FFmpeg build licence (01 vs 08):** LGPL default vs jellyfin-ffmpeg (GPLv3). See §1.8.
4. **In-process FFmpeg later (03 vs 08).** 03's P2 suggests "ffmpeg-next 9.x / rsmpeg" for warm in-process transcodes. 08 shows ffmpeg-next is maintenance-only (≤8.0) and rsmpeg has been stale since 2025-08. The only current binding is ffmpeg-the-third.
5. **Apple TV client path (04 vs 06).** 04 P1 calls for embedded Tailscale on TV clients plus a tvOS embedding spike. 06 says to rely on the official Tailscale tvOS app for v1. Both are defensible; 06 is cheaper, but then "just log in with Tailscale *in flux*" becomes two apps on Apple TV.
6. **tailscale-rs maturity and date (01 vs 04).** 01 says announced 2026-08-31, pre-alpha. 04 says preview 2026-04-15, 0.6.1 audited. 04 is right on the date; the 08-31 post still calls it pre-alpha.
7. **Dim status (01 vs 08):** "stalled since 2021" vs "dormant since 2025-09". 08 is right.
8. **getattrlistbulk speed-up (02 vs 07 vs my re-run):** 1.96× / 2.6× / 1.46× single-thread. The ratios depend on machine load; the parallel advantage (≈1.8× vs parallel std) is the robust number.
9. **hunch cost (02: 369 µs/name vs 05: 229 µs/name):** probably load-related. Either way, 10k names cost 2–4 s, which matters only on first scan.
10. **Jellyfin baseline numbers.** 01 cites 12.0 idle memory of 356–409 MiB and cold start of 2.2–2.7 s. A harness run on this machine at 22:44 today (results in `$FLUX_WORK/bench/results/jellyfin-scan-library-20261018-224449.json`, not cited by any report) shows **Jellyfin 12.2**:

    | Metric | Value |
    |---|---|
    | Idle RSS | 319 MB |
    | Startup to ready | 17.3 s |
    | Cold scan | 1,442 s for 15,645 items |
    | Unchanged rescan | 19.8 s |
    | Peak RSS | 520 MB |

    *2026-10-20 note:* this run was on a machine loaded by research agents. The same-harness re-measurement (docs/prototype/benchmarks.md) gives:
    - cold scan 1,047–1,063 s;
    - unchanged rescan 18.8–20.8 s;
    - first start 6.0–7.6 s to ready, restart 4.7–5.2 s to an authenticated API answer;
    - idle RSS 344 MiB after start and 476–483 MiB after a full scan, peak 539–549 MiB;
    - TTFS 0.18–0.62 s on remux (per-file medians);
    - far seeks 0.46–0.76 s.

    The table above is kept as the record of what was checked on 2026-10-18.

    The playback run (`…/jellyfin-playback-20261018-224518.json`) shows remux-plus-audio-transcode HLS **time-to-first-segment of 0.19–0.91 s and seek latency of 0.49–0.84 s**. Those are the real numbers flux must beat. Note that a 12.2 unchanged rescan is about 20 s, not the 10.11 catastrophe 01 leads with. I haven't verified the harness config (for example whether metadata providers were on).
11. **Truffle throughput ranges** (330–550 vs 350–500 Mb/s): these are the same data quoted differently. No real conflict.

---

## 4. Gaps: questions no report answered that matter for building flux

1. **How do Jellyfin-API clients authenticate with Tailscale identity?** 01 says to bootstrap TVs through Infuse, Swiftfin and Wholphin, and 04 says permissions come from WhoIs/CapMap. But Jellyfin clients log in with `AuthenticateByName` (username/password) or Quick Connect. Nobody designed how a tailnet identity becomes a Jellyfin access token: auto-issue on WhoIs, Quick Connect auto-approve, or a passwordless user. Without that, "no passwords" is false for every third-party client.
2. **The client mix decides which engine work matters, and nobody modelled it.** Infuse on Apple TV direct-plays almost everything (MKV, TrueHD/DTS decoded on-device), so for that client flux's remux engine is mostly idle. Remux matters for web, Swiftfin/AVPlayer, Samsung/LG/Roku and mobile. No report estimates which clients early adopters will actually use, and that decides P0 ordering between the remux engine and the Jellyfin-API layer.
3. **The target deployment is unmeasured.** Every measurement is macOS on an M1 Max. Nothing covers:
   - Linux x86 NAS (N100/Celeron), Docker, ext4/btrfs/ZFS, or SMB/NFS-mounted libraries;
   - Synology/Unraid packaging;
   - tsnet CPU per Gbps on Linux, where GSO/GRO *is* enabled (so macOS numbers may be pessimistic);
   - VAAPI/QSV transcode latency.

   The flagship "crush Jellyfin" claims must be shown on that hardware.
4. **Cold-cache and HDD scan behaviour.** Every scan number is warm-cache on SSD. Rotational NAS disks, spun-down disks (first-touch latency of 5–10 s) and SMB latency dominate real scan times, and no report measured them. 02 and 07 both flag this; nobody ran it.
5. **Remote upload bandwidth and the ABR ladder.** The DERP discussion assumes the bottleneck is Tailscale. For most households it is home upload (often 10–40 Mb/s), which forces a video transcode for remote 4K remux. Nobody specified the default remote ladder, encoder budget (concurrent HW sessions on an N100), or how path classification (04) feeds the planner (03).
6. **Software-encode fallback under each licence choice.** If the FFmpeg build is LGPL (01), what encodes H.264 when there is no GPU or the GPU is busy? openh264 (Cisco binary only for patent cover), a GPL x264 out of process, or nothing? (§1.8)
7. **A sparse or missing Cues strategy for MKV and a keyframe index for TS/AVI.** The stateless-remux design depends on a dense keyframe index. Nobody measured what fraction of a real library has usable Cues, or the cost of building an index at scan time for those that don't (§1.2).
8. **An audio track policy for in-process remux.** The probe muxes only H.264/HEVC + AAC. E-AC-3 (`dec3` synthesis), Opus (`dOps`), FLAC, TrueHD/DTS (needs transcode) and multiple audio renditions in HLS have no verified path.
9. **Consolidated schema and data model.** The initial discussion proposes Work/Edition/Asset/Track/Location/UserState. 02 proposes three-layer identity, 05 proposes field provenance and match_decision tables, and 03 wants playback prep at scan time. Nobody merged these into a schema with migrations, backup/restore, or a policy for SQLite on a network share (corruption risk).
10. **Security model for shared-in users and the admin surface.** Open questions:
    - whether WhoIs/CapMap work for *shared-in* users from other tailnets (04 lists this as untested);
    - how flux limits a shared friend to some libraries;
    - path-traversal rules for the folder-picker RPC (07);
    - the loopback header-forgery risk (06) on a multi-user NAS host.
11. **Tailscale dependency risk and policy changes.** Tailscale changed plans in 2026-04 and the ToS in 2026-08. What happens to flux users if the Personal plan shrinks, or if a household outgrows 6 users? Headscale degrades (no ts.net certs, no sharing). Nobody priced a fallback such as iroh, which is already 1.3.0 and MIT/Apache, or LAN-only pairing.
12. **The FSEvents replay on external and removable volumes, across reboots and remounts.** This is critical because media libraries usually live on external disks. It is still untested (02), and the cumulative-flags semantics (§2 #29) need a reconcile design.
13. **A Plex baseline.** 01 and 03 recommend benchmarking against Plex, but no report or harness measured Plex PMS scan, time-to-first-frame or seek on this corpus.
14. **Apple TV gateway feasibility.** An in-process loopback HTTP gateway feeding AVPlayer works in principle on iOS (06). Nobody checked tvOS background or app-lifecycle limits, or whether TailscaleKit's Go runtime can even be built for tvOS (06 says "plausible but unsupported").
15. **Subtitle end-to-end on Apple clients.** PGS/VobSub to WebVTT via OCR (03 P1) is the plan, but no report measured OCR accuracy or cost, or specified behaviour for forced or SDH tracks.
16. **The AI agent's actual hit rate.** 05 shows the deterministic matcher's floor (11/20 offline top-1), but nobody measured how many of a real library's misses an LLM hypothesis loop recovers. The AI feature's value is therefore unquantified.

---

## 5. What held up well

- Every Truffle and Mille code citation I checked (16 locations) matched the code, give or take a line. The 06 and 07 code reviews are trustworthy.
- Jellyfin source citations (HLS restart, probe arguments, item IDs, TMDB scorer) are exact.
- The platform-capability claims (Apple TV, Samsung, Roku, Chrome, Firefox) all trace to primary vendor docs.
- The economics and licensing facts (Tailscale plan, TMDB terms, TVDB tiers, model licences, LLM prices) match the primary pages as of today.

## Sources (primary)

- tailscale.com/pricing · tailscale.com/terms · tailscale.com/kb/1084/sharing · tailscale.com/docs/install/appletv.md · tailscale.com/blog/peer-relays-ga · tailscale.com/blog/peer-relays-international-networks · tailscale.com/blog/tailscale-rs-rust-tsnet-library-preview · tailscale.com/blog/easier-building-with-tailscale
- raw.githubusercontent.com/tailscale/tailscale/v1.104.1/{tsnet/tsnet.go, derp/derpserver/derpserver.go, wgengine/netstack/netstack_tcpbuf_default.go, tool/gocross/autoflags.go}
- themoviedb.org/api-terms-of-use · thetvdb.com/api-information · claude.com/pricing · huggingface.co/api/models/black-forest-labs/FLUX.2-klein-4B
- forums.plex.tv/t/changes-coming-to-remote-streaming-on-smart-tvs/937015 · forums.plex.tv/t/new-lifetime-plex-pass-pricing/938910 · plex.tv/plans · community.firecore.com/t/changes-to-plex-remote-streaming-requirements/60825
- jellyfin.org/posts/jellyfin-release-12.0 · jellyfin.org/docs/general/clients/codec-support · GitHub API (jellyfin/jellyfin issues 15070, 14386, 17871; releases; jellyfin/Swiftfin; Dusk-Labs/dim; lostb1t/remux; mangoleaf/ferrofin)
- apple.com/apple-tv-4k/specs · developer.samsung.com 2026 TV video specs · developer.roku.com/dev/docs/media · status.mux.com/incidents/g4wcv5cm02sg · firefox.com 145.0 release notes
- datatracker.ietf.org/doc/draft-ietf-moq-transport · docs.rs ffmpeg-next README · crates.io API (versions listed in §2) · linuxiac/phoronix (FFmpeg 9.0 date)
- github.com/selfhst/cdn assets/surveys/annual/2025-results.json · notify-rs/notify main `notify/src/fsevent.rs`
- Local: Truffle `truffle` @d6f6664; Mille @c8160d5; Jellyfin `$FLUX_WORK/src/jellyfin` @08c2ba6; tailscale-rs `$FLUX_WORK/research-04/tailscale-rs` @0d41a6f; re-runs of `target/release/{walk,fsev,playback-remux-probe}`
