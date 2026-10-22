//! Test media for flux's tests, examples and benches (dev-only crate).
//!
//! Nothing here points at a machine-specific path. Three sources:
//!
//! - **The real-media corpus**, from `FLUX_TEST_MEDIA`: a directory laid out
//!   like the maintainer's corpus (`features/`, `probe-fixtures/`,
//!   `library-tiny/`, `library/`, `playback-research/`, `seeds/`, see
//!   `tools/`). Unset or missing, every test that needs it skips and says
//!   why ([`media`], [`media_dir`]).
//! - **Bench results**, written to `FLUX_BENCH_RESULTS` ([`bench_results`],
//!   [`write_result`]). Unset, results are printed and not written.
//! - **Synthetic clips** ([`clip`]): a few seconds of `testsrc2` + `sine`
//!   made with ffmpeg's lavfi on first use and cached in the temp dir, so
//!   CI exercises probe → index → HLS segment → transcode on real files.
//!   Skipped (with the reason) when ffmpeg or the encoder a clip needs is
//!   missing.
//!
//! ffmpeg and ffprobe come from `FLUX_FFMPEG` / `FLUX_FFPROBE`, else `PATH`,
//! else the usual Homebrew and system locations ([`ffmpeg`], [`ffprobe`]).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

mod mkv_lace;

/// The variable naming the real-media corpus directory.
pub const MEDIA_VAR: &str = "FLUX_TEST_MEDIA";
/// The variable naming the directory bench results are written to.
pub const RESULTS_VAR: &str = "FLUX_BENCH_RESULTS";

/// The corpus root from `FLUX_TEST_MEDIA`, if it is set and a directory.
/// Says once (on stderr) why real-media tests skip otherwise.
pub fn media_root() -> Option<PathBuf> {
    static NOTE: Once = Once::new();
    match std::env::var_os(MEDIA_VAR).filter(|v| !v.is_empty()) {
        Some(v) if Path::new(&v).is_dir() => Some(PathBuf::from(v)),
        Some(v) => {
            NOTE.call_once(|| {
                eprintln!(
                    "{MEDIA_VAR}={} is not a directory: real-media tests skip",
                    Path::new(&v).display()
                )
            });
            None
        }
        None => {
            NOTE.call_once(|| eprintln!("{MEDIA_VAR} is not set: real-media tests skip"));
            None
        }
    }
}

/// `$FLUX_TEST_MEDIA/<sub>`, or, when the corpus is not configured, a
/// relative path that never exists (`<FLUX_TEST_MEDIA unset>/<sub>`), so
/// that every `exists()`/`read_dir` check on it skips and the skip message
/// names the missing variable.
pub fn media_dir(sub: &str) -> PathBuf {
    match media_root() {
        Some(root) if sub.is_empty() => root,
        Some(root) => root.join(sub),
        None => Path::new(concat!("<", "FLUX_TEST_MEDIA", " unset>")).join(sub),
    }
}

/// A corpus file or directory (`rel` below `FLUX_TEST_MEDIA`), or `None`
/// with a printed reason when the corpus or the file is missing.
pub fn media(rel: &str) -> Option<PathBuf> {
    let root = media_root()?;
    let p = root.join(rel);
    if p.exists() {
        Some(p)
    } else {
        eprintln!("skipping: {} not present", p.display());
        None
    }
}

/// The bench result directory from `FLUX_BENCH_RESULTS` (created), or
/// `None` with a printed reason.
pub fn bench_results() -> Option<PathBuf> {
    let Some(dir) = std::env::var_os(RESULTS_VAR).filter(|v| !v.is_empty()) else {
        eprintln!("{RESULTS_VAR} is not set: results are not written");
        return None;
    };
    let dir = PathBuf::from(dir);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            eprintln!("{RESULTS_VAR}={}: {e}: results are not written", dir.display());
            None
        }
    }
}

/// Writes `contents` to `$FLUX_BENCH_RESULTS/<name>` and returns the path,
/// or prints why it didn't.
pub fn write_result(name: &str, contents: &str) -> Option<PathBuf> {
    let p = bench_results()?.join(name);
    match std::fs::write(&p, contents) {
        Ok(()) => {
            eprintln!("wrote {}", p.display());
            Some(p)
        }
        Err(e) => {
            eprintln!("could not write {}: {e}", p.display());
            None
        }
    }
}

