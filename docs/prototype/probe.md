# flux-probe: prototype report

**Crate:** `crates/flux-probe`. **Contract:** `flux_core::media` (`ProbeResult`, `Track`, `HdrInfo`, …).
**Research:** research/02 §4, §5 and §9; research/08 §3.1–3.2; research/03 §4.2.

## Summary

flux-probe is a **sans-IO, range-read-driven header probe**. It reads the
headers of MKV/WebM, MP4/MOV, MPEG-TS/M2TS and AVI files and produces a
`flux_core::ProbeResult`. It never decodes.

Headline results:

| Goal | Result |
|---|---|
| Agrees with ffprobe | **0 mismatches** on 437 files: features 20/20, seeds 7/7, edge-case fixtures 16/16, library-tiny 393/393. The 394th library-tiny file (the 4-byte `.hidden.mkv`) is rejected by both flux-probe and ffprobe. |
| 2–4 reads per typical file | **1 read** for every MKV/WebM without attachments, every faststart MP4 and every AVI. **2** for moov-at-end MP4 and TS. **3–4** for a 4.4 MB `moov`, M2TS, fragmented MP4 and an MKV with fonts. Mean over the 14,761-file library: **1.26 reads, 105 KiB**. |
| Speed vs ffprobe with Jellyfin's arguments | Same 200-file sample, warm cache: **48 µs vs 35.7 ms** p50 (≈ 740×). Parse CPU alone is **21 µs p50**. |
| Full library (14,761 media files) | **4.3 s at 8 threads** (3,434 files/s), I/O-bound at the disk floor. ffprobe would take ≈ 164 s at 8-way (90 files/s). |
| HDR10 metadata from SEI | `G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)`, MaxCLL 1000, MaxFALL 400 recovered on both test files. They come from the `hvcC` SEI arrays (no extra read) and, independently, from the first keyframe by the second pass. |
| Robustness | Zero-byte, 4-byte, truncated, garbage and ≈ 5,400 mutated files per run all return an error, never a panic. An opt-in deep fuzz run (3,000 rounds × 36 files ≈ 108k mutated probes) passed in 494 s. All of it runs in debug builds, where integer overflow panics, so overflows would be caught. |

All numbers were measured on a shared, heavily loaded machine (load average 25–55 during every run). The lead should re-run the benchmarks on a quiet machine.

## What I built

About 7.5k lines of library code (including unit tests), 1.0k of integration tests and 0.6k of binaries:

| File | Role |
|---|---|
| `prober.rs` | `Prober` (header pass) and `FrameSeiProber` (keyframe pass): the sans-IO state machines, read policy and budgets. |
| `store.rs` | Fed-bytes store (merged runs), the parse context `Ctx`, the soft read budget and the "replay" design (below). |
| `sniff.rs` | Magic-byte sniffing. `ContainerHint` from the extension (it only sizes the first read). |
| `mkv.rs` | EBML / Matroska / WebM. Second-pass locator for the first block of the first cluster. |
| `mp4.rs` | ISO-BMFF / QuickTime box walker, piecewise `moov`, fragmented-MP4 duration, first-sample locator. |
| `ts.rs` | MPEG-TS / M2TS: PAT/PMT, descriptors, first-PES codec sniffing, PTS duration. |
| `avi.rs` | RIFF `hdrl`: `avih`, `strh`/`strf`/`strn`, OpenDML `dmlh`. |
| `codec/{h264,hevc,av1}.rs` | `avcC`/SPS+VUI, `hvcC`/SPS+VUI (incl. `st_ref_pic_set`, scaling lists), `av1C`, sequence-header OBU, metadata OBUs. |
| `codec/sei.rs` | SEI 137 (mastering), 144 (CLL), T.35 HDR10+ / DoVi detection. |
| `codec/audio.rs` | AAC ASC/ADTS, `dac3`/`dec3`, AC-3/E-AC-3 sync frames, DTS core (+ DTS-HD substream/XLL check), TrueHD major sync, Opus, FLAC STREAMINFO, Vorbis id header, MPEG audio, ALAC, BD-LPCM. |
| `codec/{dovi,mpegvideo,riff}.rs` | DoVi config record / TS descriptor, MPEG-2 sequence header and extension, `BITMAPINFOHEADER`/`WAVEFORMATEX` and FourCC maps. |
| `hdr.rs` | Colour merge (container wins unless "unspecified") and Sdr/Hdr10/Hdr10+/Hlg/DolbyVision classification. |
| `driver.rs` | Blocking drivers: the `RangeRead` trait, `FileSource` (`pread`), the `PageCache` (64 KiB pages), `probe_file*`, `probe_frame_sei`. |
| `ffprobe.rs` | ffprobe subprocess: fallback, oracle and JSON → `ProbeResult` mapping. Also an optional first-frame HDR read for the oracle. |
| `bin/probe-diff.rs`, `bin/probe-bench.rs` | Differential tester and benchmark. |
| `tests/gen-fixtures.sh` | Builds 18 edge-case fixtures into `$FLUX_WORK/media/probe-fixtures`. |

