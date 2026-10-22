# Portability and encoders

flux remuxes in process and only needs FFmpeg for what it cannot do itself:
audio transcodes (TrueHD, DTS, AC-3 and others → AAC), video transcodes
(→ H.264), frame posters, and indexing containers without an index (AVI,
some MPEG-TS). This page covers how flux finds FFmpeg, which video encoder
it picks, how to override that, and how the test suite runs on any machine.

## FFmpeg policy

- flux only ever runs `ffmpeg` and `ffprobe` as **separate processes**. It
  never links libav\*, so the flux binaries carry no FFmpeg code and no
  FFmpeg licence obligations.
- Release binaries use the **system FFmpeg**. `flux-server --check` reports
  what it can do (below).
- The Docker image bundles **jellyfin-ffmpeg** (GPL), with its licence and a
  link to its source. Its `tonemapx` filter gives the best software tone
  mapping, and its builds include the hardware encoders.

## Finding ffmpeg and ffprobe

| Setting | Flag | Variable | Default |
|---|---|---|---|
| ffmpeg | `--ffmpeg PATH` | `FLUX_FFMPEG` | `ffmpeg` on `PATH`, then `/usr/lib/jellyfin-ffmpeg`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin` |
| ffprobe | `--ffprobe PATH` | `FLUX_FFPROBE` | the `ffprobe` next to ffmpeg, then the same search |

Both are also TOML keys (`ffmpeg`, `ffprobe`). The server passes these
paths to everything that runs a tool: HLS audio and video transcodes, the
ffprobe index of AVI and rejected MPEG-TS files, the scan's ffprobe
fallback probe, and the frame-poster jobs (which wrap them in `nice -n 19`).
flux-probe's own fallback and the dev tools read `FLUX_FFPROBE` /
`FLUX_FFMPEG`.

## Choosing the video encoder

flux transcodes video to H.264 (High profile) only. On the first video
transcode, or in the background right after start-up, it probes the ffmpeg
binary once and caches the result (`flux_hls::encoders::FfmpegCaps`):

1. `ffmpeg -hide_banner -encoders`, `-decoders`, `-filters` and `-hwaccels`.
2. Each candidate, in this order, until one passes:

   | Order | Encoder | Probe |
   |---|---|---|
   | 1 | `h264_videotoolbox` | macOS only; a test encode |
   | 2 | `h264_nvenc` | a test encode |
   | 3 | `h264_qsv` | a test encode |
   | 4 | `h264_vaapi` | the render node exists (`/dev/dri/renderD128`), then a test encode |
   | 5 | `libx264` | a test encode (the software fallback) |

   A candidate this ffmpeg was not built with is skipped without a run. A
   test encode is ten 320×240 frames from `testsrc2` with the exact encoder
   options a transcode uses, so an ffmpeg too old for one of them (say
   `-forced_idr` on an old `h264_qsv`) rejects that encoder here instead of
   failing every transcode. A GitHub macOS runner (a VM) passes the
   `h264_videotoolbox` test encode and uses it; Linux runners have no GPU
   and land on libx264 (CI's `flux-server --check` step records this).
3. The first candidate that passes is used for every video transcode until
   the server restarts. With none, video transcodes fail with a message
   that points at `flux-server --check`; remux and audio transcodes still
   work.

### Overrides

| Setting | Flag | Variable | TOML |
|---|---|---|---|
| Encoder | `--video-encoder auto\|videotoolbox\|nvenc\|qsv\|vaapi\|libx264` | `FLUX_VIDEO_ENCODER` | `video_encoder = "…"` |
| VAAPI render node | `--vaapi-device PATH` | `FLUX_VAAPI_DEVICE` | `vaapi_device = "…"` |

A forced encoder is used even if its test encode fails (`--check` still
shows the failure). `auto` (the default) detects. `software`, `x264` and
`h264_*` names are accepted too. In code, `EngineConfig::video_encoder`
and `EngineConfig::vaapi_device` do the same, and `EngineConfig::default()`
reads both variables.

### The pipeline per encoder

Every transcode run has the same frame: seek to the source keyframe of the
run's first piece, `trim` + `setpts` to the piece, then the encoder's own
decode, scale, tone-map and rotate chain, then the encoder:

| Encoder | Decode | Scale | HDR → SDR | Rotation |
|---|---|---|---|---|
| VideoToolbox | `-hwaccel videotoolbox` (H.264/HEVC) | `scale_vt` | `scale_vt` colour conversion to BT.709 | `transpose_vt` |
| NVENC | `-hwaccel cuda` (8-bit H.264/HEVC, no rotation or tone mapping) | `scale_cuda` | software chain | software chain |
| QSV | `-hwaccel qsv` + `h264_qsv`/`hevc_qsv` decoder (no rotation or tone mapping) | `scale_qsv` | software chain | software chain |
| VAAPI | `-hwaccel vaapi` on the render node (H.264/HEVC) | `scale_vaapi` | `tonemap_vaapi` when present, else download → software → upload | `transpose_vaapi` when present, else download → software → upload |
| libx264 | software | `scale` | software chain | `transpose` / `hflip,vflip` |

The **software chain** scales first, then tone-maps with jellyfin-ffmpeg's
`tonemapx` (BT.2390) when the build has it, else with `zscale` (zimg) to
linear light, `tonemap=hable` and back to BT.709. A build with neither has
no tone mapping (HDR sources look washed out on SDR screens); `--check`
says so. Sources the hardware decoders are not trusted with (MPEG-2,
MPEG-4 Part 2, AV1, VP9, VC-1) decode in software on every path: a failed
hwaccel with hardware output frames has no software fallback in ffmpeg.

**Dolby Vision profile 5** is the exception to the table's HDR → SDR
column. It has no HDR10 or SDR base layer: the decoded picture stays in
Dolby's IPT colour space until the RPU reshapes it, and only `tonemapx`
does that (`apply_dovi`, on by default). So when the build has `tonemapx`,
such a source always takes the software chain, on VideoToolbox and VAAPI
too (`scale_vt` and `tonemap_vaapi` would leave it purple and green).
Without `tonemapx` nothing reshapes it, and `--check` says so. Frame
posters of these sources are tone-mapped by `tonemapx` as well; without
it they get no frame art.

Rate control is the same target on every encoder: the plan's bitrate on
average, peaks to 1.5×, a buffer of 2× (`-b:v`/`-maxrate`/`-bufsize`; NVENC
`-rc vbr`, VAAPI `-rc_mode VBR`). libx264 runs capped CRF instead: preset
`veryfast`, `-crf 23` under the same `-maxrate`/`-bufsize`. Every encoder
gets `-bf 0` (no B-frames: each piece's composition offset is the constant
D), `-g 1000000` and no scene-cut keyframes (`-sc_threshold 0`,
`-no-scenecut 1`), so the only IDRs are the ones `-force_key_frames` places
at piece boundaries (`-forced-idr 1` / `-forced_idr 1` make them IDRs on
x264, NVENC and QSV).

### What stays the same on every encoder

The stateless piece design (docs/prototype/hls.md) does not depend on the
encoder: pieces of about 2 s on the source's frame grid, a run of up to four
pieces per ffmpeg process, exact frame counts from the index's real frame
times, an IDR forced at every piece start, SPS/PPS in band in each piece's
first sample. Frame counts, timing and IDR positions are therefore the same
for every encoder. A piece's bytes depend on the run that produced it (rate
control carries over between a run's pieces) and, for hardware encoders, on
the session, so transcoded video is served with weak ETags
(`HlsEngine::is_deterministic`). With libx264 a piece requested cold is
byte-identical across runs and servers (`tests/synthetic.rs`).

The plan's `VideoOp::Transcode::hardware` stays the planner's "hardware
first" (so a plan, and its id, never depends on whether detection has run
yet); `--check`, the admin report and the server log say which encoder is
actually used.

## `flux-server --check`

On the development Mac (Homebrew ffmpeg 9.0.2):

```text
$ flux-server --check
flux-server 0.1.0 on macos aarch64
ffmpeg   /opt/homebrew/bin/ffmpeg  (ffmpeg version 9.0.2 Copyright (c) 2000-2026 the FFmpeg developers)
ffprobe  /opt/homebrew/bin/ffprobe  (ffprobe version 9.0.2 Copyright (c) 2007-2026 the FFmpeg developers)
video    h264 → h264_videotoolbox (hardware)
tonemap  HDR → SDR: scale_vt (hardware)
audio    aac encoder: yes
hwaccel  videotoolbox
encoders, in the order flux tries them:
  ok  h264_videotoolbox  test encode passed
  no  h264_nvenc         not built into this ffmpeg
  no  h264_qsv           not built into this ffmpeg
  no  h264_vaapi         not built into this ffmpeg
  ok  libx264            test encode passed
