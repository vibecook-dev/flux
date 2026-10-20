# 08 — Rust media ecosystem and next-generation transport

**Question.** What can flux build on, in Rust and around it, to push streaming performance as far as it will go? This covers media crates, prior art, QUIC/HTTP/3/MoQ, the web-server stack, clients and the database layer.

**Date:** 2026-10-18 · **Scope:** research plus one local micro-benchmark · **Status:** findings for decision-making, not a spec

**Evidence legend.** **[V]** means I verified it from a primary source (crates.io/GitHub API on 2026-10-18, official docs, spec pages, or local source code with file:line). **[M]** means a measurement I ran on this machine (method in Appendix A). **[S]** means a secondary source (news coverage, a blog, a search-result summary), so treat it with care. **[I]** is my own inference. Confidence is stated where it matters.

> Note on method: crate versions, licenses and dates come from the crates.io API and GitHub API, queried 2026-10-18 (the raw rows are reproduced in the tables). Late in the session the shared web-search budget ran out. A few claims (marked [S] or listed under Open questions) could not be re-verified a second time.

---

## 0. TL;DR

1. **Performance comes from avoiding work, not from the transport.** The biggest levers for time-to-first-frame (TTFF), seek latency and server CPU are: (a) never scanning or decoding what you can read from container indexes, (b) never spawning FFmpeg for a remux, and (c) keeping the data path off slow hops. QUIC and MoQ do not change VOD throughput. MoQ's own lead author says so: *"Just use HTTP. You don't need MoQ for your VOD content."* ([moq.dev/blog/you-dont-need-it](https://moq.dev/blog/you-dont-need-it/)) [V]
2. **Truffle's tsnet data plane is the throughput ceiling today, not Rust.** Same-host release builds reach **~350–500 Mb/s**, and the two Go sidecars use **~6 logical CPUs** while the Rust processes use 25–40% (`truffle/benchmark/README.md:727-737`) [V]. That covers about 3–5 concurrent 4K remux streams, but it costs a lot of CPU on NAS-class hardware. flux needs an explicit **fast path** (LAN-direct or OS-Tailscale) and a Truffle improvement track (§5.1, §10 idea C).
3. **Truffle's raw QUIC cannot carry HTTP/3 or MoQ as shipped.** It sets `max_concurrent_uni_streams(0)` (`crates/truffle-core/src/transport/quic.rs:146,181`), fixes the ALPN to `truffle-raw.{app_id}` (`quic.rs:831-837`), and keeps the `quinn::Connection` private (`quic.rs:895-899`). Both h3 and moq-lite need unidirectional streams. Its tsnet UDP adapter has no GSO/GRO, no ECN and IPv4 only, and it allocates a `Vec` per datagram in both directions (`quic_socket.rs:21-26,149,183,232-247`) [V].
4. **Index once, remux statelessly.** MP4 `moov` (stss/stsz/stco) and MKV `Cues` give a keyframe and segment map with almost no I/O. In my run the MP4 index needed **one 64 KiB read, about 0.1% of the file**, in under 1 ms [M]. With that map, flux can serve **any HLS fMP4 segment independently, in-process, with no FFmpeg process and no temp files**. That means instant seeks and near-zero CPU for the dominant MKV→Apple-TV/web path. Prior art validates the segment model: Kyoo's keyframe-aligned on-demand HLS ([zoriya.dev](https://zoriya.dev/blogs/transcoder)) and nginx-vod-module's on-the-fly MP4 repackaging ([GitHub](https://github.com/kaltura/nginx-vod-module)). Kyoo extracts keyframes by packet-scanning with ffprobe, which "can take up to 30 seconds" on slow HDDs. flux should not.
5. **matroska-demuxer is correct but very seek-heavy:** 253–6,684 seeks just to `open()` a 21–32 MB test file [M]. That is fine on local SSD and slow on SMB/NFS. Write a batched EBML index reader for scanning (or upstream a fix).
6. **FFmpeg: subprocess for transcode, Rust for containers.** `ffmpeg-next` is officially **maintenance-only** and supports FFmpeg "3.4 til 8.0" ([README](https://docs.rs/crate/ffmpeg-next/9.0.0/source/README.md)) [V], while FFmpeg **9.0 shipped 2026-08-04** ([Phoronix](https://www.phoronix.com/news/FFmpeg-9.0-Released)) [S]. `rsmpeg` last published Aug 2025 (0.18.0+ffmpeg.8.0) [V]. In-process libav bindings lag upstream and carry LGPL/GPL obligations, so keep FFmpeg as a supervised process for transcodes and use pure-Rust parsers and muxers for the hot path.
7. **Server stack:** tokio 1.53 + axum 0.8 + hyper 1.12 + tower-http 0.7 over TCP (HTTP/1.1 + HTTP/2). There is no sendfile in hyper, and it is irrelevant through tsnet anyway. io_uring runtimes are Linux-only and mostly stale. Skip them.
8. **DB:** rusqlite 0.40 (bundled SQLite) + WAL + FTS5, with nucleo for instant fuzzy title search (Mille already uses `nucleo 0.5`). Turso 0.8 (pre-1.0, MVCC `BEGIN CONCURRENT`) is promising but not for v1.
9. **Clients:** web = hls.js 1.7 / native HLS (Safari) with an optional **client-side remux** via Mediabunny (MPL-2.0; reads MKV, writes CMAF, AC-3 via WASM). Apple = AVPlayer HLS-fMP4 (it cannot play MKV, so the server remux path matters most here). Desktop = Tauri + libmpv for "plays anything". Rust core on tvOS is now practical (`aarch64-apple-tvos` is distributed by rustup on Rust 1.97 [M]) via UniFFI 0.32.
10. **MoQ** (draft-ietf-moq-transport-**22**, 2026-10-01, still a WG document, IESG milestone Dec 2026) [V] fits **live TV, watch-together and camera feeds** later. It does not fit the library playback path.

---

## 1. Framing: where TTFF, seek and throughput actually go

For one "press play" on a library title, the latency budget breaks into these parts [I]:

| Phase | Typical Plex/Jellyfin cost | What dominates | flux lever |
|---|---|---|---|
| API round trips before playback (item → playback-info → stream URL) | 2–4 RTTs | Chatty APIs, server work per request | One "play" call returns the plan plus the first URLs. Prefetch on focus (§10 E) |
| Playback decision (direct play / remux / transcode) | ms–s (some servers probe at play time) | Probing the file at play time | Decide from the scan-time index; never probe at play time |
| Starting a remux/transcode | FFmpeg spawn plus demux to the first segment, often hundreds of ms to seconds; a seek restarts it | Process startup, sequential output | **Stateless in-process remux**: segment N is computable from the index (§10 B) |
| Fetching init plus first segment | 1–2 RTT + bytes/throughput | Segment size, transport ceiling | Small first segment (1–2 s), HTTP/2 multiplexing, fast data path |
| Player buffer threshold | Player-specific | hls.js/AVPlayer config | Tune the start buffer for LAN |

Throughput budget: a 4K UHD Blu-ray remux peaks around 100–130 Mb/s [I, typical]. Over Truffle's tsnet path, with a ~350–500 Mb/s single-host ceiling and ~3 CPUs per sidecar at ~390 Mb/s, the sidecar costs roughly **0.8 CPU-core per 100 Mb/s per side** [I from `benchmark/README.md:733-737`]. That works on an M1 Max and hurts on an N100 NAS.

**Implication:** the stack choices below are judged first on whether they remove work (probe, spawn, decode, copy, hop), and only then on raw speed.

---

## 2. FFmpeg and GStreamer integration options

### 2.1 Options (versions from crates.io, 2026-10-18) [V]

| Crate | Latest | Published | License (bindings) | Model | Status notes |
|---|---|---|---|---|---|
| `ffmpeg-next` | 9.0.0 | 2026-08-05 | WTFPL | In-process safe-ish wrapper | README: *"currently in maintenance mode … (currently from 3.4 til 8.0)"*. PRs that improve the API are *"unlikely to be merged"* ([README](https://docs.rs/crate/ffmpeg-next/9.0.0/source/README.md)). ~2.2M recent downloads |
| `ffmpeg-sys-next` | 9.0.1 | 2026-10-09 | WTFPL | Raw FFI | Active point releases |
| `ffmpeg-the-third` | 6.0.0+ffmpeg-9.0 | 2026-08-09 | WTFPL | Fork of ffmpeg-next | First crate to label FFmpeg 9.0 support |
| `rsmpeg` | 0.18.0+ffmpeg.8.0 | 2025-08-24 | MIT | In-process, thinner safe wrapper (larksuite) | No release in 14 months; the sys crate `rusty_ffmpeg` 0.17.0+ffmpeg.8.1 (2026-04-10) moves faster |
| `ez-ffmpeg` | 0.19.3 | 2026-10-06 | MIT/Apache/MPL | High-level in-process, built on ffmpeg-next | Effectively one maintainer; APIs marked experimental ([dev.to overview, author appears to be the maintainer](https://dev.to/yeauty/the-state-of-media-processing-in-rust-2026-what-each-crate-actually-covers-2k2c)) [S] |
| `video-rs` | 0.12.0 | 2026-09-09 | MIT/Apache | High-level, ffmpeg-based | Self-described WIP; no audio API [S] |
| `ffmpeg-sidecar` | 2.7.0 | 2026-10-18 | MIT | **Subprocess** with typed event/progress parsing | Very active (~594k recent downloads) |
| `gstreamer` (+`-app`, `-video`) | 0.25.4 | 2026-09-21 | MIT/Apache (GStreamer itself LGPL) | Pipeline framework | Very active (freedesktop GitLab); also `gst-plugin-fmp4` 0.14.5 and `gst-plugin-hlssink3` 0.15.4 (MPL-2.0) |

Upstream context: FFmpeg 8.1 released 2026-03-16 ([AVpres](https://avpres.net/newsletter/2026-03-23.html)) [S]. FFmpeg 9.0 "Lei" released 2026-08-04, a full ABI bump with more Vulkan, animated WebP and AMF ([Phoronix](https://www.phoronix.com/news/FFmpeg-9.0-Released)) [S]. Jellyfin's own fork is at **`jellyfin-ffmpeg` v8.1.3-1 (2026-09-27)** [V GitHub API]. Homebrew on this machine ships libavcodec 62.x, i.e. FFmpeg 8.x [V local `/opt/homebrew/lib`], and it is currently broken (see the brief).

### 2.2 Recommendation

- **Transcode and complex filter graphs: supervised FFmpeg subprocess** (pin one build per platform; jellyfin-ffmpeg 8.1.x is the pragmatic choice because its hwaccel patches are battle-tested). Use `ffmpeg-sidecar` or a small in-house supervisor that parses `-progress pipe:1`. A crash is then a job failure, not a server crash, and licensing stays at arm's length (§2.3). The earlier discussion recommends the same thing (`flux/initial-discussion.md:163-180`).
- **Remux, probing and keyframe indexing: pure Rust** (§3). This is where the performance lives, and no FFmpeg binding is needed.
- **Narrow in-process libav later**, behind a feature flag, only where a process boundary measurably costs latency: for example audio-only decode/encode for segment-aligned audio transcode, or single-keyframe decode for thumbnails. Use `rsmpeg`/`rusty_ffmpeg` (thin, MIT, tracks 8.1) or `ffmpeg-the-third` (tracks 9.0). Avoid building on `ffmpeg-next` given its maintenance-mode statement. Confidence: medium. Both are small-team projects.
- **GStreamer:** do not make it the core media engine. It does buy you `fmp4mux`/`hlssink3` and the GStreamer-based Rust video widgets (§7.4). Its pipeline model and plugin-set packaging make a single static server binary harder [I].

### 2.3 Licensing (FFmpeg)

From [ffmpeg.org/legal.html](https://ffmpeg.org/legal.html) [V]: FFmpeg is **LGPL-2.1+**. If optional GPL parts are enabled, *"the GPL applies to all of FFmpeg"* (libx264 is named). `--enable-nonfree` builds (e.g. fdk-aac) are not redistributable. LGPL compliance requires dynamic linking, shipping the matching source and configure line, and no GPL libs. **Practical rule for flux** [I]: ship FFmpeg as a separate executable. If flux itself is permissively licensed, an LGPL FFmpeg (no x264/x265) is the clean choice, and HW encoders (VideoToolbox, VAAPI, NVENC, QSV) cover H.264/HEVC encode. If flux is GPL/AGPL (like Dim, AGPL-3.0, and ferrofin, GPL-3.0), a GPL build with x264/x265 is fine. **flux's own license is an open decision that gates this.**

---

## 3. Pure-Rust containers, codecs, images, subtitles, audio

### 3.1 Containers [V crates.io]

| Crate | Latest (date) | License | Read/Write | Notes for flux |
|---|---|---|---|---|
| `mp4-atom` | 0.16.2 (2026-10-06) | MIT/Apache | **R+W**, incl. `moof`/`mfra`/`sidx`/`styp`/`emsg` modules | kixelated's crate, used by the MoQ stack (`moq-mux`). Low-level: *"without validation or interpretation"*, and reading `Mdat` loads it into memory, so stream `mdat` yourself ([README](https://github.com/kixelated/mp4-atom)). Has `tokio` and `bytes` features. **Best choice for fMP4/CMAF muxing.** |
| `mp4` (alfg) | 0.14.0 (2023-08-01) | MIT | R+W | Popular but stale for 3 years |
| `re_mp4` | 0.5.1 (2026-07-08) | MIT | R | Rerun's fork of `mp4`, aimed at *"WebCodecs … Rust-based video players for the web"* ([README](https://github.com/rerun-io/re_mp4)) |
| `mp4parse` | 0.17.0 (2023-05-29) | MPL-2.0 | R | Firefox's parser; hardened; few crate releases |
| `shiguredo_mp4` | 2026.5.0 (2026-10-04) | Apache-2.0 | R+W | Active (Shiguredo, WebRTC vendor). Worth evaluating against mp4-atom |
| `mp4san` | 0.5.4 (2026-09-17) | MIT | sanitize | Signal's MP4 sanitizer, useful for untrusted uploads |
| `matroska-demuxer` | 0.8.1 (2026-08-07) | Zlib/MIT/Apache | R (demux) | Parses EBML header, SeekHead, Info, Tracks, **Cues (private field)**, Chapters, Tags, HDR colour metadata. **Very seek-heavy** (§3.2) |
| `matroska` | 0.30.1 (2026-04-15) | MIT/Apache | R (metadata) | Metadata only |
| `webm-iterable` | 0.7.1 (2026-08-28) | MIT | R+W EBML | Streaming EBML tag iterator. A good base for a custom batched Cues reader or an MKV writer |
| `symphonia` | 0.6.1 (2026-08-13) | MPL-2.0 | R (audio formats incl. MKV/MP4 audio) | 0.6 is a big re-architecture. Video/subtitle decoders only behind `exp-*` flags ([heise](https://heise.de/-11294999)) [S] |
| `m3u8-rs` / `hls_m3u8` | 6.0.1 / 0.7.0 | MIT / MIT-Apache | playlists | Either works. Generating playlists by hand is also trivial |
| `dash-mpd` | 0.21.0 | MIT | MPD | Only if DASH is ever needed |

**Gaps [V by search, medium confidence]:** no maintained pure-Rust **MPEG-TS demuxer** of note for Blu-ray `.m2ts`, no **PGS** subtitle crate, and no pure-Rust **MKV muxer** with a high-level API. `webm-iterable` can write EBML.

### 3.2 Local measurement: index-only probing vs scanning [M]

Method (Appendix A): a Rust release build on the M1 Max with a warm page cache and a counting reader under a `BufReader`. MP4: walk the top-level atoms, skip `mdat` by seeking, decode `moov` with `mp4-atom`, count video `stss`/`stsz`. MKV: `MatroskaFile::open()` (which parses Cues) vs iterating every frame.

| File | Size | Index-only time | Bytes read (index) | Seeks | Full scan time | Full-scan bytes read |
|---|---|---|---|---|---|---|
| MP4 clip A (H.264, faststart) | 66.4 MB | 0.755 ms (first run) | 64 KiB (0.10%) | 0 | 7.4 ms | 66.4 MB |
| MP4 clip B | 54.5 MB | 0.063 ms | 64 KiB (0.12%) | 0 | 4.5 ms | 54.5 MB |
| MP4 clip C | 54.0 MB | 0.075 ms | 64 KiB (0.12%) | 0 | 4.0 ms | 54.0 MB |
| MP4 clip D (5,400 frames) | 163.7 MB | 0.264 ms | 144 KB (0.09%) | 0 | 13.0 ms | 163.7 MB |
| `test1.mkv` (Matroska test suite) | 23.3 MB | 0.54 ms | 5.37 MB (23%) with 64 KiB buffer; **0.34 MB (1.4%) with 4 KiB buffer** | **482** | 39 ms | 550 MB (*re-reads*) |
| `test2.mkv` | 21.1 MB | 1.08 ms | 13.4 MB (63%) | 1,313 | 20 ms | 337 MB |
| `test4.mkv` (live-style: no Cues/SeekHead) | 21.3 MB | 8.3 ms | 141 MB (663%) / 8.8 MB with 4 KiB | **6,684** | 56 ms | 776 MB |
| `test5.mkv` (11 tracks) | 31.8 MB | 0.78 ms | 5.07 MB / 0.40 MB with 4 KiB | 1,481 | 26 ms | 397 MB |
| `test6.mkv` | 23.3 MB | 0.24 ms | 3.1 MB | 253 | 128 ms | 548 MB |

What this shows:
- **MP4:** the full keyframe/sample table comes from **one buffered read**, roughly 1/1000 of the file, in well under 1 ms [M]. By extrapolation [I], a 2-hour 24 fps movie has about 172,800 video samples (≈0.7 MB `stsz`, ≤1.4 MB `ctts` with B-frames) plus an audio table of up to ≈1.35 MB, so `moov` is ≈2–4 MB against a 4–40 GB file. That is **~1,000–10,000× less I/O than packet-scanning**, which is Kyoo's approach (`ffprobe -show_entries packet=pts_time,flags`, "can take up to 30 seconds" on slow HDDs; [zoriya.dev](https://zoriya.dev/blogs/transcoder)).
- **MKV:** `matroska-demuxer` gets the right answer, but it issues **one seek per element** (hundreds to thousands of seeks to `open`, plus one per frame while iterating). `BufReader` discards its buffer on every `seek()`, so a larger buffer makes it *worse* (2.17 GB read to scan a 23 MB file with a 256 KiB buffer) [M]. On local SSD it does not matter (sub-ms to a few ms). **On SMB/NFS, where each uncached small read is a network round trip, 500–1,500 round trips per file becomes 0.1–1.5 s per file** [I]. A purpose-built reader can get Cues-bearing files done in **2–4 ranged reads**: (1) the first ~64–256 KiB (EBML header, SeekHead, Info, Tracks), (2) one read of the `Cues` element at its SeekHead offset, (3) optional Chapters/Tags/Attachments. Fallback for Cues-less files (test4-style live recordings): scan cluster headers only.
- `SimpleBlock` keyframe flags are absent in BlockGroup-based files (test4 reported 0 keyframes) [M]. Use Cues, or codec-level parsing (NAL types), for keyframe truth.

### 3.3 Video codecs and hardware acceleration [V crates.io]

| Crate | Latest | License | Role | Risk |
|---|---|---|---|---|
| `rav1e` | 0.8.1 (2025-06) | BSD-2 | AV1 software encoder | Slow vs SVT-AV1; only for niche offline work |
| `dav1d` (+`dav1d-sys`) | 0.11.1 (2025-11) | MIT (libdav1d BSD-2) | AV1 software decode (thumbnails of AV1 sources) | Needs system libdav1d |
| `openh264` | 0.9.9 (2026-10-15) | BSD-2 | H.264 software encode/decode | **Patent caveat [I, medium]:** Cisco's patent coverage applies to Cisco-distributed binaries. Compiling from source (the crate default) does not carry it |
| `x264` / `x265` | 0.5.0 (2022) / 0.0.0 | MIT wrappers over **GPL** libs | — | Stale. Drags in GPL. Avoid |
| `objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`, `objc2-av-foundation` | 0.3.2 (2025-10) | Zlib/Apache/MIT | Generated Apple framework bindings | Low-level and unsafe-heavy, but complete. **Best path to VideoToolbox decode/encode in-process on macOS** |
| `videotoolbox` (doom-fish) | 0.21.0 (2026-09-24) | MIT/Apache | Safe VT wrapper (H.264/HEVC/ProRes encode) | Young (~10k downloads). Watch it |
| `cros-codecs` / `cros-libva` | 0.0.6 / 0.0.13 | BSD-3 | ChromeOS VA-API/V4L2 stateless codecs in Rust | Pre-1.0. Linux-only. Interesting for a Linux HW decode path without FFmpeg |
| `nvidia-video-codec-sdk` | 0.4.0 (2025-09) | MIT (wraps proprietary SDK) | NVENC/NVDEC | Small. FFmpeg's NVENC is the safer route |

**Recommendation [I]:** for v1, leave HW video work to FFmpeg (`-hwaccel videotoolbox|vaapi|qsv|cuda`). In-process VT via objc2 is a P2 optimization for **single-keyframe thumbnail decode on macOS** (§10 D).

### 3.4 Images, posters and thumbnails [V crates.io]

| Crate | Latest | License | Role |
|---|---|---|---|
| `image` | 0.25.10 (2026-03) | MIT/Apache | Decode/encode umbrella |
| `zune-jpeg` | 0.5.15 (2026-09) | MIT/Apache/Zlib | Fastest pure-Rust JPEG decode (used by `image`) |
| `fast_image_resize` | 6.1.0 (2026-07) | MIT/Apache | SIMD resize (SSE4.1/AVX2/**NEON**). The author's x86 benchmarks beat libvips on Bilinear/Bicubic/Lanczos3 (e.g. 13.2 ms vs 16.0 ms Lanczos3, 4928×3279) ([README](https://github.com/cykooz/fast_image_resize)) [S self-reported] |
| `turbojpeg` | 1.5.1 (2026-07) | Unlicense/MIT (libjpeg-turbo BSD-style) | Fast JPEG encode/decode, lossless crops |
| `mozjpeg` | 0.10.13 (2025-02) | IJG | Smallest JPEGs for posters (slower encode) |
| `image-webp` | 0.2.4 | MIT/Apache | Pure-Rust WebP |
| `webp` | 0.3.1 | MIT/Apache | libwebp bindings (lossy encode) |
| `ravif` | 0.13.0 (2026-01) | BSD-3 | AVIF encode (rav1e-based, slow, good for offline posters) |
| `jxl-oxide` | 0.12.6 (2026-05) | MIT/Apache | JPEG XL **decode** |
| `zune-jpegxl` | 0.5.2 | MIT/Apache/Zlib | JPEG XL **lossless encode** |
| `jpegxl-rs` | 0.16.0+libjxl-0.12.0 | **GPL-3.0-or-later** | libjxl wrapper. **License trap**, avoid unless flux is GPL |
| `libvips` | 2.3.0 (2026-06) | MIT (libvips LGPL) | Shrink-on-load thumbnails. Heavy system dependency |
| `img-parts`, `kamadak-exif` | 0.4.1 / 0.6.1 | MIT-Apache / BSD-2 | Metadata, ICC/EXIF |

Format note: Chrome **155** ships JPEG XL decoding, implemented with the Rust **jxl-rs** decoder ([Chrome blog, 2026-10-06](https://developer.chrome.com/blog/jpeg-xl-in-chrome)) [V]. Whether it is enabled by default is not stated explicitly. Safari has supported JXL for years [I]. For posters: **AVIF or WebP for the web today, JPEG as the universal fallback, JXL as a future option.** Store originals plus a small set of server-rendered sizes. `fast_image_resize` + `turbojpeg`/`image-webp` is a fully in-process, dependency-light pipeline [I].

### 3.5 Subtitles

| Crate | Latest | License | Formats | Status |
|---|---|---|---|---|
| `fasrt` | 0.5.0 (2026-09) | MIT/Apache | SRT, WebVTT (zero-copy) | New (~1k downloads). docs.rs build failed on 0.5.0 [S] |
| `aspasia` | 0.2.1 (2024-06) | 0BSD | SRT, ASS/SSA, WebVTT, MicroDVD + conversion | API "not particularly stable" [S] |
| `rsubs-lib` | 0.4.0 (2026-10) | MIT | SRT/VTT/SSA conversion | WIP |
| `subparse` | 0.7.0 (2021) | MPL-2.0 | SSA/ASS, SRT, MicroDVD, VobSub idx/sub | Stale |
| `ass-core` | 0.1.2 (2026-07) | MIT | ASS | Very new |
| libass bindings (`libass` 0.2.0, 2020) | — | ISC | ASS rendering | Stale. Render ASS client-side instead |

**Recommendation [I]:** text subs: SRT/ASS → WebVTT on the fly (cheap) for AVPlayer/hls.js. Full-fidelity ASS: render client-side. Web: **JASSUB** (libass compiled to WASM, WebGL-accelerated; [README](https://cdn.jsdelivr.net/npm/jassub@2.5.6/README.md)) [S]. Desktop: libmpv. Image subs (PGS/VobSub) need burn-in through the FFmpeg transcode path, or OCR as an AI task (see the metadata/AI report). No Rust PGS crate was found.

### 3.6 Audio

- `symphonia` 0.6.1 (MPL-2.0): pure-Rust demux plus decode for AAC-LC, MP3, FLAC, Vorbis, ALAC, PCM. It has **no AC-3/E-AC-3/DTS/TrueHD**, and HE-AAC and Opus are incomplete ([dev.to overview](https://dev.to/yeauty/the-state-of-media-processing-in-rust-2026-what-each-crate-actually-covers-2k2c)) [S]. Those missing codecs are exactly the ones that force transcodes. **Symphonia therefore cannot replace FFmpeg for movie audio.**
- `opus` 0.4.0 (libopus, BSD) [V]. `fdk-aac` 0.8.0 (FDK license: not OSI and not GPL-compatible) [V crate; license nuance I]. `rubato` 5.0.1 resampling [V].
- **Practical [I]:** audio transcode (DTS/TrueHD → **E-AC-3 640 kb/s 5.1** for Apple TV/receivers, or AAC stereo for phones and web) through FFmpeg's native `eac3`/`aac` encoders (LGPL). It is cheap: one core runs this at many times real time [I, unmeasured here].

---

## 4. Prior art: what to reuse and what to learn

| Project | Lang / license | Status (GitHub API 2026-10-18) [V] | Lessons for flux |
|---|---|---|---|
| **Dim** (Dusk-Labs/dim) | Rust / AGPL-3.0 | 4,125★, **last push 2025-09-21** (dormant) | Proves a Rust server with React UI and SQLite. Needed nightly Rust. Stalled. Do not depend on it |
| **ferrofin** (mangoleaf/ferrofin) | Rust / GPL-3.0 | 40★, pushed 2026-10-18 | Drop-in Jellyfin API in Rust (~20 crates, sqlx/SQLite, *"All 412 operations"*). **Self-reported vs Jellyfin:** direct-play TTFB **0.48 ms vs 4.61 ms**, idle memory **61 MiB vs 409 MiB**, cold start 174 ms vs 2,746 ms ([README](https://github.com/mangoleaf/ferrofin)) [S]. Shows that "Rust rewrite" gains in API latency and memory are real, and that streaming TTFF is a different problem. Its WASM-plugin sandbox idea is worth noting. GPL, so ideas only unless flux is GPL |
| jellyfin-rs (PainKiller0x0) | Rust / GPL-2.0 | 0★ | Early. No transcoding or HLS |
| **Stump** | Rust core + TS / MIT | 2,754★, active | Comics/books server. Reference for a Rust media-adjacent catalog with OPDS. Permissive license |
| **Kyoo** gocoder | Go | — | **Keyframe-aligned, lazily generated HLS**: every variant shares the original's keyframe boundaries, so quality switching and seeking work instantly. Transcodes start *at the requested segment*. CUDA needs `-force_idr 1`. Keyframe extraction by packet scan is slow on HDDs ([zoriya.dev](https://zoriya.dev/blogs/transcoder)) [V]. **flux should copy the segment model and replace the slow keyframe scan with container indexes (§3.2).** |
| **nginx-vod-module** (Kaltura) | C | — | The industry pattern of **on-the-fly MP4 → HLS/DASH repackaging without FFmpeg**, plus metadata, mapping and response caches. MP4 input only ([README](https://github.com/kaltura/nginx-vod-module)) [V] |
| **mediamtx** | Go / MIT | 20,449★, v1.21.2 (2026-10-08) | Live media router. Now lists **Media-over-QUIC** alongside SRT/WebRTC/RTSP/LL-HLS ([README](https://github.com/bluenviron/mediamtx)) [V]. The model for flux's possible future live-TV/camera module, and a candidate to embed or sidecar instead of building one |
| **GPAC** | C / LGPL-2.1 | v26.07.0 (2026-07-24) | Reference MP4/CMAF/DASH/HLS packager (MP4Box). Use it as a conformance oracle in tests |
| **Shaka Packager** | C++ | v3.9.3 (2026-07-27) | Packager only (no transcode). Oracle for HLS/DASH correctness |
| **hls.js** | TS / Apache-2.0 | v1.7.3 (2026-09-11) | Does client-side **TS→fMP4 transmuxing in a Web Worker**, *"Transmuxing is performed asynchronously using a Web Worker"* ([README](https://github.com/video-dev/hls.js)) [V]. Proven precedent for client-side remux |
| **Mediabunny** | TS / MPL-2.0 | 7,330★, v1.61.6 (2026-10-16) | Pure-TS read/write of MP4/MOV/**CMAF**/**MKV**/WebM/TS/HLS; transmux/transcode; AC-3/E-AC-3 via a WASM FFmpeg extension; 69.6 kB gzip for everything ([mediabunny.dev](https://mediabunny.dev/guide/supported-formats-and-codecs)) [V]. **The best existing base for client-side MKV direct play** (§7.1) |
| jsmkv, mkv-web, libav-wasm | JS/WASM | — | Earlier in-browser MKV→fMP4 remuxers. jsmkv reportedly failed on Safari. mkv-web needs COOP/COEP for SharedArrayBuffer [S] |
| **Rerun** (`re_mp4`) | Rust | — | Rust MP4 demux + browser WebCodecs. Precedent for Rust→WASM demux in a player [V README; WASM use I] |

---

## 5. Transport: Truffle, QUIC, HTTP/3, WebTransport, MoQ

### 5.1 What Truffle provides today (local code, v0.7.19 @ d6f6664) [V]

- **QUIC via quinn 0.11** (`Cargo.toml:73`; lock: quinn 0.11.9, quinn-proto 0.11.19, quinn-udp 0.5.14) with rustls 0.23 + ring (`Cargo.toml:69`). The certificate is self-signed and runtime-generated, and the client verifier skips verification on the grounds that WireGuard already authenticates (`quic.rs:1-9,161-194`). **Browsers therefore cannot connect to it** [I].
- **Raw QUIC API (RFC 021):** `Node::connect_quic`/`listen_quic` → `QuicConnection { open_stream, accept_stream, remote_address, close }` and `QuicStream { read → Vec<u8>, write, finish, into_split }` (`quic.rs:895-1040`). The ALPN is fixed to `truffle-raw.{app_id}` (`quic.rs:831-837`). The underlying `quinn::Connection` is **not exposed** (`quic.rs:895-899`).
- **Unidirectional streams disabled:** `max_concurrent_uni_streams(0)` on server and client (`quic.rs:146,181`). **HTTP/3 needs uni streams (control and QPACK streams), and moq-lite uses uni streams per group**, so neither can run over Truffle QUIC without a Truffle change [V code; protocol requirement I-high].
- **No QUIC datagrams yet:** "QUIC datagrams (RFC 9221)" is listed under Planned (`benchmark/README.md:812`).
- **tsnet UDP adapter** (`quic_socket.rs`): a 6-byte IPv4+port header per datagram through a loopback relay to the Go sidecar. The file documents *"No GSO/GRO … No ECN … IPv4 only … MTUD is disabled"* (`:21-26`). `try_send` allocates a fresh `Vec` per datagram (`:149`), `poll_recv` allocates `vec![0u8; …]` per receive (`:183`), and segment limits are hard-coded to 1 (`:232-239`).
- **HTTP serving (RFC 023):** the Go engine reverse-proxies to a loopback service with MagicDNS TLS. **HTTP/3 and WebTransport are explicit non-goals** (`docs/rfcs/023-http-serving.md:55-61`).
- **Egress (RFC 026):** a loopback port-forward to a fixed tailnet target (E2) and an HTTP proxy (E3) for browsers (`docs/rfcs/026-egress.md:44-46`). **This is exactly the "local gateway" that native players (AVPlayer, mpv) need** to reach flux through an embedded node [I].

**Measured ceilings** (same host, release builds, 2026-10-16) [V `benchmark/README.md`]:

| Item | Result | Ref |
|---|---|---|
| TCP / egress throughput | bimodal ~330–365 or ~480–550 Mb/s | `:615-619` |
| QUIC, 4 streams | 390–467 Mb/s (403 Mb/s A/B median) | `:620,727` |
| QUIC connect | 1.45–1.79 ms after the core fix (was ~43 ms) | `:595` |
| Who burns the CPU | **two sidecars ≈ 590–660% CPU** at 350–500 Mb/s; Rust 25–40% | `:733-737` |
| HTTP through the sidecar proxy, keep-alive | 3.9k / 6.1k / 8.9k req/s at 1/2/4 workers | `:740-741` |

Tailscale context: wireguard-go reached **10 Gb/s** on bare-metal Linux with UDP GSO/GRO and checksum work ([tailscale.com/blog/more-throughput](https://tailscale.com/blog/more-throughput)) [V], and UDP throughput rose about 4× in v1.54 ([tailscale.com/blog/quic-udp-throughput](https://tailscale.com/blog/quic-udp-throughput)) [S]. **tsnet, however, runs the gVisor netstack in userspace** ([Tailscale docs](https://tailscale.com/kb/1177/kernel-vs-userspace-routers)) [S]. Truffle also adds loopback bridges between Rust and Go. The 350–500 Mb/s ceiling at about 3 cores per side is therefore a *Truffle/tsnet* number, not a Tailscale-kernel number [I].

### 5.2 QUIC implementations [V crates.io]

| Crate | Latest (date) | License | Notes |
|---|---|---|---|
| `quinn` / `quinn-proto` / `quinn-udp` | 0.11.12 / 0.11.19 / 0.6.3 (Sep 2026) | MIT/Apache | De-facto Rust QUIC, used by Truffle, iroh and moq. No 0.12 yet. Debian lists **CVE-2026-25800** in `quinn-proto` < 0.11.15 ([tracker](https://tracker.debian.org/pkg/rust-quinn-proto)) [S]. Truffle's lock (0.11.19) is above that |
| `quiche` / `tokio-quiche` | 0.30.0 / 0.20.1 (Oct 2026) | BSD-2 | Cloudflare. tokio-quiche was open-sourced Nov 2025; Cloudflare says it powers **iCloud Private Relay "Proxy B"** and handles millions of H3 req/s ([blog](https://blog.cloudflare.com/async-quic-and-http-3-made-easy-tokio-quiche-is-now-open-source/)) [S]. Its H3 stack is more mature than `h3`, but it is not quinn, so it is incompatible with Truffle's socket adapter |
| `s2n-quic` | 1.91.0 (2026-10-14) | Apache-2.0 | AWS. Mature, heavier |
| `h3` / `h3-quinn` / `h3-webtransport` | 0.0.8 / 0.0.10 / 0.1.2 (all 2025-05) | MIT | README: *"still very experimental"* [S]. No release in 17 months |
| `wtransport` | 0.7.2 (2026-08) | MIT/Apache | WebTransport server/client on quinn |
| `web-transport-quinn` | 0.13.2 (2026-10-07) | MIT/Apache | moq-dev's WebTransport (most active) |
| `iroh` | 1.3.0 (2026-09-28) | MIT/Apache | P2P QUIC with NAT traversal and relays. **1.0 shipped June 2026** [S]. An in-process data plane (no Go sidecar), but a different identity model from Tailscale |

**QUIC is not free CPU-wise:** a WWW'24 measurement study found QUIC lagging HTTP/2 above ~500 Mb/s, and **45.2% slower at 1 Gb/s in Chrome**, because of receiver-side packet processing ([arXiv 2310.09423](https://arxiv.org/abs/2310.09423)) [S]. kixelated: *"QUIC does not increase throughput"* ([moq.dev](https://moq.dev/blog/you-dont-need-it/)) [V].

### 5.3 HTTP/3 and WebTransport in browsers

- **Safari 26.4 shipped WebTransport** (HTTP/3, with HTTP/2 fallback, streams, datagrams) ([WebKit, "WebKit Features for Safari 26.4"](https://webkit.org/blog/17862/webkit-features-for-safari-26-4/)) [V]. Every major engine now has it. Caniuse lists 26.4 for macOS and iOS [S]. One source says the OS (26.4) is the floor [S, unverified].
- Browser→flux over WebTransport/H3 requires a **publicly trusted certificate** (Truffle can obtain MagicDNS certs for its TLS listeners, RFC 023) or `serverCertificateHashes`. Safari's support for the latter is unverified. It also requires a UDP path to the tsnet node. Chrome's Local Network Access permission gates now cover WebTransport (`initial-discussion.md:420`) [S].
- **Verdict [I]:** no v1 benefit. Browser VOD over HTTP/2 to the same origin is fine and already multiplexed.

### 5.4 Media over QUIC (MoQ): status and fit

- **IETF:** `draft-ietf-moq-transport-22`, 2026-10-01, state *WG Document*, milestone "Request publication … to IESG" **Dec 2026** ([Datatracker](https://datatracker.ietf.org/doc/draft-ietf-moq-transport/)) [V]. At IETF 126 (Jul 2026) chairs and editors discussed moving to WGLC once the registry stabilizes, with a formal objection over splitting the SSTS/filter work out ([minutes](https://datatracker.ietf.org/doc/minutes-126-moq-202607201200/)) [S]. **Not an RFC before 2027** [I].
- **moq-dev/moq** (kixelated; 1,570★; MIT/Apache) [V]: `moq-lite` (*"a forwards-compatible subset of the IETF moq-transport draft"*), `hang` (*"like HLS/DASH, while moq-lite is like HTTP"*), `moq-relay` 0.17.7, `moq-native` 0.20.0, `moq-mux` 0.10.16 (fMP4/CMAF and HLS import), `moq-net` 0.3.15, plus TS packages `@moq/watch`/`@moq/publish` using **WebTransport + WebCodecs + WebAudio** ([README](https://github.com/moq-dev/moq)). Pre-1.0 with frequent minor bumps.
- **Cloudflare:** MoQ enabled on every server (330+ cities), and since **2026-07-31** an API to provision **isolated relays** with publisher/subscriber tokens, supporting **draft-14 and draft-16**, free in beta ([blog](https://blog.cloudflare.com/moq-relays/)) [V].
- **Fit for flux:**
  - **VOD library playback: no.** The protocol is built around live tracks and groups. VOD is better served by HTTP caching and range semantics. The author says to use HTTP for VOD and *"You should not use MoQ for high-quality content"* ([moq.dev](https://moq.dev/blog/you-dont-need-it/)) [V].
  - **Where it fits (P2):** live TV / IPTV re-streaming (sub-second channel change), **watch-together sync** (a group as a sync unit), home cameras (mediamtx already speaks MoQ), and remote "screen" playback to another device. Over Truffle it needs uni streams enabled (§5.1).

### 5.5 Does QUIC help *over Tailscale* for VOD? [I, medium-high]

- **Connection migration is already solved underneath.** WireGuard has *"full IP roaming on both ends"* ([wireguard.com](https://www.wireguard.com/)) [V], and the inner tailnet IPs do not change when a phone moves from Wi-Fi to LTE, so an inner TCP connection survives an endpoint change, subject to timeouts. QUIC's headline mobile feature is therefore largely redundant inside a tailnet.
- **Loss recovery:** Facebook saw up to **22% better MTBR and 20% fewer stalls** with QUIC, mostly on poor networks ([engineering.fb.com](https://engineering.fb.com/2020/10/21/networking-traffic/how-facebook-is-bringing-quic-to-billions/)) [V]. A home user on a decent LAN or broadband gains little. Remote viewing on bad cellular through DERP might gain something, but it also pays QUIC's CPU tax on both ends.
- **Head-of-line blocking:** HTTP/2 over TCP has cross-stream HoL. For VOD, segments are fetched mostly sequentially, so this matters little.
- **Conclusion:** media over **HTTP/1.1 + HTTP/2 on TCP** inside the tailnet. Reserve QUIC for (a) server↔server bulk (e.g. remote-transcoder offload, multi-server sync) via Truffle raw QUIC, and (b) future live/interactive features. Re-test once Truffle has GSO/GRO.

---

## 6. Web server stack for media delivery

| Component | Version [V] | Assessment |
|---|---|---|
| `tokio` | 1.53.3 (2026-10-13) | Default. Has an **`io-uring` feature gated on `tokio_unstable` + Linux** for fs ops (`tokio-1.53.3/Cargo.toml:87-88,888`; `src/fs/read_uring.rs`) [V local source] |
| `axum` | 0.8.9 (2026-04-14) | Recommended. Tower ecosystem; matches Truffle's tokio/hyper stack |
| `hyper` / `hyper-util` | 1.12.0 / 0.1.21 | HTTP/1.1 + HTTP/2. **No sendfile**: bodies are user-space chunks |
| `tower-http` | 0.7.1 (2026-08-31) | `ServeDir`/`ServeFile` support Range and ETag. Body = `ReaderStream` over `tokio::fs::File` with **64 KiB default chunks** (`tower-http-0.6.11/src/services/fs/serve_dir/mod.rs:28`, `with_buf_chunk_size` at `:121`; `services/fs/mod.rs:13`) [V, read from the 0.6.11 source] |
| `actix-web` | 4.17.0 | Fast and mature, but its own runtime model. No reason to choose it over axum here |
| `pingora` | 0.9.0 | Cloudflare's proxy framework. A proxy, not an app server. Not needed |
| `monoio` | 0.2.4 (**2024-08**) | io_uring thread-per-core. Releases stale |
| `glommio` | 0.9.0 (**2024-03**) | Linux-only. Releases stale (repo pushed 2026-08) |
| `compio` | 0.19.2 (2026-09-29) | Active, cross-platform (IOCP/io_uring/kqueue-ish polling) |
| `tokio-uring` | 0.5.0 (**2024-05**) | Repo last pushed 2025-07. Effectively stalled |

**Assessment [I]:**
- At home-server rates (≤1–2 Gb/s total), a 64 KiB–1 MiB chunked read loop costs negligible CPU (1 Gb/s ≈ 1,900 × 64 KiB chunks/s). sendfile would only matter at 10+ Gb/s or for plain-TCP LAN serving, and it is **impossible** through TLS or through the tsnet path anyway. **Not a lever.**
- io_uring runtimes would fork the codebase (Linux-only) for no gain while Truffle's sidecar is the bottleneck. **Don't.** Revisit only for the scanner's metadata syscalls on huge Linux libraries (tokio's unstable io-uring fs, or compio).
- What does matter: a **custom range/segment handler** (not ServeDir) that streams from the index with `Bytes`-backed buffers, uses 256 KiB–1 MiB reads for remux segments, sets `Cache-Control: immutable` on content-addressed segment URLs, enables HTTP/2 for API + thumbnails + segments on one connection, and supports cancellation (drop the body future when the client disconnects or seeks).

---

## 7. Clients

### 7.1 Web

- **Players:** hls.js **1.7.3** (2026-09-11) [V]; Safari iOS 17.1+ via **ManagedMediaSource** since hls.js 1.5 ([README](https://github.com/video-dev/hls.js)) [V]. Shaka Player **5.3.0** (2026-10-16) [V]. Use native HLS on Safari/iOS where it is preferable. One vendor blog claims iPhone MMS only activates with an AirPlay `<source>` alternative or with remote playback disabled [S, verify on device].
- **Codec reality (web):** Chrome decodes HEVC only with hardware support (since 107/108) [S]. Firefox HEVC needs hardware: Windows 134+, macOS 136+, Linux 137+ [S]. **E-AC-3/AC-3: Edge plays them, Chrome generally does not** (Android/ChromeOS excepted) [S]. Safari handles AC-3/E-AC-3 natively [I-high]. DTS/TrueHD play nowhere in browsers.
- **Client-side remux ("server does zero work"): feasible and worth a spike.**
  - Building blocks exist today: **Mediabunny** reads MKV and writes CMAF/MP4 with lazy, prefetching `UrlSource` reads (8 MiB default cache, 2 parallel requests) ([docs](https://mediabunny.dev/guide/reading-media-files)) [V], and `@mediabunny/ac3` provides AC-3/E-AC-3 **decode and encode** via a WASM build of FFmpeg's coders ([docs](https://mediabunny.dev/guide/extensions/ac3)) [S]. hls.js already transmuxes in a worker [V].
  - Flow: worker fetches MKV ranges → demux → fMP4 fragments → MSE/MMS. For AC-3 on Chrome: WASM decode → WebCodecs `AudioEncoder` (AAC/Opus) → mux. DTS/TrueHD would need more WASM decoders (license and patent review), or a server audio-only transcode (§10 F).
  - **Rust→WASM variant:** `matroska-demuxer` and `mp4-atom` are pure Rust over `std::io::{Read,Seek}`, so a wasm32 build is plausible [I; not compiled here because the wasm32 target is not installed and the internal disk is near-full]. Rerun's `re_mp4` + WebCodecs is a precedent. **But Mediabunny already exists, is MPL-2.0 and is tiny.** Prefer it for the web client and keep Rust for the server (one remux implementation per platform: Rust server, TS web).
  - Risks: Safari MMS quirks (jsmkv reportedly failed on Safari) [S], MKV timestamp/DTS reconstruction (§10 B), memory pressure on TVs, and seeking in Cues-less files.

### 7.2 Apple (iOS / tvOS / macOS)

- **AVPlayer is the only first-class option for Apple TV** (HDR/Dolby Vision, frame-rate matching, Atmos passthrough). It plays MP4/MOV/HLS, **not MKV** [I-high; Swiftfin ships a **VLCKit** player specifically to *"maximize direct play"* ([Swiftfin](https://github.com/jellyfin/Swiftfin)) [S]]. Most home libraries are MKV, so **MKV→HLS-fMP4 remux is the hottest path in the whole system.** That is why §10 B ranks so high.
- Rust core on Apple: `uniffi` **0.32.2** (MPL-2.0) [V]. `aarch64-apple-tvos` and `-tvos-sim` are now **listed by `rustup target list`** on Rust 1.97.1, i.e. distributed (Tier 2) [M]. The stable platform-support page previously said Tier 3 ([rustc book](https://doc.rust-lang.org/rustc/platform-support/apple-tvos.html)) [S, superseded].
- Embedded tailnet on Apple: Truffle Swift (RFC 024) exposes a loopback for `URLSession`. **AVPlayer will not use SOCKS**, so flux needs a plain loopback HTTP forward (Truffle egress E2 shape, `026-egress.md:45`) or a local HTTP gateway that rewrites HLS URLs [I]. When the OS Tailscale app is present, just use the tailnet URL.

### 7.3 Android / Google TV

Media3/ExoPlayer plays HLS-fMP4 and (via its extractors) MKV directly [I-high]. Use UniFFI Kotlin bindings for shared Rust (API client, playback planner). Not researched in depth here.

### 7.4 Desktop

| Option | Version [V] | License | Verdict |
|---|---|---|---|
| Tauri 2 + webview `<video>`/MSE | tauri 2.12.4 | MIT/Apache | Same capabilities as the browser (HEVC/AC-3 gaps) |
| Tauri + **libmpv** (`tauri-plugin-libmpv` 0.3.2, `libmpv2` 6.0.0, mpv 0.41.0) | 2025-11 / 2026-05 / 2025-12 | MPL-2.0 / **LGPL-2.1** / mpv GPL (LGPL build possible) | **Plays everything** (MKV, DTS/TrueHD, ASS, PGS, HDR tone-mapping). Embedding uses a positioned child window, not a true in-webview surface. mpv recommends its render API over window embedding ([Tauri discussion](https://github.com/orgs/tauri-apps/discussions/6343)) [S]. Best power-user client |
| iced 0.14 + `iced_video_player` 0.6 (GStreamer, wgpu texture, HW HEVC) | 2025-12 | MIT/Apache | Viable native UI. GStreamer packaging is painful on Windows [S] |
| gpui + `gpui-video-player` 0.1.2 (GStreamer) | 2025-11 | MIT | Needs gpui from the Zed repo. Early |
| egui-video 0.9.0 | 2024-10 | MIT | Stale |
| Slint 1.18 | 2026-09 | GPL / royalty-free / commercial | No first-party video |

**Recommendation [I]:** web-first UI in Tauri (shared with the web client) with an "Open in mpv engine" mode using libmpv's **render API** into a native view (P2). Desktop direct-plays everything and turns server work for desktop into pure byte serving.

---

## 8. Database and search

| Option | Version [V] | Assessment |
|---|---|---|
| `rusqlite` (bundled SQLite) | 0.40.2 (2026-08-08) | **Recommended.** Synchronous, fastest, full SQLite features (FTS5, JSON, WAL, `RETURNING`, custom functions). Pattern: one dedicated writer thread fed by a channel, N reader connections, `PRAGMA journal_mode=WAL; synchronous=NORMAL; mmap_size` |
| `sqlx` (SQLite) | 0.9.0 (2026-05-21) | Async API with compile-time-checked queries. Uses a worker thread per connection under the hood [I]. Fine, but adds macro/build friction. ferrofin uses it |
| `turso` (Rust SQLite rewrite) | 0.8.3 (2026-10-15); repo 24.8k★ | README: *"powers production applications today"* but *"we have not yet reached 1.0"*. `BEGIN CONCURRENT` MVCC, io_uring async I/O, vector exact search, **FTS experimental (tantivy-powered)** ([GitHub](https://github.com/tursodatabase/turso)) [V]. Watch it, but not as the v1 primary store. Keep the DB behind a repository trait |
| `libsql` | 0.10.0 | Turso's C fork. No compelling reason for a local single-node app |
| `redb` / `fjall` / `heed` (LMDB) | 4.4.0 / 3.1.12 / 0.22.1 | KV stores. Good for **caches** (probe cache, image cache index, segment cache metadata), not the relational catalog |
| `tantivy` | 0.27.0 (2026-10-17) | Full-text engine. Only needed for multilingual fuzzy search over large metadata (cast, plot) beyond what FTS5 handles |
| `sqlite-vec` | 0.1.9 | Embedding search inside SQLite ("more like this", AI poster dedup) |
| `nucleo` | (Mille uses 0.5, `mille/Cargo.toml:54`; `crates/mille-core/src/search.rs:1`) | **Instant search-as-you-type** over titles held in memory. 100k titles is trivially in RAM [I] |

**Recommendation:** SQLite (rusqlite) + FTS5 (`unicode61` with diacritics removal, plus a `trigram` table for substring search) + an in-memory nucleo index for the search box. Keep media files' derived artifacts (index blobs, trickplay sprites) in the filesystem or a content-addressed cache, not in SQLite. Note that Mille pins `notify = "6.1"` (`mille/Cargo.toml:24`) while crates.io is at **8.2.0** [V], a small upgrade flux should drive if it reuses Mille's watcher.

---

## 9. Recommended crate stack

| Layer | Crate | Version (2026-10-18) | License | Role in flux | Risk |
|---|---|---|---|---|---|
| Runtime | `tokio` | 1.53.3 | MIT | Async runtime | Low |
| HTTP | `axum` / `hyper` / `tower-http` | 0.8.9 / 1.12.0 / 0.7.1 | MIT | API, range and segment serving | Low. Write a custom segment handler |
| TLS | `rustls` (+`ring` or `aws-lc-rs` 1.18.2) | 0.23.46 | Apache/ISC/MIT | TLS where Truffle doesn't terminate it | Low. Track RUSTSEC advisories (Truffle bumped to 0.23.45 for RUSTSEC-2026-0285) |
| Mesh | **Truffle** (`truffle-core`) | 0.7.19 | (repo) | Identity, discovery, tailnet HTTP proxy, egress, raw QUIC | **Medium**: sidecar CPU ceiling, uni streams off, no GSO |
| QUIC (server↔server, later) | `quinn` | 0.11.12 | MIT/Apache | Via Truffle raw QUIC | Low |
| MP4/fMP4 | `mp4-atom` | 0.16.2 | MIT/Apache | moov parsing and **CMAF segment muxing** | Medium: low-level, small bus factor (kixelated), but MoQ depends on it |
| MKV | custom batched EBML reader on `webm-iterable` 0.7.1, with `matroska-demuxer` 0.8.1 as reference/fallback | — | MIT / Zlib-MIT-Apache | Scan-time index + segment block reads | Medium: you own the edge cases (§10 B) |
| Probe fallback | `ffprobe` (subprocess) | FFmpeg 8.1 | LGPL/GPL | Unusual containers (TS/M2TS/AVI), verification | Low |
| Transcode | FFmpeg subprocess (`ffmpeg-sidecar` 2.7.0 or own supervisor) | jellyfin-ffmpeg 8.1.3-1 | MIT (crate); FFmpeg LGPL/GPL | Video/audio transcode, burn-in, HW accel | Low-medium: pin and test the build |
| In-process libav (later) | `rsmpeg` / `rusty_ffmpeg` or `ffmpeg-the-third` | 0.18.0 / 0.17.0 / 6.0.0 | MIT / WTFPL | Audio-only transcode, keyframe decode | Medium: lagging releases, unsafe surface |
| Apple HW | `objc2-video-toolbox`, `objc2-av-foundation` | 0.3.2 | Zlib/Apache/MIT | macOS in-process keyframe decode | Medium: unsafe FFI |
| Images | `image`, `zune-jpeg`, `fast_image_resize`, `turbojpeg`, `image-webp`, `ravif` | 0.25.10, 0.5.15, 6.1.0, 1.5.1, 0.2.4, 0.13.0 | MIT/Apache/BSD | Posters, thumbnails, trickplay sprites | Low |
| Subtitles | `aspasia` or `fasrt` + own WebVTT writer | 0.2.1 / 0.5.0 | 0BSD / MIT-Apache | SRT/ASS → WebVTT | Medium: young crates. The format is small enough to own |
| Playlists | `m3u8-rs` | 6.0.1 | MIT | HLS playlists (or hand-rolled) | Low |
| DB | `rusqlite` (bundled) | 0.40.2 | MIT | Catalog, FTS5 | Low |
| Search | `nucleo` (+ `tantivy` 0.27 optional) | 0.5 | MPL-2.0 / MIT | Instant search | Low |
| Cache KV | `redb` | 4.4.0 | MIT/Apache | Probe/index/image caches | Low |
| Hashing | `blake3`, `xxhash-rust` | 1.8.7, 0.8.19 | CC0/Apache, BSL-1.0 | Content IDs, partial-file fingerprints | Low |
| FS | `ignore` / `jwalk`, `notify` | 0.4.33 / 0.9.0, 8.2.0 | Unlicense-MIT / MIT, CC0 | Walk/watch (via Mille) | Low |
| FFI to clients | `uniffi` | 0.32.2 | MPL-2.0 | Swift/Kotlin shared core | Low-medium |
| Desktop | `tauri` + `libmpv2` | 2.12.4 + 6.0.0 | MIT-Apache + LGPL-2.1 | Desktop client, mpv engine | Medium: embedding work |
| Web player | hls.js / Mediabunny / JASSUB | 1.7.3 / 1.61.6 / 2.5.x | Apache-2.0 / MPL-2.0 / MIT(+libass ISC) | Playback, client remux, ASS | Low-medium |
| Future live | `moq-native` / `moq-relay` / `hang` / `web-transport-quinn` | 0.20.0 / 0.17.7 / 0.21.16 / 0.13.2 | MIT/Apache | Live TV, watch-together | High: drafts moving, pre-1.0 |

---

## 10. Push-the-limit architecture ideas (ranked by impact ÷ effort)

Scores run 1–5. Impact means effect on TTFF, seek latency, scan speed or server CPU versus Plex/Jellyfin. Effort means engineering cost.

| Rank | Idea | Impact | Effort | Ratio |
|---|---|---|---|---|
| 1 | **E. One-round-trip play plus predictive prefetch** | 4 | 1.5 | 2.7 |
| 2 | **A. Container-index capture at scan (moov / Cues), ffprobe only as fallback** | 4 | 2 | 2.0 |
| 3 | **D. Keyframe-only, HW-decoded trickplay and thumbnails** | 4 | 2 | 2.0 |
| 4 | **F. Audio-only transcode with cache; video always copied when possible** | 4 | 2 | 2.0 |
| 5 | **C. Data-path selection plus Truffle data-plane fixes** | 4.5 | 2.5 | 1.8 |
| 6 | **I. Instant search: nucleo in memory + FTS5** | 2.5 | 1 | 2.5 (low absolute impact) |
| 7 | **B. Stateless, segment-addressable in-process remux to CMAF/HLS** | 5 | 4 | 1.25 (*highest absolute impact*) |
| 8 | **G. Client-side remux for web (Mediabunny worker)** | 3 | 3 | 1.0 |
| 9 | **H. libmpv desktop engine ("server only serves bytes")** | 3 | 3.5 | 0.9 |
| 10 | **J. HTTP/3 over Truffle raw QUIC** | 2 | 3 | 0.7 |
| 11 | **K. MoQ for live TV / watch-together** | 2 (now) | 4 | 0.5 |
| 12 | **L. io_uring / thread-per-core runtime** | 1 | 4 | 0.25: **don't** |

**E. One-round-trip play plus predictive prefetch.** Expose `POST /play {item, device-caps}`, which returns the decided plan (direct / remux / transcode), the master playlist *inline*, and the init and first-segment URLs. When a poster gains focus for more than ~300 ms (TV) or on hover (web), the client prefetches the plan, init and segment 0. Because §10 B makes segment 0 a cheap in-process read, prefetching costs almost nothing server-side. *Target:* TTFF on LAN ≤ 300 ms warm [I target].

**A. Container-index capture at scan.** For MP4/MOV, read `moov` (one or two ranged reads; with moov-at-end, read the tail). For MKV, read SeekHead → Cues (2–4 ranged reads). Store a compact per-asset index: keyframe PTS list plus byte offsets (MKV cluster or relative positions; MP4 sample → chunk offset), codec private data (avcC/hvcC/dvcC), HDR metadata, audio/sub tracks, chapters. At 5,000 keyframes × ~12 bytes that is ≈60 KB per movie [I]. Use ffprobe only for TS/M2TS/AVI/odd files and as an async verifier. *Evidence:* §3.2 (0.1% of bytes for MP4) [M]; Kyoo's packet scan takes up to 30 s [V]. *Watch-outs:* matroska-demuxer's seek pattern on NAS (§3.2); Cues may index only some keyframes (that is fine for segmenting); Cues-less MKVs need a cluster-header scan.

**D. Keyframe-only HW trickplay and thumbnails.** Jellyfin measured **~110× faster** trickplay (244 vs 2.2 fps) by switching from the `fps` filter to `-skip_frame nokey` ([jellyfin#11336](https://github.com/jellyfin/jellyfin/issues/11336)) [V]; it is still off by default there. flux makes keyframe-only the default, uses the scan index to seek straight to keyframes nearest the 10-s grid, and decodes with VideoToolbox/VAAPI. It also resizes with `fast_image_resize` and encodes WebP/JPEG sprite sheets in-process. Phase 1 is an FFmpeg subprocess per title with `-skip_frame nokey -hwaccel …`. Phase 2 is in-process VT decode of the individual keyframe samples read via the index (no demux pass at all). *Target:* a 2-h 4K HEVC title's trickplay in ≤ 20 s on M1 Max [I target, unmeasured].

**F. Audio-only transcode with cache.** The most common reason for a full transcode is audio (DTS, TrueHD, or AC-3 on Chrome), not video [I-high]. Copy the video bitstream (§10 B) and transcode only audio with FFmpeg (`-vn`, to E-AC-3 5.1 for Apple TV or AAC for web/phones). Run it ahead of the playhead and cache by `(asset, track, target)`. Audio encoding is cheap and the cache is reusable across sessions. Handle AAC priming and segment alignment by transcoding contiguous ranges, not per segment [I].

**C. Data-path selection plus Truffle data-plane fixes.** (1) The server advertises multiple endpoints: LAN `http(s)://<lan-ip>`, OS-Tailscale IP when the host runs tailscaled, and the Truffle tsnet node. The client races or probes them and picks the fastest authenticated path, with Truffle remaining the identity and discovery plane. (2) Upstream Truffle work: batch I/O and buffer reuse in `TsnetUdpSocket` (remove the per-datagram `Vec`s), GSO/GRO between Rust and sidecar (or a shared-memory ring), IPv6 framing, uni streams plus configurable ALPN, and expose `quinn::Connection`. (3) Measure sidecar CPU per Mb/s on low-end Linux. *Evidence:* the sidecar ceiling of 350–500 Mb/s at ~6 CPUs [V]. *Target:* ≥ 1 Gb/s with ≤ 1 core on the fast path [I target].

**B. Stateless, segment-addressable in-process remux.** Segment *N* of a title is defined by the keyframe index from A (Kyoo's alignment model). The handler reads the source byte range for [kf_N, kf_{N+1}), parses blocks or samples, and emits `styp`+`moof`+`mdat` via `mp4-atom`, streaming the `mdat` payload straight from the file buffer. There is no process, no temp files and no state, so any segment can be served cold and seeks are instant. Transcoded variants (via FFmpeg) use the same boundaries (`-force_key_frames` at index PTS, `-force_idr 1` on NVENC), so ABR switching works. Prior art: nginx-vod-module (MP4 only) [V], Kyoo (FFmpeg-based) [V].
*Hard parts [I]:* MKV stores **PTS only**, so decode order and composition offsets must be reconstructed per GOP for `trun` (B-frame reordering). The others: building `dac3`/`dec3` from AC-3/E-AC-3 headers; HEVC/Dolby Vision sample entries (`hvc1` vs `hev1`, `dvh1` plus `dvcC`, which Apple requires); MKV header-stripping/lacing; audio–video interleave and timestamp offsets for edit lists; and VFR content. Validate against GPAC/Shaka Packager outputs and Apple's `mediastreamvalidator`.
*Targets:* CPU < 2% of one core per 4K remux stream; any segment served in < 20 ms + I/O; seek-to-play < 250 ms on LAN [I targets].

**G. Client-side remux for the web.** A Mediabunny worker reads the original MKV by range and appends fMP4 to MSE/MMS. AC-3 goes through the WASM decoder and a WebCodecs encode on Chrome. The server serves only bytes, falling back to B. Most valuable when the server is weak (NAS) and the client is strong (laptop) [I].

**H. libmpv desktop engine.** Direct play for every container, codec and subtitle format. Server work becomes byte serving plus thumbnails [I].

**I. Instant search.** nucleo (already in Mille) gives sub-ms fuzzy matching on titles in memory, with FTS5 for full-text fields.

**J. HTTP/3 over Truffle raw QUIC.** Only after C's measurements show a TCP-specific bottleneck. Requires the Truffle changes listed in C(2).

**K. MoQ for live TV / watch-together.** Adopt `moq-lite`/`hang` (or mediamtx as a sidecar) when flux adds live sources. Track draft-ietf-moq-transport toward the RFC (2027) [I].

**L. io_uring.** Linux-only and a stale ecosystem; the bottleneck is elsewhere. **Don't.**

---

## 11. Recommendations for flux

**P0 (decide now)**
1. **Media delivery = HTTP/1.1 + HTTP/2 over TCP** (axum/hyper/tower-http on tokio), with direct-play byte ranges and HLS-CMAF for remux/transcode. **No MoQ, QUIC or HTTP/3 for VOD in v1** (§5.3–5.5).
2. **FFmpeg stays out of process** for transcodes. Pin one tested build per platform (start from jellyfin-ffmpeg 8.1.x; evaluate 9.0 later). **Decide flux's license first** because it determines LGPL vs GPL FFmpeg (§2.3).
3. **Build the container index at scan time in pure Rust** (`mp4-atom` for MP4; a batched EBML/Cues reader for MKV), with ffprobe as fallback and verifier. Do not use `matroska-demuxer` unmodified for scanning network shares (§3.2).
4. **Design the data path as a choice, not a given.** Truffle provides identity, discovery and tailnet reachability, while bulk bytes take the fastest authenticated route. Budget sidecar CPU explicitly (§5.1).
5. **Catalog = rusqlite (bundled) + WAL + FTS5** with a single writer thread. Keep the DB behind a trait so Turso can be evaluated later.

**P1 (first quarter of building)**
6. Ship **stateless in-process remux (MKV/MP4 → CMAF HLS)** for H.264/HEVC + AAC/AC-3/E-AC-3, and **audio-only transcode with cache** for DTS/TrueHD (§10 B, F).
7. **Keyframe-only trickplay** by default, HW-accelerated (§10 D).
8. Upstream **Truffle changes**: uni streams plus configurable ALPN, expose `quinn::Connection`, batch I/O and buffer reuse in `TsnetUdpSocket`, IPv6, and a loopback forward suitable for AVPlayer (§5.1, §10 C).
9. Web client on hls.js 1.7 (or native HLS on Safari), with a **Mediabunny client-remux spike** (§7.1). Apple client on AVPlayer with a Rust core via UniFFI (tvOS target available) (§7.2).

**P2 (later, opportunistic)**
10. libmpv desktop engine. In-process VideoToolbox keyframe decode. JPEG XL posters once Chrome's default status is confirmed.
11. MoQ (`moq-lite`/`hang`, or mediamtx) for live TV, watch-together and cameras. HTTP/3 + WebTransport for browsers (Safari 26.4+) via MagicDNS certs.
12. Turso (post-1.0) if concurrent writes become a bottleneck; tantivy if FTS5 relevance is insufficient; sqlite-vec for AI similarity features.

---

## 12. Prototype suggestions

1. **Stateless remux spike (highest value).** An axum server builds a Cues/moov index for one MKV (H.264 or HEVC + AC-3) and one MP4, serves `master.m3u8`, `init.mp4` and `seg/{n}.m4s` on demand via `mp4-atom`, and plays in Safari/AVPlayer, hls.js (Chrome) and tvOS. *Success:* passes Apple `mediastreamvalidator`; TTFF ≤ 300 ms on LAN; seek-to-play ≤ 250 ms anywhere in the file; server CPU < 2% of one core per stream; zero temp files.
2. **Batched MKV indexer vs matroska-demuxer vs ffprobe** on a 1,000-file library over SMB from a NAS. *Success:* ≤ 4 reads and ≤ 256 KiB per Cues-bearing file; ≥ 20× faster than `ffprobe -show_format -show_streams` per file; identical track and keyframe tables on 100% of the Matroska test suite (`ietf-wg-cellar/matroska-test-files`).
3. **Truffle data-plane bake-off.** Stream a 120 Mb/s remux plus seeks through (a) LAN direct, (b) OS Tailscale, (c) Truffle tsnet proxy, (d) Truffle raw QUIC, on M1 Max and an x86 mini PC. *Success:* a table of Mb/s, CPU-ms per MB per process, TTFF and p99 segment latency that identifies the bottleneck. Goal: fast path ≥ 1 Gb/s at ≤ 1 core.
4. **Keyframe-only trickplay benchmark.** `ffmpeg -skip_frame nokey -hwaccel videotoolbox` vs software vs the current fps filter on 4K HEVC HDR and 1080p H.264. *Success:* ≥ 100× real time, ≤ 20 s per 2-h 4K title on M1 Max, sprites visually aligned within one GOP.
5. **Mediabunny client-remux spike.** MKV H.264/HEVC + AC-3 via MSE (Chrome/Edge/Firefox) and MMS (Safari macOS/iOS). *Success:* TTFF within 1.5× of server remux; works on Safari iOS; AC-3 audible on Chrome through WASM decode.
6. **(Conditional) h3 over Truffle raw QUIC**, after enabling uni streams. *Success:* throughput ≥ TCP on the same path with ≤ 1.2× CPU; otherwise drop it.

---

## 13. Open questions

1. **flux's license** (permissive vs GPL/AGPL) decides the FFmpeg build (x264/x265) and whether GPL prior art (ferrofin, Dim) can be reused as code.
2. **Target server hardware.** If NAS-class (N100/ARM) is a target, the sidecar's ~0.8 core per 100 Mb/s per side becomes a hard constraint (§1). Is a non-tsnet fast path acceptable for the maintainer's "just log in with Tailscale" vision?
3. Will the Truffle maintainers (who also maintain flux) accept the QUIC changes: uni streams, custom ALPN, `quinn::Connection` access, GSO/batching, IPv6?
4. How common are **Cues-less or partially-indexed MKVs** in real libraries (remuxes from MakeMKV vs mkvmerge vs FFmpeg)? This decides how often the cluster-scan fallback runs.
5. **MKV DTS reconstruction** for B-frame content in the in-process remuxer: correctness across x264/x265 pyramids and VFR? This needs a conformance suite.
6. Chrome 155 JPEG XL: on by default? Safari `serverCertificateHashes` support? The iPhone MMS AirPlay requirement? All [S] or unverified.
7. Does Apple TV AVPlayer accept every Dolby Vision profile flux would pass through in remuxed fMP4 (profile 5/8.1/7)? This was out of scope here and belongs with the playback-engine report.
8. Licensing and patents for client-side WASM decoders (AC-3 is patent-expired [I]; DTS/TrueHD unclear).
9. The search budget ran out before I could independently re-verify: Tailscale-specific roaming behaviour for long-lived inner TCP flows, current quinn single-connection throughput figures, and the exact Firefox/Chrome AC-3 status in 2026.

---

## Appendix A: micro-benchmark method [M]

- Code: `$FLUX_WORK/research08/probebench/src/main.rs` (Rust 1.97.1, `--release`, `CARGO_TARGET_DIR=$FLUX_WORK/target`, `CARGO_HOME` on the SSD), with deps `mp4-atom 0.16.2` and `matroska-demuxer 0.8.1`.
- I/O accounting: a `Counting<File>` wrapper counts bytes returned by `read()` and calls to `seek()`, beneath a `BufReader` (64 KiB default, `BUF` env to override; 4 KiB and 256 KiB also tested).
- Each file is read fully once before timing (warm page cache), so **times are CPU/syscall-bound, not disk-bound**. Bytes read and seeks are the transferable metrics for HDD/NAS.
- Media: Matroska test files 1, 2, 4, 5 and 6 from `ietf-wg-cellar/matroska-test-files` (downloaded to `$FLUX_WORK/research08/media/`), plus four pre-existing local H.264 MP4 clips on the SSD (54–164 MB, faststart). The clips were only opened read-only for header parsing.
- Machine: Apple M1 Max, 64 GB, macOS 26 (Darwin 25.5), external Samsung SSD.
- Caveats: short files (≤ 2.5 min). Movie-length moov sizes are extrapolated, not measured. No ffprobe comparison because Homebrew FFmpeg is broken on this machine.

## Appendix B: key sources

- Local: `truffle/crates/truffle-core/src/transport/quic.rs`, `…/quic_socket.rs`, `truffle/benchmark/README.md`, `truffle/docs/rfcs/023-http-serving.md`, `…/026-egress.md`, `mille/Cargo.toml`, `mille/crates/mille-core/src/search.rs`, `flux/initial-discussion.md`.
- crates.io API (`https://crates.io/api/v1/crates/<name>`) and GitHub API (`https://api.github.com/repos/<owner>/<repo>`), queried 2026-10-18.
- MoQ: https://datatracker.ietf.org/doc/draft-ietf-moq-transport/ · https://github.com/moq-dev/moq · https://moq.dev/blog/you-dont-need-it/ · https://blog.cloudflare.com/moq-relays/
- Browsers: https://webkit.org/blog/17862/webkit-features-for-safari-26-4/ · https://developer.chrome.com/blog/jpeg-xl-in-chrome · https://github.com/video-dev/hls.js
- QUIC/Tailscale: https://blog.cloudflare.com/async-quic-and-http-3-made-easy-tokio-quiche-is-now-open-source/ · https://arxiv.org/abs/2310.09423 · https://engineering.fb.com/2020/10/21/networking-traffic/how-facebook-is-bringing-quic-to-billions/ · https://tailscale.com/blog/more-throughput · https://tailscale.com/blog/making-tailscale-faster · https://www.wireguard.com/
- Media: https://ffmpeg.org/legal.html · https://docs.rs/crate/ffmpeg-next/9.0.0/source/README.md · https://www.phoronix.com/news/FFmpeg-9.0-Released · https://github.com/kixelated/mp4-atom · https://mediabunny.dev/guide/supported-formats-and-codecs · https://mediabunny.dev/guide/extensions/ac3 · https://zoriya.dev/blogs/transcoder · https://github.com/kaltura/nginx-vod-module · https://github.com/jellyfin/jellyfin/issues/11336 · https://github.com/mangoleaf/ferrofin · https://github.com/tursodatabase/turso