/// A fresh scratch directory `<temp dir>/flux-tests/<name>-<pid>-<n>`
/// (removed first if it exists). The temp dir follows `TMPDIR`.
pub fn scratch(name: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join("flux-tests")
        .join(format!("{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Copies a file: an APFS clone (`cp -c`, instant, no data copied) on
/// macOS, a plain copy elsewhere or when cloning fails. Creates the parent.
pub fn clone_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if cfg!(target_os = "macos") {
        let cloned = Command::new("cp")
            .arg("-c")
            .arg(src)
            .arg(dst)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if cloned {
            return Ok(());
        }
    }
    std::fs::copy(src, dst).map(|_| ())
}

/// Copies a directory tree to `dst` (which must not exist): `cp -cR`
/// (clones) on macOS, `cp -a` on other Unix systems, a recursive copy
/// otherwise. Symlinks stay symlinks where `cp` does it.
pub fn clone_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if cfg!(unix) {
        let flag = if cfg!(target_os = "macos") { "-cR" } else { "-a" };
        let ok = Command::new("cp")
            .arg(flag)
            .arg(src)
            .arg(dst)
            .status()
            .is_ok_and(|s| s.success());
        return if ok {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "cp {flag} {} {} failed",
                src.display(),
                dst.display()
            )))
        };
    }
    copy_dir(src, dst)
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// A tool from `$<var>`, else `PATH`, else `/opt/homebrew/bin`,
/// `/usr/local/bin` or `/usr/bin`.
pub fn find_tool(name: &str, var: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let file = exe(name);
    let from_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(&file))
        .find(|p| p.is_file());
    from_path.or_else(|| {
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
            .iter()
            .map(|d| Path::new(d).join(&file))
            .find(|p| p.is_file())
    })
}

/// ffmpeg, or `None` with a printed reason.
pub fn ffmpeg() -> Option<PathBuf> {
    static TOOL: OnceLock<Option<PathBuf>> = OnceLock::new();
    let t = TOOL.get_or_init(|| find_tool("ffmpeg", "FLUX_FFMPEG")).clone();
    if t.is_none() {
        eprintln!("skipping: ffmpeg not found (FLUX_FFMPEG, PATH)");
    }
    t
}

/// ffprobe, or `None` with a printed reason.
pub fn ffprobe() -> Option<PathBuf> {
    static TOOL: OnceLock<Option<PathBuf>> = OnceLock::new();
    let t = TOOL.get_or_init(|| find_tool("ffprobe", "FLUX_FFPROBE")).clone();
    if t.is_none() {
        eprintln!("skipping: ffprobe not found (FLUX_FFPROBE, PATH)");
    }
    t
}

/// The encoders this ffmpeg lists (`ffmpeg -encoders`), cached.
pub fn ffmpeg_encoders() -> &'static [String] {
    static ENC: OnceLock<Vec<String>> = OnceLock::new();
    ENC.get_or_init(|| {
        let Some(ff) = find_tool("ffmpeg", "FLUX_FFMPEG") else {
            return Vec::new();
        };
        let out = Command::new(ff)
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .output();
        let Ok(out) = out else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                let flags = w.next()?;
                let name = w.next()?;
                (flags.len() == 6 && !flags.starts_with('-') && name != "=").then(|| name.to_string())
            })
            .collect()
    })
}

/// True when ffmpeg has encoder `name`; prints why not otherwise.
pub fn has_encoder(name: &str) -> bool {
    let ok = ffmpeg_encoders().iter().any(|e| e == name);
    if !ok {
        eprintln!("skipping: this ffmpeg has no {name} encoder");
    }
    ok
}