### Design: "replay" sans-IO

Container parsers are written as plain sequential code over a `Ctx`. When they
touch bytes that have not been fed yet, they return `PErr::Need{offset, len}`.
`Prober` turns that into `Step::Need`. When the driver feeds the bytes, the
parse is **re-run from the start** over the larger store.

Header parsing costs microseconds, so replaying it 1–4 times is free. In
exchange, every parser is a straight line of `?`s instead of a hand-written
state machine per container. The only measurable cost is TS: the head scan is
replayed per feed, which costs up to 1.2 ms CPU for a 1 MiB M2TS head.

Two features keep the read count low:

- **A `Need` asks only for the part not already cached.** The driver then
  rounds it to whole pages and feeds everything it read.
- **Optional sections never block the result.** Chapters, attachments and
  tags run under a **soft read budget** of 24 reads; past it they become
  warnings. A hard budget (64 reads, 64 MiB) rejects pathological files.

### Read policy per container

**Matroska / WebM**
1. Read 64 KiB.
2. Walk the top level for free over cached bytes: EBML → Segment → SeekHead, Info, Tracks, Tags…
3. The SeekHead (and any nested SeekHead) locates everything else. **Cues and Clusters are never read.** `has_index` means the SeekHead lists Cues.
4. Without a SeekHead, fall back to a bounded linear walk with reads (at most 256 elements).
5. Unknown-size Segment and Cluster elements, Void and CRC-32 are handled.
6. Attachments: only the `AttachedFile` child headers are read, never `FileData`. That costs one hop per font that lies past the cache.
7. Tags are parsed only if they are already cached. The title is normally in `Info`.

**MP4 / MOV**
1. Walk the top-level box headers, seeking over `mdat`.
2. A tail of 256 KiB or less is read in one go, which typically gets a moov-at-end in a single read.
3. A `moov` of 256 KiB or less is read whole. A larger one is read **piecewise**: about one read per `trak`, plus `udta`.
4. Inside `stbl`, only `stsd` is read. `stts` is parsed only if already cached (as a frame-rate fallback). `stsz`/`stco`/`ctts`/`stss` are never read.
5. `has_index` is set for any non-fragmented file with a `stbl`, because the sample tables are mandatory there.
6. Fragmented MP4: the duration comes from the last `moof` (`tfdt` plus the `trun` durations). That `moof` is located through `mfro` → `mfra/tfra`, with a tail scan as the fallback.

**MPEG-TS / M2TS**
1. With a TS hint, read a 256 KiB head. Grow it once to 1 MiB if the PMT, or any A/V stream's first PES, is still missing. ffmpeg muxes audio 50–430 KB late.
2. Read a 64 KiB tail, grown to 1 MiB if it holds no PTS.
3. Duration = max last PTS − min first PTS, with 33-bit wrap handling.

**AVI**: one read of `hdrl`.

### HDR

1. Classify from the merged colour: container `Colour`/`colr` wins over the VUI unless a field is "unspecified".
2. DoVi config (`dvcC`/`dvvC`/`dvwC`, MKV `BlockAdditionMapping`, TS descriptor 0xB0) or a DoVi RPU → `DolbyVision`.
3. PQ → `Hdr10`, or `Hdr10Plus` once a T.35 SEI is seen. HLG → `Hlg`. Anything else is `Sdr`.
4. Mastering and CLL come from, in order:
   1. container boxes/elements (`mdcv`/`clli`, MKV `MasteringMetadata`/`MaxCLL`);
   2. the **SEI NAL arrays inside `hvcC`**, where x265 puts them, so no extra read is needed;
   3. if still missing (or for HLG/DoVi without mastering), `needs_frame_sei = true`.