missing:
  - no Dolby Vision reshaping (needs jellyfin-ffmpeg's tonemapx): Dolby Vision profile 5 sources transcode with a purple/green picture
```

Every ffmpeg without `tonemapx` (upstream builds such as Homebrew's and
the Linux distributions') lists that Dolby Vision line, even with hardware
tone mapping. With `FLUX_VIDEO_ENCODER=libx264` the same machine reports
`h264 → libx264 (software, forced)` and also lists no tone mapping
(Homebrew's ffmpeg has neither `zscale` nor `tonemapx`) and no hardware
encoder. `--check` probes every candidate, not just up to the first that
passes, and exits 1 when ffmpeg or ffprobe cannot run (2 for a bad flag).
`--doctor` is an alias. Owners and admins get the same report as JSON from
`GET /api/v1/admin/media-tools`, from the server's cached detection.

## Platforms

flux builds on macOS and Linux (glibc, x86_64 and aarch64). How that was
checked: native build, clippy and tests on macOS (Apple Silicon); for
Linux, `cargo zigbuild --workspace --all-targets --target
x86_64-unknown-linux-gnu` (links every binary, test and example) and
`cargo zigbuild clippy` for x86_64 and aarch64, with zig 0.17 as the C
cross-compiler. jemalloc's build then needs zig's archiver
(`AR="zig ar"`, `RANLIB="zig ranlib"`); without it macOS's `ar` writes
an empty `libjemalloc.a` and the link fails. GitHub CI on Linux runners is
the final check.

- **macOS-only APIs are gated.** `malloc_zone_pressure_relief` (macOS) and
  `malloc_trim` (Linux glibc) behind `flux_catalog`'s release hook,
  `PRIO_DARWIN_BG` vs `setpriority(PRIO_PROCESS, 19)` for job threads,
  `kern.maxfilesperproc` only on macOS when raising `RLIMIT_NOFILE`,
  `statfs` file-system names (`f_fstypename` on macOS/BSD, `f_type` magic
  numbers on Linux), jemalloc's background purge thread on Linux and a
  stand-in thread elsewhere. Mille gates `getattrlistbulk` and FSEvents
  itself (statx and inotify on Linux).
- **Windows compiles but is not supported.** `cargo check --workspace
  --all-targets --target x86_64-pc-windows-gnu` passes: positioned reads
  use `seek_read`, ffprobe index scans are killed with `taskkill /T`
  instead of a process-group `SIGKILL`, the host name comes from
  `COMPUTERNAME`, jemalloc is off, and Unix-only tests (symlinks,
  permission bits) are compiled out. What is missing is file identity:
  std has no stable device/file index on Windows, so the "same file, not
  replaced or re-pointed" checks in flux-hls (`source.rs`) and the scan
  fall back to size and mtime. That needs a Windows identity source
  (volume serial + file index) before flux can be trusted there.
  Embedded remote access looks for `sidecar-slim.exe` next to the binary,
  but no Windows sidecar is packaged. Windows is not a release target.

## Tests anywhere

Nothing in `crates/` names a machine-specific path. Tests, examples and
benches take their inputs from the environment and skip, saying why, when
something is missing:

| Variable | What | Unset |
|---|---|---|
| `FLUX_TEST_MEDIA` | The real-media corpus: `features/`, `probe-fixtures/`, `library-tiny/` (+ `.manifest.jsonl`), `library/`, `playback-research/`, `seeds/` | real-media tests skip |
| `FLUX_BENCH_RESULTS` | Where benches and examples write result JSON (and find Jellyfin baselines) | results are printed, not written |
| `FLUX_TEST_CLIPS` | Cache of the clips made with ffmpeg | `<temp dir>/flux-test-clips-v1` |
| `FLUX_TMDB_EXPORTS` | TMDB daily ID exports (`movie_ids_*.json.gz`) | the export test skips |
| `FLUX_FFMPEG`, `FLUX_FFPROBE` | The tools | `PATH` and the usual locations |
| `FLUX_VIDEO_ENCODER` | Force an encoder for every engine a test makes | detect |
| `FLUX_SMOKE_WORK` | pipeline_smoke's catalogs | `<temp dir>/flux-integrate` |

The dev-only crate `crates/flux-test-media` holds the helpers. Its `clip()`
makes small synthetic files with ffmpeg's lavfi (`testsrc2` + `sine`, a few
seconds) on first use: H.264/AAC MP4, HEVC/AC-3 MKV, H.264/AAC and
open-GOP HEVC/AAC MPEG-TS, MPEG-4/MP2 AVI, HEVC Main 10 HDR10 MKV, and the
Phase D regression clips (variable frame rate, no DefaultDuration, a 1 s
start time, a 90° display matrix, anamorphic, 1080p60). So CI with a plain
ffmpeg (libx264, libx265) runs probe → index → HLS segment → transcode for
real:

- `flux-probe/tests/synthetic.rs`: every clip probes as made.
- `flux-hls/tests/synthetic.rs`: every clip plays as HLS (frame counts by
  ffprobe), and libx264 transcodes (forced, so macOS exercises the Linux
  software path) produce pieces that each start with an IDR, are
  contiguous, keep every source frame and are deterministic.
- `flux-hls/tests/phase_d.rs` and `merge_seams.rs`: the Phase D
  regressions on the generated clips, through whichever encoder is
  detected (`FLUX_VIDEO_ENCODER=libx264` to force the software path).
- `flux-hls::encoders` unit tests pin each encoder's filter chain and
  arguments.

CI runs these on Linux (libx264) and on a GitHub macOS VM with
`FLUX_VIDEO_ENCODER=libx264`. That VM's VideoToolbox passes the test
encode, but once flushed a run one frame short (287 of 288 frames in
`vfr_transcode_keeps_every_frame`; the run that ends a file has no
`-frames:v`, so its last piece takes what the encoder emits). VideoToolbox
is therefore verified on Apple hardware, by running the suite without
`FLUX_VIDEO_ENCODER`.

To run the suite against the corpus:

```sh
FLUX_TEST_MEDIA=/path/to/media cargo test --workspace
FLUX_VIDEO_ENCODER=libx264 cargo test -p flux-hls   # the software path on a Mac
```