/// A synthetic clip ([`clip`]): a few seconds of lavfi `testsrc2` (+ `sine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Clip {
    /// 10 s, 640×360 25 fps H.264 (2 s GOP, B-frames) + stereo AAC, MP4
    /// (faststart).
    H264AacMp4,
    /// 10 s, 640×360 25 fps HEVC (2 s GOP) + stereo AC-3, Matroska.
    HevcAc3Mkv,
    /// 10 s, 640×360 25 fps H.264 (2 s GOP) + stereo AAC, MPEG-TS.
    H264AacTs,
    /// 10 s, 640×360 25 fps HEVC (2 s open GOP) + stereo AAC, MPEG-TS.
    HevcAacTs,
    /// 10 s, 320×240 25 fps MPEG-4 Part 2 + MP2, AVI (no index flux reads:
    /// the ffprobe-scan path).
    Mpeg4Mp2Avi,
    /// 6 s, 640×360 24 fps HEVC Main 10 tagged HDR10 (PQ, BT.2020, mastering
    /// display and content light level), Matroska: the tone-mapping path.
    Hdr10HevcMkv,
    // The Phase D regression clips (crates/flux-hls/tests/phase_d.rs):
    /// H.264 23.976 fps with a 59.94 stretch, DefaultDuration 23.976 (PB-1).
    VfrSectionMkv,
    /// HEVC with the same VFR stretch (PB-1/PB-2 transcode, PB-NEW-2).
    VfrHevcShortMkv,
    /// HEVC 30 fps, 12 s.
    Hevc30Mkv,
    /// [`Clip::Hevc30Mkv`] with its DefaultDuration voided (PB-2).
    Hevc30NoDefaultDurationMkv,
    /// HEVC 24 fps, 6 s, no B-frames.
    Hevc6Mkv,
    /// [`Clip::Hevc6Mkv`] with timestamps starting at 1 s (PB-NEW-1).
    Start1HevcMkv,
    /// [`Clip::H264AacMp4`] with a 90° display matrix, MOV (PB-9).
    Rot90H264Mov,
    /// [`Clip::Hevc30Mkv`] with a 90° display matrix, MOV (PB-9).
    Rot90HevcMov,
    /// 720×576 shown 1024×576 (SAR 64:45), H.264, Matroska.
    AnamorphicMkv,
    /// [`Clip::AnamorphicMkv`] remuxed to MP4 (`pasp`).
    AnamorphicMp4,
    /// HEVC 1080p60, 3 s: a transcode needs level 4.2 (PB-12).
    Hevc1080p60Mkv,
    // Interlacing (flux-hls's planner and deinterlacer), 4 s 720×576 25 fps
    // H.264 in Matroska, no audio:
    /// x264 `fake-interlaced`: an SPS that allows field coding, progressive
    /// frames, flagged progressive (FlagInterlaced 2).
    FakeInterlacedMkv,
    /// Interlaced MBAFF frames (top field first), flagged interlaced.
    InterlacedMkv,
    /// Interlaced frames encoded as progressive frames (fields woven in, no
    /// `+ildct`), flagged interlaced.
    WovenFieldsMkv,
    /// 10 s, 640×360 25 fps H.264 (2 s GOP) + two audio tracks of 1 kHz
    /// tone bursts (30 ms every 250 ms, so a shifted stretch shows): stereo
    /// AC-3, then stereo E-AC-3, Matroska.
    Ac3Eac3BurstsMkv,
    /// [`Clip::Ac3Eac3BurstsMkv`] with each audio track's frames laced 8 to
    /// a block (256 ms, EBML lacing), as some muxers store (E-)AC-3; no
    /// Cues.
    LacedAc3Eac3Mkv,
    /// 24 s, 640×360 H.264 at 23.976 fps on a 90 kHz clock (samples of
    /// 3754, 3754, 3753 ticks: no constant duration), 5 s GOPs, no
    /// B-frames, 16 reference frames, no audio, MP4, cut like a stream
    /// edited without re-encoding: frames 130–189 and 300–339 are taken out
    /// after encoding, so the frames before the cuts are held 2.5 s and
    /// 1.7 s, and a decoder drops frames after the cuts (their references
    /// are gone) until it recovers.
    HeldFramesMp4,
    /// 30 s, 640×360 H.264 at 25 fps on a 90 kHz clock (3600 ticks a
    /// sample), 5 s GOPs, no B-frames, 16 reference frames, no audio, MP4,
    /// with frames 650–700 taken out after encoding: one sample duration
    /// for over nine tenths of the samples, then a frame held 2.08 s, from
    /// before the 2 s piece at 26 s to after it.
    HeldLateMp4,
}

const CLIP_COUNT: usize = 24;

/// The tone bursts of [`Clip::Ac3Eac3BurstsMkv`]: 1 kHz, 30 ms every 250 ms.
const BURSTS: &str = "aevalsrc=exprs=if(lt(mod(t\\,0.25)\\,0.03)\\,0.5*sin(2*PI*1000*t)\\,0):s=48000:d=10";

/// The variable-frame-rate timestamps of the PB-1 clips: 23.976 fps, a
/// 59.94 stretch from frame 144 to 240, then 23.976 again.
const VFR: &str =
    "settb=1/90000,setpts='if(lt(N,144),N*1001/24000,if(lt(N,240),144*1001/24000+(N-144)*1001/60000,\
                   144*1001/24000+96*1001/60000+(N-240)*1001/24000))/TB'";

