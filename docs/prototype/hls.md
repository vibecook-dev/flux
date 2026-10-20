# flux-hls prototype — playback planner, index-driven remux, transcode

*Crate:* `crates/flux-hls` · *Spec:* research/03 §4 + §12, research/08 §10 (A, B, E, F) · *Started from:* `bench/playback-remux-probe/src/main.rs` (refactored, not rewritten)

> **Phase C** (browser validation, fallbacks, Opus/Vorbis/PCM, `plan_from_probe`, binary index, LRU caches, 2 s transcode pieces, I-frame playlists) is the next section; §1–§9 describe the first prototype and are kept as written, with superseded gaps marked in §8. Results of the first prototype are in §4 (validation), §5 (AVPlayer) and §6 (benchmarks vs Jellyfin). Those numbers were measured on the shared M1 Max while 7 other agents were compiling (load average 10–130); the lead should re-run `hls-bench` and `hls-validate` on a quiet machine.

## Phase C — real browsers, real files (2026-10-19)

*Branch `phase-c/hls`.* All numbers come from the **shared** M1 Max (another project's dev server and three other agents were running; the 1-minute load average is recorded with every result, here 3–15). Commands use `nice -n 19`, `-j 4`, `CARGO_TARGET_DIR=$FLUX_WORK/target/c-hls`.

**Summary**

- **Browsers verified.** New harness (`browser/run.mjs` + `player.html`) plays every file through hls.js 1.7.3 in the system Chrome 155 (headless, playwright-core) and in the stock Firefox 157 (WebDriver BiDi). It runs the web client's own hls.js settings. Results: **Chrome 51/52 and Firefox 52/52** over 52 files: 24 corpus files, 12 scratch files and 16 probe fixtures. There were 0 fatal hls.js errors, 0 `video.error`, and no MSE exceptions besides `QuotaExceeded` at 60 Mb/s. Every audio switch is correct in both browsers, and every WebVTT track shows its first cue at the source time + D, with 0 ms error. The one Chrome failure is `mkv_fonts.mkv`, whose playlist claims 4 s against 2 s of video (gap in C.11).
- **Seven browser findings** (C.1). Four are fixed in flux-hls: Firefox on macOS can't seek open-GOP HEVC; MP3 can't go in fMP4 for MSE; hls.js starts on the cheapest audio tier and can't switch back from FLAC/Opus to AAC; WebVTT was 83 ms early on transcodes. Three are for the web client or informational: hls.js `progressive` mode makes large segments slow; 60 Mb/s overflows the MSE quota; player time is source time + D.
- **Fallbacks instead of errors** (C.2): fragmented MP4 (indexed from `moof`/`trun`), QuickTime AC-3 without `dac3`, AVI (B-frame timing, audio seek), MP4/MOV PCM; Matroska/MP4 are never `NoIndex`.
- **Opus/Vorbis/PCM → AAC** from MKV/MP4 (C.3), sample-exact: seam ratio 0.99–1.03, the same as a continuous encode.
- **`plan_from_probe`** equals `HlsEngine::plan` on 144/144 corpus cases (C.4).
- **Binary index:** 25 % of the JSON size for a 2 h MP4, and 6.5× faster to decode (C.5).
- **LRU caches by bytes**, configurable (C.6).
- **Video transcode in 2 s pieces from warm encoder runs** (C.7):
  - time to first segment 0.99–1.66 s → 0.55–0.68 s;
  - median cold seek 1.96–3.66 s → 0.83–1.20 s;
  - AVPlayer first frame 0.69–0.97 s (was 1.24–1.60 s).
- **I-frame playlists** for copied video (C.8): verified with ffmpeg on 92 file × preset cases and with hls.js's I-frame player.
- **Gates** (C.9):
  - hls-validate: **123/123** (the old 72/72 plus 51 new cases);
  - AVPlayer: **22/22** (the 13 earlier cases + 9 new);
  - flux-hls tests: 54 unit + 16 integration;
  - workspace build, clippy, test and fmt: clean.

### C.1 Browser validation

**How.** `node crates/flux-hls/browser/run.mjs --browser chrome|firefox [--presets …] [--dirs …] [--files …]`. It installs nothing: it loads playwright-core and hls.js from `web/node_modules` by absolute path.

The harness:
- spawns `hls-serve` (`nice -n 19`) and serves `player.html`;
- gives the browser a profile under `tmp/hls/`;
- mutes audio (`--mute-audio` in Chrome, `media.volume_scale=0` in Firefox) but keeps the audio clock running.

Per case (file × preset), `window.fluxRun` measures:
- **Startup:** TTFF (`loadSource` → first `requestVideoFrameCallback`), and play past segment 0.
- **Five seeks:** forward, backwards, far forward, mid-segment, back near the start. Long files seek to 600/3600/5100/30/1234.5 s.
  - **Seek latency:** `seeked` and first presented frame.
  - **Accuracy:** `mediaTime − target`.
  - **Play-on:** whether playback continues for 1 s.
- **Audio:** every audio rendition switched to and back (`setAudioOption`).
- **Subtitles:** first WebVTT cue time vs the source cue (ffprobe) + D, and whether it becomes active.
- **I-frames:** the I-frame variant through `hls.createIFramePlayer()`.
- **Errors:** hls.js `ERROR` events, `video.error`, and MSE exceptions (`appendBuffer`/`remove` are wrapped; SourceBuffer `error` events).
- **Frames:** dropped/decoded (`getVideoPlaybackQuality`).
- **A/V drift:** `mediaTime` vs the clock-predicted time at `expectedDisplayTime`.
- **Codec support:** `isTypeSupported` for 14 codec strings.

Partial results survive a time-out.

**Final runs:** `bench/results/flux-hls-browser-{chrome,firefox}-20261019-043118.json`. Per-case tables: `python3 crates/flux-hls/browser/summarize.py <json>`.

**Run setup**

| | Chrome 155 (preset `chrome`) | Firefox 157 (preset `firefox_macos`) |
|---|---|---|
| cases ok | **51/52** | **52/52** |
| load (1 min) | 3.8 → 2.3 | 2.3 → 7.6 |

**Time to first frame** (from `prepare`, i.e. `POST /play`, to the first presented frame; median / max):

| plan | Chrome | Firefox |
|---|---|---|
| direct stream | 197 / 650 ms | 325 / 814 ms |
| audio conversion | 197 / 1 487 ms | 324 / 1 043 ms |
| video transcode | 288 / 434 ms | 639 / 1 306 ms |

**Seeks, sync and errors**

| | Chrome | Firefox |
|---|---|---|
| seeks to first frame (median / p95 / max) | 23 / 134 / 2 793 ms (258 seeks) | 24 / 244 / 1 863 ms (260 seeks) |
| audio rendition switches | 4/4 | 4/4 |
| WebVTT first cue vs source + D | 3/3 at 0 ms | 3/3 at 0 ms |
| I-frame player: right keyframe shown | 101/105 | 88/93 |
| rVFC drift p50 per file | −24…+6 ms | −12…−8 ms |
| dropped / decoded frames | 15 / 15 847 | 193 / 14 711 (192 in 60 Mb/s 4K HEVC) |
| hls.js fatal errors | 0 | 0 |
| hls.js non-fatal errors | `bufferSeekOverHole` 2, `bufferFullError` 2, `aborted` 1 | `bufferSeekOverHole` 5, `bufferFullError` 4, `bufferStalledError` 3, `aborted` 4 |

Notes:
- **Drift.** Firefox's `expectedDisplayTime` sits about half a frame later than Chrome's, which explains its negative drift.
- **Worst cases.** The 1.5 s and 2.8 s maxima are `hevc_4k_60mbps.mkv`, caused by hls.js `progressive` mode (finding 5).

**Codec support measured** (`MediaSource.isTypeSupported`; identical in Chrome 155 and Firefox 157 on this Mac):

| supported | not supported |
|---|---|
| avc1, hvc1/hev1 Main and Main10, av01 10-bit, **vp09 in MP4**, AAC, Opus in MP4, FLAC in MP4 | **MP3 in MP4** (`mp4a.40.34` and `mp4a.6B`), AC-3, E-AC-3, Dolby Vision (`dvh1`) |

Chrome 155 also answers `canPlayType('application/vnd.apple.mpegurl') = "maybe"`, so it has native HLS. The web client uses hls.js there, and that is what was tested.

**Findings** (evidence in the result JSONs; the full Firefox source quote is in `planner::Quirks`):

1. **Firefox on macOS can't seek open-GOP HEVC.**
   - **Cause.** Firefox's `MP4Demuxer` (macOS builds) treats only IDR pictures as HEVC keyframes: "VideoToolbox can return a bad data error if a CRA frame is the first sample after a seek". x265 files have one IDR, then CRAs.
   - **Symptoms.**
     - `synth_hevc10_opengop_10min.mkv`: every seek outside the buffer stalled forever. hls.js logged `bufferAppendNoProgress`, "appended without buffered range growth", for every fragment.
     - `hevc_main10_hdr10_hvc1.mp4`: seeks inside the buffer took 0.8–2.9 s, proportional to the target. Firefox decodes from the IDR at 0.
   - **Fix.**
     - The index now samples two keyframes per file (`IndexTrack::open_gop`; HEVC CRA/BLA, H.264 non-IDR I). It costs two small reads.
     - `planner::quirks(client)` keys on `web/firefox*/macos`, and `HlsEngine::plan` transcodes open-GOP HEVC for those clients, including direct play.
   - **Result:** every case passes in Firefox. On the 10 s-GOP file, now a transcode, seeks take 0.10–1.45 s (they were 2.6–5.3 s before the piece transcoder).
   - **H.264 open GOP** (non-IDR I frames, `h264_long_gop.mkv`) seeks fine in Firefox, so it is not quirked.
   - **Chrome** handles CRA after a seek, answering research open question 1 for Chrome.
   - **Not in the probe:** `plan_from_probe` can't see the GOP structure, so it doesn't apply this quirk.
2. **MP3 in fMP4 is rejected by both browsers' MSE**, although both play MP3 files.
   - `fmp4_audio` no longer includes MP3, so an MP3 track becomes AAC, with the reason `CodecNotPackageable{Mp3}`.
   - Tested with `h264_mp3.mkv` (seam 0.99).
3. **hls.js starts on the lowest-BANDWIDTH codec tier.**
   - **Start tier.** With one `EXT-X-STREAM-INF` per audio codec group, the "always offer stereo AAC" fallback put the AAC transcode in its own, cheaper tier. hls.js played that instead of the FLAC copy (`h264_flac.mkv`), which also cost a server transcode.
   - **Switching bug.** Switching from a FLAC (or Opus) SourceBuffer back to AAC fails in hls.js 1.7.3:
     - it misses the `changeType` (its `pickMostCompleteCodecName` knows `fLaC`/`Opus` but Chrome's SourceBuffer codec is `flac`/`opus`);
     - the AAC append then errors: `bufferAppendingError` → `mediaSourceRequiresReset` → stall.
   - **Fix.** The stereo-AAC fallback (Apple rule 2.3) is now added only for **Dolby or >2-channel** copies. A stereo or mono FLAC/Opus/MP3/ALAC copy stands alone.
   - **Still affected:** cross-codec switching between *different* source tracks (Opus eng + AC-3 jpn).
     - The harness reports it as an hls.js warning.
     - The flux web client restarts the session on an audio change, so it is not affected.
4. **WebVTT was 83 ms early on transcoded video in hls.js.**
   - hls.js maps `X-TIMESTAMP-MAP` against the first *video decode time*:
     - a copy starts decoding at 0 (D is in the composition offsets);
     - the transcode put D in `tfdt`.
   - **Fix.** Transcoded pieces now carry D as a constant composition offset, with `tfdt` from 0. Cues are within 0 ms in both browsers.
5. **The web client's hls.js `progressive: true` is slow for large segments.**
   - The test file was `hevc_4k_60mbps.mkv` (41 MB segments) in Chrome.
   - TTFF was 1 722 ms with it vs 587 ms with `progressive: false`; seeks 1.0–3.1 s vs 0.09–0.28 s (same server, load 12–15).
   - flux produces each segment before sending it, so progressive parsing only adds work.
   - **Proposed web change:** `progressive: false` (see `proposed_core_changes`).
6. **60 Mb/s content overflows MSE.**
   - At 60 Mb/s with the web client's 30 s forward buffer, Chrome's ~150 MB video SourceBuffer quota throws `QuotaExceededError`. hls.js reports `bufferFullError` (non-fatal), shrinks its buffer and plays on; the harness counts it as a warning.
   - Byte-capped segments would help (gap); so would a `maxBufferSize`-led config in the web client.
7. **Player time = source time + D** (D = 0–83 ms in the corpus) for copy and transcode alike.
   - Subtitles and audio share the shift.
   - Exact seeks land on the last frame ≤ target (`frameErrorMs` −42…0).
   - hls.js nudges the initial 83 ms gap (`bufferSeekOverHole`, non-fatal).
   - Frame-exact markers from the catalog (intro skip) need +D. `PlayResponse` could carry it (proposed).

**Not run:**
- **Safari:** `safaridriver` answers "You must enable 'Allow remote automation'", and system settings were not changed. AVPlayer (C.9) covers Apple's native HLS.
- **TVs.**

**One intermittent failure:** a `fragParsingError` ("Failed to find demuxer by probing fragment data") on audio fragment 0, at 65 ms after load, in 1 of 3 Chrome runs of `h264_pcm.mkv`. It was not reproduced in 4 later runs. It looks like an hls.js `startFragPrefetch` race.

### C.2 Fallbacks instead of errors (integration.md §4.2 #2–#3)

Every file below plays (hls-validate, Chrome, Firefox, AVPlayer). Scratch copies live in `tmp/hls/media/`; the probe fixtures are in `media/probe-fixtures/`.

| source | before | now |
|---|---|---|
| Fragmented MP4: `frag_h264_aac.mp4` (60 s, B-frames), `frag_hevc_hdr10.mp4`, fixture `fragmented.mp4` | "no keyframe index" | `IndexMethod::Fragments`: every `moof`/`traf`/`trun` (tfhd/trex defaults, tfdt gaps folded into durations) becomes ordinary sample tables. One 64 KB read per fragment (2.3–16 ms). Remuxed: 1439/1439 and 719/719 frames. |
| QuickTime AC-3: `qt_h264_ac3.mov` (60 s), fixture `quicktime_ac3.mov` | "moov: missing box: dac3" | The `dac3` sits inside a QuickTime `wave` box, which mp4-atom does not read. Entries without a *direct* `dac3`/`dec3` are renamed before decoding, and the config is synthesized from the first sync frame, as for MKV. AC-3 copy (Apple TV) or AAC, seam 1.00. |
| AVI: `avi_h264_ac3.avi` (H.264 with B-frames + AC-3), `avi_h264_nob_ac3.avi`, fixtures `h264_ac3.avi`, `mjpeg_pcm.avi` | "no keyframe index" (AVI packets have no pts) | Keyframe times come from `dts + has_b_frames × frame` (ffmpeg's decoder timing): 1439/1439 frames, where `dts` alone gave 1441. Audio seeks the demuxer 3 s early and cuts on the output side: AVI's index landed *after* an input `-ss` and lost 1.5 s of AC-3. MJPEG is now known (`codec_of`) and transcoded. |
| MP4/MOV PCM (`sowt`/`twos`/`lpcm`/`in24`/`fl32`…): `h264_pcm.mov` | codec `Other`: the track was dropped (not transcodable) | Indexed. Tables are expanded **per chunk**, not per sample: PCM has 2.9 M one-frame samples a minute. Transcoded to AAC. |
| Cue-less MKV planned from the catalog probe | `NoIndex` → transcode | The planner never says `NoIndex` for Matroska/MP4: it plans direct stream, since the index uses ClusterScan. |

**Not done:** AVI *copy*. H.264-in-AVI is Annex B, needs PTS from POC when it has B-frames, and AVI audio chunks don't align with frames. AVI is always transcoded; the planner keeps `ContainerUnsupported`/`NoIndex` for it.

### C.3 Opus, Vorbis, PCM → AAC/E-AC-3 from MKV/MP4

**Before:** the transcoder piped elementary streams, so these tracks were dropped from plans. AV1+Opus on the legacy preset played silent.

**How each codec is fed now** (`codec/wrap.rs`):
- **Opus and Vorbis** go into a minimal Matroska stream on ffmpeg's stdin: EBML header, one track, one SimpleBlock per packet.
  - **Opus:** `OpusHead` is sent with pre-skip 0, so the decoder outputs every sample of the first packet fed. flux trims the pre-roll itself.
  - **Vorbis:** packet lengths come from the setup header's mode block flags, read backwards from the framing bit, as FFmpeg and liboggz do. The first packet decodes to nothing, which is accounted for.
- **PCM** goes raw: `-f s16le|s24le|f32le|u8… -ar -ac`.

**Placement:** filters renumber samples (`asetpts=N/SR/TB`) *before* `atrim`, so container timestamps never matter. The averaged sample position stays sample-exact.

**Results** (hls-validate):

| file | seam ratio | packet counts | A/V start |
|---|---|---|---|
| `h264_opus.mkv` / `.mp4` | 1.03 / 1.01 | exact | ≤ 0.5 ms |
| `h264_vorbis.mkv` | 1.03 | exact | ≤ 0.5 ms |
| `h264_pcm.mkv` / `.mov` | 1.02 | exact | ≤ 0.5 ms |
| `h264_mp3.mkv` | 0.99 | exact | ≤ 0.5 ms |
| `av1_opus.mkv` (legacy) | 1.00 | exact | ≤ 0.5 ms |

A continuous encode scores ≈ 1.0.

### C.4 Planner

- **`planner::plan_from_probe(&ProbeResult, &ClientCaps, &PlanPrefs)`**: `plan` plus the engine's limits as a pure function of container and codec. `HlsEngine::finalize_plan` shares the logic (`finalize_with`).
  - **What it models:**
    - TS/M2TS/AVI audio copies → AAC;
    - transcodes only of codecs the transcoder reads;
    - duplicates collapse.
  - **Test:** `plan_from_probe_equals_engine_plan_on_corpus` asserts 144/144 equal (24 files × 6 presets). The raw probe plan differed in 23/144 before.
- **`flux_probe::ffprobe::codec_of`** is now the ffprobe codec table (plus `mpeg1video` and `msmpeg4*`). flux-hls depends on flux-probe.
- **Never `NoIndex`** for Matroska/WebM/MP4/MOV.
- **New presets:** `presets::firefox_macos()` (`by_name("firefox_macos")`), `planner::Quirks` / `quirks(client)`, and `plan_with_index_facts`.

### C.5 Compact binary `PlaybackIndex`

**Format** (`to_bytes` / `from_bytes`, `index/binary.rs`):
- `FXIX` magic, a format byte and `INDEX_VERSION`;
- a JSON head holding the index with its big arrays emptied, so struct changes stay additive;
- delta + zigzag LEB128 varints for keyframes and MP4 sample tables.

`INDEX_VERSION` went 1 → 2 (`open_gop`, the `Fragments` method), so cached v1 indexes are rebuilt.

**Results** (`cargo run --release -p flux-hls --example index_codec`; `bench/results/flux-hls-index-encoding.json`; load 3.4; median of 9 decodes):

| file | samples | JSON | binary | ratio | decode JSON → binary |
|---|---|---|---|---|---|
| long_2h_faststart.mp4 | 510 128 | 6.81 MB | 1.71 MB | 25 % | 24.1 → 3.7 ms |
| long_2h_moov_end.mp4 | 510 128 | 6.80 MB | 1.71 MB | 25 % | 26.7 → 3.7 ms |
| long_2h_360p.mkv (Cues) | — | 244 KB | 45 KB | 18 % | 0.37 → 0.06 ms |
| h264_aac_faststart.mp4 | 4 253 | 55 KB | 19 KB | 34 % | 0.18 → 0.04 ms |

### C.6 Bounded caches

All engine caches are byte-budgeted LRUs (`cache::LruCache`):

| cache | budget (`EngineConfig`) | default |
|---|---|---|
| prepared assets (index + expanded MP4 tables + segment plan) | `asset_cache_bytes` | 256 MB |
| WebVTT tracks | `subtitle_cache_bytes` | 32 MB |
| transcoded audio segments | `audio_cache_bytes` | 256 MB |
| transcoded video segments | `video_cache_bytes` | 512 MB |

- **Weighing:** in-flight entries weigh nothing until built. An asset still in use survives eviction through its `Arc`.
- **Failed `prepare`s** are no longer cached.
- **Observability:** `HlsEngine::cache_usage()` reports entries, bytes and evictions, and hls-serve's `/_stats` shows it. flux-server's own asset LRU can now go (its TODO).

### C.7 Video transcode: 2 s pieces, warm encoder runs

**Design.**
- **Pieces.** The transcoded rendition has its own segmentation. Each source keyframe span is cut into ~2 s pieces on the frame grid (`video_segment_target`).
- **Runs.** One ffmpeg/VideoToolbox run encodes 4 consecutive pieces (`video_run_segments`):
  - `-ss T_k -noaccurate_seek`, `trim` to the piece, `setpts`;
  - an IDR forced at every boundary;
  - exact `-frames:v`.
- **Streaming.** The run's fragmented MP4 is parsed *as it streams*. Each piece is published the moment its frames are complete, so a request waits for its own piece only, and the VT set-up (~0.35 s measured) is paid once per run.
- **Startup.** `warm()` starts the run at the play position, and init waits for that run's `avcC`.
- **Prefetch.** 3 pieces ahead start the next run.
- **API.** `HlsEngine::segment_count` and `HlsEngine::segment_at(asset, plan, rendition, t)` give per-rendition numbering: `v/{n}.m4s` of a transcode is piece n, not remux segment n.

**Correctness** (hls-validate, decoded with ffmpeg):
- frame counts exact on every transcode: 719/719, 899/899, 1439/1439, and **14 386/14 386** for the 10-minute open-GOP file with 10 s GOPs;
- no ffmpeg errors.

**Speed.**
- **Command:** `hls-bench --preset legacy --runs 3 --no-sweep --files <one file> --seeks …`.
- **Isolation:** a fresh server per run *and per file*, so prefetch runs of other files don't compete.
- **Results:** `bench/results/flux-hls-vtranscode-ttff.json`.
- **Load:** 3.2–6.4.
- **Before** = Phase B binary (6 s segments, one ffmpeg per segment); **after** = this branch.

| file | time to first segment, before → after | cold seek (median, range), before → after |
|---|---|---|
| av1_opus.mkv (AV1 software decode → H.264) | 0.995 → **0.546 s** | 1.96 s (0.88–2.00) → **0.87 s** (0.52–1.07) |
| hevc_main10_hdr10_eac3.mkv (4K HDR10 → 1080p SDR) | 1.146 → **0.675 s** | 2.31 s (1.13–2.33) → **0.94 s** (0.83–1.02) |
| mpeg2_ac3.ts (ffprobe index, MPEG-2) | 1.140 → **0.585 s** | 2.34 s (1.00–2.38) → **0.83 s** (0.58–1.09) |
| synth_hevc10_opengop_10min.mkv (10 s GOP) | 1.664 → **0.615 s** | 3.66 s (2.07–3.71) → **1.20 s** (1.18–1.25) |

How to read it:
- **Time to first segment** runs from the master playlist to video segment 0 (init plus the first piece).
- **Cold seek** is a segment that was never prefetched.
- **10 s GOP file:** its seeks still decode up to one GOP before the piece.
- **AVPlayer** (C.9), first frame advancing:
  - AV1 0.69 s (was 1.24 s);
  - HDR→SDR 0.84 s (was 1.39 s);
  - MPEG-2 TS 0.97 s (was 1.60 s);
  - exact seeks 0.86–1.47 s (was 1.30–2.36 s).

### C.8 I-frame playlists (trick play)

- **What is served.** For copied video, the master playlist lists `EXT-X-I-FRAME-STREAM-INF` → `v/iframes.m3u8` (`EXT-X-I-FRAMES-ONLY`, one segment per source keyframe). `v/kf/{n}.m4s` holds keyframe n alone, on the copied rendition's timeline and sharing `v/init.mp4`.
  - It is free: the index already lists every keyframe, and no transcode is involved.
  - New `HlsResource::IFramePlaylist` / `IFrame(n)`.
- **HEVC fix.** CRA keyframes are rewritten to **BLA_W_LP**, one NAL header byte with the same slice-header syntax, so each I-frame starts its own coded video sequence.
  - **Problem:** played back to back, CRAs 10 s apart derived their POC from each other across an lsb wrap, and ffmpeg output every pair swapped.
  - **Rejected alternative:** an end-of-sequence NAL instead made ffmpeg decode one frame of 60.
- **Verification.**
  - **hls-validate** decodes the I-frame playlist: 92 file × preset cases, 13 395 keyframes, every one decoded and in order.
  - **Chrome** with hls.js's `createIFramePlayer()` shows the keyframe at or before each target in 26–53 ms.

### C.9 Gates

- **hls-validate:** **123/123**. Command: `hls-validate --presets chrome,appletv,legacy` over the 24 corpus files, the 12 scratch files and 5 probe fixtures. *(Result file lost when `flux-hls-validate.json` was overwritten; see D.2. Superseded by 183/183 and 213/213.)*
  - **Output:** `bench/results/flux-hls-validate.json`; load 4.3–6.8; 12 min.
  - **Seam ratio** 0.39–1.04 on 51 transcoded renditions with seams.
  - **Global max/p999** 1.01–1.5. The 1.2–1.5 values are at stream start/end in the AVI files, located by decoding, not seams.
  - **Also checked:** I-frame playlists as above.
- **AVPlayer:** **22/22** with an empty `errorLog`; load 5.6. Remux: first frame 0.25–0.55 s, exact seeks 50–92 ms. Transcode: first frame 0.69–1.02 s, seeks 0.86–1.47 s. Command: `node crates/flux-hls/browser/avplayer.mjs`; output `bench/results/flux-hls-avplayer.json`. It covers the 13 earlier cases plus fragmented MP4 (H.264 and HEVC HDR10), QuickTime AC-3, AVI with B-frames, and Opus/Vorbis/PCM (MKV, MOV)/MP3 → AAC.
- **Remux speed:** `hls-bench --runs 3 --preset chrome` on the five Jellyfin-harness files (load about 5; `bench/results/flux-hls-playback-20261019-115615.json`). Unchanged from the pre-Phase-C binary run in the same session:
  - **TTFS:** video 0.8–11.4 ms (before 0.7–14.1); with audio 1.2–117 ms.
  - **Cold seeks:** 0.3–13 ms video, 0.2–85 ms audio.
  - **CPU:** 0.77 ms per MB served (before 0.86).
  - **Peak RSS:** 75 MB (before 71).
- **Tests:** `cargo test -p flux-hls --release`: 54 unit + 16 integration tests. The integration tests cover plan agreement, fallback indexing, binary round trips, I-frames and pieces.
- **Workspace:** `cargo build --workspace --all-targets`; `cargo clippy --workspace --all-targets` with 0 warnings, and `-p flux-hls --no-default-features` clean; `cargo test --workspace`: **366 passed, 0 failed, 4 ignored** (354 before); `cargo fmt --all --check` clean.

### C.10 Public API (additive; flux-server compiles unchanged)

**New items:**
- `plan_from_probe`, `presets::firefox_macos`, `planner::{Quirks, quirks, plan_with_index_facts}`;
- `PlaybackIndex::{to_bytes, from_bytes}`;
- `HlsEngine::{segment_at, segment_count, cache_usage}` and `CacheUsage`;
- `EngineConfig::{asset_cache_bytes, subtitle_cache_bytes, video_segment_target, video_run_segments}`;
- `HlsResource::{IFramePlaylist, IFrame}`;
- `vtranscode::{Piece, pieces, first_piece_of}`;
- `IndexMethod::Fragments`, `IndexTrack::open_gop`;
- `VideoTranscodeStats::{runs, first_piece_ms}`.

**Breaking only for exhaustive matches** (flux-server uses wildcards): the new enum variants.

**Behaviour changes:**
- `video_lookahead` now counts pieces (default 3);
- transcoded video segment numbers are pieces;
- `INDEX_VERSION` = 2.

**Needed in flux-server** (listed in the report's `proposed_core_changes`):
- `segment_at` for the video prefetch URL;
- `plan_from_probe` instead of direct-play-or-fail;
- binary index BLOBs;
- `prepare` instead of the private LRU.

### C.11 Known gaps after Phase C

- **AVI is never copied.** Even H.264 without B-frames is transcoded (C.2).
- **HEVC on Firefox/macOS:** open-GOP HEVC is transcoded. A cheaper option needs a client that accepts CRA, or BLA-rewritten segment starts with RASL dropping, which would change continuous playback.
- **hls.js cross-codec audio switching** (FLAC/Opus → AAC) is an hls.js 1.7.3 bug; not worked around beyond the planner rule.
- **MSE quota at 60 Mb/s** (warning only); no byte-capped segments yet.
- **Not tested:** Safari and TVs. Firefox was tested on macOS only.
- **The first transcoded segment is 2 s,** but each *run* still starts at a source keyframe. A seek into a 10 s GOP decodes up to 10 s before its piece (the 1.2 s seeks on the 10 s GOP file).
- **Intermittent hls.js `fragParsingError`** (C.1), once.
- **The last segment's EXTINF uses the container duration.** When a subtitle or audio track outlasts the video, the playlist overstates the video by the difference (`mkv_fonts.mkv`: 4 s vs 2 s of video), and hls.js ends at the video's end. MP4 knows the exact end; Matroska would need a walk of the last GOP.
- **hls.js's I-frame player** (an experimental API in 1.7) sometimes doesn't present the first `loadMediaAt` target, or re-presents the previous frame. The I-frame *stream* itself is verified frame by frame with ffmpeg (C.8).
- **Benchmarks** were taken on a shared machine with a warm page cache.

### C.12 Re-running

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-hls TMPDIR=$FLUX_WORK/tmp/hls/tmp
nice -n 19 cargo build --release -p flux-hls --bins --examples -j 4
R=$CARGO_TARGET_DIR/release W=$FLUX_WORK
# browsers (results: $W/bench/results/flux-hls-browser-<browser>-<ts>.json)
nice -n 19 node crates/flux-hls/browser/run.mjs --browser chrome --dirs $W/media/features,$W/media/playback-research,$W/tmp/hls/media,$W/media/probe-fixtures
nice -n 19 node crates/flux-hls/browser/run.mjs --browser firefox --presets firefox_macos --port 18094 --dirs …
python3 crates/flux-hls/browser/summarize.py $W/bench/results/flux-hls-browser-chrome-*.json
# AVPlayer, validation, transcode TTFF, index encoding
nice -n 19 node crates/flux-hls/browser/avplayer.mjs
nice -n 19 $R/hls-validate --presets chrome,appletv,legacy FILE...
nice -n 19 $R/hls-bench --serve-bin $R/hls-serve --runs 3 --preset legacy --no-sweep --files av1_opus.mkv,hevc_main10_hdr10_eac3.mkv,mpeg2_ac3.ts --seeks 20,10,25
nice -n 19 $R/examples/index_codec
```

Scratch test files (`tmp/hls/media/`, made from the read-only corpus):

```sh
S=$W/media/features/h264_multi_audio_subs.mkv; FF="nice -n 19 ffmpeg -hide_banner -loglevel error -y"
$FF -i $W/media/features/h264_aac_faststart.mp4 -c copy -movflags +frag_keyframe+empty_moov+default_base_moof frag_h264_aac.mp4
$FF -i $W/media/features/hevc_main10_hdr10_hvc1.mp4 -c copy -tag:v hvc1 -movflags +frag_keyframe+empty_moov+default_base_moof frag_hevc_hdr10.mp4
$FF -i $S -map 0:v -map 0:2 -c copy -f mov qt_h264_ac3.mov
$FF -i $S -map 0:v -map 0:2 -c copy -bsf:v h264_mp4toannexb avi_h264_ac3.avi
$FF -i $S -map 0:v -map 0:1 -c:v copy -c:a libopus -b:a 128k -ac 2 h264_opus.mkv   # also .mp4
$FF -i $S -map 0:v -map 0:1 -c:v copy -c:a vorbis -strict -2 -ac 2 h264_vorbis.mkv
$FF -i $S -map 0:v -map 0:1 -c:v copy -c:a pcm_s16le -ac 2 h264_pcm.mkv          # also -f mov h264_pcm.mov
$FF -i $S -map 0:v -map 0:1 -c:v copy -c:a libmp3lame -b:a 192k -ac 2 h264_mp3.mkv
$FF -i $W/media/features/h264_flac.mkv -c copy h264_flac.mp4
# avi_h264_nob_ac3.avi: 30 s x264 -bf 0 re-encode of h264_aac_faststart.mp4 + the AC-3 track, Annex B
```

## 1. What was built

A library plus three tools:

| Piece | Module | What it does |
|---|---|---|
| Playback index | `index::{mkv, mp4, external}` | Matroska: EBML header → SeekHead → Info/Tracks/Cues, plus a headers-only walk of the first cluster(s) for first timestamps, HDR SEI, AC-3 headers (2–9 ms, 7–16 reads). Cue-less MKV → `ClusterScan` with 8 MB sequential reads. MP4/MOV: `moov` sample tables via `mp4-atom` 0.16 (front or back `moov`). TS/M2TS/AVI: `ffprobe` packet scan (`FfprobeScan`, ffmpeg paths only). Serializable (`serde`) for the catalog. |
| Segment plan | `segment` | Cut at the first keyframe ≥ 6 s after the segment start; exact durations in the video timescale (frame-grid snapped); `TARGETDURATION` = max rounded EXTINF. |
| fMP4/CMAF writer | `fmp4`, `body` | Init `ftyp(iso6,cmfc)+moov(mvhd,trak,mvex/trex)`; media `styp+moof(mfhd,traf/tfhd default-base-is-moof,tfdt v1,trun v1)+mdat`. Sample entries avc1/avc3, hvc1/hev1, dvh1/dvhe, av01; mp4a+esds, ac-3+dac3, ec-3+dec3 (synthesized from the first sync frame), Opus+dOps, fLaC+dfLa; colr nclx, mdcv, clli, dvcC/dvvC pass-through. The body is header bytes + `(file offset, len)` ranges streamed with positioned reads. |
| Playlists | `playlist` | Multivariant (audio groups per codec with GROUP-ID/LANGUAGE/NAME/DEFAULT/AUTOSELECT/CHANNELS, WebVTT subtitle group, CODECS, RESOLUTION, FRAME-RATE, VIDEO-RANGE, BANDWIDTH/AVERAGE-BANDWIDTH), media playlists (VOD, INDEPENDENT-SEGMENTS, EXT-X-MAP, exact EXTINF), single-segment WebVTT playlists. URL router `HlsResource::parse`. |
| Planner | `planner` | `plan(&ProbeResult, &ClientCaps, &PlanPrefs) -> PlaybackPlan` with `Reason`s in the flux-core decision order; presets `chrome_desktop`, `safari_macos`, `firefox`, `apple_tv_4k`, `android_tv` (+ `legacy_h264`). |
| Engine | `engine` | `HlsEngine` (Tokio): `prepare`, `plan`, `master_playlist`, `media_playlist`, `init_segment`, `media_segment`, `webvtt`, `serve(HlsResource)`, `warm`. Blocking reads on the blocking pool behind a read-admission semaphore. |
| Audio transcode | `transcode` | Stateless per-segment ffmpeg encodes (AAC stereo, E-AC-3 5.1) sample-aligned to one global frame grid; two parallel parts per on-demand segment; cache + prefetch; `kill_on_drop`, timeout, cancellation, bounded concurrency. |
| Video transcode | `vtranscode` | ffmpeg + VideoToolbox H.264, keyframe-aligned (`-ss T_k -noaccurate_seek`, exact `-frames:v`), HDR→SDR via `scale_vt`, recipe-addressed cache + prefetch. |
| Subtitles | `subtitle` | Matroska SRT/ASS/SSA/WebVTT → WebVTT with `X-TIMESTAMP-MAP` (one pass over the clusters, cached). |
| Tools | `bin/hls_validate`, `bin/hls_serve`, `bin/hls_bench` | Corpus validation with ffmpeg/ffprobe, dev server, loopback benchmark comparable to the Jellyfin harness. |

~10.5 k lines of library code, ~1.4 k lines of tools, 55 tests (43 unit + 12 against the real corpus).

**Headline (load ≈ 10, warm page cache, loopback):** time to first segment 0.8–12.5 ms (Jellyfin 191–913 ms in its loaded 2026-10-18 baseline; 176–617 ms in benchmarks.md's 2026-10-20 head-to-head), 1.2–119 ms including a demuxed audio rendition that needs a TrueHD/E-AC-3 transcode; cold seeks 0.2–14 ms (Jellyfin 318–840 ms then; 459–762 ms far into a 2 h file in benchmarks.md), 86 ms when the audio needs a transcode; 1.1 ms p50 / 2.1 ms p99 server time per remuxed segment (pooled, and mostly the 2 h 360p file's small segments; the 4K 60 Mb/s file's 41 MB segments took 90 ms p50, §6); 0.98 ms CPU per MB served; 69 MB peak RSS (Jellyfin ≈ 370 MB in its loaded 2026-10-18 run; 388 MiB median after benchmarks.md's 7-file session). Whole corpus: 72/72 file×client cases decode with zero ffmpeg errors and exact packet counts (one AVI transcode fills a source frame gap); AVPlayer plays 13/13 checked variants with exact seeks in 44–91 ms (remux).

## 2. Public API (what `flux-server` calls)

```rust
use flux_hls::{EngineConfig, HlsEngine, HlsResource, PlanPrefs, presets, Rendition, AudioKey};

let engine = HlsEngine::new(EngineConfig {               // defaults: 6 s target, ffmpeg/ffprobe in /opt/homebrew/bin,
    index_cache_dir: Some("/…/flux/index".into()),      // 4 audio + 2 video encodes, 256 MB audio / 512 MB video cache
    ..Default::default()
});
let asset: Arc<Asset> = engine.prepare(&path).await?;    // build or load the cached PlaybackIndex + SegmentPlan
// or: engine.prepare_with_index(index_from_catalog).await?  (PlaybackIndex is serde; to_json/from_json)
let plan: PlaybackPlan = engine.plan(&asset, &client_caps, &PlanPrefs::default());   // or planner::plan(&probe, …)
engine.warm(&asset, &plan, 0);                            // one-round-trip play: start segment 0 of every rendition

// GET /media/{plan_id}/{*rest}
let res = HlsResource::parse(rest).ok_or(NotFound)?;     // master.m3u8, v/…, a/{key}/…, s/{track}.m3u8|.vtt
let r: HlsResponse = engine.serve(&asset, &plan, res).await?;
// r.content_type, r.cache_control ("immutable" for init/segments), r.body: SegmentBody
// r.body.len() → Content-Length; r.body.into_stream(1 << 20) → Stream<Item = io::Result<Bytes>>
```

Lower level: `engine.master_playlist / media_playlist / init_segment / media_segment / webvtt`, `Asset::{video_segment, audio_copy_segment, audio_frames}` (sync), `PlaybackIndex::{build, build_with_fallback, to_probe_result}`, `planner::{plan, video_problems, audio_problem, presets::*}`, `HlsError` (`thiserror`: `NotFound` → 404, `Unsupported` → 501, others → 500).

Every init/media segment is a pure function of `(asset, plan recipe, rendition, n)`: no session state, any segment can be requested cold and in any order (the integration tests request segments in reverse). Audio rendition keys are `{track}` (copy) or `{track}-aac2` / `{track}-eac36` (transcode), because one source track can be offered both ways.

## 3. Design decisions and the evidence for them

### 3.1 Index

- **Matroska**: one 64 KB read covers EBML header, SeekHead, Info and Tracks for ffmpeg/mkvmerge files; Cues are one more read via the SeekHead; a ~1 MB read of the first cluster yields the first timestamp of every track, the first keyframe (HDR SEI, in-band parameter-set detection) and the first AC-3/E-AC-3 sync frame (for `dac3`/`dec3`). 7–11 reads per file in the corpus.
- **Exact timing**: the video timescale comes from `DefaultDuration` (41 708 333 ns → 24000/1001, ticks 1001); PTS are snapped to that grid, so ms-rounded Matroska timestamps (0, 42, 83, 125…) become 0, 1001, 2002… exactly. Matroska `CodecDelay` (ffmpeg writes it for AAC/Opus/E-AC-3 priming) is subtracted from block times exactly like ffmpeg's demuxer.
- **Audio grid origin**: ffmpeg's Matroska muxer *truncates* timestamps to ms (found while validating: averaging `ts − n·frame` was biased by −0.33 ms). The origin is the first frame's time when it is consistent with the average of the first 24 frames.
- **Reorder depth** from the SPS (`max_num_reorder_frames` in H.264 VUI, `sps_max_num_reorder_pics` in HEVC); measured on the first frames (+1 margin) only when the SPS lacks it.
- **Colour**: Matroska `Colour` and SPS VUI are merged field by field — ffmpeg wrote a `Colour` with only the matrix for the HDR10 clip, which hid BT.2020/PQ until the merge (this was the cause of `VIDEO-RANGE=SDR` on an HDR10 file). Mastering display / CLL come from `MasteringMetadata`, else declarative SEI in `hvcC`, else the first keyframe's SEI (parse, don't decode).
- **MP4**: `moov` (front or back) decoded with `mp4-atom` 0.16; stts/ctts/stsz/stsc/stco/co64/stss/elst stored compactly and expanded in memory. Coarse timescales (16000 Hz with alternating 656/672 deltas) get their frame rate from the average, snapped to standard rationals — without `FRAME-RATE`, AVPlayer rejected the 4K HEVC variant (`CoreMediaErrorDomain -1002`, bisected).
- **No Cues**: `ClusterScan` walks every cluster with 8 MB sequential, non-retained reads.
- **TS/M2TS/AVI**: `ffprobe` packet scan; these are never remuxed in-process (`has_index = false` → planner picks the ffmpeg paths).

### 3.2 Segments and timestamps

- Segment k = video blocks in file (decode) order from the keyframe with PTS T_k up to the keyframe with PTS T_{k+1}; audio frames whose *presentation* time is in [T_k, T_{k+1}) (reading slightly past the next keyframe because muxers interleave by decode time). Packet counts match the source exactly for every file in the corpus.
- **DTS = sorted PTS** per segment (monotonic and continuous across segments because each segment's PTS set is a contiguous range, open GOP included); **CTS = PTS + D − DTS** with D = reorder depth × frame duration (also ≥ −first PTS of any track, e.g. AAC priming). Every track's presentation is shifted by the same D, so A/V sync is preserved and CTS ≥ 0 everywhere: `hls-validate` reports `cts_negative = 0` and a video start shift of 0.000 ms against `start = D` for every file — the 67 ms ffmpeg presentation shift seen with negative CTS in the probe is gone.
- **Copied audio placement**: constant-size frames are placed on the track's frame grid. Concatenated files shift the grid phase at joins (−3 ms then −14 ms in the corpus's `show_e0*` episodes; with a single global grid one segment boundary got a duplicate timestamp). `grid_positions` splits frames into regions at phase jumps and places each region on its own phase, quantized to 0.5 ms so independently produced segments agree; normal files land exactly on the global grid.
- **Large segments** (> 16 MB, e.g. 4K remux): headers-only walk with 128 KB non-retained windows, payloads streamed later as file ranges (≈1.2× reads instead of 2×, bounded memory); small segments are read once and payloads are zero-copy `Bytes` slices of the parse buffer.
- In-band VPS/SPS/PPS (detected on the first keyframe) are stripped from keyframes so `hvc1`/`avc1` stay valid; Dolby Vision P5 → `dvh1`, P8 → `hvc1` + record, P7 → HDR10 base layer (record dropped).

### 3.3 Audio transcode: per-segment, stateless, sample-aligned (chosen with evidence)

Two designs were on the table: per-segment stateless encodes with pre-roll/trim (research/03 §5), or contiguous runs ahead of the playhead (research/08 F). Per-segment was chosen because it keeps the remux path's property that a seek is a single GET with no worker to restart, **provided the seams are inaudible**. The naive version is not: ffmpeg's `-ss` on a Matroska source cuts at ms-rounded timestamps, and TrueHD access units (0.83 ms) are finer than that rounding, so consecutive encodes would be misaligned by up to ~24 samples (a click). The implemented design:

1. One global output frame grid per track: origin a0 = the track's first sample, step F (AAC 1024, E-AC-3 1536). Segment k owns grid frames [g_k, g_{k+1}), g_k = first frame starting at/after T_k.
2. flux demuxes the source frames itself and pipes the **elementary stream** to ffmpeg (`-f truehd|dts|ac3|eac3|flac|mp3|aac(ADTS)`), so ffmpeg decodes sample-exactly from the first frame fed; the first frame's global sample position is the average of `pts·rate − samples_before` over every fed frame (cancels the 1 ms rounding). TrueHD feeding starts at a major sync.
3. `atrim=start_sample` makes the encoder input start at grid position E + d, where d is the encoder delay — **measured with an impulse**: native `aac` 1024, `aac_at` 2112, `eac3`/`ac3` 256 samples. Output frame j then covers exactly [E + jF, E + (j+1)F) in every encode; 3 frames of pre-roll warm the encoder, 2 of post-roll give the last kept frame real look-ahead.
4. Kept frames are packaged with tfdt = a0 + g_k·F + D. Init segments are synthesized from the recipe (no ffmpeg run).
5. Because every encode sits on the same grid, an on-demand segment is split at a grid frame into **two parts encoded in parallel** (decode dominates: TrueHD ≈ 20× real time per core under load); prefetch of the next segments uses one part. Results are cached by (asset, track, recipe, n); in-flight requests are de-duplicated.
6. Index-less sources (TS/AVI): ffmpeg demuxes the file directly with an accurate `-ss` to grid position E + d (90 kHz TS timestamps are sub-sample precise).

**Seam evidence** (`hls-validate`, decoded with ffmpeg): the peak second difference within ±64 samples of each segment seam divided by the 99.9th percentile elsewhere is 0.39–1.04 for every transcoded rendition; a continuous single-pass ffmpeg encode of the same track scores ≈1.0. Sensitivity check: slipping the stream by 1 sample at one seam raises the metric to 2.0–3.7, by 8 samples to 10–23. The global max/p999 (which also covers the in-segment part joins) is 1.01–1.03 vs 1.00–1.01 for the continuous encode. A/V start offset vs source: 0.00–0.33 ms (copy and transcode).

### 3.4 Video transcode

ffmpeg + VideoToolbox H.264 High@4.1, one IDR per segment, no B-frames:
`-hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld -ss T_k -noaccurate_seek -i src -frames:v N -fps_mode passthrough -vf trim=start=0,scale_vt=w:h[:color_matrix=bt709:color_primaries=bt709:color_transfer=bt709] … -f mp4 -movflags +frag_keyframe+empty_moov+default_base_moof pipe:1`.
Segment boundaries are the remux plan's source keyframes, so restarts never decode-and-discard. Two pitfalls found by validation: the default CFR vsync padded the open-ended last segment by one frame (720/719), and `-fps_mode passthrough` alone let open-GOP leading pictures with negative timestamps through (767/719); `trim=start=0` + passthrough + exact `-frames:v` gives exact counts (14 386/14 386 for the 10-minute open-GOP HEVC). ffmpeg's fMP4 output is parsed with `mp4-atom` and re-timed onto the asset timeline (tfdt = T_k + D) so it lines up with any audio rendition; SPS/PPS are also carried in-band in each segment's IDR. The init request encodes segment 0 (to learn the avcC) and caches it, so TTFF ≈ one segment encode. Software decode (AV1 on M1, MPEG-2, MPEG-4 Part 2, VC-1) uses `scale` + `format=nv12`. Frame alignment against the source: PSNR 35.5 dB aligned vs 24.3 dB shifted by one frame (AV1 → H.264).

### 3.5 Planner and presets

Decision order: subtitle first (image subtitle the client can't render → burn-in → video transcode) → direct play (container + every selected track + bitrate) → HLS copy → copy + audio transcode → video transcode. Every non-direct step records a `Reason` (`ContainerUnsupported`, `AudioCodecUnsupported`, `VideoCodecUnsupported`, `HdrUnsupported`, `SubtitleNeedsBurnIn`, `NoIndex`, `BitrateTooHigh`, `ClientQuirk` for DV P7 / codecs HLS can't carry). Audio policy: copy what the client decodes; otherwise E-AC-3 5.1 (640 kb/s) for multichannel sources on clients with E-AC-3 plus stereo AAC (192 kb/s); always offer stereo AAC for the main track (Apple rule 2.3, produced lazily so it costs nothing unless selected). `HlsEngine::plan` then rewrites copies it cannot package into AAC and drops renditions it cannot produce (Opus/Vorbis/PCM transcode from indexed sources).

Presets (research/03 §2): `chrome_desktop` (H.264 ≤ High 8-bit, HEVC Main/Main10 hardware, AV1, VP9; AAC/Opus/FLAC/MP3; no AC-3/E-AC-3/DTS/TrueHD; MP4/WebM progressive), `safari_macos` (H.264, HEVC + DV 5/8, no AV1 on M1; AAC, AC-3, E-AC-3, FLAC, ALAC, Opus stereo), `firefox` (adds MKV progressive), `apple_tv_4k` (gen 4/A19: AV1, DV 8.1/5, HDR10+/HLG; Dolby audio; no TrueHD/DTS), `android_tv` (Media3: MKV progressive, Dolby audio, renders SRT/ASS/PGS), `legacy_h264` (H.264-only 1080p SDR device; exercises video transcode).

### 3.6 Subtitles

Text tracks (SRT, ASS/SSA, WebVTT in Matroska) are extracted with one sequential pass over the clusters (8 MB non-retained reads), converted to WebVTT (ASS override blocks stripped, `\N` → line break, SRT keeps `<i>/<b>/<u>`), cached per (file, track), and served as a one-segment WebVTT playlist (`s/{t}.m3u8` → `s/{t}.vtt`) with `X-TIMESTAMP-MAP=MPEGTS:{D·90000},LOCAL:0` so cue times stay on the source timeline despite the presentation offset D. AVPlayer lists the `sbtl` track for the multi-subtitle clip.

## 4. Validation (`hls-validate`, whole corpus)

24 files (20 feature clips + 4 playback-research files) × 3 presets (chrome, appletv, legacy) = **72/72 OK**. For each, every playlist/init/segment is written to `$FLUX_WORK/tmp/hls-validate/<file>/<preset>/` and checked: `ffmpeg -v error -i master.m3u8 -map 0 -c:s webvtt -f null -` prints nothing; per-rendition packet counts vs `ffprobe -count_packets` of the source; video DTS strictly increasing, CTS ≥ 0, no presentation gaps beyond the source's own; audio gaps/overlaps between consecutive packets across segments < 1 ms beyond the source's own; A/V start offset vs the source; transcoded audio seam smoothness (§3.3). The validator forces HLS (`allow_direct_play = false`); "Direct stream" = video and audio copied. Results: `bench/results/flux-hls-validate.json`. *Audit, 2026-10-20:* that file was later overwritten (it now holds 2 rows), so 72/72 can't be traced. Phase D's 183/183 (`flux-hls-validate-20261020-044028.json`) and the merge's 213/213 (`…-061447.json`) cover these files and more.

| file | index | segs | chrome | appletv | legacy | video pkts out/src | audio renditions (seam ratio) | A/V Δ ms | status |
|---|---|---|---|---|---|---|---|---|---|
| av1_opus.mkv | Cues 4.1 ms | 5 | Direct stream | Direct stream | Transcode | 719/719 | 1 | 0.50 | ok |
| av1_opus.webm | Cues 1.7 ms | 5 | Direct stream | Direct stream | Transcode | 719/719 | 1 | 0.50 | ok |
| h264_aac_faststart.mp4 | Moov 0.5 ms | 10 | Direct stream | Direct stream | Direct stream | 1439/1439 | 1 | 0.00 | ok |
| h264_aac_moov_end.mp4 | Moov 9.8 ms | 10 | Direct stream | Direct stream | Direct stream | 1439/1439 | 1 | 0.00 | ok |
| h264_aac.m2ts | FfprobeScan 156 ms | 5 | Transcode | Transcode | Transcode | 719/719 | 1-aac2 (1.04) | 0.00 | ok |
| h264_dts51.mkv | Cues 5.0 ms | 5 | Audio conversion | Audio conversion | Audio conversion | 719/719 | 1-aac2 (0.67), 1-eac36 (1.00) | 0.00 | ok |
| h264_flac.mkv | Cues 2.4 ms | 5 | Audio conversion | Audio conversion | Audio conversion | 719/719 | 1, 1-aac2 (1.00) | 0.00 | ok |
| h264_long_gop.mkv | Cues 0.3 ms | 7 | Direct stream | Direct stream | Direct stream | 1439/1439 | 1 | 0.33 | ok |
| h264_multi_audio_subs.mkv | Cues 0.2 ms | 10 | Audio conversion | Direct stream | Direct stream | 1439/1439 | 1, 2-aac2 (1.00), 2 | 0.33 | ok |
| h264_truehd71.mkv | Cues 8.1 ms | 5 | Audio conversion | Audio conversion | Audio conversion | 719/719 | 1-aac2 (1.00), 1-eac36 (0.39) | 0.00 | ok |
| hevc_4k_60mbps.mkv | Cues 35 ms | 10 | Audio conversion | Audio conversion | Transcode | 1439/1439 | 1-aac2 (1.00), 1 | 0.33 | ok |
| hevc_main10_hdr10_eac3.mkv | Cues 10 ms | 5 | Audio conversion | Audio conversion | Transcode (tone-mapped) | 719/719 | 1-aac2 (1.00), 1 | 0.33 | ok |
| hevc_main10_hdr10_hvc1.mp4 | Moov 4.9 ms | 5 | Direct stream | Direct stream | Transcode (tone-mapped) | 719/719 | 1 | 0.00 | ok |
| intro_shared.mkv | Cues 7.4 ms | 4 | Direct stream | Direct stream | Direct stream | 480/480 | 1 | 0.33 | ok |
| long_2h_360p.mkv | Cues 7.7 ms | 1199 | Direct stream | Direct stream | Direct stream | 172627/172627 | 1 | 0.33 | ok |
| mpeg2_ac3.ts | FfprobeScan 197 ms | 5 | Transcode | Transcode | Transcode | 899/899 | 1-aac2 (1.00) | 0.00 | ok |
| mpeg4_mp3.avi | FfprobeScan 97 ms | 5 | Transcode | Transcode | Transcode | 751/750 ¹ | 1-aac2 (0.97) | 0.00 | ok |
| show_e01_with_intro.mkv | Cues 5.4 ms | 18 | Direct stream | Direct stream | Direct stream | 2518/2518 ² | 1 | 0.33 | ok |
| show_e02_with_intro.mkv | Cues 5.3 ms | 20 | Direct stream | Direct stream | Direct stream | 2878/2878 ² | 1 | 0.33 | ok |
| show_e03_with_intro.mkv | Cues 5.2 ms | 23 | Direct stream | Direct stream | Direct stream | 3238/3238 ² | 1 | 0.33 | ok |
| bbb_1080_10s_30MB_actually_mp4.mp4 | Moov 4.8 ms | 2 | Direct stream | Direct stream | Direct stream | 300/300 | — (video only) | — | ok |
| bbb_1080_10s.mkv | Cues 21 ms | 2 | Direct stream | Direct stream | Direct stream | 300/300 | — (video only) | — | ok |
| synth_h264_10min.mkv | Cues 33 ms | 55 | Direct stream | Direct stream | Direct stream | 14386/14386 | 1 | 0.33 | ok |
| synth_hevc10_opengop_10min.mkv | Cues 9.2 ms | 60 | Direct stream | Direct stream | Transcode | 14386/14386 | 1 | 0.33 | ok |

¹ The AVI source has a one-frame timestamp gap; the CFR transcode fills it. ² The source has a one-frame video gap at a concat join, reproduced as-is; audio has 0 gaps/overlaps after the region-aware placement (§3.2). A/V Δ of 0.33 ms for AAC tracks is ffprobe's ms-truncated source timestamp (−21.0 ms) vs our exact −21.333 ms (1024 samples at 48 kHz). Index times in this table were taken under load ~20; after the final first-frames change (headers-only walk), Cues indexes take 2–9 ms and read 0.5–6 MB (`tmp/v11.json`). Cue-less MKV (`ffmpeg -live 1` copy of the 466 MB HEVC file): `ClusterScan` 1.14–1.44 s from the SSD (≈325–410 MB/s, 62 reads), 0.06 s when page-cached; it then validates 14386/14386 with 60 segments.

Unit/integration tests: `cargo test -p flux-hls --release` — 43 unit tests + 12 integration tests against the corpus (index facts, MP4 front/back moov agreement, HDR10 from VUI+SEI, segment continuity requested in reverse order, exact audio partition, MP4 sample tables, Cue-less fallback, planner on real indexes, transcoded audio grid alignment, WebVTT, TS via ffprobe scan + video transcode, AV1→H.264 exact frame count).

## 5. AVPlayer (`bench/playback-remux-probe/avcheck/check`, macOS)

Against `hls-serve` (release build, Safari preset unless noted; exact seeks, `toleranceBefore/After = 0`). Results: `bench/results/flux-hls-avplayer.json`. **13/13 play with empty `errorLog`**:

| file | path | readyToPlay | first frame advancing | exact seek completes | position 2 s later |
|---|---|---|---|---|---|
| hevc_main10_hdr10_eac3.mkv (4K HDR10, E-AC-3 copy) | remux | 0.238 s | 0.399 s | 83 ms (→20 s) | 21.90 |
| hevc_main10_hdr10_hvc1.mp4 (4K HDR10) | remux | 0.204 s | 0.315 s | 64 ms | 21.95 |
| hevc_4k_60mbps.mkv | remux | 0.206 s | 0.365 s | 83 ms (→40 s) | 41.90 |
| synth_hevc10_opengop_10min.mkv (open GOP) | remux | 0.173 s | 0.286 s | 47 ms (→255.5 s) | 257.43 |
| h264_multi_audio_subs.mkv (+ WebVTT) | remux | 0.157 s | 0.278 s | 60 ms (→45 s) | 46.94 |
| h264_truehd71.mkv | remux + TrueHD→E-AC-3/AAC | 0.255 s | 0.421 s | 87 ms | 21.90 |
| h264_dts51.mkv | remux + DTS→E-AC-3/AAC | 0.201 s | 0.357 s | 91 ms | 21.89 |
| long_2h_360p.mkv | remux | 0.147 s | 0.258 s | 56 ms (→5100 s) | 5101.93 |
| synth_h264_10min.mkv | remux | 0.163 s | 0.285 s | 44 ms | 257.43 |
| h264_aac_moov_end.mp4 | remux | 0.159 s | 0.277 s | 68 ms | 41.94 |
| av1_opus.mkv (Safari on M1: no AV1) | VT transcode | 1.131 s | 1.242 s | 1.81 s | 21.93 |
| hevc_main10_hdr10_eac3.mkv (legacy) | VT transcode + HDR→SDR | 1.278 s | 1.391 s | 1.30 s | 21.93 |
| mpeg2_ac3.ts | ffprobe index + VT transcode | 1.485 s | 1.600 s | 2.36 s | 21.92 |

Earlier runs also covered h264_flac.mkv (FLAC copy), h264_long_gop.mkv (10 s GOP), show_e02_with_intro.mkv, h264_aac_faststart.mp4 and a tolerant seek in the open-GOP file — all clean. `bbb_1080_10s.mkv` reaches readyToPlay but waits forever; the research documented the same behaviour for ffmpeg's own HLS output of that file (content-specific). Not verified: Chrome/Firefox (hls.js/MSE), TVs.

## 6. Benchmarks vs Jellyfin 12.2 (`hls-bench`)

Same five files and procedure as `tools/bench/jellyfin_playback_bench.py`, over loopback HTTP against `hls-serve`: `prepare` (≈ PlaybackInfo / `POST /play`), then TTFS = master → variant → init → segment 0, then cold seeks to the segment covering 600/3600/5100/30 s. flux's audio is demuxed, so `TTFS +audio` adds the default audio rendition's playlist → init → segment 0 **sequentially** (the fair comparison with Jellyfin's muxed segments; players fetch both in parallel). Chrome preset (Jellyfin used a browser profile too). Medians of 5 runs, each a fresh `hls-serve` process (cold index and caches; OS page cache not dropped — needs root). Load average 9–15 during the run (Jellyfin's baseline was also "under load"). File: `bench/results/flux-hls-playback-20261019-074407.json`.

*Audit, 2026-10-20:* the Jellyfin column below is one run from 2026-10-18 (`jellyfin-playback-20261018-224518.json`), on a machine loaded by research agents, with a fixed file order. h264_multi was the first play after Jellyfin started, so its 139 ms PlaybackInfo and 327 ms TTFS carry about +100 ms each of start-up cost (BENCH-6). The like-for-like figures (n = 3, rotated order, warm-up excluded) are in benchmarks.md §5.4: TTFS 189 / 395 / 199 / 617 / 195 ms for these five files, and seeks of 131 ms (h264_multi, 30 s), 2,027 ms (4K, 30 s) and 487–698 ms (2 h, far).

| file | flux plan | prepare | TTFS Jellyfin | **TTFS flux (video)** | **TTFS flux (+audio)** |
|---|---|---|---|---|---|
| h264_multi_audio_subs.mkv | remux, AAC copy (AC-3→AAC lazily) | 0.5 ms (JF 139 ms) | 327 ms | **2.3 ms** | **3.1 ms** |
| h264_truehd71.mkv | remux + TrueHD→AAC | 0.2 ms (JF 21 ms) | 521 ms | **2.9 ms** | **87 ms** |
| hevc_main10_hdr10_eac3.mkv | remux + E-AC-3→AAC | 0.4 ms (JF 18 ms) | 304 ms | **5.1 ms** | **119 ms** |
| hevc_4k_60mbps.mkv (41 MB seg 0) | remux + E-AC-3→AAC | 1.2 ms (JF 12 ms) | 913 ms | **12.5 ms** | **117 ms** |
| long_2h_360p.mkv | remux | 0.6 ms (JF 11 ms) | 191 ms | **0.8 ms** | **1.2 ms** |

| file | seek | Jellyfin | **flux video** | **flux audio** |
|---|---|---|---|---|
| h264_multi_audio_subs.mkv | 30 s | 318 ms | **1.5 ms** | **0.6 ms** |
| hevc_4k_60mbps.mkv | 30 s | 840 ms | **13.9 ms** | **86 ms** (transcode) |
| long_2h_360p.mkv | 600 s | 485 ms | **0.2 ms** | **0.2 ms** |
| long_2h_360p.mkv | 3600 s | 701 ms | **0.2 ms** | **0.2 ms** |
| long_2h_360p.mkv | 5100 s | 607 ms | **0.2 ms** | **0.2 ms** |
| long_2h_360p.mkv | 30 s | 12 ms | **0.2 ms** | **0.2 ms** |

Sweep (every video + default-audio segment of the five files once, 973 MB, fresh server):
- **Server time per segment** (`x-flux-ms`, production before streaming): video p50 **1.1 ms**, p99 **2.1 ms**, pooled over 1,229 video segments, 1,199 of them the 2 h 360p file's small ones. Per file, the video p50 is 0.6–2.1 ms, except the 4K 60 Mb/s file's 41 MB segments: 90 ms p50, 119 ms p99. Copied audio p50 0.09 ms; transcoded audio 112–121 ms p50, 187–341 ms p99 per file (includes waiting on in-flight prefetches; the transcoder's own max was 523 ms under load). *Audit, 2026-10-20:* the audio range first given here, 87–136 / 125–341 ms, mixed this run with a later one (`flux-hls-playback-20261019-115615.json`, load 4.5: 88–137 ms p50, 123–339 ms p99).
- **CPU**: **0.98 ms of server CPU per MB served** (1.0–1.1 ms/MB in the research probe, which also wrote files) + 3.9 s of ffmpeg CPU for the 21 transcoded audio segments (≈180 ms CPU per 6 s segment with two parts).
- **Peak RSS of the server: 69 MB** (Jellyfin 361–383 MB in its 2026-10-18 run; 388 MiB median peak in benchmarks.md's playback runs). Before the header-only walks and payload capture it was 152–172 MB (4K segments parsed with whole-range reads).
- Across runs under heavier load (load 108→130, earlier build): TTFS 1.6–27.5 ms, TTFS+audio up to 244 ms, seeks 0.3–30 ms video / 119 ms audio (`flux-hls-playback-20261019-064827.json`).
- **Audio transcode latency** (`examples/tseg`): TrueHD→AAC and DTS→AAC 135–195 ms per 6 s segment with two parallel parts (load ~30), vs 220–440 ms single-process; target < 300 ms met.
- **Video transcode** (VT, load 10–60): 1.15 s (1080p H.264→H.264), 3.4 s (4K HEVC10 HDR→1080p SDR), 2.6 s (AV1 software decode) per 6 s segment; AVPlayer ready 1.1–1.5 s.

## 7. How to run

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/agent-hls
cargo test   -p flux-hls --release                 # 43 unit + 12 corpus tests (skip if the SSD is absent)
cargo clippy -p flux-hls --all-targets             # clean
cargo build  -p flux-hls --release --bins
R=$CARGO_TARGET_DIR/release

# Validate files for client presets (chrome, safari, firefox, appletv, androidtv, legacy)
$R/hls-validate [--presets chrome,appletv,legacy] [--no-seams] FILE...
#   → table on stdout, JSON in bench/results/flux-hls-validate.json, output in tmp/hls-validate/<file>/<preset>/

# Dev server (Safari/hls.js/AVPlayer): /media/{file}/[{preset}/]master.m3u8, /prepare, /_stats
$R/hls-serve $FLUX_WORK/media/features --port 18080 --preset safari [--index-cache DIR]

# Benchmark (spawns hls-serve per run) → bench/results/flux-hls-playback-<ts>.json
$R/hls-bench --serve-bin $R/hls-serve --runs 5 [--preset chrome] [--files a.mkv,b.mkv]

# AVPlayer check
$FLUX_WORK/bench/playback-remux-probe/avcheck/check http://127.0.0.1:18080/media/<file>/safari/master.m3u8 <seek-s> exact

# Debug aids
cargo run -p flux-hls --release --example dumpidx -- FILE      # print the playback index
cargo run -p flux-hls --release --example tseg -- FILE         # time transcoded audio segments
```

## 8. Known gaps (honest list)

- *(Phase C: Chrome and Firefox verified, C.1; Safari still not.)* **Browsers not verified**: Chrome/Firefox (hls.js/MSE) and TVs were not tested; only ffmpeg and AVPlayer. Open-GOP HEVC after a seek in MSE remains research open question 1.
- *(Fixed in Phase C, C.3.)* **Opus/Vorbis/PCM transcode from MKV/MP4** is not implemented (the transcoder pipes elementary streams; these need an Ogg/WAV wrapper). Such tracks are dropped from a plan when the client can't play them (e.g. AV1+Opus on the legacy preset plays without audio). TS/AVI sources are fine (ffmpeg demuxes them).
- **Subtitle burn-in** (`VideoOp::Transcode { burn_subtitle }`) returns 501. Image subtitles (PGS/VobSub) are not converted.
- **Dolby Vision**: P5/P8 records pass through (not tested: no DV media in the corpus); P7→8.1 RPU rewrite is not implemented (P7 plays as its HDR10 base layer). E-AC-3 with dependent substreams (7.1): `dec3` `chan_loc` mapping unverified.
- **Video transcode** *(Phase C: set-up now paid once per 4-piece run and a 2 s first piece, C.7)*: H.264 only, one bitrate (no ABR ladder), VT session set-up paid per segment (~0.3 s; needs in-process libav to keep sessions warm), no short first segment, no CMAF chunked transfer of in-progress segments; tone mapping only on the hardware-decode path (`scale_vt`); software-decoded HDR (e.g. AV1 HDR) is not tone-mapped. Burn-in absent.
- *(Phase C: 2 s pieces from warm encoder runs, C.7.)* **TTFF for transcodes** is ≈ one segment encode (1.1–1.5 s under load), above the research target of ≤ 1.2 s p50 on a quiet machine only marginally; a short first segment would help.
- **Copied audio** assumes no audio frame with PTS ≥ T_k is stored before keyframe k's cluster (true for ffmpeg-muxed files; checked by exact packet counts on the corpus, not on mkvmerge or scene releases). Matroska header stripping is implemented but untested on real files; zlib/bzip/lzo-compressed tracks are rejected.
- **MP4**: sample tables are expanded in memory (~24 B/sample ≈ 12 MB for a 2 h movie); fragmented-MP4 sources (`moof`) are not indexed *(Phase C: indexed, C.2)*.
- *(Fixed in Phase C: byte-budgeted LRU, C.6; binary index, C.5.)* **Engine caches** (assets, WebVTT) have no eviction; prepared assets are invalidated by size/mtime only. The JSON index cache can be large for long MP4s (binary encoding would be better).
- **Cue-less MKV** indexing reads the whole file (≈1.1–1.4 s per 466 MB from SSD) — fine once and cached, slow on HDD/NAS. TS/M2TS/AVI indexing runs an ffprobe packet scan (100–200 ms for 25–50 MB clips; ~1 s/GB expected).
- *(Fixed in Phase C, C.8.)* **No I-frame playlists / trickplay** yet (the index makes them cheap: research/03 §9).
- **Benchmarks** were taken on a busy shared machine with a warm page cache; the lead should re-run `hls-bench` on a quiet machine, and ideally with a cold cache (`sudo purge`).
- `est_cpu_ms_per_min` in plans is a rough constant-based estimate, not calibrated.

## 9. Proposed flux-core changes

1. **API route doc** (`api.rs`): audio renditions are keyed by `{rendition}` = `{track}` (copy) or `{track}-aac2` / `{track}-eac36` (transcode), not `{track}` — one source track can be offered both ways. Subtitles need a playlist: `s/{track}.m3u8` → `s/{track}.vtt`.
2. `Reason::CodecNotPackageable { codec }` for "the client could decode it, but HLS fMP4 can't carry it" (TrueHD/DTS/Vorbis/PCM on a device with pass-through); the planner currently uses `ClientQuirk` for this.
3. `ProbeResult`/`Track`: expose Matroska `CodecDelay` and an exact frame duration (ticks + timescale), so the catalog's probe and flux-hls agree on timing facts without re-reading.
4. `BitstreamFix::StripInbandParameterSets` is decided by the engine from the index (the probe can't know); consider dropping it from the plan or filling it from the index.
5. Consider moving `ClientCaps` presets (`flux_hls::planner::presets`) next to `ClientCaps` in flux-core so the web client and tests share them.
6. Add an optional `PlaybackPlan::warm_urls` / `PlayResponse::prefetch` filled from `HlsEngine::warm` (one-round-trip play, research/08 idea E).

## Phase D — correctness and robustness (2026-10-20)

> **After the merge** ([fixes.md](fixes.md)): the proposed engine and server wiring is done: `HlsEngine::shutdown` kills index scans, `EngineConfig::index_scan_limits`, `rt.shutdown_timeout`, streamed WebVTT (`webvtt_body`) and logged body errors. `INDEX_VERSION` is 5 (both Phase D branches had bumped it to 4). Predicted piece counts (ffprobe-indexed sources, TS) now end at the forced IDR nearby.

*Branch `phase-d/hls-correct`* (worktree `wt/fix-hls-correct`, from `1fbcb63`). This is the correctness part of Phase D: the playback (PB-*) and robustness (ROB-*) findings of the adversarial review (`review/findings-d1.md`) that live in flux-hls, plus three documented gaps. The performance part (MPEG-TS in process, engine) is reported separately.

Built and tested with `nice -n 19 cargo … -j 3`, `CARGO_TARGET_DIR=$FLUX_WORK/target/r-robust`, on the shared M1 Max (1-minute load 40–260 while other agents compiled; recorded in each result file).

**Summary**

| finding | what was wrong | fix | evidence (before → after) |
|---|---|---|---|
| PB-1 | Matroska VFR with a DefaultDuration: every PTS snapped to the grid, frames collided | snap only within 1 ms of the grid (`IndexTrack::grid_point`); an off-grid last frame lasts until the next segment | `vfr_section.mkv`: 58 colliding PTS → 0, DTS strictly increasing, 288/288 |
| PB-2 | transcode without DefaultDuration assumed 24 fps: wrong `-frames:v`, frames dropped/borrowed, evenly re-timed | measured frame rate in the index; each run counts and times its frames from the index's real timestamps (`Asset::video_frame_ticks`, `vtranscode::run_frames`) | `vfr_hevc_short.mkv` legacy 230/288 → 288/288; `hevc30_nodd.mkv` legacy: 359 of 359 frame intervals off 1/30 s → 0 |
| PB-2 (found here) | no DefaultDuration also left the reorder depth out of D: copied B-frames got negative CTS, players shifted video 46 ms | D = reorder depth × measured frame (+1 ms rounding per frame) | `hevc30_nodd.mkv` chrome A/V Δ 46.0 → 0.33 ms |
| PB-NEW-1 | ffmpeg added the file's start_time to `-ss` | `-seek_timestamp 1` for Matroska/MP4 | `start1_hevc.mkv` legacy 120/144 → 144/144, piece 0 starts at the first frame |
| PB-5 | abandoned seeks kept encoder runs and their permits | per-run cancellation token; a run whose last waiter drops unserved is cancelled (ffmpeg killed by `kill_on_drop`, permit freed); a request that finds a cancelled run starts a new one | reviewer repro: 1 run left going → 0 (within 39 ms) |
| PB-7 | a subtitle read the whole file before the first cue | read the track's cue points (ffmpeg and mkvmerge index subtitle blocks); else a header-only cluster walk through 4 KiB windows; `Asset::for_each_subtitle_block` + `subtitle::WebVttWriter` hand cues over as they are read | `h264_multi_audio_subs.mkv`: 49.5 MB read → 25 KB (first cue after 9 KB); walk without Cues 7.6 MB (15 %) |
| PB-9 | tkhd rotation dropped: portrait video sideways (copy) or squashed (software transcode) | matrix read into `IndexTrack::extra`, written into the copy's `tkhd`; transcodes turn the pixels upright (`-noautorotate`, `transpose_vt`/`transpose`, upright init size and RESOLUTION) | `rot90_hevc.mov` legacy: 640x360 sideways → 360x640 upright; copy init carries the source matrix |
| PB-10 | Matroska BANDWIDTH counted every track's bytes | subtract the other tracks' bitrates (codec headers, else measured on the first blocks) | `h264_truehd71.mkv` chrome: signalled video 8.65 → 6.07 Mb/s; served 6.00 Mb/s |
| PB-12 | CODECS and `-level` fixed at 4.1 whatever the frame rate | level from output size × rate (Table A-1), CODECS from the same level | 1080p60: `avc1.64002a`, encoder avcC level 0x2a |
| PB-NEW-2 | hls-validate passed a transcode that lost frames | fewer frames than the source fails; a surplus only per real source hole (> 1.5 frames) | `vfr_hevc_short.mkv` legacy: "ok" at 230/288 before |
| PB-NEW-3 | `--help` was an input file; results overwrote the shared file | `--help`, unknown flags are errors, inputs must exist; results to `flux-hls-validate-<UTC>.json`, never replaced without `--force` (both bins) | unit tests |
| ROB-1 | play-time ffprobe index scan: no timeout, no kill | own process group, stdout/stderr drained on threads, killed on a 20 s stall or after 30 s + 30 s/GB; `external::kill_running()` for shutdown | fake ffprobe (shell + `sleep` child): error after 0.82 s, no process left; `kill_running` ends a scan in 0.5 s |
| ROB-2 | stsz/ctts counts trusted: 1.6 MB file → GBs of RAM, seconds of CPU | sample count ≤ file size and ≤ 32 M per track before any per-sample array; ctts/stts runs clamped; keyframes found in one pass without expanding tables (was 4 expansions per play) | stsz bomb: clean error; ctts bomb (u32::MAX runs): 1.7 ms |
| ROB-V2 | a `moof` of size 0 read the rest of the file into memory | `moof` of size 0 or > 64 MiB rejected | 128 MiB file: index error instead of +257 MB RSS |
| ROB-5 | truncated MP4: 200 + Content-Length, then a cut body | every file range is checked against the file length when the segment is built: an error before any response | 64 KiB cut of `mp4_named.mkv`: clean error |
| anamorphic | no `pasp` | SAR from Matroska DisplayWidth/Height, MP4 `pasp`, else the SPS VUI; init writes `pasp` and a display-width `tkhd` | 720x576 at 64:45 → `pasp` 64:45, `tkhd` 1024 |
| EXTINF | the last segment ended at the container duration | it ends at the video's own end (MP4: sample tables; Matroska: one read of the last GOP) | `mkv_fonts.mkv`: 4.0 s → 2.0 s |
| AVI reasons | "repackages (no quality loss)" next to a transcode | already fixed for AVI in `c2ff303`; the open-GOP quirk path (e.g. HEVC MOV on Firefox/macOS) still did it: removed, with an invariant test over every preset × container | planner test |

### D.1 How the fixes work

- **Frame grid (PB-1).** `IndexTrack::snap_ticks` snaps a timestamp only when it lies within `GRID_TOLERANCE_NS` (1 ms, the Matroska rounding: mkvmerge rounds, ffmpeg truncates) of the DefaultDuration grid. Off-grid times keep their own value on the track timescale (≥ 1000 ticks/s, so whole milliseconds stay distinct and ordered). Segment bounds use the same function, so they agree. The last frame of a segment lasts one frame when it is on the grid (CFR, open GOP included) and until the next segment otherwise.
- **Real frames for the transcoder (PB-2).** A run reads its segments' frame times from the index before taking an encode permit: Matroska block headers (64 KiB windows, no payloads) or the MP4 sample table, plus the next segment for open-GOP leading pictures. Per piece: `-frames:v` is the real count, `trim` cuts halfway between the last dropped and the first kept frame, IDRs are forced halfway before each piece's first frame, and samples keep the source's durations (the last until the next frame). `Piece::frames` stays the frame-rate prediction for playlists; a piece with no real frame (a long hole in a VFR source) falls back to it. The first sample is no longer forced to `sync`; a miscount is logged instead.
- **Run cancellation (PB-5).** `inflight` maps each piece to its watch channel and its run's `RunCtl` (a child of the engine's cancellation token and a waiter count). A `Waiter` guard counts a request while it waits; dropped unserved as the last waiter, it cancels the run. Served requests don't: the rest of a run is the playhead's prefetch. `warm()` and prefetch runs have no waiter and are left alone. The run's cleanup unregisters its pieces before failing their channels, and a request that finds a cancelled run retries with a new run, so a player that re-requests a piece right after aborting still gets it.
- **Subtitles (PB-7).** `MkvLayout::cues_pos` keeps the Cues position. A track's cue points (time, cluster, `CueRelativePosition`, `CueDuration`) lead to one small read per cluster and per block; a cue that misses is scanned for in its cluster, and if none leads anywhere the cluster walk runs instead. The walk reads only block headers: `BlockWalker::only_track` skips a block after its track number, never reading lace headers or payloads. Cluster timestamps follow an optional CRC-32 (ffmpeg writes one; the first version missed it and walked whole clusters).
- **Display (PB-9, anamorphic).** `IndexTrack::extra` (`TrackExtra`: `matrix`, `rotation`, `sar`, `end_ns`) holds the new facts; it is `#[serde(default)]`, so old serialized indexes still parse (and `INDEX_VERSION` 4 rebuilds them anyway). `EntrySpec::Video` gained `pasp` and `matrix`. Rotation is clockwise degrees from the QuickTime matrix (a b; c d) = (cos θ, sin θ; −sin θ, cos θ): an iPhone portrait clip is 90°; `ffmpeg -display_rotation 90` (counter-clockwise, ffprobe `rotation: 90`) is 270°. Transcodes use `-noautorotate` (a boolean flag in ffmpeg 9: `-autorotate 0` made "0" an output file) and rotate explicitly after scaling.
- **Index scan limits (ROB-1).** `ScanLimits { stall: 20 s, base: 30 s, per_gb: 30 s }`. The packet scan writes a line per packet, so only a real stall trips the stall limit; the deadline scales with the file. `PlaybackIndex::build_with_fallback_limits` and `Asset::open_with_limits` take explicit limits. A run killed by `kill_running` reports `HlsError::Cancelled`.
- **Sample tables (ROB-2).** `Mp4Tables::check` runs before anything per sample: count ≤ file length (every sample is a byte of the file; sizes may still sum past the end of a file being copied, whose head plays) and ≤ `MAX_SAMPLES` (32 M; PCM is expanded per chunk and exempt). `Mp4Tables::timing()` walks stts/ctts runs without allocating; the index build, the first-sync-sample read and the segment plan's byte counts use it or `sample_location`/`bytes_of`, so the per-sample arrays are built once, by the asset.
- **BANDWIDTH (PB-10).** Each Matroska audio track gets a bitrate: AC-3/E-AC-3 from the frame header (as before), PCM exactly, others from the bytes and timestamps of the blocks the first-frames walk already reads. The segment plan subtracts the sum from each cluster range (never below a fifth of it). A quiet start under-measures lossless audio, which errs towards a higher BANDWIDTH.
- **Video end (EXTINF).** MP4: the max of PTS + duration over the samples (same single pass). Matroska: the block headers from the last keyframe's cluster to the end of the cluster data, read in one read when that is ≤ 8 MiB (16 KiB windows beyond). Index cost on the corpus, warm cache: +1 read and +0.1–1.2 ms per file (the 4K 60 Mb/s file walks a long last GOP: +47 reads, +0.8 MB); the `≤ 16 reads` budget test still holds.

### D.2 Validation over the whole corpus

`hls-validate --presets chrome,appletv,legacy` over every media file in `media/features`, `media/playback-research`, `media/probe-fixtures`, `media/seeds` and the Phase C scratch files in `tmp/hls/media`: **183/183 ok** (61 files × 3 presets). This restores and extends the lost Phase C file (123/123: the corpus, the scratch files and 5 fixtures; now every fixture and seed too).

- **Results:** `bench/results/flux-hls-validate-20261020-044028.json` (30.5 min at nice 19, load 29–50 from other agents' builds).
- **Plans:** 84 direct stream, 41 audio conversion, 58 transcodes; indexes by Cues 28, Moov 16, FfprobeScan 13, Fragments 3, ClusterScan 1 (per file).
- **Checks:** 0 ffmpeg errors; 1 764 123 video frames, every count equal to the source's except `mpeg4_mp3.avi` (751/750: the source's one-frame hole, filled; allowed by the new rule); max A/V start Δ 0.51 ms; transcoded-audio seam ratio ≤ 1.04 (93 renditions), global max/p999 1.00–1.50 (the AVI stream ends, as in Phase C); I-frame playlists decoded in 107 cases (35 004 keyframes).
- **The validator itself changed** (PB-NEW-2/3 and one artifact): a transcode that loses a frame fails; the ffmpeg decode check gives the null muxer a 90 kHz video time base (its default 1/frame-rate base can't hold a VFR stretch, so the source of `vfr_section.mkv` itself produced the reviewer's "58 ffmpeg errors"; its 39 PTS collisions were real, and are fixed).

Scratch clips for the findings (`tmp/d-hls-correct/scratch/make_scratch.sh`), old binary (main) → new, `--presets chrome,legacy` (`tmp/d-hls-correct/scratch-{before,after}.json`):

| clip | preset | before | after |
|---|---|---|---|
| vfr_section.mkv (H.264, 59.94 stretch in 23.976) | chrome | 288 packets, 39 PTS collisions (in-process check: 58) | 288, 0 collisions |
| vfr_hevc_short.mkv (same, HEVC) | legacy | 230/288, reported "ok" | 288/288 |
| hevc30_nodd.mkv (no DefaultDuration) | legacy / chrome | 360 frames, but 359 of 359 intervals off 1/30 s (re-spread at an assumed 24 fps) / A/V Δ 46 ms | 0 intervals off / 0.33 ms |
| start1_hevc.mkv (start_time 1 s) | legacy | 120/144 | 144/144 |
| rot90_hevc.mov | legacy | 640x360, sideways | 360x640 upright |
| hevc1080p60.mkv | legacy | CODECS avc1.640029, `-level 4.1` | avc1.64002a, level 4.2 |
| mkv_fonts.mkv | chrome | last EXTINF 4 s | 2 s (the video) |

### D.3 Browsers and AVPlayer (anamorphic, rotation)

`node crates/flux-hls/browser/display.mjs` (Chrome headless via hls.js 1.7.3 and AVPlayer via `avcheck/check`, both at nice 19; `bench/results/flux-hls-display-20261020-050641.json`; the base build in `tmp/d-hls-correct/display-before.json`). Chrome's `videoWidth×videoHeight` / AVPlayer's `presentationSize`:

| case | before (main) | after |
|---|---|---|
| anamorphic.mkv / .mp4, 720x576 at 64:45, copy | Chrome 720x576 (squashed); AVPlayer 1024x576 (it reads the SPS's SAR) | **1024x576** in both (`pasp`) |
| rot90_h264.mov, copy (tkhd 90° CCW) | 640x360 in both (sideways) | **360x640** in both (the copied matrix) |
| rot90_hevc.mov, transcode | 640x360 in both | **360x640** in both (upright pixels) |

So Chrome's MSE (`frameWidth` stays 720 while `videoWidth` is 1024) and AVPlayer both honour `pasp` and the `tkhd` matrix of an fMP4 init. Firefox and Safari were not run (Phase C's harness covers Firefox; its `isTypeSupported` run is unchanged).

### D.4 Tests

- `tests/phase_d.rs`: 18 regression tests, one or more per finding, on the corpus plus the scratch clips (each skips when its input is missing). The playback reviewer's own repros (`review_playback.rs`: `vfr_matroska_frames_keep_unique_timestamps`, `webvtt_extraction_reads_entire_file`, `abandoned_seeks_delay_the_next_piece`) fail on `1fbcb63` (58 colliding PTS; 49 514 254 of 49 560 555 bytes read; 1 encoder run left going) and pass on this branch (0; 25 353 bytes; 0).
- Unit tests: grid snapping and `matrix_rotation` (index), sample-table bombs and the non-allocating table views (mp4), `pasp`/matrix in the init (fmp4), levels and run cancellation (vtranscode), the "no lossless repackage on a transcode" invariant over every preset × container × open GOP (planner), argument handling and the frame-count rule (bins).
- Gate: `cargo build --workspace --all-targets`, `cargo clippy --workspace --all-targets` (0 warnings), `cargo test --workspace` (**483 passed, 0 failed, 4 ignored**), `cargo fmt --all --check`.

### D.5 API changes

- **New:** `IndexTrack::extra: TrackExtra` (`matrix`, `rotation`, `sar`, `end_ns`); `IndexTrack::grid_point`, `index::GRID_TOLERANCE_NS`; `MkvLayout::cues_pos`; `mkv::SubtitleSource`; `index::external::{ScanLimits, build_with_limits, kill_running}`; `PlaybackIndex::build_with_fallback_limits`; `Asset::{open_with_limits, video_frame_ticks, for_each_subtitle_block}`; `subtitle::WebVttWriter`; `mp4::{MAX_SAMPLES, MAX_MOOF, SampleTiming}`, `Mp4Tables::{timing, sample_size, sample_location, bytes_of}`; `fmp4::IDENTITY`; `EntrySpec::Video { pasp, matrix }`; `vtranscode::{h264_level, source_fps, output_size, RunFrames, run_frames}`.
- **Changed:** `VideoTarget::codec_string(fps)` (was fixed 4.1); `INDEX_VERSION` 3 → 4 (cached indexes rebuild once; flux-server already checks the version).
- Struct literals of `IndexTrack` and `EntrySpec::Video` outside these files need the new fields (`extra: Default::default()`, `pasp`/`matrix`).
- flux-hls depends on `libc` (`killpg`).

### D.6 Changes needed outside this branch's files

1. **engine.rs** (owned by the performance work):
   - `HlsEngine::shutdown()`: also call `crate::index::external::kill_running()`, so no blocking thread waits on an ffprobe scan (ROB-1).
   - `EngineConfig::index_scan_limits: index::external::ScanLimits`, passed by `prepare()` through `Asset::open_with_limits` (ROB-1, configurable).
   - `webvtt()`: take a `reads` permit around the extraction, and add `webvtt_stream(asset, track)`: `Asset::for_each_subtitle_block` on the blocking pool feeding an mpsc channel through `subtitle::WebVttWriter`, caching the text when complete (PB-7: the first cue streams after a few reads even without Cues).
2. **flux-server main.rs:** after `block_on`, `rt.shutdown_timeout(Duration::from_secs(2))` instead of the implicit drop (ROB-1: a blocking task can no longer hold SIGTERM).
3. **flux-server media.rs:** log body-stream errors with `tracing::warn!` (ROB-5: a file that shrinks after the segment was built still cuts the body, now visibly); serve the sidecar `s/{t}.vtt` from `webvtt_stream` (chunked) when it isn't cached (PB-7).

### D.7 Known gaps after Phase D

- **PB-7 is fixed in flux-hls, not end to end:** with Cues (ffmpeg and mkvmerge both index subtitle blocks) a track costs a few reads; without them the walk reads 15 % of the file, and until D.6 lands the engine still waits for the whole extraction before the first cue.
- **ROB-1 shutdown:** without D.6 #1–#2 a stuck scan still holds shutdown, but for at most its stall limit (20 s) instead of forever.
- **Matroska rotation** (`Projection`/`ProjectionPoseRoll`) is not read; only MP4/MOV `tkhd`.
- **VFR transcodes:** piece boundaries (and their EXTINF) are still predicted from the frame grid; the encode uses the real frames. A piece the source has no frame for falls back to the predicted counts.
- **Matroska BANDWIDTH** subtracts audio bitrates measured on the first blocks: a quiet start under-measures lossless audio, so BANDWIDTH stays somewhat high (Chrome case: +1 % against served bytes).
- **Index cost:** the video end adds one read per Matroska file (+0.1–1.2 ms warm); a last GOP over 8 MiB is walked block by block (4K 60 Mb/s: +47 reads).
- **Not run:** Firefox and Safari for `pasp`/rotation; the reviewer's end-to-end flux-server scripts (`exp_play_ffprobe_hang.py`, `exp_mp4_bombs.py`, `exp_trunc_segment.py`): the same paths are covered in-process by `tests/phase_d.rs`.

### D.8 Re-running

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/r-robust TMPDIR=$FLUX_WORK/tmp/d-hls-correct/tmp
W=$FLUX_WORK; D=$W/tmp/d-hls-correct
zsh $D/scratch/make_scratch.sh                                   # scratch clips (small)
nice -n 19 cargo test -p flux-hls -j 3 -- --test-threads 2       # unit + phase_d + real_media
nice -n 19 cargo build --release -p flux-hls --bins -j 3
nice -n 19 $CARGO_TARGET_DIR/release/hls-validate --presets chrome,appletv,legacy --out $D/validate-corpus $(cat $D/corpus-files.txt)
nice -n 19 node crates/flux-hls/browser/display.mjs              # Chrome + AVPlayer geometry
```

## Phase D: performance (2026-10-20)

> **After the merge** ([fixes.md](fixes.md)): flux-server sends weak ETags for `!is_deterministic` resources, and a video transcode of an MPEG-TS source is fed from `ts::video_input` through stdin (the 4K HDR10 M2TS: 719/719 frames). The TS index fills hls-correct's `TrackExtra` (SPS aspect ratio, video end).

*Branch `phase-d/hls-perf`.* Numbers come from the **shared** M1 Max: every process ran at `nice -n 19`, other agents were compiling and validating, and the 1-minute load average is recorded with each result (here 4–120). The OS page cache was not dropped (that needs root).

**Summary**

- **MPEG-TS and Blu-ray M2TS are remuxed in-process** (new module `ts`). Until now every TS/M2TS file hit `NoIndex` and was video-transcoded. On the Chrome preset (`hls-bench`, medians of 3 fresh servers):
  - time to first segment **0.34–1.30 s → 0.7–13 ms**;
  - cold seeks **0.40–1.43 s → 0.5–12 ms**;
  - ffmpeg CPU for 2 minutes of TS video **12.0 s → 1.0 s** (the 1.0 s is AC-3/E-AC-3 → AAC audio Chrome can't play);
  - index **116–1016 ms → 6–108 ms** per file (page-cached).
- **Index of a 2 h, 5.8 GB TS:** 27 s on its first read, which is the disk's speed (≈ 213 MB/s here) with 0.26 s of CPU; 0.9 s page-cached, where ffprobe's packet scan (the old index) takes 4.6 s.
- **Validated:** `hls-validate` on every TS/M2TS of the corpus plus 11 scratch files (PTS wrap, discontinuities, VFR, TrueHD/DTS/LPCM/LATM/MP2/MP3 audio, HDR10 4K M2TS, 10-min open-GOP HEVC, 2 h): exact packet counts, 0 DTS errors on every remux. Chrome plays them through hls.js (D.3).
- **Engine and source fixes:** PB-3 (WebVTT keyed by file identity), PB-4 (sources close idle, deleted or replaced files), PB-6 (warm only the default audio), PB-11 (deterministic audio transcodes, weak-ETag API for video transcodes), and `warm()` keeps what it produced.

### D.1 TS remux: design

A TS file has no index, so the index is built by reading the file once. Anything the remux can't reproduce faithfully is rejected with `Unsupported("MPEG-TS remux: …")`; the caller falls back to the ffprobe index and the ffmpeg paths, as before. That index then carries the rejection as a warning, which the planner reads.

**Modules** (`crates/flux-hls/src/ts/`):

| module | what it does |
|---|---|
| `packet` | 188-byte (TS) and 192-byte (M2TS: 4-byte arrival timestamp first) packets; sync recovery (two sync bytes a stride apart); continuity counters: a repeated packet is dropped, a lost one marks its PES damaged and is counted. |
| `psi` | PAT/PMT sections and flux-probe's stream classification (stream types, HDMV registration, DVB descriptors, sniffing of undescribed private data). Track order, numbers (PID) and codecs therefore equal the catalog's probe, which `PlanPrefs::audio_track` relies on. |
| `pes` | PES headers (PTS/DTS), reassembly (bounded and unbounded PES), 33-bit unwrapping to the value nearest a reference. |
| `video` | Annex B NAL units (start codes found 8 bytes at a time), access-unit inspection up to the first slice, H.264 and HEVC SPS facts, `avcC`/`hvcC` built from in-band parameter sets, length-prefixed samples (AUDs and filler dropped; VPS/SPS/PPS dropped too, they are in the record). |
| `audio` | ADTS, AC-3/E-AC-3 and MPEG audio framing across PES boundaries: the PES timestamp belongs to the first frame that *starts* in that PES, later frames follow on. E-AC-3 dependent substreams join their independent frame. |
| `scan` | The index: one pass with 8 MB reads. |
| `remux` | Video and audio fragments, transcoder input frames and I-frames, from the index alone. |

**Index** (`scan`):

1. **Head** (256 KB, then 1 MB, as flux-probe reads it): PAT → PMT → the track list.
2. **Pass**: for the video PID, every PES header and the start of every access unit up to its first slice. For audio PIDs, PES timestamps and the first payload bytes.
3. **Keyframes:**
   - H.264: an IDR, or an I/SI slice with a recovery-point SEI or the random access indicator (ffmpeg's rule);
   - HEVC: IRAP (IDR, CRA, BLA). `open_gop` is set when a later keyframe isn't an IDR (Firefox quirk, C.1).
   - Checked by decoding from them (D.3): every frame from a keyframe on decodes.
4. **Timeline:**
   - Timestamps are unwrapped per PID next to the previous one.
   - A step of the video's DTS by more than −0.5 s or +10 s is a **discontinuity**. The later epoch is shifted so the video continues one frame later. Audio jumps are matched to the nearest video jump by file position, and each PID switches epoch at its own first PES after it.
   - The **origin** is ffmpeg's `start_time`: the earliest first timestamp of the audio/video streams. So the ffmpeg paths (`-ss` in the video transcoder and in the audio fallback) and the remux share one timeline.
5. **Frame rate:**
   - Constant when ≥ 95 % of the PTS steps are one frame within 1 ms, at most 4 + 0.1 % are not whole frames (joins), and snapping every frame to the grid keeps every time distinct.
   - The 1 ms allows TS remuxed from Matroska, whose timestamps are whole milliseconds (41/42 ms at 23.976). The rate is then the nearest standard rational (24000/1001 …), as for Matroska.
   - Otherwise VFR: a 90 kHz timescale and no snapping. A 59.94 section in a 23.976 stream stays VFR, so PB-1 can't happen here.
6. **D** = reorder depth × frame. The depth is the larger of the SPS's and the stream's measured one: the largest (sorted PTS − PTS) in decode order per GOP, which is what DTS = sorted PTS needs.
7. **Configuration records** come from the first keyframe's parameter sets, HDR from the SPS VUI and the SEI (mastering display, light level).

**Rejected** (the ffprobe index and the ffmpeg paths take over):
- video other than H.264/HEVC;
- field-coded H.264 (PAFF: `field_pic_flag` in a slice header; MBAFF is fine);
- a PES with more than one picture (checked on the first 120);
- video PES without PTS;
- parameter sets that change mid-stream (same id, different bytes);
- scrambled video;
- audio/video from more than one program;
- keyframe times that disagree with file order.

**Segments** (`remux`):
- **Video n** = the video PES that start in `[pos(kf_n), pos(kf_n+1))`. One PES is one access unit, so no frame straddles the range. Timestamps follow the Matroska rules: PTS snapped to the frame grid, DTS = sorted PTS, CTS = PTS + D − DTS.
- **Audio n** = the frames presented in `[T_n, T_n+1)`. Audio is muxed ahead of or behind the video, so the window starts 1.5 s of bytes either side of the video range and doubles until it provably holds every such frame: a PES at or before `T_n` at its start, a complete PES at or after `T_n+1` at its end. It gives up after ~48 s of skew on a side, e.g. for a track that ended.
- Each frame is placed at its own 90 kHz timestamp, so a frame is placed the same way whichever segment it falls in. TS timestamps are sample-exact, unlike Matroska's milliseconds.

**Audio:**

| codec | how |
|---|---|
| AAC in ADTS | copied: raw frames + `esds` from the ADTS header (HE-AAC as implicit SBR) |
| AC-3, E-AC-3 | copied: `dac3`/`dec3` from the first access unit (`codec::audio`) |
| MP2, MP3 | frames demuxed here, transcoded on the existing elementary-stream path (sample-exact grid) |
| DTS, TrueHD, Blu-ray LPCM, AAC in LATM, others | ffmpeg reads the file: `-filter_complex [0:i:<pid>]anull` (by PID; a filtergraph input takes the *first* stream of the PID, because ffmpeg adds a second, AC-3 core, stream on a Blu-ray TrueHD PID and `-map 0:i:<pid>` maps both) |

**Planner hooks:**
- `ts::probe_remuxable(probe)`: TS/M2TS with H.264/HEVC main video is indexable, like Cue-less Matroska, unless the index carries the rejection warning.
- `ts::copyable(track)`: AAC (not LATM, stream type 0x11), AC-3, E-AC-3; used by `plan_from_probe`.
- `INDEX_VERSION` 3 → 4, so cached ffprobe indexes of TS files are rebuilt.

### D.2 Results

**Speed** (`hls-bench --preset chrome --runs 3`, fresh `hls-serve` per run, page cache warm; seeks 20/10/25 s for the short files, 300/500/30 s for 10 min, 600/3600/5100/30 s for 2 h; `bench/results/flux-d-hls-perf-ttfs-20261019-220437.json`; load 4–63). The table leaves out the short files' third seek, to 25 s. Before, that seek hit a piece the transcode run had already produced (0.4 / 0.7 / 29 ms). After, it took 4.7 / 4.1 / 10.3 ms, so it was not a cold seek for the old build:

| file | before (main) | after | TTFS before → after | cold seeks before → after | index before → after |
|---|---|---|---|---|---|
| h264_aac.m2ts (30 s, H.264 + AAC) | Transcode | Direct stream | 697 → **5.6 ms** | 1.05 / 0.87 s → **4.6 / 4.4 ms** | 116 → 6 ms |
| h264_aac_ac3.ts (60 s, + AC-3) | Transcode | Audio conversion | 999 → **4.4 ms** | 0.72 / 1.07 s → **4.3 / 4.0 ms** | 126 → 10 ms |
| hevc_hdr10_eac3.m2ts (30 s, 4K HDR10 + E-AC-3) | Transcode (tone-mapped) | Audio conversion | 1303 → **13 ms** (+audio 128 ms) | 1.10 / 0.84 s → **12 / 11 ms** | 327 → 14 ms |
| hevc10_opengop_10min.ts (10 min, HEVC 10-bit) | Transcode | Direct stream | 792 → **9.4 ms** | 1.13 / 1.43 / 0.99 s → **7.7 / 6.9 / 7.0 ms** | 753 → 66 ms |
| long_2h_360p.ts (2 h) | Transcode | Direct stream | 343 → **0.7 ms** | 0.54 / 0.52 / 0.40 / 0.61 s → **0.5–0.7 ms** | 1016 → 108 ms |

- **CPU** (sweep of every segment of the three short files, 2 min of video): ffmpeg **12.0 s → 1.0 s**; flux itself 163 → 207 ms for 87 → 127 MB served. Video segment server time p50/p99 is **301/997 ms → 3.4/9.9 ms**. Peak RSS 115 MB both.
- **Memory:** a TS segment is assembled in memory (payloads are interleaved with other PIDs' packets, so they can't be streamed as file ranges): about one segment's video bytes per request in flight.

**Index of a 2 h TS** (`bench/results/flux-d-hls-perf-index-20261020.json`): 5.76 GB of 1080p H.264, 172,800 frames, 1,896 keyframes.

| | first read | page-cached | CPU | RSS |
|---|---|---|---|---|
| TS scan | 27.1 s (≈ 213 MB/s, the disk) | 0.86–1.01 s | 0.26 s user | 25 MB |
| ffprobe packet scan (before) | — | 4.61 s | 2.78 s user | 35 MB |

The scan reads the whole file. ffmpeg's packet-scan index did too, and every play then paid a video transcode on top. TS has no index to read instead, so a cold 2 h recording costs one sequential read before its first play. Proposed: build TS indexes at scan time (D.6).

### D.3 Validation

**`hls-validate --presets chrome,appletv,legacy`** (`tmp/d-hls-perf/validate/v{2,3}.json`). Scratch files were made by `tmp/d-hls-perf/scripts/make_scratch.sh`, stream copies of the corpus.

| file | plan (chrome / appletv / legacy) | video pkts | audio | status |
|---|---|---|---|---|
| seeds/seed_h264_ts.ts | Direct stream ×3 | 144/144 | AAC copy | ok |
| features/h264_aac.m2ts | Direct stream ×3 | 719/719 | AAC copy | ok |
| probe-fixtures/hevc_mp2_eac3.ts | Audio conv. / Audio conv. / Transcode | 72/72 | MP2 → AAC; E-AC-3 copy on Apple TV | ok |
| probe-fixtures/h264_lpcm.m2ts | Audio conversion ×3 | 72/72 | LPCM → AAC, E-AC-3 5.1 (ffmpeg by PID) | ok |
| features/mpeg2_ac3.ts | Transcode ×3 (rejected: MPEG-2) | 899/899 | AC-3 → AAC | ok |
| h264_aac_ac3.ts | Audio conv. / Direct ×2 | 1439/1439 | AAC copy, AC-3 copy or AAC (seam 1.00) | ok |
| h264_ptswrap.ts (33-bit wrap 3 s in) | Direct stream ×3 | 1439/1439 | AAC copy | ok |
| h264_discont.ts (3 joined recordings: clock restarts, then +1000 s) | Direct stream ×3 | 1539/1539 | AAC copy | ok (0 ffmpeg errors; the source itself has 141) |
| h264_dts.ts | Audio conversion ×3 | 719/719 | DTS → AAC 0.67 / E-AC-3 1.00 | ok |
| h264_truehd.m2ts | Audio conversion ×3 | 719/719 | TrueHD → AAC 1.00 / E-AC-3 0.46 | ok |
| h264_latm.ts | Audio conversion ×3 | 1439/1439 | LATM → AAC 1.00 | ok |
| h264_mp3.ts | Audio conversion ×3 | 1439/1439 | MP3 → AAC 0.99 | ok |
| hevc_hdr10_eac3.m2ts (4K HDR10) | Audio conv. ×2 / Transcode | 719/719 (copy) | E-AC-3 copy / AAC | ok / ok / **error** (D.5) |
| hevc10_opengop_10min.ts | Direct ×2 / Transcode | 14386/14386 (copy) | AAC copy | ok / ok / **error** (D.5) |
| long_2h_360p.ts | Direct stream ×3 | 172627/172627 | AAC copy | ok |
| vfr_section.ts (59.94 section in 23.976) | Direct stream ×3 | 288/288, 0 DTS errors | AAC copy | "FAIL": ffmpeg's null muxer reports the same 100 errors decoding the *source* (frames closer than its 1/24 s time base) |

- Every remux has exact packet counts, strictly increasing DTS, CTS ≥ 0, and A/V offsets of 0.00–0.05 ms.
- A final run with the branch head over the 13 short files (`validate/v4.json`, load 8–32) repeats these results: 36/39 ok, and the 3 others are `vfr_section.ts`'s null-muxer artifact.
- The **legacy** column's video transcodes go through ffmpeg's `-ss` into the TS. That path is unchanged from main; D.5 explains the two errors.

**Unit and corpus tests** (in `src/ts/tests.rs` and the module tests; they skip when the SSD is absent):
- PTS wrap and two discontinuities, synthesized from `seeds/seed_h264_ts.ts`: one continuous timeline, every frame and every AAC frame exactly once.
- `plan_from_probe` equals the engine's plan for every TS/M2TS × 6 presets, with the same track numbers as flux-probe.
- M2TS HEVC + MP2 + E-AC-3 and LPCM plans.
- The MPEG-2 fallback carries the rejection warning.
- `ts::video_input` decodes every frame from a keyframe on.
- Units: packet sync recovery, continuity counters, PES reassembly, 33-bit unwrap, ADTS/MPA framing across PES, `avcC`/`hvcC` from real parameter sets, constant-rate detection (incl. millisecond timestamps and VFR), reorder offset, clock jumps.

**Browser** (Chrome 155 + hls.js 1.7.3 with the web client's settings; `browser/run.mjs` copied to `tmp/d-hls-perf/browser/` so that its profile lives under `tmp/d-hls-perf/`):
- **15/15 ok** over the 15 TS/M2TS files above, minus the MPEG-2 one (`bench/results/flux-d-hls-perf-browser-chrome-20261020-050922.json`; load 88 → 11).
- 0 fatal hls.js errors, 0 `video.error`, 0 dropped frames.
- TTFF median 231 ms (p95 434). Seek to first frame median 25 ms (p95 185) over 75 seeks; the largest were the 2 h file at 3600 s (480 ms) and the 10 s-GOP HEVC (≈ 190 ms).
- A/V drift p50 −7…+5 ms. Audio switches 4/4. The I-frame player showed keyframes on all 13 files it ran on (the two others have a single keyframe).
- Non-fatal events: `bufferSeekOverHole` once (the initial gap of D, C.1 finding 7) and `aborted` once.

**Gates:**
- `cargo test -p flux-hls`: 87 unit + 17 integration tests.
- Workspace: `cargo build --workspace --all-targets`; `cargo clippy --workspace --all-targets` with 0 warnings; `cargo test --workspace` with **481 passed, 0 failed, 4 ignored**; `cargo fmt --all --check` clean.

### D.4 Engine and source fixes (review findings)

- **PB-4: sources don't pin files.** `Source` opens the file on demand. One reaper thread closes the descriptor:
  - after 30 s without reads (`EngineConfig::source_idle_timeout`, `Source::set_idle_timeout`);
  - at once when the file is unlinked (`nlink == 0`);
  - when its path names another inode (checked every 2 s).
  
  The next read reopens the file and refuses it if its size or mtime changed. Test: `deleted_file_is_closed_and_its_space_freed` finds no descriptor left on the deleted inode (`/dev/fd`), within 0.5 s.
- **PB-3:** the WebVTT cache key is `(path:size:mtime, track)` (`asset_key`), like every other engine cache. Test: `webvtt_cache_follows_the_file_identity`.
- **PB-6:** `warm()` produces the video and the renditions of the plan's first audio track (`HlsEngine::warm_renditions`). On Apple TV that is its E-AC-3 and its AAC, one of which the device picks. Test: `warm_produces_only_the_default_audio` (0 transcodes for the AC-3 track nobody plays).
- **`warm()` keeps its product.** Remuxed segments it produces are read into memory and kept in a byte-budgeted LRU (`EngineConfig::warm_cache_bytes`, 64 MB, `CacheUsage::warmed`). `media_segment` serves from it, or awaits the warm production in flight, instead of producing the segment a second time. Test: `warm_keeps_the_segments_it_produced` (no source reads for the request that follows, same bytes).
- **PB-11.** Transcoded segments must not depend on request history.
  - **Audio: made deterministic.** Prefetched segments now use the same two-part partition as on-demand ones. The parts run one after the other on the single prefetch permit, and the encoder is deterministic. Test: `transcoded_audio_bytes_do_not_depend_on_prefetch`.
  - **Video: weak ETags.** VideoToolbox runs are not bit-exact, and a piece's rate control depends on its run. Fixed run boundaries would add up to three pieces of encode to a seek and still wouldn't make VT deterministic. So `HlsEngine::is_deterministic(plan, res)` is false for a video transcode's init and segments, and flux-server should send `ETag: W/"…"` for those (proposed below).
  - Timing and frame counts are exact either way. The "pure function" claim now reads: byte-identical for remux and audio transcodes, equivalent for video transcodes.

### D.5 Known gaps

- **Video transcode from TS.** The video transcoder still seeks with `-ss T_k -noaccurate_seek` into the TS. ffmpeg's TS seek lands on any PES by DTS, not on the keyframe, and HEVC then fails on missing references:
  - the 4K HDR10 M2TS lost 48 of 143 frames in its last run ("encoder stopped after 2 of 3 pieces");
  - the open-GOP HEVC TS exited with RPS errors.
  
  This is the same path main used for every TS. `ts::video_input` gives ffmpeg the video PID from the keyframe's own packet (with PAT/PMT) on stdin instead, and the test shows that decodes every frame. Wiring it into `vtranscode.rs` is proposed. Copies are not affected.
- **TS indexing reads the whole file** at first play (27 s for a cold 5.8 GB file on this disk). Proposed: index TS/M2TS at scan time, or in a post-scan job.
- **Rejected inputs fall back to transcoding:** PAFF (field-coded) H.264, changing parameter sets, multi-program captures, scrambled streams. They are not remuxed.
- **Subtitles in TS** (DVB, PGS, teletext) are not converted. Dolby Vision in TS plays as its base layer: the 0xB0 descriptor is not read.
- **VFR TS** uses a 90 kHz timescale. Its transcode pieces then fall back to one per segment, as for VFR Matroska.
- **`hls-validate`** counts ffmpeg's null-muxer errors that the VFR source produces too (`vfr_section.ts`). Comparing against the source's own count would fix that.

### D.6 Proposed changes outside this branch

1. **flux-server `media.rs`:** use `HlsEngine::is_deterministic(&entry.plan, &res)` to send `ETag: W/"{plan}/{rest}"` for transcoded video (PB-11). `If-None-Match` already compares weakly; `If-Range` must not match a weak tag.
2. **`vtranscode.rs`** (hls-correct): for `Layout::Ts`, replace `-ss T_k -noaccurate_seek -i <file>` with `-f mpegts -i pipe:0` fed by `ts::video_input(asset, seg_kf, Some(next_run_kf))`, written from a blocking task as the audio transcoder feeds stdin. The input's start time is the keyframe's PTS, so `trim`/`setpts` stay as they are.
3. **flux-scan / jobs:** build and store the playback index of TS/M2TS files after the scan (a full read each), so a first play doesn't wait for it.
4. **`INDEX_VERSION`:** this branch bumps it to 4. If hls-correct bumps it too, the merge takes the next number.
5. **`browser/run.mjs`:** a `--tmp DIR` for the browser profile (this run used a copy).
6. **`hls-validate`:** baseline ffmpeg errors on the source for VFR inputs (D.5).

### D.7 Re-running

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/r-playback TMPDIR=$FLUX_WORK/tmp/d-hls-perf/tmp
W=$FLUX_WORK; D=$W/tmp/d-hls-perf
zsh $D/scripts/make_scratch.sh                                  # scratch TS/M2TS from the corpus
nice -n 19 cargo build --release -p flux-hls --bins --examples -j 3
R=$CARGO_TARGET_DIR/release
nice -n 19 $R/hls-validate --presets chrome,appletv,legacy --out $D/validate/vN --json $D/validate/vN.json \
  $W/media/seeds/seed_h264_ts.ts $W/media/probe-fixtures/{hevc_mp2_eac3.ts,h264_lpcm.m2ts} $W/media/features/{h264_aac.m2ts,mpeg2_ac3.ts} $D/media/*.ts $D/media/*.m2ts
zsh $D/scripts/bench.sh before after && python3 $D/scripts/aggregate.py   # bin-before = main 1fbcb63
nice -n 19 $R/examples/dumpidx FILE.ts                            # index facts and time
nice -n 19 cargo test -p flux-hls --lib -j 3                      # ts::tests, engine::tests, source::tests
```