5. The **second pass** (`FrameSeiProber`, run automatically by `probe_file` when needed) locates the first keyframe:
   - MKV: the first block of our track in the first cluster(s).
   - MP4: sample 1 = the first `stco` entry, sized by the first `stsz` entry.
6. It walks the length-prefixed NALs. Non-VCL units are read and parsed (SPS, SEI 137/144/T.35, NAL 62 DoVi RPU). **Slice data is skipped by length and never read**, so a trailing RPU costs at most one small extra read.
7. AV1 uses metadata OBUs (HDR_CLL, HDR_MDCV converted to SEI units, T.35).
8. TS gets the same facts from the first PES during pass one.

### Why hand-rolled bit parsers

I did not use `h264-reader`, `hevc_parser`, `dolby_vision` or `scuffle-*`:

- We need a small read-only subset: profile, level, depth, size, VUI colour/timing and three SEI payloads. That came to about 1k lines for H.264/HEVC/AV1/SEI, and 2.1k for all codec configs, audio included, with their unit tests.
- Every accessor returns `Option`, so hostile input cannot panic. Partial buffers (truncated reads) degrade gracefully: a truncated `avcC` keeps profile/level.
- No extra dependencies. The only new dependencies are `thiserror`, `serde` and `serde_json` (for the ffprobe JSON), all workspace-pinned.
- `dolby_vision` (RPU conversion) belongs in flux-hls, for the P7→8.1 rewrite, not in the scanner.

## Public API

```rust
// sans-IO
pub struct Prober;
impl Prober {
    pub fn new(file_size: u64, hint: ContainerHint) -> Self;
    pub fn with_read_size(file_size: u64, hint: ContainerHint, read_size: u64) -> Self;
    pub fn start(&mut self) -> Result<Step, ProbeError>;                     // first Need
    pub fn feed(&mut self, offset: u64, data: &[u8]) -> Result<Step, ProbeError>;
    pub fn into_frame_sei(self) -> Option<FrameSeiProber>;                    // after Done
    pub fn io(&self) -> (u32 /*feeds*/, u64 /*bytes*/);
}
pub enum Step { Need { offset: u64, len: u32 }, Done(ProbeResult) }
pub struct FrameSeiProber;  // start()/feed() -> SeiStep
pub enum SeiStep { Need { offset: u64, len: u32 }, Done(FrameSei) }
pub struct FrameSei { mastering, cll, hdr10plus, dovi_rpu, color, reads, bytes_read }
impl FrameSei { pub fn apply(&self, r: &mut ProbeResult); }
pub struct ContainerHint(pub Option<Container>);   // ::from_path / ::from_extension / ::NONE
pub fn sniff(head: &[u8]) -> Option<Container>;

// blocking drivers
pub trait RangeRead { fn size(&self) -> u64; fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize>; }
pub struct FileSource;                  // pread
pub struct PageCache<R>;                // 64 KiB pages (or with_page_size), counts reads/bytes
pub fn probe_file(path: &Path) -> Result<ProbeResult, ProbeError>;          // header + SEI pass if needed
pub fn probe_file_with(path: &Path, opts: &ProbeOptions) -> Result<ProbeResult, ProbeError>;
pub fn probe_source<R: RangeRead>(src: R, hint: ContainerHint, opts: &ProbeOptions) -> Result<ProbeResult, ProbeError>;
pub fn probe_cached<R: RangeRead>(cache: &mut PageCache<R>, hint: ContainerHint, opts: &ProbeOptions) -> Result<ProbeResult, ProbeError>;
pub fn probe_frame_sei(path: &Path, result: &mut ProbeResult) -> Result<(), ProbeError>;
pub struct ProbeOptions { pub frame_sei: FrameSeiMode /*IfNeeded|Always|Never*/, pub ffprobe_fallback: bool, pub read_size: u64 }
pub enum ProbeError { Io, Empty, UnknownFormat{magic}, Truncated{..}, Malformed{..}, Unsupported, Budget{..}, BadFeed, Ffprobe }
pub const MEDIA_EXTENSIONS: &[&str]; pub fn is_media_path(&Path) -> bool;

// oracle / fallback
pub mod ffprobe { pub fn probe(&Path) -> Result<ProbeResult, ProbeError>; pub fn run_json(..); pub fn from_json(&Value, ext) -> ProbeResult;
                  pub fn add_first_frame_hdr(&Path, &mut ProbeResult); pub const DEFAULT_ARGS, JELLYFIN_ARGS; pub fn codec_of(&str) -> Codec; }
```