/// How a clip is made.
enum Recipe {
    /// ffmpeg with these arguments (the output path follows).
    Ffmpeg(Vec<String>),
    /// A copy of another clip with its Matroska DefaultDuration voided.
    VoidDefaultDuration(Clip),
    /// A copy of another Matroska clip with its audio frames laced this
    /// many to a block.
    LaceAudio(Clip, usize),
}

impl Clip {
    /// Every clip.
    pub const ALL: [Clip; CLIP_COUNT] = [
        Clip::H264AacMp4,
        Clip::HevcAc3Mkv,
        Clip::H264AacTs,
        Clip::HevcAacTs,
        Clip::Mpeg4Mp2Avi,
        Clip::Hdr10HevcMkv,
        Clip::VfrSectionMkv,
        Clip::VfrHevcShortMkv,
        Clip::Hevc30Mkv,
        Clip::Hevc30NoDefaultDurationMkv,
        Clip::Hevc6Mkv,
        Clip::Start1HevcMkv,
        Clip::Rot90H264Mov,
        Clip::Rot90HevcMov,
        Clip::AnamorphicMkv,
        Clip::AnamorphicMp4,
        Clip::Hevc1080p60Mkv,
        Clip::FakeInterlacedMkv,
        Clip::InterlacedMkv,
        Clip::WovenFieldsMkv,
        Clip::Ac3Eac3BurstsMkv,
        Clip::LacedAc3Eac3Mkv,
        Clip::HeldFramesMp4,
        Clip::HeldLateMp4,
    ];

