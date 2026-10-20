# 03 — Playback and streaming engine for flux

*Research date: 2026-10-18. Scope: the server-side playback planner and delivery engine (direct play, remux, hybrid audio, transcode, subtitles, HDR, trickplay, HTTP delivery), with measurable targets. Client UI, metadata and Tailscale embedding are covered in other reports.*

**How to read this report**

- **[V]** means verified against a primary source, or measured on this machine for this report.
- **[S]** means a secondary source, such as a trade press article, forum post or third-party table.
- **[I]** means my inference or estimate.

Confidence is stated where it matters. Local code is cited as `path:line`. Jellyfin source is cited at master commit `08c2ba6e` (the Jellyfin 12.2 era).

---

## 0. TL;DR

1. **Seeks are the main place to beat Jellyfin, ahead of raw remux CPU.** Jellyfin delivers remux and transcode through an `ffmpeg` process tied to the playback session.
   - On a backward seek, or a jump more than `24 / SegmentLength` segments ahead (4 segments, or 24 s, at the 6 s default), Jellyfin kills `ffmpeg` and restarts it with `-ss` [V] (`Jellyfin.Api/Controllers/DynamicHlsController.cs:1472-1512`).
   - It serves segment N only after segment N+1 exists, or after the job exits. It checks for this with 100 ms polling [V] (`DynamicHlsController.cs:1953-1977`).
   - flux can make every segment a pure function of `(file, index, recipe, N)`. A seek is then just `GET seg/N`, with no process restart, no shared state and no race.