### Integration notes

**flux-scan**
- Call `probe_file(path)`. It does the SEI pass automatically. It never spawns ffprobe unless `ProbeOptions::ffprobe_fallback` is set.
- `UnknownFormat` covers FLV, MPEG-PS/VOB, ASF and OGM. Route those to a fallback worker.
- To reuse the fingerprint's first-64 KiB read, pass a shared `PageCache` to `probe_cached`. The probe's first read then costs zero I/O.
- `bytes_read`/`reads` in the result are real I/O.

**Conventions**
- `Track.language` is "as found". MKV `Language` defaults to `eng` per RFC 9559, and ffmpeg writes `und` explicitly. MP4 has `und`. TS uses its ISO-639 descriptor.
- `Track.default` follows MKV `FlagDefault` (default 1) and MP4 `tkhd` enabled. It is false for TS and AVI.
- Audio `bit_depth` is set only for PCM, FLAC, ALAC and TrueHD.
- Opus `sample_rate` is always 48000, as ffprobe reports.
- `codec_tag` is the raw id: `V_MPEGH/ISO/HEVC`, `hvc1`, `0x1b`, `xvid`, `0x0055`…

**flux-hls**: `has_index` and `faststart` tell the planner whether play-time index loading is cheap.

## How to run

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/agent-probe
crates/flux-probe/tests/gen-fixtures.sh              # once: edge-case fixtures (needs ffmpeg)
cargo test -p flux-probe                              # 66 tests; skips media tests if the SSD is absent
cargo test -p flux-probe --test robustness -- --ignored   # deep fuzz (~108k mutated probes)
cargo build --release -p flux-probe --bins
R=$CARGO_TARGET_DIR/release
$R/probe-diff $FLUX_WORK/media/features        # exit 1 on any mismatch
$R/probe-diff $FLUX_WORK/media/library-tiny --frame-hdr never
$R/probe-bench $FLUX_WORK/media/library --threads 8 --ffprobe-sample 200 [--read-size 16384]
cargo run --release -p flux-probe --example dump -- FILE…   # ProbeResult as JSON (SEI=always to force pass 2)
```

Results JSON is written to `$FLUX_WORK/bench/results/`:

- `flux-probe-diff-{features,seeds,probe-fixtures,library-tiny}.json`
- `flux-probe-bench-{features,library-tiny,library}.json`

## Results

### Differential test vs ffprobe 9.0.2

`probe-diff` compares, per file:

- container (MKV ≡ WebM and MP4 ≡ MOV, because ffprobe cannot tell them apart);
- duration (±max(100 ms, 0.5%));
- the ordered V/A/S track kinds;
- codec (ASS ≡ SSA);
- width and height;
- sample aspect ratio (core v2; unstated ≡ 1:1, see [core-v2.md](core-v2.md) §14);
- bit depth;
- channels and sample rate;
- language (`und` ≡ none);
- default and forced flags;
- HDR format, mastering and CLL, whenever ffprobe reports them.

ffprobe 9 does **not** report `color_transfer` at stream level for the HEVC
HDR10 MKV, even though the SPS VUI signals PQ. So for HDR-looking files the
oracle also reads ffprobe's first frame (`-show_frames -read_intervals %+#1`).

| Corpus | Files | Result |
|---|---|---|
| features | 20 | 20 ok |
| seeds | 7 | 7 ok |
| probe-fixtures | 16 | 16 ok |
| library-tiny | 394 | 393 ok. `.hidden.mkv` (4 bytes) is rejected by both. |

Core v2 re-run (with SAR, one shared extension list): features 20/20, seeds 7/7, library-tiny 393/393 (+1 rejected by both), and probe-fixtures plus 8 anamorphic fixtures 24/24, with `.flv`/`.mpg` now listed as `fallback` (flux answers `UnknownFormat`; the scanner sends those to ffprobe). See [core-v2.md](core-v2.md).

The fixtures cover:

- **Matroska:** a live/unknown-size MKV, VP9 + Opus WebM, and an MKV with ASS + 2 font attachments.
- **HDR:** H.264 10-bit PQ with mastering only in SEI (MKV and MP4), and HEVC HLG.
- **MP4/MOV:** fragmented MP4, QuickTime `.mov` with 5.1 AC-3 and a title, MP4 with E-AC-3 5.1 and Nero chapters, 2-hour MP4s (moov-at-end and faststart, 4.4 MB `moov`), and an MP4 named `.mkv`.
- **TS:** HEVC + MP2 + E-AC-3 TS with languages, and Blu-ray-style M2TS with LPCM.
- **AVI:** H.264 + AC-3 AVI and MJPEG + PCM AVI.
- **ffprobe fallback:** FLV and MPEG-PS.

Byte-patched tests add three more structures:

- MKV with its **SeekHead turned into Void** (linear walk; same tracks and chapters);
- **unknown-size Segment and Cluster** (the keyframe pass still finds the block);
- **hvcC with its SEIs hidden** (forces the keyframe pass, which takes 2 reads).

**How honest is "0 mismatches"?** I built the probe against these files, and every one is ffmpeg-muxed. Not covered:

- mkvmerge, MakeMKV, HandBrake, camera MOVs and real Blu-ray rips;
- real Dolby Vision or HDR10+ content (those paths are covered only by synthetic-structure unit tests).

`probe-diff` also does not compare profile, level, frame rate, titles, chapters, attachments or bitrates. The tests check several of those directly: chapters, titles, attachments, profile/level and frame rate on the feature clips.

### I/O per file (reads / bytes, 64 KiB pages)

| Files | Reads | Bytes |
|---|---|---|
| All 16 feature MKV/WebM, incl. 2-hour, 4K HDR and 424 MB files | 1 | 64 KiB |
| MP4 faststart, incl. HEVC HDR10 | 1 | 64 KiB |
| MP4 moov-at-end (60 s) | 2 | 112 KiB |
| 2-hour MP4, faststart / moov-at-end (4.4 MB `moov`) | 3 / 4 | 320 / 325 KiB |
| Fragmented MP4 | 3 | 461 KiB |
| MPEG-TS (features, seed, fixture) | 2 | 330–355 KiB |
| M2TS with audio PES 427 KB in | 3 | 1.1 MiB |
| AVI | 1 | 64 KiB |
| MKV with 2 font attachments (1.1 MB of fonts) | 3 | 320 KiB |
| H.264 PQ, SEI pass included | 1 | 64 KiB (the first cluster/sample is in the first page) |

For comparison, ffprobe reads the whole 4.49 MB `moov` and up to 17.7 MB to 34.6 MB of TS with Jellyfin's arguments (research/02 §5.1).

### Throughput (release build, 8 threads, M1 Max, load average 40–55)

| Corpus | flux 1 thread, warm: files/s (p50) | flux 8 threads: files/s | flux parse CPU only, p50 / p90 | ffprobe Jellyfin args, 1 / 8 threads: files/s (p50) |
|---|---|---|---|---|
| features (20) | 2,037 (44 µs) | 9,839 | 20.6 / 101 µs | 16.9 (43 ms) / 98 |
| library-tiny (394) | 20,301 (39 µs) | 53,147 | 21.3 / 46 µs | 20.1 (44.5 ms) / 113 |
| library (14,761) | 815 (535 µs) [2] | 3,434 | 20.9 / 271 µs | 26.7 (35.7 ms) / 90 |

On the same ffprobe samples (warm), the per-file p50 speedup is:

- **×1,164** on features;
- **×1,142** on library-tiny;
- **×744** on the library.

[2] The full library does not stay in the page cache under this machine's
memory pressure, so the "warm" pass is really I/O. I measured the raw floor
on the same files: **open + one 64 KiB `pread` = 446 µs p50**, and flux's
p50 is 535 µs. **The probe runs at the disk's I/O floor.** Parsing is
≈ 21 µs. On the full library that means:

- flux probes all 14,761 files in **≈ 4.3 s** at 8 threads (≈ 18 s single-threaded);
- ffprobe with Jellyfin's arguments would need ≈ 164 s at 8 threads (≈ 550 s single-threaded);
- Jellyfin's whole cold scan of this library takes 1,047–1,063 s (benchmarks.md, 2026-10-20). *First quoted here as 1,442 s, from the loaded 2026-10-18 baseline.*

**Read-size experiment.** I measured raw `pread` latency on this USB SSD:

| Read size | p50 |
|---|---|
| 4 KiB | 270 µs |
| 16 KiB | 291 µs |
| 64 KiB | 459 µs |
| 256 KiB | 1,068 µs |