    /// File name of the cached clip.
    pub fn file_name(self) -> &'static str {
        match self {
            Clip::H264AacMp4 => "synth_h264_aac.mp4",
            Clip::HevcAc3Mkv => "synth_hevc_ac3.mkv",
            Clip::H264AacTs => "synth_h264_aac.ts",
            Clip::HevcAacTs => "synth_hevc_aac.ts",
            Clip::Mpeg4Mp2Avi => "synth_mpeg4_mp2.avi",
            Clip::Hdr10HevcMkv => "synth_hevc_hdr10.mkv",
            Clip::VfrSectionMkv => "vfr_section.mkv",
            Clip::VfrHevcShortMkv => "vfr_hevc_short.mkv",
            Clip::Hevc30Mkv => "hevc30.mkv",
            Clip::Hevc30NoDefaultDurationMkv => "hevc30_nodd.mkv",
            Clip::Hevc6Mkv => "hevc6.mkv",
            Clip::Start1HevcMkv => "start1_hevc.mkv",
            Clip::Rot90H264Mov => "rot90_h264.mov",
            Clip::Rot90HevcMov => "rot90_hevc.mov",
            Clip::AnamorphicMkv => "anamorphic.mkv",
            Clip::AnamorphicMp4 => "anamorphic.mp4",
            Clip::Hevc1080p60Mkv => "hevc1080p60.mkv",
            Clip::FakeInterlacedMkv => "fake_interlaced.mkv",
            Clip::InterlacedMkv => "interlaced_mbaff.mkv",
            Clip::WovenFieldsMkv => "interlaced_as_frames.mkv",
            Clip::Ac3Eac3BurstsMkv => "bursts_ac3_eac3.mkv",
            Clip::LacedAc3Eac3Mkv => "bursts_ac3_eac3_laced.mkv",
            Clip::HeldFramesMp4 => "held_frames.mp4",
            Clip::HeldLateMp4 => "held_late.mp4",
        }
    }

    /// The clip whose [`Clip::file_name`] is `name`.
    pub fn named(name: &str) -> Option<Clip> {
        Clip::ALL.into_iter().find(|c| c.file_name() == name)
    }

    /// Encoders the clip (and the clips it is made from) need.
    pub fn encoders(self) -> &'static [&'static str] {
        match self {
            Clip::H264AacMp4 | Clip::H264AacTs | Clip::VfrSectionMkv | Clip::Rot90H264Mov => {
                &["libx264", "aac"]
            }
            Clip::AnamorphicMkv
            | Clip::AnamorphicMp4
            | Clip::FakeInterlacedMkv
            | Clip::InterlacedMkv
            | Clip::WovenFieldsMkv
            | Clip::HeldFramesMp4
            | Clip::HeldLateMp4 => &["libx264"],
            Clip::HevcAc3Mkv => &["libx265", "ac3"],
            Clip::Mpeg4Mp2Avi => &["mpeg4", "mp2"],
            Clip::Hdr10HevcMkv | Clip::Hevc1080p60Mkv => &["libx265"],
            Clip::Ac3Eac3BurstsMkv | Clip::LacedAc3Eac3Mkv => &["libx264", "ac3", "eac3"],
            Clip::HevcAacTs
            | Clip::VfrHevcShortMkv
            | Clip::Hevc30Mkv
            | Clip::Hevc30NoDefaultDurationMkv
            | Clip::Hevc6Mkv
            | Clip::Start1HevcMkv
            | Clip::Rot90HevcMov => &["libx265", "aac"],
        }
    }

    /// Video frames of the base clips (`None` for the Phase D clips).
    pub fn frames(self) -> Option<u32> {
        match self {
            Clip::H264AacMp4
            | Clip::HevcAc3Mkv
            | Clip::H264AacTs
            | Clip::HevcAacTs
            | Clip::Mpeg4Mp2Avi
            | Clip::Ac3Eac3BurstsMkv
            | Clip::LacedAc3Eac3Mkv => Some(250),
            Clip::Hdr10HevcMkv => Some(144),
            Clip::FakeInterlacedMkv | Clip::InterlacedMkv | Clip::WovenFieldsMkv => Some(100),
            _ => None,
        }
    }

    fn recipe(self) -> Option<Recipe> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let src = |c: Clip| clip(c).map(|p| p.to_string_lossy().into_owned());
        let video = |size: &str, rate: &str, secs: u32| {
            s(&[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size={size}:rate={rate}:duration={secs}"),
            ])
        };
        let tone = |secs: u32| {
            s(&[
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:sample_rate=48000:duration={secs}"),
            ])
        };
        let x264 = |gop: &str| {
            s(&[
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                gop,
                "-keyint_min",
                gop,
                "-sc_threshold",
                "0",
                "-bf",
                "2",
            ])
        };
        let x265 = |params: &str| {
            s(&[
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-x265-params",
                &format!("{params}:log-level=error"),
            ])
        };
        let aac = s(&["-c:a", "aac", "-b:a", "128k", "-ac", "2"]);
        let mut a = Vec::new();
        match self {
            Clip::H264AacMp4 | Clip::H264AacTs => {
                a.extend(video("640x360", "25", 10));
                a.extend(tone(10));
                a.extend(x264("50"));
                a.extend(aac);
                if self == Clip::H264AacMp4 {
                    a.extend(s(&["-movflags", "+faststart", "-f", "mp4"]));
                } else {
                    a.extend(s(&["-f", "mpegts"]));
                }
            }
            Clip::HevcAc3Mkv => {
                a.extend(video("640x360", "25", 10));
                a.extend(tone(10));
                a.extend(x265("keyint=50:min-keyint=50:scenecut=0"));
                a.extend(s(&["-c:a", "ac3", "-b:a", "192k", "-ac", "2", "-f", "matroska"]));
            }
            Clip::HevcAacTs => {
                a.extend(video("640x360", "25", 10));
                a.extend(tone(10));
                a.extend(x265("keyint=50:min-keyint=50:scenecut=0:open-gop=1"));
                a.extend(aac);
                a.extend(s(&["-f", "mpegts"]));
            }
            Clip::Mpeg4Mp2Avi => {
                a.extend(video("320x240", "25", 10));
                a.extend(tone(10));
                a.extend(s(&[
                    "-c:v", "mpeg4", "-q:v", "6", "-g", "50", "-c:a", "mp2", "-b:a", "128k", "-ac", "2",
                    "-f", "avi",
                ]));
            }
            Clip::Hdr10HevcMkv => {
                a.extend(video("640x360", "24", 6));
                a.extend(s(&[
                    "-c:v",
                    "libx265",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p10le",
                    "-color_primaries",
                    "bt2020",
                    "-color_trc",
                    "smpte2084",
                    "-colorspace",
                    "bt2020nc",
                    "-x265-params",
                    "keyint=48:min-keyint=48:scenecut=0:log-level=error:hdr10=1:\
                     colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:\
                     master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):\
                     max-cll=1000,400",
                    "-f",
                    "matroska",
                ]));
            }
            Clip::VfrSectionMkv | Clip::VfrHevcShortMkv => {
                let size = if self == Clip::VfrSectionMkv {
                    "640x360"
                } else {
                    "320x180"
                };
                a.extend(video(size, "24000/1001", 12));
                a.extend(tone(12));
                a.extend(s(&[
                    "-filter_complex",
                    &format!("[0:v]{VFR}[v]"),
                    "-map",
                    "[v]",
                    "-map",
                    "1:a",
                    "-fps_mode",
                    "passthrough",
                    "-enc_time_base:v",
                    "1:90000",
                ]));
                if self == Clip::VfrSectionMkv {
                    a.extend(s(&[
                        "-c:v", "libx264", "-preset", "veryfast", "-g", "48", "-bf", "2",
                    ]));
                } else {
                    a.extend(s(&[
                        "-c:v",
                        "libx265",
                        "-preset",
                        "ultrafast",
                        "-x265-params",
                        "keyint=48:log-level=error",
                    ]));
                }
                a.extend(aac);
                a.extend(s(&["-f", "matroska"]));
            }
            Clip::Hevc30Mkv => {
                a.extend(video("640x360", "30", 12));
                a.extend(tone(12));
                a.extend(s(&["-map", "0:v", "-map", "1:a"]));
                a.extend(x265("keyint=30"));
                a.extend(aac);
                a.extend(s(&["-f", "matroska"]));
            }
            Clip::Hevc30NoDefaultDurationMkv => return Some(Recipe::VoidDefaultDuration(Clip::Hevc30Mkv)),
            Clip::Hevc6Mkv => {
                a.extend(video("640x360", "24", 6));
                a.extend(tone(6));
                a.extend(x265("keyint=24:bframes=0"));
                a.extend(aac);
                a.extend(s(&["-f", "matroska"]));
            }
            Clip::Start1HevcMkv => {
                a.extend(s(&["-i", &src(Clip::Hevc6Mkv)?]));
                a.extend(s(&["-c", "copy", "-output_ts_offset", "1", "-f", "matroska"]));
            }
            Clip::Rot90H264Mov | Clip::Rot90HevcMov => {
                let from = if self == Clip::Rot90H264Mov {
                    Clip::H264AacMp4
                } else {
                    Clip::Hevc30Mkv
                };
                a.extend(s(&["-display_rotation", "90", "-i", &src(from)?, "-c", "copy"]));
                if self == Clip::Rot90HevcMov {
                    a.extend(s(&["-tag:v", "hvc1"]));
                }
                a.extend(s(&["-f", "mov"]));
            }
            Clip::AnamorphicMkv => {
                a.extend(video("720x576", "25", 6));
                a.extend(s(&[
                    "-vf",
                    "setsar=64/45",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-g",
                    "25",
                    "-an",
                    "-f",
                    "matroska",
                ]));
            }
            Clip::AnamorphicMp4 => {
                a.extend(s(&["-i", &src(Clip::AnamorphicMkv)?, "-c", "copy", "-f", "mp4"]));
            }
            Clip::Hevc1080p60Mkv => {
                a.extend(video("1920x1080", "60", 3));
                a.extend(x265("keyint=60"));
                a.extend(s(&["-an", "-f", "matroska"]));
            }
            Clip::FakeInterlacedMkv | Clip::InterlacedMkv | Clip::WovenFieldsMkv => {
                a.extend(video("720x576", "25", 4));
                // `setfield` marks the frames interlaced, so ffmpeg flags the
                // file so (Matroska FlagInterlaced 1); libx264 codes them as
                // fields (MBAFF) only with `+ildct`.
                match self {
                    Clip::FakeInterlacedMkv => {
                        a.extend(s(&["-x264-params", "fake-interlaced=1:pic-struct=1"]))
                    }
                    Clip::InterlacedMkv => a.extend(s(&["-vf", "setfield=tff", "-flags", "+ildct+ilme"])),
                    _ => a.extend(s(&["-vf", "setfield=tff"])),
                }
                a.extend(s(&[
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                    "-g",
                    "25",
                    "-an",
                    "-f",
                    "matroska",
                ]));
            }
            Clip::Ac3Eac3BurstsMkv => {
                a.extend(video("640x360", "25", 10));
                a.extend(s(&["-f", "lavfi", "-i", BURSTS]));
                a.extend(s(&["-map", "0:v", "-map", "1:a", "-map", "1:a"]));
                a.extend(x264("50"));
                a.extend(s(&[
                    "-c:a:0", "ac3", "-c:a:1", "eac3", "-b:a", "192k", "-ac", "2", "-f", "matroska",
                ]));
            }
            Clip::LacedAc3Eac3Mkv => return Some(Recipe::LaceAudio(Clip::Ac3Eac3BurstsMkv, 8)),
            Clip::HeldFramesMp4 | Clip::HeldLateMp4 => {
                let (rate, secs, gop, cuts) = if self == Clip::HeldFramesMp4 {
                    ("24000/1001", 24, "120", "between(n,130,189)+between(n,300,339)")
                } else {
                    ("25", 30, "125", "between(n,650,700)")
                };
                a.extend(video("640x360", rate, secs));
                a.extend(s(&[
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                    "-g",
                    gop,
                    "-keyint_min",
                    gop,
                    "-sc_threshold",
                    "0",
                    "-bf",
                    "0",
                    "-refs",
                    "16",
                    // The cuts: encoded packets dropped (mid-GOP, so every
                    // keyframe stays).
                    "-bsf:v",
                    &format!("noise=drop='{cuts}'"),
                    "-video_track_timescale",
                    "90000",
                    "-an",
                    "-f",
                    "mp4",
                ]));
            }
        }
        Some(Recipe::Ffmpeg(a))
    }
}