2. **In-process MKV→CMAF remux in Rust works, and I measured it.** I built a throwaway probe of about 600 lines that turns MKV into fMP4 HLS without `ffmpeg`. It is not production code. It used Matroska Cues plus a hand-written ISO BMFF writer ([§4.5](#45-experiment-in-process-mkv--cmaf-remux-probe)). On 10-minute 1080p files (HEVC Main10 open-GOP, and H.264 with B-frames, each with AAC):
   - **Index:** 64–74 ms, reading only Info, Tracks and Cues.
   - **Remux CPU per segment** (parse, `moof` and copy): **p50 0.5 ms, p99 2.4–3.2 ms**.
   - **Total process CPU:** 0.08–0.09 % of one M1 Max core at 1× real time, including disk writes.
   - **Correctness:** every packet was preserved (14,386/14,386 video and 28,126/28,126 audio). `ffmpeg` decoded the output with zero errors.
   - **AVPlayer (macOS) playback:** ready in about 0.2 s, exact seeks completed in 62–67 ms, and the error log was empty [V].

3. **Raw CPU is not the differentiator.** `ffmpeg -c copy` used about the same CPU for the whole file (0.53 s CPU, against 0.47–0.53 s for the probe). It restarted at a seek point in about 55–58 ms on a warm cache [V].
   - What actually wins is statelessness: no spawn or kill per seek, any segment is random-access and cacheable, and there is no "wait for N+1" rule.
   - Precise control also matters: the Dolby Vision 7→8.1 rewrite, parameter-set hygiene, and progressive CMAF chunks.

4. **Transcode time-to-first-frame is set by seek alignment and encoder setup.** On the M1 Max with VideoToolbox (VT), HEVC10→H.264 1080p, a 6 s first segment took:
   - 1.56–1.80 s with an accurate `-ss` (decoding up to a 10 s GOP just to discard it).
   - **0.72–0.92 s** when started exactly on a keyframe (`-noaccurate_seek`).
   - Opening a VT decode+encode session costs about 0.25–0.4 s on its own [V].

   Design rules that follow:
   - Align transcode segments to source keyframes (the Kyoo approach).
   - Use a short first segment.
   - Keep encoder sessions warm, which needs in-process libav or long-lived workers.
   - Stream in-progress segments as CMAF chunks.

5. **2026 client landscape** (details in [§2](#2-client-capability-matrix-2026)):
   - **Chrome:** HEVC needs hardware support [V]. Native HLS is on by default since Chrome 142 and had launch bugs, so prefer MSE through hls.js [V]. Chrome 145+ can play MKV progressively, per the Jellyfin docs [S].
   - **Firefox 145** added Matroska with AVC, HEVC, VP8/9, AV1, AAC, Opus and Vorbis [V].
   - **Apple TV 4K (4th gen, A19, announced 2026-10-13)** finally lists AV1 and Dolby Vision 8.1 [V].
   - **Samsung 2026 TVs:** no DTS, and MSE is limited to FHD [V].
   - **TrueHD and DTS:** still need server-side audio transcode for browsers, Apple TV and Samsung.

6. **Recommended architecture:**
   - An explicit, explainable **playback planner**. Its order of preference is direct file > index-driven in-process remux > remux + bitstream fixups (DV 7→8.1, parameter-set strip) > remux + per-segment audio transcode > supervised FFmpeg video transcode with keyframe-aligned, cacheable segments.
   - Build everything playback needs at scan time: the keyframe index, segment plan, init segments, subtitles and trickplay.
   - Serve through Truffle's HTTP proxy. That path is HTTP/1.1 over tsnet; Truffle measured 330–550 Mb/s on the same host [V]. Exotic I/O (io_uring, sendfile, kTLS) is irrelevant at home scale.

---

## 1. What "beating Plex and Jellyfin" means, and where their time goes

| Metric | Definition (client-observed unless noted) | Why it matters |
|---|---|---|
| **TTFF** (time to first frame) | Tap "Play" → first video frame rendered | The perceived quality of the whole product |
| **Seek latency** | Seek committed → first frame at the new position | Scrubbing, skipping intros, "rewind 10 s" |
| **Server CPU per stream** | CPU-seconds per minute of playback, including child processes | NAS and mini-PC headroom; how many concurrent streams fit |
| **Reliability** | Playback failures, stalls, A/V desync, and seek failures per 100 sessions | Users churn on "it didn't play" far more than on 200 ms |

### Jellyfin's mechanics (verified in source)

- **Fixed or keyframe segments.** Transcodes use equal-length segments, because `ffmpeg` forces keyframes with `-force_key_frames:0 "expr:gte(t,n_forced*N)"` (`MediaBrowser.Controller/MediaEncoding/EncodingHelper.cs:2043`).
  - Video remux of MKV uses the keyframes from the Matroska Cues (`src/Jellyfin.MediaEncoding.Hls/Playlist/DynamicHlsPlaylistGenerator.cs:37-46`; `src/Jellyfin.MediaEncoding.Keyframes/Matroska/MatroskaKeyframeExtractor.cs:20-55`).
  - Extraction is only allowed for `["mkv"]` by default (`MediaBrowser.Model/Configuration/EncodingOptions.cs:63`).
  - **Jellyfin therefore already does keyframe-exact remux playlists for MKV. It still produces the bytes with an `ffmpeg` process.** [V]
- **Segment length.** For video copy it is 6 s, or 3 s for "segmented live streams". Transcodes default to 3 s unless the client asks otherwise (`MediaBrowser.Controller/Streaming/StreamState.cs:74-104`). [V]
- **Seek handling.** For each segment request, Jellyfin returns the segment immediately if the file already exists on disk. Otherwise it restarts `ffmpeg` in any of these cases [V] (`DynamicHlsController.cs:1463-1530`):
  - the init segment is being requested;
  - there is no current job;
  - `segmentId < currentTranscodingIndex`;
  - `segmentId - currentTranscodingIndex > 24 / SegmentLength`.

  The restart itself is not cheap: it waits for in-flight requests (100 ms polling), runs `KillTranscodingJobs(...)`, deletes the last file, and then runs `StartFfMpeg(...)` with `-ss` and `-copyts -avoid_negative_ts disabled ... -start_at_zero`.
- **Segment readiness rule.** Segment N is served only once segment N+1 exists or the job has exited. Readiness is polled every 100 ms (`DynamicHlsController.cs:1953-1977`). This adds 0–100 ms of jitter plus one extra segment of production time to every "cold" segment. [V]
- **Defaults.** Throttling is off, segment deletion is off (`SegmentKeepSeconds=720`), and tone mapping is off (`EncodingOptions.cs:23-38`). [V]
- **Jellyfin 12.0** (released 2026-09-08; 12.2 on 2026-10-05) [V] ([release notes](https://github.com/jellyfin/jellyfin/releases/tag/v12.0)):
  - moved to FFmpeg 8.1;
  - added a spec-compliant `dvh1` HLS variant for Dolby Vision Profile 5;
  - added Dolby Vision in MKV on webOS 25+, and AV1 fMP4 stream copy on TV clients;
  - replaced libpgs with **libbitsub** (PGS and VobSub rendering in the browser);
  - added an `HlsAudioSeekStrategy` option;
  - fixed "potential A/V desync in HLS when transcoding video while remuxing audio".
- **Pain points.** Users report freezes when seeking during transcode start-up, loops back to segment 0, and "Current HLS implementation doesn't support non-keyframe breaks" warnings [S] ([forum thread](https://forum.jellyfin.org/t-media-always-restarts-after-segment-92-during-transcoding?pid=62097), [symfonium report](https://support.symfonium.app/t/seeking-not-working-properly-while-transcoding/3967)). Moderators describe the post-seek buffer rebuild as expected behaviour [S] ([forum](https://forum.jellyfin.org/t-solved-slow-transcoding-playback-start)).

### Plex (closed source, weak evidence)

- Plex's own definitions [S] ([support.plex.tv/articles/200250387](https://support.plex.tv/articles/200250387-streaming-media-direct-play-and-direct-stream/), via search summary):
  - **Direct Play** sends the file unmodified.
  - **Direct Stream** repackages compatible audio and video into another container during playback.
  - Subtitles the app cannot handle force a burn-in transcode.
- Plex Web uses **DASH** (`/video/:/transcode/universal/start.mpd`) whenever it cannot direct play [S] ([forum](https://forums.plex.tv/t/plex-behind-a-https-proxy-media-must-be-transcoded-in-order-to-use-the-dash-protocol/672918)).
- The "Plex Transcoder" is an FFmpeg fork [I/S].
- I found no first-party TTFF or seek numbers for Plex. **Both competitors must be measured with the harness in [§13](#13-measurable-targets-and-how-to-measure-them)**, not assumed.

---

## 2. Client capability matrix (2026)

Legend: ✅ supported; 🔶 conditional; ❌ no; ? unverified.

### 2.1 Video codecs and containers

| Client | H.264 | HEVC | AV1 | MKV | MP4 / fMP4 HLS | Notes |
|---|---|---|---|---|---|---|
| **Chrome / Edge (desktop)** | ✅ | 🔶 hardware decoder only. Default since Chrome 107 on Windows, macOS, ChromeOS and Android; Chrome 108 on Linux with VAAPI. Works in file, MSE and WebCodecs [V] ([Chromium](https://www.chromium.org/audio-video/), [StaZhu](https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding)). Edge on Windows needs the "HEVC Video Extension" [S] | ✅ (Chrome 70+) [S] ([caniuse](https://caniuse.com/av1)) | 🔶 progressive only. Chromium lists Matroska [V]; Jellyfin's docs say it needs Chrome 145+ [S] ([Jellyfin codec table](https://jellyfin.org/docs/general/clients/codec-support/)). Not in MSE | ✅. Native HLS on by default since **Chrome 142**; 141.0.7390.123+ had playback-error bugs, so Mux recommends forcing MSE [V] ([Mux incident](https://status.mux.com/incidents/g4wcv5cm02sg)) | Dolby Vision 8.1/8.4 plays as HEVC on Chrome 122+ when HEVC is supported. P5 only encrypted on Windows, behind a flag [S] ([StaZhu](https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding)) |
| **Firefox** | ✅ | 🔶 Windows 134+, macOS 136+, Linux 137+ (VA-API), Android 137+ [S] ([Phoronix](https://www.phoronix.com/news/Firefox-137-VA-API-HEVC), [Fx Android notes](https://www.mozilla.org/firefox/android/137.0/releasenotes)) | ✅ | ✅ **Firefox 145**: "Added Matroska support for … AVC, HEVC, VP8, VP9, AV1, AAC, Opus, and Vorbis" [V] ([145 notes](https://www.firefox.com/en-US/firefox/145.0/releasenotes/)). Jellyfin still disables MKV for Firefox [S] | ✅ through MSE (hls.js) | No AC-3/E-AC-3 |
| **Safari macOS / iPadOS** | ✅ | ✅ (hvc1/dvh1) | 🔶 hardware only (M3+, A17 Pro+), Safari 17+; fixed in 17.5 for iPhone 15 Pro [S] ([Bitmovin](https://bitmovin.com/blog/apple-av1-support)) | ❌ | ✅ native HLS; MSE and ManagedMediaSource | This dev machine (M1 Max) **cannot** play AV1 in Safari [I] |
| **Safari iOS** | ✅ | ✅ | 🔶 as above | ❌ | ✅ native HLS. ManagedMediaSource since 17.1, only when an AirPlay alternative is present or remote playback is disabled [V] ([WebKit 17.1](https://webkit.org/blog/14735/webkit-features-in-safari-17-1/)) | Use native HLS on iPhone |
| **Apple TV 4K (3rd gen, A15)** | ✅ | ✅ | ❌ | ❌ (AVPlayer) | ✅ HLS fMP4 | DV P5 only, per [S] ([Infuse forum](https://community.firecore.com/t/dolby-vision-profile-7-8-support-ts-mkv-files/19713?page=80)) |
| **Apple TV 4K (4th gen, A19, announced 2026-10-13, ships 2026-11-13)** | ✅ | ✅ | ✅ | ❌ (AVPlayer) | ✅ | Spec page: "Dolby Vision (Profile 8.1 and 5), HDR10, HDR10+, HLG" and "AV1, VP9, H.265/HEVC, H.264/AVC" [V] ([apple.com specs](https://www.apple.com/apple-tv-4k/specs/)) |
| **Android / Google TV (Media3)** | ✅ | ✅ device-dependent | 🔶 hardware, or the Media3 1.9 dav1d extension [S] ([Media3 1.9](https://android-developers.googleblog.com/2025/12/media3-190-whats-new.html)) | ✅ Matroska supported (progressive; DASH demuxed only) [S] ([Media3 formats](https://developer.android.com/media/media3/exoplayer/supported-formats)) | ✅ | Google TV Streamer: Dolby Vision, HDR10, HDR10+, HLG; Dolby audio only, no DTS listed [S] ([Google Store](https://store.google.com/product/google_tv_streamer_specs)) |
| **LG webOS 25** | ✅ | ✅ | ✅ | ✅ (MPEG-2, MPEG-4, H.264, VP8, VP9, HEVC, AV1) | ✅ | DTS, DTS-HD and DTS:X are listed, but "some models may not support" them. Only WebVTT is listed for subtitles [V] ([webOS 25 AV](https://webostv.developer.lge.com/develop/specifications/video-audio-250)). Users report DV P5/P8 in MKV now works [S] |
| **Samsung Tizen 2026** | ✅ | ✅ (MKV, MP4 and TS only) | ✅ up to 2160p120 at 40 Mbps | ✅ | ✅ | **"The DTS Audio codec is not supported on 2026 TVs"**; **"MSE supports resolutions up to FHD; it does not support UHD"**, so 4K needs AVPlay [V] ([Samsung 2026](https://developer.samsung.com/smarttv/develop/specifications/media-specifications/2026-tv-video-specifications.html)) |
| **Roku** | ✅ | ✅ (4K models) | 🔶 **DASH only** | ✅ (AVC, HEVC) | ✅ HLS (TS or CMAF). **"Muxing audio and video isn't supported for CMAF"**, so audio must be demuxed [V] ([Roku docs](https://developer.roku.com/dev/docs/media)) | DV listed as `dvhe.05` / `dvav.09`; DTS is pass-through only; side-loaded WebVTT, SRT and TTML |

### 2.2 Audio

| Codec | Chrome | Edge (Win) | Firefox | Safari / Apple TV | Android TV | LG webOS 25 | Samsung 2026 | Roku |
|---|---|---|---|---|---|---|---|---|
| AAC | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| AC-3 / E-AC-3 | ❌ [S] | ✅ [S] | ❌ | ✅ (E-AC-3 JOC for Atmos in HLS) [V] | ✅ (usually pass-through) | ✅ | ✅ | ✅ (pass-through) |
| Opus in MP4 | ✅ | ✅ | ✅ | 🔶 stereo, iOS 17 / macOS 14+ [S] | ✅ | ✅ (MKV) | ✅ | MKV only |
| FLAC in MP4 | ✅ | ✅ | ✅ | ✅ in HLS fMP4 (Apple spec 2.2/2.5) [V] | ✅ | — | — | MKV only |
| DTS / DTS-HD MA | ❌ | ❌ | ❌ | ❌ | 🔶 pass-through | 🔶 model-dependent | ❌ | pass-through |
| TrueHD (Atmos) | ❌ | ❌ | ❌ | ❌. Infuse decodes to LPCM and loses the Atmos metadata [S] | 🔶 pass-through on some boxes | ? | ❌ | ❌ |

Apple HLS authoring rules that matter for flux [V] ([HLS Authoring Spec](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices), revision 2025-06-26):
- 1.5 / 1.39: HEVC and AV1 **MUST** be fMP4.
- 1.10: prefer `hvc1` / `dvh1` (parameter sets in the sample description) over `hev1` / `dvhe`.
- 2.3: stereo AAC **MUST** be provided.
- 2.25: FLAC and ALAC **MUST** be fMP4.
- 5.2: subtitles MUST be WebVTT or IMSC1-in-fMP4.
- 6.1: I-frame playlists MUST be provided for scrubbing.
- 7.4: segments MUST start with an IDR.
- 7.5: target duration SHOULD be 6 s.
- 7.7: segments MUST NOT exceed the target duration by more than 0.5 s.

These are App Store authoring rules. AVPlayer tolerates more, and our probe played 9.4–16.2 s source-GOP segments fine [V].

### 2.3 Planner implications

- **Browsers** (MSE, MMS or native HLS) need fMP4 or CMAF with AAC, Opus or FLAC. Edge, Safari and Apple TV can also take AC-3/E-AC-3.
- **MKV direct play** is now plausible on Chrome 145+, Firefox 145+, LG, Samsung, Roku and Android, when the codecs fit. It never works on AVPlayer, Safari or Apple TV. That is exactly where the **in-process remux** pays off.
- **TrueHD and DTS** are effectively always transcoded for Apple, browsers and Samsung. The only exception is TVs or AVRs with pass-through on native apps.
- **4K on Samsung** must use AVPlay, not MSE. **Roku** needs demuxed CMAF audio. Design for **demuxed audio renditions** from day one.

---

## 3. Delivery modes compared

| Mode | Server work | Seek behaviour | Jellyfin | Plex | flux proposal |
|---|---|---|---|---|---|
| Direct play (progressive file) | Byte-range serving | Client range requests: ~1 RTT plus decode | ✅ | ✅ | ✅ (`Range`, `sendfile` when unproxied) |
| Remux to HLS | Jellyfin: `ffmpeg -c copy` per session | Jellyfin: restart `ffmpeg` beyond 24 s or backwards | ✅ "Remux" | "Direct Stream" | **In-process, stateless, index-driven** |
| Remux + audio transcode | `ffmpeg`: video copy, audio encode | Restart, as above | "Direct Stream" | "Direct Stream" | Per-segment, **stateless** audio transcode ([§5](#5-hybrid-copy-video-transcode-audio)) |
| Video transcode | `ffmpeg` hardware or software | Restart at `-ss` with accurate seek | ✅ | ✅ | Keyframe-aligned segments, warm sessions, recipe cache ([§6](#6-transcoding-when-unavoidable)) |

---

## 4. In-process remux in Rust (the core proposal)

### 4.1 Idea

Treat a playable rendition as a **virtual file system computed from an index**:

```text
scan time:   file ──► container index (keyframes: pts, byte offset; codec configs; track list)
                  ──► segment plan (keyframe-aligned boundaries, ~6 s target)
                  ──► init segment(s) (ftyp+moov+mvex per track/variant), cached
request:     GET /v/{asset}/{recipe}/seg/{n}.m4s
                  ──► byte ranges [cue(n) .. cue(n+1)) from the source
                  ──► parse blocks in memory (only this GOP) ──► styp+moof+mdat header
                  ──► respond: header bytes + source sample bytes (copy or vectored write)
```

Consequences:
- **Seek = GET of segment N.** No job, no restart, no wait for N+1, no kill or cleanup races.
- **Deterministic and cacheable.** Segments carry ETag = H(file identity, recipe version), so they can be immutable in every HTTP cache, including the client's.
- **Exact playlists before any bytes exist.** Durations come from the index. This is the same trick Jellyfin already uses for MKV playlists [V], extended to producing the bytes as well.
- **Prior art for just-in-time packaging from an index:**
  - [Kaltura nginx-vod-module](https://github.com/kaltura/nginx-vod-module): "on-the-fly repackaging of MP4 files to DASH, HDS, HLS, MSS", MP4 input only upstream [S].
  - Unified Origin (commercial).
  - Kyoo (keyframe-aligned lazy transcoding, [blog](https://zoriya.dev/blogs/transcoder)).
  - The MoQ `hang` crate's timeline track, "exactly what an HLS/DASH origin needs to render playlists without touching media bytes" [V] (`hang-0.21.16/src/timeline.rs:1-20`).

### 4.2 Index sources and their cost

| Container | Source of keyframes and offsets | Cost | Fallback |
|---|---|---|---|
| **MKV / WebM** | `SeekHead` → `Info`, `Tracks`, `Cues` (CueTime, CueClusterPosition, CueRelativePosition) | **64–74 ms** for a 466 MB file on a USB SSD. My walker read every top-level header; going straight through SeekHead needs 3–5 `pread`s [V/I] | No Cues, or unknown-size clusters (live captures, some rips): full block-header scan took **3.7–6.4 s per 466 MB** on USB SSD with one `pread` per block [V]. Use large sequential reads instead (about 1 s/GB at 500 MB/s [I]), or play through FFmpeg until the background index is ready |
| **MP4 / MOV** | `moov`: `stss` (sync samples), `stts` / `ctts`, `stsz`, `stco` / `co64`, `elst` | One read of `moov` (KB to MB). This is the complete sample table, which is better than Cues | Fragmented MP4 sources: walk `moof`s, or use `sidx` / `mfra` |
| **TS / M2TS / AVI** | No reliable index | Full scan | Use the FFmpeg remux path. Low priority |

> Lesson from the test corpus: `test-videos.co.uk`'s "…mkv" download was actually an MP4 (`ftypisom`) [V]. **Sniff magic bytes. Never trust extensions.**

Store per asset: the video keyframe list (pts, cluster offset, relative position), codec configuration records, colour and HDR metadata, DV configuration, the per-track language and flags, and the **reorder depth** (from SPS `max_num_reorder_frames` / `sps_max_num_reorder_pics`) for timestamp shifting ([§4.4](#44-what-the-fmp4-writer-must-handle)). For a 2 h movie this is about 1–3 k keyframes, i.e. tens of KB.

### 4.3 Segment planning

- Cut at video keyframes at or after `target` (default 6 s). Segment duration then equals the source GOP structure:
  - x264 with `-g 250` and scene-cut gave 6.0–16.2 s segments;
  - x265 with `keyint=240` gave 9.4–10.0 s [V].
- AVPlayer played both. Apple's "≤ target + 0.5 s" is an authoring rule [V].
- `EXT-X-TARGETDURATION` must be ≥ every rounded EXTINF (RFC 8216).
- **Very long GOPs** (some encodes use 10–20 s keyint) make seek granularity coarse, but players can still seek precisely inside a segment by decoding forward. AVPlayer exact seeks took 62–67 ms [V].
- **Open-GOP HEVC** (x265 enables open GOP by default) cuts at CRA pictures. When decoding starts at a CRA, its leading RASL pictures are undecodable and get dropped. In the probe, starting a decode at segment 37 alone gave 236 frames from 240 packets [V]. Continuous playback is unaffected. Behaviour after a seek in Chrome's MSE HEVC path is **unverified**; this is open question 1.
- **Demuxed audio.** Roku requires it, and it suits track switching. Audio segments use the same boundaries; an AAC frame is assigned to segment k if its PTS lies in [T_k, T_{k+1}). With Cues that means reading the neighbouring cluster's audio blocks, or keeping a ±1-cluster window.

### 4.4 What the fMP4 writer must handle

| Concern | Detail |
|---|---|
| **Boxes** | Init: `ftyp` (`iso6`, `cmfc`), `moov` / `mvhd`, per-track `trak` / `tkhd` / `mdia` / `mdhd` / `hdlr` / `minf` / `stbl` (empty tables), `stsd`, and `mvex` / `trex`. Media: `styp` (`msdh`, `msix`), `moof` / `mfhd`, per-track `traf` / `tfhd` (default-base-is-moof) / `tfdt` (v1, 64-bit) / `trun`, then `mdat`. The probe's writer is about 250 lines [V] |
| **Codec configuration pass-through** | Matroska `CodecPrivate` *is* the ISO record: `V_MPEG4/ISO/AVC` → `avcC`, `V_MPEGH/ISO/HEVC` → `hvcC`, `V_AV1` → `av1C`, `A_AAC` → the AudioSpecificConfig inside `esds`. The probe copied them verbatim, and both AVPlayer and `ffmpeg` accepted them [V]. AC-3 / E-AC-3 have no CodecPrivate: synthesize `dac3` / `dec3` by parsing the first sync frame [I]. Opus: `dOps` from `OpusHead` |
| **Dolby Vision** | MKV `BlockAdditionMapping` (`dvcC` / `dvvC` with BlockAddIDExtraData) → put the same record in a `dvcC` / `dvvC` box. Sample entry is `dvh1` for P5 and `hvc1` + `dvvC` for 8.1 (backward compatible). `mp4-atom` 0.16.2 has **no** `dvcC` / `dvvC` support, so write them yourself [V] (`mp4-atom-0.16.2/src/moov/trak/mdia/minf/stbl/stsd/hevc/hvc1.rs:5-16`) |
| **HDR metadata** | MKV `Colour` (Matrix, Transfer, Primaries, MasteringMetadata, MaxCLL / MaxFALL) → `colr` (nclx), `mdcv`, `clli`. Apple: HDR10 static metadata SHOULD be in `hvcC`, not SEI (rule 1.8) [V] |
| **Parameter sets** | `hvc1` / `avc1` expect parameter sets out of band. Some MKVs repeat VPS / SPS / PPS in-band. Either strip them per keyframe sample (cheap NAL walk; only keyframes are rewritten) or signal `hev1`. AVPlayer prefers `hvc1` [V spec, I behaviour] |
| **Timestamps (B-frames)** | MKV stores **PTS only**. Decode order is file order. Synthesize **DTS_i = (sorted PTS)_i**: monotonic by construction and continuous across segments, because each segment's PTS set is a contiguous range. Then CTS_i = PTS_i − DTS_i. **Pitfall [V]:** with B-frames this makes some CTS negative (PTS < DTS). `ffmpeg`'s MOV demuxer then shows a 2-frame (67 ms) presentation shift, though AVPlayer played it. **Recommendation:** add a constant global offset D = reorder depth × frame duration to all tracks so that CTS ≥ 0 everywhere. The probe's `shift` mode found D = 67 ms for BBB with 0 violations [V]. Alternatively use a single `elst` in init. Validate across players |
| **Timescales** | Video uses the MKV 1 ms grid, which gives rounding jitter (41/42 ms at 23.976). Better: derive the exact rational rate from `DefaultDuration` (41,708,333 ns → 24000/1001) and snap to timescale 24000. Audio timescale = sample rate, with 1024-sample AAC durations [V probe uses this] |
| **Lacing** | Audio blocks may use Xiph, EBML or fixed lacing. Split them and compute timestamps from the codec frame duration [V implemented]. Video is effectively never laced |
| **Header-stripping ContentCompression** | Rare legacy MKVs. Re-prepend the stripped bytes, which means copying the sample rather than referencing it [I] |
| **Sample flags** | Sync is `0x02000000`; non-sync is `0x01010000` (depends-on and non-sync) [V] |
| **Exact segment sizes** | With Cues only, sizes are known once the cluster range is parsed, before sending. `Content-Length` can always be set. This also makes Go's ReverseProxy copy without forced flushes; see [§10](#10-delivery-performance-what-actually-matters) |

### 4.5 Experiment: in-process MKV → CMAF remux probe

**Code:** `$FLUX_WORK/bench/playback-remux-probe/src/main.rs`

- Std-only EBML walker and a hand-written ISO BMFF writer.
- `mp4-atom` 0.16.2 is used only to re-parse the output as an independent structural check.
- Built with `CARGO_TARGET_DIR=$FLUX_WORK/target`.

**Hardware:** M1 Max, 64 GB. Media is on an external Samsung USB SSD (`pread` of about 470–500 MB/s); the files were recently written, so the page cache was partly warm.

| Input | Index (Info + Tracks + Cues) | Full header scan | Segments | Remux CPU / segment (parse + `moof` + copy) | Process CPU, all segments (incl. `pread` and file writes) | % of one core at 1× | Validation |
|---|---|---|---|---|---|---|---|
| Synthetic HEVC Main10 1080p23.976, x265 open-GOP, bframes = 4, AAC 2.0, 600 s, 465.8 MB, 6.2 Mb/s | 64 ms (60 cues) | 6.37 s (42,812 blocks) | 60 (9.43–10.01 s) | p50 0.56 ms, p99 3.23 ms | 466 ms | **0.078 %** | `ffmpeg` full decode, 0 errors. 14,386/14,386 video and 28,126/28,126 audio packets. `ffmpeg` decodes the HLS playlist. Init + segment 37 alone decodes (236 of 240 frames; RASL dropped). **AVPlayer: plays; exact seeks 62–64 ms; empty `errorLog`** |
| Synthetic H.264 High 1080p23.976, x264 veryfast, `-g 250`, bf = 3, AAC, 600 s, 465.6 MB | 74 ms (158 cues) | 3.74 s | 55 (6.05–16.18 s) | p50 0.49 ms, p99 2.37 ms | 534 ms | **0.089 %** | Same checks pass. AVPlayer exact seek 65–67 ms |
| Big Buck Bunny 1080p30 H.264 (real encode, rewrapped MP4 → MKV), 10 s, 29.5 MB, 24 Mb/s | 0.035 ms | 0.37 ms | 2 | 2.4 ms | 21 ms | 0.21 % | `ffmpeg` OK, 300/300 frames. AVPlayer reaches `readyToPlay` but waits forever ("minimize stalls"). **`ffmpeg`'s own `-f hls` output of the same file behaves identically**, so this is content-specific, not the muxer [V] |

Baselines on the same machine [V]:

| Operation | Result |
|---|---|
| `ffmpeg -c copy` full-file → fMP4 HLS, 466 MB HEVC file | 2.60 s wall, **0.53 s CPU** (0.21 user + 0.32 sys). CPU parity with the probe |
| `ffmpeg` restart at a seek point (`-ss`, copy, fMP4 HLS) → first segment listed | **55–58 ms** (5 positions, warm cache) |
| `ffmpeg` spawn + probe + seek + copy 1 frame | 30–40 ms |
| AVPlayer client, localhost HTTP, pre-built segments | `readyToPlay` 0.17–0.27 s; first frame advancing 0.29–0.39 s; keyframe-tolerant seek 88–100 ms; exact seek 62–67 ms. The first-ever cold HEVC run took 1.95 s to ready |

What this does **not** show:
- Cold-cache HDD behaviour. A seek on a spinning disk is roughly 10 ms plus about 8 MB / 150 MB/s, so 60–120 ms per cold segment [I].
- Browser MSE (Chrome or Firefox through hls.js), and TVs.
- 4K DV P7 / P8 material.
- Real scene-release quirks: in-band parameter sets, header stripping, VFR.

Those go in the P0 corpus test ([§13](#13-measurable-targets-and-how-to-measure-them)).

### 4.6 CPU cost per remux stream

- **Measured:** about 1.0–1.1 ms of process CPU per MB served. This includes `pread` and writing the segment to a file, which a server replaces with a socket write [V].
- **Estimate per stream** [I]:
  - 1080p remux at 6–10 Mb/s: about 0.1 % of a core;
  - UHD Blu-ray remux at 60–80 Mb/s (about 10 MB/s): about 1 % of one M1 Max core;
  - an ARM NAS core is about 3–5× slower [I], so about 3–5 %.
- **Through Truffle**, the dominant per-byte costs move to the Go sidecar: loopback copy, gVisor netstack and userspace WireGuard ChaCha20-Poly1305. **I did not measure this.** It is likely several times the remux cost [I], so measure it (open question 4).
- **Memory:** a segment buffer of one GOP (8–45 MB for 1080p–4K) if fully buffered. Streaming the payload ranges directly cuts this to tens of KB per stream [I].

### 4.7 Zero-copy shape

A segment body is `[header bytes (styp + moof + mdat header)] + [list of (file_offset, len)]`.
- In MKV, each frame's payload is contiguous in the file, separated from the next only by a 4–12-byte block header.
- So a server can stream with positioned reads into one reusable buffer, or with vectored writes.
- On an unproxied plain-HTTP LAN path it can even use `sendfile` per range.
- `shiguredo_mp4`'s `Fmp4SegmentMuxer` has exactly this API: it returns "moof + mdat header, payload not included" [V] (`shiguredo_mp4-2026.5.0/src/mux_fmp4_segment.rs:1-45`).

### 4.8 Crates (versions from the crates.io API, 2026-10-18)

| Crate | Version (updated) | Licence | Role | Assessment |
|---|---|---|---|---|
| `mp4-atom` | 0.16.2 (2026-10-06) | MIT / Apache-2.0 | Low-level ISO BMFF encode / decode (kixelated, MoQ ecosystem) | Good for parsing MP4 sources and for tests. Has `trun` / `tfdt` / `hvcC` / `av1C` / `esds` / `ac-3` / `ec-3` / `dOps` / `elst` / `ctts` / `stss`. **No `dvcC` / `dvvC`, `mdcv` or `clli`** [V] |
| `shiguredo_mp4` | 2026.5.0 (2026-10-04) | Apache-2.0 | MP4 / fMP4 mux and demux with a header-only fMP4 segment API | A strong candidate for the writer. Limited to one track per kind; no AC-3 / E-AC-3 / DV sample entries [V] |
| `muxide` | 0.2.5 (2026-05-20) | MIT / Apache-2.0 | Pure-Rust MP4 / fMP4 recorder muxer | Aimed at recording; young |
| `mp4` (alfg) | 0.14.0 (**2023-08-01**) | MIT | Classic reader / writer | Stale. Avoid |
| `mp4box` (alfg) | 0.14.0 (2026-09-16) | — | Parser and editor with fMP4 support | Alternative parser |
| `re_mp4` | 0.5.1 (2026-07-08) | MIT / Apache-2.0 | Rerun's MP4 parser | Parser only |
| `matroska-demuxer` | 0.8.1 (2026-08-07) | Zlib / MIT / Apache-2.0 | Demuxer | Copies frame data into a `Vec`, and its **Cues are private** (used only for `seek`). Not suitable for an offset index [V] (`src/lib.rs:1451, 1685`). Write a thin EBML indexer: about 300 lines in the probe |
| `webm-iterable` / `ebml-iterable` | 0.7.1 (2026-08-28) | MIT | EBML iteration | Usable for tag walking |
| `hang` | 0.21.16 (2026-10-17) | MIT / Apache-2.0 | MoQ media layer (catalog, CMAF container, timeline) | A design reference for segment timelines. A future option for MoQ delivery |
| `sheathe` | 0.6.1 (2026-08-21; 171 downloads) | — | "Pure-Rust HLS/DASH/CMAF packager" with Matroska input | A file-writing VOD packager and very young. Worth reading as a reference only [S] |
| `dolby_vision` (dovi_tool) | 3.4.0 (2026-08-12) | MIT | RPU parse / convert / write | `parse_unspec62_nalu` → `convert_with_mode(To81)` → `write_hevc_unspec62_nalu` [V] (`src/rpu/dovi_rpu.rs:89, 231, 354`) |
| `m3u8-rs` / `hls_m3u8` | 6.0.1 / 0.7.0 | MIT | Playlist types | Playlists are trivial; optional |

**Recommendation:**
- Own the MKV indexer, the segmenter and the fMP4 writer (about 1.5–3 k LOC including DV, HDR boxes, AC-3 / E-AC-3 `dac3` / `dec3` and Opus).
- Use `mp4-atom` for MP4 *source* parsing and round-trip tests.
- Keep `shiguredo_mp4` as the alternative writer if we prefer to adopt rather than build.

---

## 5. Hybrid: copy video, transcode audio

Why audio is special: every common audio codec is "all sync samples". **An audio segment can be transcoded independently and statelessly**:
1. decode from slightly before segment start (encoder or decoder priming);
2. encode;
3. trim to [T_k, T_{k+1});
4. emit an fMP4 audio fragment.

That keeps the remux path's stateless seek even when audio must change [I; standard technique]. The two catches:
- **AAC priming** (1024–2112 samples) means overlap and trim are needed, with `elst` or sample trimming. The seams need to be validated.
- **Opus** has pre-skip, carried in `dOps` / `OpusHead`.

| Need | Option | Notes |
|---|---|---|
| Decode TrueHD, DTS / DTS-HD MA, E-AC-3, AC-3 | **libavcodec** (`mlp` / `truehd`, `dca` with XLL, `eac3`, `ac3`) | `symphonia` 0.6.1 decodes AAC, ALAC, FLAC, MP1/2/3, PCM and Vorbis, **but not AC-3 / DTS / TrueHD** [I from Symphonia's documented codec set]. So libav is required for the hard cases |
| Encode AAC | FFmpeg native `aac` (LGPL); `aac_at` (AudioToolbox, macOS); `libfdk_aac` (non-free licence) | `aac_at` and `aac` are both present in the Homebrew FFmpeg 9.0.2 here [V] |
| Encode E-AC-3 (for 5.1 / 7.1 to Apple TV, Edge, TVs) | FFmpeg `eac3` encoder | **No Atmos (JOC)**. TrueHD Atmos → E-AC-3 loses the objects [S] (Infuse forum confirms Atmos is lost in LPCM too) |
| Encode Opus | `libopus` (BSD), or the `opus` crate 0.4.0 | Good for Chrome, Firefox, Android and LG; Safari only stereo-in-MP4 [S] |
| Process model | Phase 1: `ffmpeg` subprocess, audio only (`-c:v copy -c:a aac`). Phase 2: in-process libav via `ffmpeg-next` 9.0.0 or `rsmpeg` 0.18 (FFmpeg 8.0) | Measured `ffmpeg` "video copy + AAC encode" first 6 s segment after a seek: **150–790 ms** [V]. Most of it is the HLS muxer waiting for the next keyframe cut and process start-up. A per-segment in-process audio transcoder needs to decode and encode only about 6 s of audio, roughly 10–30 ms of CPU [I] |

Policy:
- Prefer **copying** E-AC-3 / AC-3 where the client accepts it (Safari, Apple TV, Edge, TVs).
- Transcode TrueHD and DTS to E-AC-3 5.1 at 640 kb/s for multichannel clients, plus a **stereo AAC** rendition for everything (Apple rule 2.3).
- Cache per-segment audio by recipe.

---

## 6. Transcoding when unavoidable

### 6.1 FFmpeg in 2026

FFmpeg versions [V] ([ffmpeg.org news](https://ffmpeg.org/index.html), [releases dir](https://ffmpeg.org/releases/), [Phoronix 9.0](https://www.phoronix.com/news/FFmpeg-9.0-Released)):
- **7.1** (2024-09-30): Vulkan H.264 and HEVC encode at "feature-parity with their VAAPI counterparts".
- **8.0 "Huffman"** (2025-08-22): Vulkan AV1 encode, Vulkan VP9 decode, VAAPI VVC decode.
- **8.1 "Hoare"** (2026-03-16): D3D12 H.264 / AV1 encode, Rockchip H.264 / HEVC encode, Vulkan ProRes.
- **9.0 "Lei"** (2026-08-04; 9.0.2 is current): more Vulkan, AMF and VT ProRes RAW decode. This machine's Homebrew build is 9.0.2 with `h264_videotoolbox`, `hevc_videotoolbox`, `aac_at`, `scale_vt` and `libplacebo`-less `tonemap` [V].
- Jellyfin 12 ships FFmpeg 8.1 [V].

| Platform | Decode | Encode | Keep frames on GPU | HDR→SDR tone mapping |
|---|---|---|---|---|
| macOS (Apple Silicon) | VT: H.264, HEVC (incl. 10-bit), AV1 on M3+ | `h264_videotoolbox`, `hevc_videotoolbox` (no AV1 encode) | `-hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld` + `scale_vt` | Upstream `scale_vt=color_matrix=bt709:color_primaries=bt709:color_transfer=bt709` (VTPixelTransferSession) [V] ([docs](https://ffmpeg.org/ffmpeg-filters.html#scale_005fvt)). Jellyfin's FFmpeg adds VT-native and Metal tone mapping; Metal handles DV P5 [V] ([Jellyfin Apple HWA](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/apple)). Their doc: an M1 does 3× 4K DV HEVC10 → SDR transcodes |
| Linux Intel | VAAPI / QSV | VAAPI / QSV (H.264, HEVC, AV1 on Arc and newer) | `hwupload` / `scale_vaapi` / `vpp_qsv` | `tonemap_vaapi`, `tonemap_opencl`, `libplacebo` (Vulkan) |
| Linux / Windows NVIDIA | NVDEC | NVENC (H.264, HEVC, AV1 on Ada) | CUDA frames, `scale_cuda` | `libplacebo` via Vulkan interop (`hwupload=derive_device=vulkan,libplacebo=inherit_device=1`) [V] |
| AMD | VAAPI / AMF / Vulkan | AMF, VAAPI, Vulkan | — | `libplacebo`, `tonemap_opencl` |
| Any Vulkan 1.3 | Vulkan video | `h264_vulkan`, `hevc_vulkan` (7.1), `av1_vulkan` (8.0) | Vulkan frames | `libplacebo`, which **applies Dolby Vision RPU reshaping** (`apply_dolbyvision`, default on) [V] |

### 6.2 Measured transcode TTFF on M1 Max

All runs: FFmpeg 9.0.2, 1080p HEVC Main10 source with 10 s GOPs, output H.264 8 Mb/s fMP4 HLS [V].

| Scenario | First segment ready after (re)start |
|---|---|
| VT decode+encode, `-ss` accurate (default), 6 s segments | 1.7–2.7 s (cold first run 2.7 s) |
| libx264 veryfast, same | 2.7–5.5 s |
| VT, accurate seek, hls_time 2 / 1 | 1.09–1.47 s / 0.93–1.47 s |
| VT, **`-noaccurate_seek` at a known keyframe**, 6 s of output (`-f null`) | **0.72–0.92 s** (against 1.56–1.80 s with accurate seek) |
| VT decode + encode of 1 frame at a keyframe | 0.38–0.40 s, i.e. about **0.25–0.35 s of VT session set-up** |
| Software decode of 1 frame: accurate vs keyframe-aligned seek | 0.63–0.73 s vs 0.14–0.17 s |

Takeaways [I from the measurements]:

1. **Keyframe-aligned restarts roughly halve transcode seek latency.** Plan transcode segment boundaries on *source* keyframes (Kyoo does this: "create a new segment only on one of those timestamps", [zoriya.dev](https://zoriya.dev/blogs/transcoder)). Then any restart starts exactly at a keyframe with `-noaccurate_seek`, and never decodes and discards.
2. **A short first segment.** After a start or seek, emit a 1–2 s segment first (for example, split the first source GOP with forced IDRs in the *output*), then 6 s segments.
3. **Session reuse saves another ~0.25–0.4 s per seek** on macOS. That requires in-process libav, which can flush and re-seek while keeping the `AVCodecContext`, or a long-lived worker protocol. A one-shot CLI cannot do this.
4. **Progressive delivery:** serve the in-progress segment as CMAF chunks (`moof` + `mdat` every ~0.5–1 s), using chunked transfer and hls.js `progressive` loading. The first chunk arrives about 200–300 ms after the encoder starts.
5. **CUDA caveat:** `-force_key_frames` creates I-frames but not IDRs; you need `-force_idr 1` [S] ([zoriya.dev](https://zoriya.dev/blogs/transcoder)).

### 6.3 Segment-on-demand algorithm (transcode)

```text
plan:      boundaries B_0..B_n = source keyframes chosen ≈6 s apart (first segment ≈1–2 s)
request n: if cache[recipe, n] → serve (immutable)
           elif worker W for (session, recipe) is producing and n ∈ [W.pos, W.pos + LOOKAHEAD] → wait / stream chunks
           else → cancel W (if no other session shares it), start W' at B_n
                  (-noaccurate_seek -ss B_n, output IDRs forced exactly at B_{n+1..}, fragments to stdout pipe)
worker:    flux reads fMP4 fragments from the pipe, splits at planned boundaries, writes cache[recipe, k],
           wakes waiters; throttle when W.pos − client.pos > 60–120 s; kill on 30 s idle
cache key: H(source identity, track selection, video recipe {codec, w×h, bitrate, tonemap},
             audio recipe, burn-in subtitle id, pipeline version)
```

- **Process model:** start with a **supervised `ffmpeg` subprocess** for crash isolation, a simple licence boundary and easy upgrades, with `tokio::process` plus cancellation tokens.
  - Spawn overhead is only 30–40 ms here [V], so the subprocess is not the bottleneck. The waste comes from the restart's accurate seek and session set-up.
  - Move to in-process libav (`ffmpeg-next` 9.0.0 / `ffmpeg-sys-next` 9.0.1, `rsmpeg` 0.18.0 for FFmpeg 8.0, `ez-ffmpeg` 0.19.3) for the warm-session seek path once the planner is stable.
- **Licensing:** an LGPL-only FFmpeg build (no x264 / x265) is enough for hardware encoders. GPL is needed for x264 / x265 software fallbacks [I].

---

## 7. Subtitles

| Source format | Web (hls.js / native) | Apple (AVPlayer) | Android (Media3) | TVs (webOS, Tizen, Roku) | flux strategy |
|---|---|---|---|---|---|
| SRT / WebVTT / mov_text | WebVTT ✅ | WebVTT ✅ (only WebVTT or IMSC1) [V] | ✅ | WebVTT (webOS lists only `.vtt` [V]; Roku: WebVTT, SRT, TTML [V]) | Extract at scan time → WebVTT sidecar. One-segment VTT playlist for VOD (rule 5.7 allows a longer target duration [V]) with `X-TIMESTAMP-MAP` (rule 5.3) |
| ASS / SSA (styled, anime) | **Client render: JASSUB 2.5.22** (libass → WASM + WebGL, npm 2026-10-19 [V]) | No libass in AVPlayer. Convert to WebVTT (lossy), or burn in if the user insists | Media3 renders SSA basically; libass extensions exist | WebVTT (lossy) or burn-in | Default: client render where possible, otherwise lossy WebVTT. Burn-in only on explicit user request |
| PGS / VobSub (image) | **Client render: libbitsub 1.13.0** (Rust → WASM, WebGPU / WebGL2, PGS + VobSub; Jellyfin 12 uses it) [V npm; V Jellyfin notes] | Not supported, so burn-in is the default | Media3 supports PGS | Usually not supported | **AI opportunity:** OCR image subtitles to text WebVTT at scan time (Apple Vision / Tesseract / a small VLM). This removes burn-in transcodes for AVPlayer and TVs and keeps HDR remux intact [I]. Keep burn-in (overlay on GPU) as the fallback |
| CEA-608 / 708 | hls.js ✅ | ✅ | ✅ | varies | Pass through in-band |

Burn-in is "the most intensive method of transcoding" (Jellyfin docs [V]). The planner must treat subtitle choice as a first-class input. A bad default subtitle (forced PGS) is a common cause of an unnecessary 4K transcode.

---

## 8. HDR and Dolby Vision

| Format | What it is | Clients (2026) | flux fallback chain |
|---|---|---|---|
| HDR10 (PQ + static metadata) | Base HEVC / AV1 | Nearly all 4K clients | Copy. If the display or client is SDR: client tone maps (Apple, Chrome on macOS) → server tone map (`scale_vt` / `libplacebo` / `tonemap_opencl`) |
| HDR10+ | SEI dynamic metadata | Samsung, Apple TV, Google TV; Chrome ignores it and plays HDR10 [S] | Copy. Clients without HDR10+ play HDR10 |
| HLG | Backward-compatible transfer | Broad | Copy |
| **DV P5** (IPTPQc2, single layer, no fallback) | Streaming services | Apple (all), LG, Roku (`dvhe.05`), Android TV with DV; Chrome only encrypted on Windows [S] | Copy with `dvh1` + `dvcC` (Jellyfin 12 added the spec-compliant `dvh1` variant [V]). Non-DV client: **must transcode with DV reshaping** (libplacebo `apply_dolbyvision`, Jellyfin Metal), otherwise colours are green or purple [S] |
| **DV P7** (dual layer BL + EL + RPU, UHD BD) | EL in NAL type 63 and RPU in NAL type 62 inside the base-layer access units [V] ([FFmpeg `dovi_split`](https://ffmpeg.org/ffmpeg-bitstream-filters.html)) | Apple: not supported, plays as HDR10 [S]. LG webOS 25: user reports say P7 behaviour changed | **In-process P7 → 8.1 during remux:** drop NAL 63, convert NAL 62 with `dolby_vision` `convert_with_mode(To81)`, write `hvc1` + `dvvC` (profile 8, compat id 1). This is per-sample NAL rewriting (a memcpy-class cost) [I; the crate API is V]. FFmpeg ≥ 7 has `dovi_rpu` (strip / compression) and git master has `dovi_split` [V] |
| **DV P8.1** (HDR10-compatible BL + RPU) | Most web-DL and converted rips | Apple TV 4K (4th gen) lists 8.1 [V]. Chrome 122+ plays it as HEVC / HDR10 [S] | Copy. Non-DV clients play the HDR10 base (`hvc1` sample entry with `dvvC`) |
| DV P8.4 (HLG base) | iPhone recordings | Apple | Copy |
| DV P10 (AV1) | AV1 + RPU | Apple (A19 / M3+?), Android; Media3 1.10 extracts P10 in MP4 [S] | Copy where AV1 + DV is supported |

The planner needs, per asset: the DV profile, BL compatibility id, EL presence, RPU presence, mastering metadata and MaxCLL. Per client: DV profiles, HDR formats, and whether the display is actually HDR. That last one comes from runtime queries such as `matchMedia('(dynamic-range: high)')` and AVPlayer `eligibleForHDRPlayback`.

---

## 9. Trickplay and scrubbing thumbnails

| Mechanism | Who uses it | Notes |
|---|---|---|
| Sprite sheets (JPEG tiles) + manifest | Jellyfin. Defaults: **every 10 s, width 320, 10×10 tiles, keyframe-only extraction off, hardware off, 1 thread, BelowNormal priority** [V] (`MediaBrowser.Model/Configuration/TrickplayOptions.cs:14-70`) | Exposed as an image playlist for Roku-style clients |
| BIF | Roku, Plex | Simple container of JPEGs at fixed intervals |
| HLS **I-frame playlists** (`EXT-X-I-FRAMES-ONLY`, `EXT-X-I-FRAME-STREAM-INF`) | Apple: **MUST** for scrubbing; 1 fps dense I-frame renditions SHOULD; fMP4 I-frame segments MUST include the `moof` [V]; visionOS wants a 160 px-high thumbnail list [V] | hls.js supports I-frame trick play including `mjpg` renditions [V] (hls.js README) |

flux approach [I]:
- **I-frame playlist almost for free.** The index already knows every keyframe's offset and size. Serve `/iframe/{k}.m4s` as a synthesized `moof` + `mdat` holding a single sync sample. Bandwidth equals the keyframe size, and nothing has to be decoded on the server.
- **Thumbnails cheaply.** Decode **only keyframes** (hardware decoders, `-skip_frame nokey`, or in-process libav with packets fed from our index), scale to 320 px (and 160 px high for visionOS), and pack sprites plus a WebVTT thumbnail track.
  - For a 2 h film with 2–10 s GOPs that is about 700–3,600 decodes.
  - At 5–20 ms per hardware keyframe decode [I], that is roughly 5–70 s per film, in the background at low priority. This is precisely Jellyfin's "keyframe only" option, which is off by default because "not compatible with all decoders / files".
  - Fall back to full decode only for files where keyframe decode fails.
- Generate trickplay **on first play or idle time**, not during the initial scan, to keep scans fast.

---

## 10. Delivery performance: what actually matters

**The real byte path through Truffle:**

```text
client ──(WireGuard/UDP)──► tsnet (gVisor netstack, userspace) ──► Go http.Server (TLS via tsnet cert or plain HTTP)
       ──► httputil.ReverseProxy (loopback) ──► flux (Rust, 127.0.0.1)
```

What the code shows [V]:
- **tsnet `ListenTLS`** wraps the listener with `tls.Config{GetCertificate: s.getCert}` and **no `NextProtos`** (`tailscale.com@v1.100.0/tsnet/tsnet.go:1409-1431`; Truffle pins v1.104.1). The proxy therefore speaks **HTTP/1.1 only** [I, high confidence].
  - Truffle can also serve plain HTTP (`tls:false`) because "WireGuard already encrypts the path" (`truffle/packages/sidecar-slim/main.go:2244-2250`).
- **The proxy server** sets `ReadHeaderTimeout` to 10 s and `IdleTimeout` to 120 s, and deliberately leaves **no `ReadTimeout`** so long-lived streaming works (`main.go:2555-2559`).
  - The ReverseProxy keeps **64 idle upstream connections per host** (`main.go:3239-3283`).
  - Its default `FlushInterval` is 0. Go flushes immediately only when the upstream `Content-Length` is −1 (`$GOROOT/src/net/http/httputil/reverseproxy.go:609-624`, Go 1.25.6).
  - So flux should **set `Content-Length` on finished segments**, and use chunked responses (flushed immediately) for in-progress CMAF chunks.
- **Truffle's own static file route** uses `http.ServeContent`, which handles Range (`main.go:3164-3206`).
- **Truffle measurements** [V] (`truffle/benchmark/README.md`):
  - same-host TCP throughput is bimodal at **330–365 or 480–550 Mb/s** (line 616);
  - HTTP proxy keep-alive reaches **3,705 req/s** (line 604);
  - a new HTTP connection costs **p50 0.79 ms, plus a ~1 s stall on 0.8–0.9 % of new connections** from TIME_WAIT 4-tuple reuse (lines 593, 609–614);
  - the first two-machine LAN run showed **RTT p50 ≈ 6.7 ms with spikes to ~100 ms** (line 775).

  **Implications:**
  1. Use keep-alive. hls.js and AVPlayer do.
  2. 500 Mb/s is enough for several 4K remuxes.
  3. A Wi-Fi RTT spike adds directly to seek latency.

| Technique | Matters at home scale? | Recommendation |
|---|---|---|
| `sendfile` / zero-copy | No (≤ 1 Gb/s, ≤ 10 streams; Truffle's Go proxy copies anyway) | Positioned `pread` into a reused buffer on Tokio's blocking pool. Optional `sendfile` for direct LAN, plain-HTTP file serving |
| io_uring (`tokio-uring` 0.5.0 — 2024-05, stale; `monoio` 0.2.4 — 2024-08; `compio` 0.19.2 — 2026-09) | No. Linux only, and Tokio's own io-uring file support still needs `tokio_unstable` [S] ([tokio #7621](https://hub.harborframework.com/tasks/abundant/tokio-rs__tokio-7621)) | Stay on Tokio 1.53 + hyper 1.12 + axum 0.8.9 |
| kTLS (`ktls` 6.0.2) | No. TLS ends in the Go sidecar | — |
| HTTP/2 | Minor: multiplexing, header compression | Not available through tsnet `ListenTLS` today. Fine |
| HTTP/3 / QUIC | No for VOD | Revisit only for MoQ (`hang` / `moq-lite` → `moq-net`) or a custom native client |
| **Read-ahead** | **Yes** on HDD and NAS | After serving segment n, prefetch segment n+1's byte range (`posix_fadvise(WILLNEED)` on Linux, `F_RDADVISE` on macOS). On "detail page open", prefetch segment 0 or the resume segment |
| **Caching headers** | Yes | Segments and init: `Cache-Control: public, max-age=31536000, immutable` with an ETag tied to the recipe. Playlists: short TTL |
| **Segment size vs remote bandwidth** | **Yes** | A 4K remux segment of 6–10 s is 45–100 MB. Over a 50 Mb/s remote link the first segment takes 7–16 s, so emit **CMAF chunks** (`moof` per ~1 s) for progressive hls.js, or have the planner choose a transcode or lower variant from Truffle's path info (direct vs DERP) and measured throughput [I] |

---

## 11. Low-latency start

Do **at scan time** (Mille-driven scanner):
- container sniff, the index (Cues or `moov`), and the codec, HDR and DV descriptors;
- the segment plan and init segments for the default track combination;
- text subtitle extraction to WebVTT;
- image-subtitle OCR (optional);
- the planner's per-asset "compat facts".

Then playback needs **zero probing**. Jellyfin also avoids a per-play `ffprobe` by caching media streams [I].

TTFF budget for remux on LAN (targets) [I, grounded in the measured numbers above]:

| Step | Budget | Evidence |
|---|---|---|
| `POST /play` → plan + URLs (a single round trip; no separate PlaybackInfo dance) | ≤ 15 ms server | DB lookup only |
| Multivariant + media playlist | ≤ 5 ms | Generated from the cached plan |
| Init segment | ≤ 5 ms | Cached bytes |
| First segment: read one GOP + mux | 15–35 ms SSD; 60–150 ms HDD cold | Measured 21–32 ms cold-ish on USB SSD for ~8 MB; mux 0.5–3 ms [V] |
| Network (8 MB): 1 GbE / tailnet same LAN / 50 Mb/s remote | 65 ms / 120–200 ms / 1.3 s (chunked: ~0.2 s to first chunk) | Truffle 330–550 Mb/s [V]; arithmetic [I] |
| Client start-up | 150–300 ms | AVPlayer measured ready 0.17–0.27 s, first frame advancing 0.29–0.39 s on localhost [V] |

Techniques:
1. **Predictive prefetch:** warm the page cache for segment 0 (or the resume segment) and its init when a detail page opens.
2. **Resume-aware start:** start the playlist request already at the resume segment rather than seeking after start.
3. **Prefer MSE (hls.js 1.7.3) over Chrome's native HLS** until it matures [V].
4. **Native HLS on iOS Safari and AVPlayer.**
5. LL-HLS is a live-streaming tool and unnecessary for VOD. Use **CMAF chunked segments** with hls.js `progressive` for the transcode and remote-high-bitrate cases.

---

## 12. Proposed architecture: playback planner + delivery engine

### 12.1 Components

```text
            ┌──────────────── scan time (Mille walker/watcher → probe workers) ────────────────┐
 files ───► │ sniff → MediaIndex (MKV Cues / MP4 moov) → StreamFacts (codec, HDR, DV, audio, subs) │
            │ → SegmentPlan (keyframes) → InitSegments → Subtitles(WebVTT/OCR) → Trickplay (idle)  │
            └──────────────────────────────────────────────────────────────────────────────────────┘
                                         │ SQLite (catalog) + blob cache on SSD
 client ─► POST /api/play {item, client_caps, prefs, net} ─► PlaybackPlanner ─► PlaybackPlan (+reasons)
                                                                   │
            ┌──────────────────────────── DeliveryEngine (axum/hyper, Tokio) ─────────────────────┐
            │ /file/{asset}            DirectFile: Range, sendfile when unproxied                  │
            │ /hls/{plan}/main.m3u8    playlists from SegmentPlan (exact EXTINF)                   │
            │ /hls/{plan}/v/init.mp4   cached; /v/{n}.m4s → Segmenter (pure fn) + BitstreamFilters │
            │ /hls/{plan}/a/{t}/{n}.m4s → copy | AudioSegmentTranscoder (stateless per segment)   │
            │ /hls/{plan}/iframe/{k}.m4s, /trick/{w}/{sheet}.jpg, /sub/{t}.vtt                    │
            │ TranscodeSupervisor: keyframe-aligned workers, recipe cache, cancellation, limits  │
            └──────────────────────────────────────────────────────────────────────────────────────┘
                                         │
                  Truffle HTTP proxy (tsnet, WireGuard) ─► browsers / AVPlayer / Media3 / TVs
```

### 12.2 Planner contract (sketch)

```rust
pub struct ClientCaps {             // declared by the client, refined by probes + learned quirks
    pub containers: BitSet<Container>,        // Mp4Progressive, MkvProgressive, HlsFmp4, HlsTs, Dash
    pub video: Vec<VideoCap>,                 // codec, max profile/level, bit depth, max WxH@fps, hdr: {Hdr10, Hdr10Plus, Hlg, Dv{profiles}}
    pub audio: Vec<AudioCap>,                 // codec, max channels, passthrough flags
    pub subs: BitSet<SubFmt>,                 // WebVtt, Ass(client-render), Pgs(client-render), Ttml
    pub display_hdr: bool, pub max_bitrate_bps: Option<u64>, pub needs_demuxed_audio: bool,
}
pub enum VideoOp { Copy { entry: SampleEntry /*avc1,hvc1,dvh1,av01*/, fix: Vec<BsFix> /*Dv7To81, StripInbandPs*/ },
                   Transcode { codec: Codec, w: u32, h: u32, kbps: u32, tonemap: Option<ToneMap>, burn_sub: Option<TrackId> } }
pub enum AudioOp { Copy, Transcode { codec: Codec, ch: u8, kbps: u32 } }
pub enum Delivery { DirectFile, HlsCmaf { muxed_audio: bool, chunked: bool }, ProgressiveMp4Virtual }
pub struct PlaybackPlan {
    pub delivery: Delivery, pub video: VideoOp, pub audio: Vec<(TrackId, AudioOp)>,
    pub subs: Vec<(TrackId, SubOp /* Sidecar(WebVtt) | ClientRender | BurnIn */)>,
    pub reasons: Vec<Reason>,          // e.g. ContainerUnsupported(Mkv), AudioUnsupported(TrueHd), DvProfileUnsupported(7)
    pub est_server_cost: Cost,         // cpu_ms_per_min, gpu_session: bool
}
```

Decision order: first check which subtitle the user picked, because PGS can force burn-in. Then:
1. **DirectFile** if the container and every selected track are supported, and the bitrate fits the path.
2. **HlsCmaf + Copy** (in-process remux).
3. **HlsCmaf + Copy + BsFix** (DV 7→8.1, parameter-set strip).
4. **HlsCmaf + Copy + AudioOp::Transcode** (stateless per segment).
5. **Transcode**: hardware first, keyframe-aligned, tone mapping chosen by HDR or DV type.

Every step records a `Reason`, so the UI and diagnostics can explain choices such as "Converting TrueHD 7.1 → E-AC-3 5.1 because Apple TV cannot decode TrueHD".

### 12.3 State and failure model

- **Remux and audio-segment paths are stateless.** They need no per-session process; a session is just `{plan_id, last_segment, client_pos}` for analytics and prefetch.
- **Only the transcode path holds resources.** Each TranscodeSupervisor worker has:
  - an owner (`plan_id`) and a cancellation token;
  - a GPU-session budget per device;
  - a heartbeat;
  - its outputs written atomically (temp file + rename) into the recipe cache. Late output from a cancelled worker is harmless, because content-addressed segments are identical.
- **Failure ladder.** When the client reports `MEDIA_ERR_DECODE` or an AVPlayer error event, the planner retries one rung lower (for example Copy → Transcode). It also records a **quirk** for `(client model, OS version, stream facts)` so the next play goes straight to the working rung.

---

## 13. Measurable targets and how to measure them

| Metric | Target (p50 / p95) | Conditions | How to measure |
|---|---|---|---|
| TTFF, direct play or remux | **≤ 0.5 s / ≤ 1.0 s** | LAN or tailnet direct path; SSD; warm catalog | Client probes. AVPlayer: tap → first `AVPlayerItemVideoOutput.hasNewPixelBuffer` (the probe used `currentTime` advancing). Web: Playwright click → first `requestVideoFrameCallback`. Media3: `AnalyticsListener.onRenderedFirstFrame` |
| TTFF, HDD cold | ≤ 0.9 s / ≤ 1.5 s | 5400 rpm HDD, cache dropped | `echo 3 > drop_caches` (Linux) or `purge` (macOS) before each run |
| Seek latency, remux | **≤ 250 ms / ≤ 500 ms** LAN; ≤ 400 / 800 ms tailnet | Random seeks across 2 h file, exact and keyframe-tolerant | Web: `seeking` → first rVFC after `seeked`. AVPlayer: `seek(to:completionHandler:)` + first new pixel buffer. The probe measured 62–67 ms on localhost |
| TTFF / seek, transcode (hardware) | **≤ 1.2 s / ≤ 2.0 s** | 1080p HEVC10 → H.264, M1 Max / Intel QSV | Same probes. Server spans: `worker_spawn`, `first_fragment`, `segment_complete` |
| Server CPU, remux stream | **≤ 0.2 % of one core per 10 Mb/s**, excluding Truffle sidecar | 1080p and 4K corpus | `proc_pid_rusage` / cgroup v2 `cpu.stat` over 10 min of playback ÷ content seconds. Report the Truffle sidecar separately |
| Server CPU, audio transcode | ≤ 2 % of a core per stream [I] | TrueHD 7.1 → E-AC-3 5.1 + AAC 2.0 | Same |
| Memory | ≤ 64 MB RSS per remux stream (streamed ranges) | 4K | `ps` / jemalloc stats |
| Reliability | ≥ 99.5 % sessions without failure or stall > 1 s; 0 A/V desync > 45 ms | 50-file corpus × 6 client families | Automated nightly matrix. Desync via the probe's PTS logging against an audio click track |
| Index cost | ≤ 100 ms per MKV with Cues; ≤ 1.5 s/GB without | USB SSD / HDD | Scanner tracing spans (the probe measured 64–74 ms) |

**Benchmark protocol.** Run on the same box, corpus and clients against **Jellyfin 12.2** (web via hls.js, Swiftfin/AVPlayer, Android TV) and **Plex PMS** (latest). Report p50/p95/p99 over at least 30 runs per cell, alternating products. Record all of: cache state, path (LAN / tailnet direct / DERP), client and OS versions, and bytes on the wire.

**Corpus (P0)** — real files from the maintainer's libraries plus synthetic edge cases:
- HEVC open GOP; long GOP (20 s);
- VFR;
- H.264 with in-band SPS, and header-stripped MKV;
- DV P5, P7 FEL and MEL, P8.1;
- HDR10+;
- TrueHD Atmos, DTS-HD MA, E-AC-3 JOC, Opus, FLAC;
- PGS, VobSub, ASS with fonts;
- an MKV without Cues;
- AVI and M2TS legacy files.

---

## 14. Recommendations for flux

| # | Recommendation | Priority |
|---|---|---|
| 1 | Make the **index-driven, stateless in-process remux** (MKV Cues / MP4 `moov` → CMAF fMP4) the default non-direct path. Every segment is a pure function, so a seek is a GET | P0 |
| 2 | Build the **explainable PlaybackPlanner** with typed `ClientCaps`, `Reason` codes and a learned **quirks table**. Treat subtitle choice and display HDR as first-class inputs | P0 |
| 3 | **Timestamp policy:** synthesize DTS from sorted PTS, apply a global offset D = reorder depth × frame duration so that CTS ≥ 0 (or one `elst`), and use an exact rational video timescale. Gate it with a cross-player conformance test (AVPlayer, hls.js on Chrome / Firefox / Edge, Media3, webOS, Tizen, Roku) | P0 |
| 4 | **Demuxed audio renditions** from day one (Roku requirement, track switching, stateless audio transcode). Always offer stereo AAC (Apple rule 2.3) | P0 |
| 5 | Do all playback preparation **at scan time**: index, plan, init segments, WebVTT extraction, DV / HDR facts. No `ffprobe` on the play path | P0 |
| 6 | Build the **measurement harness** (AVPlayer probe, Playwright + hls.js, Media3) and the corpus *before* optimizing. Publish p50/p95 against Jellyfin 12.2 | P0 |
| 7 | **Transcode segments aligned to source keyframes** with `-noaccurate_seek` restarts, a short first segment, recipe-addressed cache, and supervised `ffmpeg` workers with cancellation and GPU budgets | P0 |
| 8 | **Per-segment stateless audio transcode** (libav decode of TrueHD / DTS / E-AC-3 → AAC / E-AC-3 / Opus) with priming overlap and trim | P1 |
| 9 | **DV P7 → 8.1 on the fly** during remux (drop NAL 63, convert RPU with `dolby_vision`), plus `dvcC` / `dvvC` / `mdcv` / `clli` writing | P1 |
| 10 | **Serve in-progress transcode segments as CMAF chunks** (chunked HTTP; hls.js `progressive`). Also chunk remux segments when the path is slow (remote / DERP) | P1 |
| 11 | **I-frame playlists from the index** (synthesized single-sample fragments) and **keyframe-only trickplay sprites** generated at idle time | P1 |
| 12 | **Image-subtitle OCR → WebVTT** (an AI feature) to avoid burn-in transcodes on AVPlayer and TVs. libbitsub / JASSUB for web client-side rendering | P1 |
| 13 | **In-process libav** (`ffmpeg-next` 9.x) for warm encoder sessions on seek (saves about 0.25–0.4 s per transcode seek on macOS) once the planner is stable | P2 |
| 14 | Consider a **"virtual progressive MP4"** endpoint (a byte-range-addressable MP4 synthesized from the MKV index) for clients that handle MP4 well but not HLS or MKV (some TVs, Samsung 4K via AVPlay) | P2 |
| 15 | Ignore io_uring, kTLS, HTTP/3 and `sendfile` for now. Invest in read-ahead, page-cache prefetch and immutable caching instead | P2 |

---

## 15. Risks and open questions

1. **Open-GOP HEVC in Chrome and Firefox MSE after a seek.** CRA starts with RASL leading pictures. AVPlayer and `ffmpeg` were fine [V]; MSE is untested. The fallback is to drop RASL samples at segment start on random access, which needs a seek-specific variant.
2. **Negative CTS vs offset vs `elst`:** which representation does every target player handle with correct A/V sync? `ffmpeg` showed a 67 ms shift with negative CTS [V].
3. **Real-world MKV pathologies:** in-band parameter sets with `hvc1`, header stripping, missing or inaccurate Cues, unknown-size clusters, VFR timestamp jitter.
4. **Truffle sidecar CPU per Mb/s** (gVisor + userspace WireGuard + Go proxy copy) for 4K streams on a NAS-class CPU. This may dominate total server CPU and is unmeasured.
5. **Chrome native HLS** stability (Chrome 142/143 bug history) [V], and whether hls.js should *always* force MSE.
6. **Samsung Tizen 4K** requires AVPlay instead of MSE [V]. That means a separate TV client path.
7. **Atmos:** no open-source E-AC-3 JOC encoder exists. TrueHD Atmos sources lose their objects on any non-pass-through client [S].
8. **AV1 on Apple:** only A19 (Apple TV 4K 4th gen), A17 Pro+ and M3+ [V/S]. Older Apple devices need a transcode for AV1 sources.

---

## Appendix A — Reproduce the experiment

```sh
# probe (Rust, std + mp4-atom 0.16.2 for validation)
cd $FLUX_WORK/bench/playback-remux-probe
CARGO_TARGET_DIR=$FLUX_WORK/target cargo build --release
P=$FLUX_WORK/target/release/playback-remux-probe
M=$FLUX_WORK/media/playback-research
$P $M/synth_hevc10_opengop_10min.mkv out/hevc 6000          # 'neg' CTS mode (default)
$P $M/bbb_1080_10s.mkv out/bbb_shift 2000 shift             # global offset D, CTS >= 0
ffmpeg -v error -i out/hevc/video.m3u8 -map 0 -f null -      # decode check
# AVPlayer check (Swift): avcheck/check http://127.0.0.1:18731/hevc/video.m3u8 255.5 exact
# test media generation: media/playback-research/gen.sh (x265 ultrafast open-GOP + AAC + SRT; x264 veryfast)
```

## Appendix B — Sources

**Primary or official**
- Jellyfin source, master `08c2ba6e`:
  - [DynamicHlsController.cs](https://github.com/jellyfin/jellyfin/blob/master/Jellyfin.Api/Controllers/DynamicHlsController.cs)
  - [DynamicHlsPlaylistGenerator.cs](https://github.com/jellyfin/jellyfin/blob/master/src/Jellyfin.MediaEncoding.Hls/Playlist/DynamicHlsPlaylistGenerator.cs)
  - [MatroskaKeyframeExtractor.cs](https://github.com/jellyfin/jellyfin/blob/master/src/Jellyfin.MediaEncoding.Keyframes/Matroska/MatroskaKeyframeExtractor.cs)
  - [StreamState.cs](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Controller/Streaming/StreamState.cs)
  - [EncodingOptions.cs](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Model/Configuration/EncodingOptions.cs)
  - [TrickplayOptions.cs](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Model/Configuration/TrickplayOptions.cs)
  - [EncodingHelper.cs](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Controller/MediaEncoding/EncodingHelper.cs)
- Jellyfin releases: [12.0](https://github.com/jellyfin/jellyfin/releases/tag/v12.0)
- Jellyfin docs: [transcoding](https://jellyfin.org/docs/general/post-install/transcoding/), [codec support](https://jellyfin.org/docs/general/clients/codec-support/), [Apple HWA](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/apple)
- Apple: [HLS Authoring Specification](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices), [Apple TV 4K specs](https://www.apple.com/apple-tv-4k/specs/)
- WebKit: [Safari 17.1 features (ManagedMediaSource)](https://webkit.org/blog/14735/webkit-features-in-safari-17-1/)
- Chromium: [audio/video](https://www.chromium.org/audio-video/)
- Firefox: [145 release notes](https://www.firefox.com/en-US/firefox/145.0/releasenotes/)
- TV platforms: [Samsung 2026 video specs](https://developer.samsung.com/smarttv/develop/specifications/media-specifications/2026-tv-video-specifications.html), [LG webOS 25 AV format](https://webostv.developer.lge.com/develop/specifications/video-audio-250), [Roku streaming specs](https://developer.roku.com/dev/docs/media)
- FFmpeg: [news](https://ffmpeg.org/index.html), [releases](https://ffmpeg.org/releases/), [filters](https://ffmpeg.org/ffmpeg-filters.html), [bitstream filters](https://ffmpeg.org/ffmpeg-bitstream-filters.html), [formats](https://ffmpeg.org/ffmpeg-formats.html)
- Players: [hls.js README](https://github.com/video-dev/hls.js), [Mux Chrome native-HLS incident](https://status.mux.com/incidents/g4wcv5cm02sg)
- crates.io API (crate versions); npm registry (`hls.js` 1.7.3, `jassub` 2.5.22, `libbitsub` 1.13.0, `shaka-player` 5.3.0)
- Crate sources read locally: `mp4-atom` 0.16.2, `shiguredo_mp4` 2026.5.0, `matroska-demuxer` 0.8.1, `hang` 0.21.16, `muxide` 0.2.5, `dolby_vision` 3.4.0 (under `$FLUX_WORK/cache/playback-research/crates/`)
- Local code:
  - Truffle `truffle/packages/sidecar-slim/main.go` and `truffle/benchmark/README.md` (v0.7.19, `d6f6664`)
  - tsnet `tsnet.go:1409-1431` (v1.100.0 module cache)
  - Go `net/http/httputil/reverseproxy.go:609-624` (Go 1.25.6)

**Secondary**
- [StaZhu Chromium HEVC guide](https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding)
- [caniuse HEVC](https://caniuse.com/hevc) / [AV1](https://caniuse.com/av1)
- [Phoronix: Firefox 137 HEVC](https://www.phoronix.com/news/Firefox-137-VA-API-HEVC)
- [Phoronix: FFmpeg 9.0](https://www.phoronix.com/news/FFmpeg-9.0-Released)
- [Bitmovin: Apple AV1](https://bitmovin.com/blog/apple-av1-support)
- [Kyoo transcoder blog](https://zoriya.dev/blogs/transcoder)
- [nginx-vod-module](https://github.com/kaltura/nginx-vod-module)
- [Plex support: Direct Play / Direct Stream](https://support.plex.tv/articles/200250387-streaming-media-direct-play-and-direct-stream/)
- [Plex forum: Plex Web uses DASH](https://forums.plex.tv/t/plex-behind-a-https-proxy-media-must-be-transcoded-in-order-to-use-the-dash-protocol/672918)
- [Infuse forum: DV P7 and TrueHD Atmos](https://community.firecore.com/t/dolby-vision-profile-7-8-support-ts-mkv-files/19713?page=80)
- [Media3 1.9](https://android-developers.googleblog.com/2025/12/media3-190-whats-new.html)
- [Media3 supported formats](https://developer.android.com/media/media3/exoplayer/supported-formats)
- [Google TV Streamer specs](https://store.google.com/product/google_tv_streamer_specs)
- [Tokio io-uring issue #7621 (mirror)](https://hub.harborframework.com/tasks/abundant/tokio-rs__tokio-7621)
- Jellyfin forum threads cited in [§1](#1-what-beating-plex-and-jellyfin-means-and-where-their-time-goes)