Most MKV/AVI headers fit in 16 KiB. I made the read/page size a knob (`ProbeOptions::read_size`, `Prober::with_read_size`, `PageCache::with_page_size`). End-to-end runs at 16 KiB vs 64 KiB were too noisy on this machine to call, because the page cache was being evicted between runs. The default stays at **64 KiB**, which minimises round trips: the right trade-off for HDD, NAS and Truffle.

Recommendation for flux-scan: use 16 KiB for local SSDs and 64–256 KiB for network or spinning disks. Re-measure on a quiet machine.

## Known gaps

**Untested formats**
- No real Dolby Vision or HDR10+ files. Detection paths are covered by unit tests on synthetic MKV `BlockAdditionMapping`/MP4 `dvvC`/TS descriptor records and T.35 payloads only.
- RPU detection without a DoVi config record relies on the keyframe pass, which runs only for PQ/HLG tracks.

**Codec details we don't extract**
- MKV `A_DTS`: the DTS-HD MA/HRA profile is only detected in TS, where the first frame is in hand. In MKV it would need a block read; only `A_DTS/LOSSLESS` and `A_DTS/EXPRESS` codec ids are mapped.
- TrueHD Atmos and E-AC-3 JOC are not flagged.
- VP9 profile and bit depth when CodecPrivate is absent (ffmpeg omits it). That would need the first frame's uncompressed header.
- AVI codec configs (profile, level, depth from the first `movi` frame) and the MPEG-4 Part 2 profile are not parsed.

**MP4**
- `elst` is not parsed. `ProbeResult` has no field for a presentation offset, so flux-hls reads it at play time. See the proposed core changes.
- QuickTime chapter text tracks (`tref/chap`) need sample tables and are not read; only Nero `chpl` chapters are. Such tracks are excluded from `tracks`, with a warning.
- Fragmented MP4: `has_index = false`, and the keyframe SEI pass returns `Unsupported`.

**Matroska**
- `has_index` comes from the SeekHead only. Files with no SeekHead but Cues at the end report `false`.
- Tags are parsed only when already cached, so per-track `BPS` bitrates are often absent.
- Ordered chapters and segment linking are only warned about.
- Header-stripping `ContentCompression` disables the keyframe pass. Encryption is only warned about.
- Each attachment beyond the head costs one read. Font-heavy files hit the soft budget (24 reads) and return a partial list plus a warning.

**MPEG-TS**
- No Blu-ray `.mpls`/`.clpi` parsing.
- No PCR/PTS discontinuity handling, so concatenated broadcast captures report a wrong duration.
- Duration omits the last frame (≤ 1 frame short, within tolerance).
- A PMT not found within the first 1 MiB is an error.
- The replay design rescans the head on every feed. That costs up to 1.2 ms CPU on a 1 MiB M2TS head and could become incremental.

**Process and measurement**
- The ffprobe fallback is a plain subprocess, with no timeout and no sandbox. flux-scan should run it in a bounded worker.
- Benchmarks ran at load average 25–55 with other agents compiling. "First pass" numbers are not truly cold, because I could not purge the cache. "Warm" numbers on the full library are I/O-bound.

## Proposed core changes (flux-core)

1. **`Track.codec_config: Option<Vec<u8>>`** (`avcC`/`hvcC`/`av1C`/ASC/`dOps`, 5 B to 2.5 KB). flux-hls needs these verbatim for init segments. The probe already holds them, so storing them avoids a header read at play time.
2. **`Track.reorder_depth: Option<u8>`**, from the SPS `max_num_reorder_frames` / `sps_max_num_reorder_pics`. This is flux-hls's CTS offset D (research/03 §4.4), and it is cheap to add to the SPS parsers.
3. **`Track.sample_aspect_ratio: Option<Rational>`**, from MP4 `pasp`, MKV `DisplayWidth`/`DisplayHeight` or the VUI. Anamorphic DVD and SD content needs it for correct display.
4. **`Track.start_offset_ms: Option<i64>`**, from MP4 `elst` (or the TS first-PTS delta), for A/V sync in remux.
5. **`Attachment.data_offset: Option<u64>`.** The probe knows where `FileData` starts, so fonts could be served by a range read without re-parsing the header.
6. **More disposition flags** (commentary, visual-impaired, original), which MKV and MP4 carry. They are useful for automatic track selection.