/// Bump when a recipe changes, so cached clips are remade.
const RECIPE_VERSION: u32 = 1;

/// The cache directory of synthetic clips: `$FLUX_TEST_CLIPS`, else
/// `<temp dir>/flux-test-clips-v<recipe version>`.
pub fn clip_dir() -> PathBuf {
    match std::env::var_os("FLUX_TEST_CLIPS").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => std::env::temp_dir().join(format!("flux-test-clips-v{RECIPE_VERSION}")),
    }
}

/// The synthetic clip `c`, made on first use (once per process, published
/// with an atomic rename so parallel test processes never see half a
/// file) and kept in [`clip_dir`]. `None`, with a printed reason, when
/// ffmpeg or an encoder it needs is missing or the encode fails.
pub fn clip(c: Clip) -> Option<PathBuf> {
    static CELLS: [OnceLock<Option<PathBuf>>; CLIP_COUNT] = [const { OnceLock::new() }; CLIP_COUNT];
    CELLS[c as usize].get_or_init(|| make_clip(c)).clone()
}

/// [`clip`] by file name (`"hevc30_nodd.mkv"`), for tests that list inputs
/// by name.
pub fn clip_named(name: &str) -> Option<PathBuf> {
    match Clip::named(name) {
        Some(c) => clip(c),
        None => {
            eprintln!("skipping: no recipe for a clip named {name}");
            None
        }
    }
}

fn make_clip(c: Clip) -> Option<PathBuf> {
    let dir = clip_dir();
    let out = dir.join(c.file_name());
    if out.is_file() {
        return Some(out);
    }
    let ff = ffmpeg()?;
    if !c.encoders().iter().all(|e| has_encoder(e)) {
        return None;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("skipping: cannot create {}: {e}", dir.display());
        return None;
    }
    let tmp = dir.join(format!(".{}.{}.tmp", c.file_name(), std::process::id()));
    let made = match c.recipe()? {
        Recipe::Ffmpeg(args) => run_ffmpeg(&ff, &args, &tmp, c.file_name()),
        Recipe::VoidDefaultDuration(from) => {
            let src = clip(from)?;
            std::fs::read(&src).map_err(|e| e.to_string()).and_then(|mut b| {
                match void_default_duration(&mut b) {
                    true => std::fs::write(&tmp, &b).map_err(|e| e.to_string()),
                    false => Err("no DefaultDuration in the first 64 KiB".into()),
                }
            })
        }
        Recipe::LaceAudio(from, per) => {
            let src = clip(from)?;
            std::fs::read(&src)
                .map_err(|e| e.to_string())
                .and_then(|b| mkv_lace::lace_audio(&b, per))
                .and_then(|b| std::fs::write(&tmp, b).map_err(|e| e.to_string()))
        }
    };
    if let Err(e) = made {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("skipping: {}: {e}", c.file_name());
        return None;
    }
    if std::fs::rename(&tmp, &out).is_err() && !out.is_file() {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("skipping: could not publish {}", out.display());
        return None;
    }
    let _ = std::fs::remove_file(&tmp);
    Some(out)
}

/// How long ffmpeg may take to make one clip (they take seconds) before it
/// is killed and tried again.
const CLIP_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs ffmpeg to write `out`. An ffmpeg still running after
/// [`CLIP_TIMEOUT`] is killed and run once more; a second overrun panics,
/// naming the clip. Every test waiting on that clip would otherwise hang
/// until CI's job timeout, as all three flux-hls synthetic tests did once
/// on a Linux arm64 runner.
fn run_ffmpeg(ff: &Path, args: &[String], out: &Path, name: &str) -> Result<(), String> {
    for attempt in 1..=2 {
        let mut child = Command::new(ff)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(args)
            .arg(out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", ff.display()))?;
        let stderr = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut b = Vec::new();
                let _ = pipe.read_to_end(&mut b);
                b
            })
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if start.elapsed() < CLIP_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Err(e) => return Err(format!("waiting for ffmpeg: {e}")),
            }
        };
        let err = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
        match status {
            Some(s) if s.success() => return Ok(()),
            Some(s) => {
                return Err(format!(
                    "ffmpeg could not make it ({s}): {}",
                    String::from_utf8_lossy(&err).trim()
                ))
            }
            None => eprintln!(
                "{name}: ffmpeg still running after {} s (attempt {attempt} of 2), killed",
                CLIP_TIMEOUT.as_secs()
            ),
        }
    }
    panic!(
        "{name}: ffmpeg hung twice making this clip ({} s each); args: {args:?}",
        CLIP_TIMEOUT.as_secs()
    );
}

/// Overwrites the first Matroska DefaultDuration element (ID 23 E3 83, in
/// the first 64 KiB) with a Void element of the same length.
fn void_default_duration(b: &mut [u8]) -> bool {
    let head = &b[..b.len().min(1 << 16)];
    let Some(i) = head.windows(3).position(|w| w == [0x23, 0xE3, 0x83]) else {
        return false;
    };
    let Some(&size) = b.get(i + 3) else { return false };
    if size & 0x80 == 0 {
        return false; // a one-byte size vint is all the muxers write here
    }
    let n = 4 + (size & 0x7F) as usize;
    if i + n > b.len() || n < 2 {
        return false;
    }
    b[i] = 0xEC;
    b[i + 1] = 0x80 | (n - 2) as u8;
    b[i + 2..i + n].fill(0);
    true
}

/// Video frames of a media file as ffprobe reads them: `(pts seconds, key)`
/// in presentation order, or `None` with a printed reason.
pub fn ffprobe_video_frames(path: &Path) -> Option<Vec<(f64, bool)>> {
    let probe = ffprobe()?;
    let out = Command::new(probe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts_time,key_frame,pict_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    let mut frames = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // key_frame,pts_time,pict_type in the order ffprobe prints them.
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        let key = f.first().is_some_and(|k| *k == "1");
        let pts = f.get(1).and_then(|t| t.parse::<f64>().ok()).unwrap_or(f64::NAN);
        frames.push((pts, key));
    }
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_media_dir_never_exists() {
        if media_root().is_none() {
            assert!(!media_dir("features").exists());
            assert!(media_dir("features").to_string_lossy().contains(MEDIA_VAR));
        }
    }

    #[test]
    fn base_clips_are_what_they_say() {
        for c in Clip::ALL {
            let Some(want) = c.frames() else { continue };
            let Some(p) = clip(c) else { continue };
            let Some(frames) = ffprobe_video_frames(&p) else {
                continue;
            };
            assert_eq!(frames.len() as u32, want, "{}", c.file_name());
            assert!(frames[0].1, "{} starts on a keyframe", c.file_name());
        }
    }

    #[test]
    fn every_clip_has_a_unique_name() {
        for (i, c) in Clip::ALL.iter().enumerate() {
            assert_eq!(*c as usize, i, "Clip::ALL is in declaration order");
            assert_eq!(Clip::named(c.file_name()), Some(*c));
        }
    }

    #[test]
    fn voids_the_default_duration() {
        // DefaultDuration (4-byte value) between two other bytes.
        let mut b = vec![0xAA, 0x23, 0xE3, 0x83, 0x84, 1, 2, 3, 4, 0xBB];
        assert!(void_default_duration(&mut b));
        assert_eq!(b, [0xAA, 0xEC, 0x86, 0, 0, 0, 0, 0, 0, 0xBB]);
        assert!(!void_default_duration(&mut [0u8; 16]));
    }
}
