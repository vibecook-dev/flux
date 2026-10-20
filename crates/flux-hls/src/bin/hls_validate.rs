//! hls-validate: produce every playlist and segment of a file for several
//! client presets and check them with ffmpeg/ffprobe.
//!
//! ```text
//! hls-validate [--presets chrome,appletv] [--out DIR] [--json PATH] [--force] [--no-seams] FILE...
//! ```
//!
//! Results go to `bench/results/flux-hls-validate-<UTC stamp>.json` (or
//! `--json`); an existing file is never replaced without `--force`. The
//! file is rewritten after every case, so an interrupted run keeps what it
//! validated.
//!
//! Per file × preset it writes `<out>/<file>/<preset>/` and checks:
//! - `ffmpeg -v error -i master.m3u8 -map 0 -f null -` prints nothing;
//! - packet counts per copied rendition equal the source stream's;
//! - video: DTS strictly increasing, CTS ≥ 0, no presentation gaps, and the
//!   output starts exactly at the presentation offset D (no ffmpeg shift);
//! - audio: no timestamp gaps/overlaps between packets across segments;
//! - A/V start offset vs the source;
//! - transcoded audio: seam smoothness (second-difference peak at segment
//!   seams vs the 99.9th percentile elsewhere) against a continuous encode.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};

#[path = "shared/cli.rs"]
mod cli;
use flux_core::{AudioOp, PlaybackPlan, SubtitleOp, VideoOp};
use flux_hls::playlist::{AudioKey, AudioRecipe, HlsResource, Rendition};
use flux_hls::{presets, Asset, EngineConfig, HlsEngine, PlanPrefs};
use serde_json::{json, Value};

/// ffmpeg (`FLUX_FFMPEG`, else `PATH`).
fn ffmpeg() -> PathBuf {
    flux_hls::ffmpeg_tools::default_ffmpeg()
}

/// ffprobe (`FLUX_FFPROBE`, else next to ffmpeg, else `PATH`).
fn ffprobe() -> PathBuf {
    flux_hls::ffmpeg_tools::default_ffprobe(&ffmpeg())
}

fn run(cmd: &mut Command) -> Result<(bool, String, String)> {
    let o = cmd.output().with_context(|| format!("{cmd:?}"))?;
    Ok((
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    ))
}

/// (pts, dts, duration) in seconds for every packet of one stream.
fn packets(input: &Path, select: &str) -> Result<Vec<(f64, f64, f64)>> {
    let (_, out, _) = run(Command::new(ffprobe())
        .args([
            "-v",
            "error",
            "-select_streams",
            select,
            "-show_entries",
            "packet=pts_time,dts_time,duration_time",
            "-of",
            "csv=p=0",
        ])
        .arg(input))?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            let p = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok());
            Some((p(0)?, p(1).or(p(0))?, p(2).unwrap_or(0.0)))
        })
        .collect())
}

fn source_counts(src: &Path) -> Result<BTreeMap<u32, (String, u64)>> {
    let (_, out, _) = run(Command::new(ffprobe())
        .args([
            "-v",
            "error",
            "-count_packets",
            "-show_entries",
            "stream=index,codec_name,nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(src))?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some((
                f.first()?.parse().ok()?,
                (f.get(1)?.to_string(), f.get(2)?.trim().parse().ok()?),
            ))
        })
        .collect())
}

fn first_pts(src: &Path, select: &str) -> Option<f64> {
    let (_, out, _) = run(Command::new(ffprobe())
        .args([
            "-v",
            "error",
            "-select_streams",
            select,
            "-read_intervals",
            "%+#1",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(src))
    .ok()?;
    let packet = out.lines().next()?.trim().trim_end_matches(',').parse().ok();
    // AVI has no packet pts: use the decoder's first frame time, which is
    // where ffmpeg (and any player) presents it (B-frame delay included).
    packet.or_else(|| {
        let (_, out, _) = run(Command::new(ffprobe())
            .args([
                "-v",
                "error",
                "-select_streams",
                select,
                "-read_intervals",
                "%+#16",
                "-show_entries",
                "frame=best_effort_timestamp_time",
                "-of",
                "csv=p=0",
            ])
            .arg(src))
        .ok()?;
        out.lines().next()?.trim().trim_end_matches(',').parse().ok()
    })
}

/// Decodes one audio input to mono f32 at `rate`.
fn decode_mono(input: &Path, map: Option<&str>, rate: u32) -> Result<Vec<f32>> {
    let mut c = Command::new(ffmpeg());
    c.args(["-v", "error", "-i"]).arg(input);
    if let Some(m) = map {
        c.args(["-map", m]);
    }
    c.args(["-ac", "1", "-ar", &rate.to_string(), "-f", "f32le", "-"]);
    let o = c.output()?;
    Ok(o.stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

/// Largest second difference anywhere (skipping the first/last 0.5 s, where
/// encoders prime and flush), over the 99.9th percentile.
fn global_ratio(x: &[f32], skip: usize) -> Option<f64> {
    if x.len() < 2 * skip + 3 {
        return None;
    }
    let mut d2: Vec<f32> = (skip..x.len() - skip - 1)
        .map(|n| (x[n + 1] - 2.0 * x[n] + x[n - 1]).abs())
        .collect();
    let max = d2.iter().fold(0f32, |a, &b| a.max(b)) as f64;
    d2.sort_by(|a, b| a.total_cmp(b));
    let p999 = d2[((d2.len() - 1) as f64 * 0.999) as usize].max(1e-9) as f64;
    Some(max / p999)
}

/// Peak second difference within ±w samples of each seam, divided by the
/// 99.9th percentile of the second difference away from seams.
fn seam_ratio(x: &[f32], seams: &[usize], w: usize) -> Option<(f64, f64)> {
    if x.len() < 3 {
        return None;
    }
    let d2: Vec<f32> = (1..x.len() - 1)
        .map(|n| (x[n + 1] - 2.0 * x[n] + x[n - 1]).abs())
        .collect();
    let near = |n: usize| seams.iter().any(|&s| n + w >= s && n <= s + w);
    let mut body: Vec<f32> = d2
        .iter()
        .enumerate()
        .filter(|(n, _)| !near(*n))
        .map(|(_, v)| *v)
        .collect();
    if body.is_empty() {
        return None;
    }
    body.sort_by(|a, b| a.total_cmp(b));
    let p999 = body[((body.len() - 1) as f64 * 0.999) as usize].max(1e-9) as f64;
    let peak = seams
        .iter()
        .filter(|&&s| s > w && s + w < d2.len())
        .map(|&s| d2[s - w..s + w].iter().fold(0f32, |a, &b| a.max(b)))
        .fold(0f32, f32::max) as f64;
    Some((peak / p999, p999))
}

fn tfdt(seg: &[u8]) -> Option<u64> {
    use mp4_atom::{Any, Decode};
    let mut cur = std::io::Cursor::new(seg);
    loop {
        match Any::decode(&mut cur).ok()? {
            Any::Moof(m) => return m.traf.first()?.tfdt.as_ref().map(|t| t.base_media_decode_time),
            _ => continue,
        }
    }
}

struct Written {
    seg_ms: Vec<f64>,
    bytes: u64,
    /// Per audio rendition key: tfdt of every segment.
    audio_tfdt: BTreeMap<String, Vec<u64>>,
}

async fn write_all(
    engine: &HlsEngine,
    asset: &std::sync::Arc<Asset>,
    plan: &PlaybackPlan,
    out: &Path,
) -> Result<Written> {
    let mut w = Written {
        seg_ms: vec![],
        bytes: 0,
        audio_tfdt: BTreeMap::new(),
    };
    std::fs::create_dir_all(out)?;
    let put = |res: &HlsResource, data: &[u8]| -> Result<()> {
        let p = out.join(res.path());
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, data)?;
        Ok(())
    };
    put(
        &HlsResource::Master,
        engine.master_playlist(asset, plan)?.as_bytes(),
    )?;
    let mut renditions = vec![Rendition::Video];
    renditions.extend(
        plan.audio
            .iter()
            .map(|a| Rendition::Audio(AudioKey::from_plan(a))),
    );
    for r in &renditions {
        put(
            &HlsResource::Playlist(*r),
            engine.media_playlist(asset, plan, r)?.as_bytes(),
        )?;
        put(
            &HlsResource::Init(*r),
            &engine.init_segment(asset, plan, r).await?,
        )?;
        for n in 0..engine.segment_count(asset, plan, r) as u32 {
            let t = Instant::now();
            let body = engine.media_segment(asset, plan, r, n).await?;
            let bytes = body.into_bytes().await?;
            w.seg_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            w.bytes += bytes.len() as u64;
            if let Rendition::Audio(k) = r {
                w.audio_tfdt
                    .entry(k.path())
                    .or_default()
                    .push(tfdt(&bytes).unwrap_or(0));
            }
            put(&HlsResource::Segment(*r, n), &bytes)?;
        }
    }
    if matches!(plan.video, VideoOp::Copy { .. }) && asset.index.keyframes.len() > 1 {
        let p = HlsResource::IFramePlaylist;
        let r = engine.serve(asset, plan, p).await?;
        put(&p, &r.body.into_bytes().await?)?;
        for n in 0..asset.index.keyframes.len() as u32 {
            let p = HlsResource::IFrame(n);
            let r = engine.serve(asset, plan, p).await?;
            put(&p, &r.body.into_bytes().await?)?;
        }
    }
    for s in plan.subtitles.iter().filter(|s| s.op == SubtitleOp::WebVtt) {
        let r = Rendition::Subtitle(s.track);
        put(
            &HlsResource::Playlist(r),
            engine.media_playlist(asset, plan, &r)?.as_bytes(),
        )?;
        put(
            &HlsResource::WebVtt(s.track),
            engine.webvtt(asset, s.track).await?.as_bytes(),
        )?;
    }
    Ok(w)
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

async fn validate(
    engine: &HlsEngine,
    src: &Path,
    preset: &str,
    out_root: &Path,
    seams: bool,
) -> Result<Value> {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let t0 = Instant::now();
    let asset = match engine.prepare(src).await {
        Ok(a) => a,
        Err(e) => {
            return Ok(
                json!({ "file": name, "preset": preset, "status": "unsupported", "error": e.to_string() }),
            )
        }
    };
    let index_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let caps = presets::by_name(preset).context("preset")?;
    let direct_label = engine.plan(&asset, &caps, &PlanPrefs::default()).label();
    // Always exercise the HLS path (direct play is just byte serving).
    let plan = engine.plan(
        &asset,
        &caps,
        &PlanPrefs {
            allow_direct_play: false,
            ..Default::default()
        },
    );
    let reasons: Vec<String> = plan.reasons.iter().map(|r| r.explain()).collect();
    let mut row = json!({
        "file": name, "preset": preset, "index_ms": index_ms, "index_method": format!("{:?}", asset.index.method),
        "index_reads": asset.index.stats.reads, "index_bytes": asset.index.stats.bytes_read,
        "segments": asset.segments.len(), "target_duration": asset.segments.target_duration(),
        "planner_label": direct_label, "hls_label": plan.label(), "reasons": reasons,
        "presentation_offset_ms": asset.index.presentation_offset_ns as f64 / 1e6,
    });
    row["video_op"] = json!(match &plan.video {
        VideoOp::Copy { .. } => "copy".to_string(),
        VideoOp::Transcode {
            codec,
            width,
            height,
            tonemap,
            ..
        } => {
            format!(
                "transcode {codec:?} {width}x{height}{}",
                if *tonemap { " tonemap" } else { "" }
            )
        }
    });
    let dir = out_root.join(&name).join(preset);
    let _ = std::fs::remove_dir_all(&dir);
    let t1 = Instant::now();
    let written = match write_all(engine, &asset, &plan, &dir).await {
        Ok(w) => w,
        Err(e) => {
            row["status"] = json!("error");
            row["error"] = json!(format!("{e:#}"));
            return Ok(row);
        }
    };
    row["write_s"] = json!(t1.elapsed().as_secs_f64());
    let mut seg_ms = written.seg_ms.clone();
    row["segment_ms_median"] = json!(median(&mut seg_ms));
    row["segment_ms_max"] = json!(seg_ms.last().copied().unwrap_or(0.0));
    row["bytes"] = json!(written.bytes);

    // 1. ffmpeg decode of the whole master playlist.
    let master = dir.join("master.m3u8");
    // Subtitle streams need an explicit encoder with the null muxer. The
    // null muxer's video time base defaults to 1/frame rate, which can't
    // hold a VFR stretch (the source itself fails that way): use 90 kHz.
    let (_, _, err) = run(Command::new(ffmpeg())
        .args(["-v", "error", "-i"])
        .arg(&master)
        .args([
            "-map",
            "0",
            "-c:s",
            "webvtt",
            "-enc_time_base:v",
            "1:90000",
            "-f",
            "null",
            "-",
        ]))?;
    let errors: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
    row["ffmpeg_errors"] = json!(errors.len());
    row["ffmpeg_first_errors"] = json!(errors.iter().take(3).collect::<Vec<_>>());

    // 2. packets per rendition vs source
    let src_counts = source_counts(src)?;
    let vt = asset.index.video_track().map(|t| t.index).unwrap_or(0);
    let vpk = packets(&dir.join("v/index.m3u8"), "v:0")?;
    let src_v = src_counts.get(&vt).map(|c| c.1).unwrap_or(0);
    let d = asset.index.presentation_offset_ns as f64 / 1e9;
    let mut dts_violations = 0;
    let mut cts_negative = 0;
    for w in vpk.windows(2) {
        if w[1].1 <= w[0].1 {
            dts_violations += 1;
        }
    }
    for p in &vpk {
        if p.0 + 1e-6 < p.1 {
            cts_negative += 1;
        }
    }
    let mut sorted: Vec<f64> = vpk.iter().map(|p| p.0).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let fd = asset
        .index
        .video_track()
        .and_then(|t| t.frame_rate)
        .map(|r| r.den as f64 / r.num as f64)
        .unwrap_or(0.0);
    let gaps = if fd > 0.0 {
        sorted
            .windows(2)
            .filter(|w| (w[1] - w[0] - fd).abs() > fd * 0.25)
            .count()
    } else {
        0
    };
    let src_v_first = first_pts(src, &format!("{vt}")).unwrap_or(0.0);
    // The source's own discontinuities (e.g. concat joins) are reproduced, not errors.
    let src_vpk = packets(src, &format!("{vt}"))?;
    let mut src_sorted: Vec<f64> = src_vpk.iter().map(|p| p.0).collect();
    src_sorted.sort_by(|a, b| a.total_cmp(b));
    let src_gaps = if fd > 0.0 {
        src_sorted
            .windows(2)
            .filter(|w| (w[1] - w[0] - fd).abs() > fd * 0.25)
            .count()
    } else {
        0
    };
    let out_v_first = sorted.first().copied().unwrap_or(0.0);
    row["video"] = json!({
        "packets": vpk.len(), "source_packets": src_v, "match": vpk.len() as u64 == src_v,
        "dts_non_increasing": dts_violations, "cts_negative": cts_negative, "pts_gaps": gaps, "source_pts_gaps": src_gaps,
        "start_shift_ms": ((out_v_first - d) - src_v_first) * 1000.0,
    });

    let mut audio_rows = Vec::new();
    for a in &plan.audio {
        let key = AudioKey::from_plan(a);
        let pl = dir.join(HlsResource::Playlist(Rendition::Audio(key)).path());
        let apk = packets(&pl, "a:0")?;
        let mut max_gap: f64 = 0.0;
        let mut max_overlap: f64 = 0.0;
        for w in apk.windows(2) {
            let delta = w[1].0 - (w[0].0 + w[0].2);
            if delta > 0.0 {
                max_gap = max_gap.max(delta);
            } else {
                max_overlap = max_overlap.max(-delta);
            }
        }
        let src_a = src_counts.get(&a.track).map(|c| c.1).unwrap_or(0);
        // Source discontinuity, measured the same way (Matroska timestamps are
        // ms-rounded, so only jumps beyond 2 ms count).
        let spk = packets(src, &format!("{}", a.track))?;
        let src_jump = spk
            .windows(2)
            .map(|w| (w[1].0 - (w[0].0 + w[0].2)).abs())
            .filter(|d| *d > 0.002)
            .fold(0f64, f64::max);
        let src_a_first = first_pts(src, &format!("{}", a.track)).unwrap_or(0.0);
        let out_a_first = apk.first().map(|p| p.0).unwrap_or(0.0);
        let av_src = src_a_first - src_v_first;
        let av_out = out_a_first - out_v_first;
        let out_dur = apk.last().map(|p| p.0 + p.2).unwrap_or(0.0) - out_a_first;
        let mut r = json!({
            "rendition": key.path(), "op": match &a.op { AudioOp::Copy => "copy".to_string(), AudioOp::Transcode { codec, channels, .. } => format!("{codec:?} {channels}ch") },
            "packets": apk.len(), "source_packets": src_a,
            "match": matches!(a.op, AudioOp::Copy).then_some(apk.len() as u64 == src_a),
            "max_gap_ms": max_gap * 1000.0, "max_overlap_ms": max_overlap * 1000.0, "source_max_jump_ms": src_jump * 1000.0,
            "av_offset_source_ms": av_src * 1000.0, "av_offset_out_ms": av_out * 1000.0,
            "av_offset_delta_ms": (av_out - av_src) * 1000.0, "duration_s": out_dur,
        });
        if seams && key.recipe != AudioRecipe::Copy {
            r["seams"] = seam_check(src, a.track, &pl, &written, key, &dir)
                .unwrap_or_else(|e| json!({ "error": e.to_string() }));
        }
        audio_rows.push(r);
    }
    row["audio"] = json!(audio_rows);
    let subs: Vec<Value> = plan
        .subtitles
        .iter()
        .filter(|s| s.op == SubtitleOp::WebVtt)
        .map(|s| {
            let p = dir.join(HlsResource::WebVtt(s.track).path());
            let cues = std::fs::read_to_string(&p)
                .map(|t| t.matches(" --> ").count())
                .unwrap_or(0);
            json!({ "track": s.track, "cues": cues, "source_packets": src_counts.get(&s.track).map(|c| c.1) })
        })
        .collect();
    row["subtitles"] = json!(subs);
    // I-frame playlist (copied video): decodes cleanly, one frame per keyframe.
    let mut iframes_ok = true;
    if matches!(plan.video, VideoOp::Copy { .. }) && asset.index.keyframes.len() > 1 {
        let pl = dir.join(HlsResource::IFramePlaylist.path());
        let (_, _, err) = run(Command::new(ffmpeg())
            .args(["-v", "error", "-i"])
            .arg(&pl)
            .args(["-map", "0:v:0", "-f", "null", "-"]))?;
        let packets_n = packets(&pl, "v:0")?.len();
        // Decoded frames, not packets: a decoder may drop or reorder I-frames.
        let (_, out, _) = run(Command::new(ffprobe())
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=pts_time",
                "-of",
                "csv=p=0",
            ])
            .arg(&pl))?;
        let pts: Vec<f64> = out
            .lines()
            .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
            .collect();
        let in_order = pts.windows(2).all(|w| w[1] > w[0]);
        let kfs = asset.index.keyframes.len();
        iframes_ok = err.trim().is_empty() && packets_n == kfs && pts.len() == kfs && in_order;
        row["iframes"] = json!({ "keyframes": kfs, "packets": packets_n, "decoded": pts.len(),
                                 "in_order": in_order, "ffmpeg_errors": err.lines().count() });
    }
    let audio_ok = row["audio"]
        .as_array()
        .map(|v| {
            v.iter().all(|a| {
                let tol = 1.0 + a["source_max_jump_ms"].as_f64().unwrap_or(0.0);
                a["match"] != json!(false)
                    && a["max_gap_ms"].as_f64().unwrap_or(0.0) < tol
                    && a["max_overlap_ms"].as_f64().unwrap_or(0.0) < tol
            })
        })
        .unwrap_or(true);
    let video_count_ok = video_count_ok(
        vpk.len() as u64,
        src_v,
        matches!(plan.video, VideoOp::Transcode { .. }),
        &src_sorted,
        fd,
    );
    row["video"]["count_ok"] = json!(video_count_ok);
    let ok = errors.is_empty()
        && video_count_ok
        && dts_violations == 0
        && cts_negative == 0
        && gaps <= src_gaps
        && audio_ok
        && iframes_ok;
    row["status"] = json!(if ok { "ok" } else { "FAIL" });
    Ok(row)
}

/// Whether the output has every source frame. A copy must match exactly.
/// A transcode may never lose a frame; it may add one only per real hole in
/// the source (an interval over 1.5 frames, which a constant-rate encode
/// fills). The old tolerance, "|out − src| ≤ the source's count of
/// irregular intervals", let a VFR transcode that lost 58 of 288 frames
/// pass, because every VFR interval counted (PB-NEW-2).
fn video_count_ok(out: u64, src: u64, transcode: bool, src_pts_sorted: &[f64], frame_s: f64) -> bool {
    if !transcode {
        return out == src;
    }
    let holes = if frame_s > 0.0 {
        src_pts_sorted
            .windows(2)
            .filter(|w| w[1] - w[0] > 1.5 * frame_s)
            .count() as u64
    } else {
        0
    };
    out >= src && out - src <= holes
}

/// Seam smoothness of a transcoded rendition vs a continuous ffmpeg encode.
fn seam_check(
    src: &Path,
    track: u32,
    playlist: &Path,
    w: &Written,
    key: AudioKey,
    dir: &Path,
) -> Result<Value> {
    let rate = 48_000;
    let tf = w.audio_tfdt.get(&key.path()).context("tfdt")?;
    let t0 = *tf.first().context("no segments")?;
    let seams: Vec<usize> = tf.iter().skip(1).map(|&t| (t - t0) as usize).collect();
    let ours = decode_mono(playlist, None, rate)?;
    // Continuous single-pass encode with the same codec/settings as reference.
    let (codec, br, fmt, ext) = match key.recipe {
        AudioRecipe::Eac3 { .. } => ("eac3", "640k", "eac3", "eac3"),
        _ => ("aac", "192k", "adts", "aac"),
    };
    let ch = match key.recipe {
        AudioRecipe::Eac3 { channels } | AudioRecipe::Aac { channels } => channels,
        AudioRecipe::Copy => 2,
    };
    let refp = dir.join(format!("continuous_{}.{ext}", key.path()));
    run(Command::new(ffmpeg())
        .args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args([
            "-map",
            &format!("0:{track}"),
            "-ac",
            &ch.to_string(),
            "-c:a",
            codec,
            "-b:a",
            br,
            "-f",
            fmt,
        ])
        .arg(&refp))?;
    let reference = decode_mono(&refp, None, rate)?;
    let (ours_ratio, ours_floor) = seam_ratio(&ours, &seams, 64).context("too short")?;
    // Sensitivity: the same metric after slipping everything past the first
    // seam by 1 and 8 samples (what a misaligned encode would look like).
    let slip = |k: usize| -> Option<f64> {
        let s0 = *seams.first()?;
        let mut v = ours[..s0].to_vec();
        v.extend_from_slice(ours.get(s0 + k..)?);
        seam_ratio(&v, &seams[..1], 64).map(|r| r.0)
    };
    let (ref_ratio, ref_floor) = seam_ratio(&reference, &seams, 64).unwrap_or((0.0, 0.0));
    Ok(json!({
        "seams": seams.len(), "peak_over_p999": ours_ratio, "p999": ours_floor,
        "slip1_peak_over_p999": slip(1), "slip8_peak_over_p999": slip(8),
        "global_max_over_p999": global_ratio(&ours, rate as usize / 2),
        "continuous_global_max_over_p999": global_ratio(&reference, rate as usize / 2),
        "continuous_peak_over_p999": ref_ratio, "continuous_p999": ref_floor,
        "decoded_samples": ours.len(), "continuous_samples": reference.len(),
    }))
}

const USAGE: &str = "usage: hls-validate [--presets chrome,appletv,legacy] [--out DIR] [--json PATH] [--force] [--no-seams] FILE...

Produces every playlist and segment of each FILE for each client preset and
checks them with ffmpeg/ffprobe (packet counts, timestamps, A/V start, audio
seams, I-frame playlists).

  --presets LIST  presets to validate (default chrome,appletv)
  --out DIR       where segments are written (default $TMPDIR/hls-validate)
  --json PATH     results file (default $FLUX_BENCH_RESULTS/flux-hls-validate-<UTC>.json;
                  not written when neither is given)
  --force         overwrite an existing results file
  --no-seams      skip the audio seam analysis
  -h, --help      this text";

/// Command-line options.
#[derive(Debug, PartialEq)]
struct Opts {
    presets: Vec<String>,
    out: PathBuf,
    json: Option<PathBuf>,
    force: bool,
    seams: bool,
    files: Vec<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<cli::Parsed<Opts>> {
    let mut args = args.into_iter();
    let mut o = Opts {
        presets: vec!["chrome".to_string(), "appletv".to_string()],
        out: std::env::temp_dir().join("hls-validate"),
        json: None,
        force: false,
        seams: true,
        files: Vec::new(),
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(cli::Parsed::Help),
            "--presets" => {
                o.presets = args
                    .next()
                    .context("--presets needs a list")?
                    .split(',')
                    .map(String::from)
                    .collect();
                if let Some(bad) = o.presets.iter().find(|p| presets::by_name(p).is_none()) {
                    anyhow::bail!("unknown preset {bad}\n{USAGE}");
                }
            }
            "--out" => o.out = PathBuf::from(args.next().context("--out needs a directory")?),
            "--json" => o.json = Some(PathBuf::from(args.next().context("--json needs a path")?)),
            "--force" => o.force = true,
            "--no-seams" => o.seams = false,
            "--" => o.files.extend(args.by_ref().map(PathBuf::from)),
            f if cli::is_flag(f) => return Err(cli::unknown_flag(f, USAGE)),
            f => o.files.push(PathBuf::from(f)),
        }
    }
    if o.files.is_empty() {
        anyhow::bail!("{USAGE}");
    }
    Ok(cli::Parsed::Run(o))
}

#[tokio::main]
async fn main() -> Result<()> {
    let o = match parse_args(std::env::args().skip(1))? {
        cli::Parsed::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        cli::Parsed::Run(o) => o,
    };
    // Nothing is written unless every input exists and the results file
    // would not replace another.
    cli::check_inputs(&o.files)?;
    let json_path = match (cli::bench_results(), o.json.clone()) {
        (None, None) => {
            eprintln!("results are printed, not written (pass --json PATH or set FLUX_BENCH_RESULTS)");
            None
        }
        (dir, explicit) => Some(cli::results_path(
            dir.as_deref().unwrap_or(Path::new(".")),
            "flux-hls-validate",
            explicit,
            o.force,
        )?),
    };
    if let Some(d) = json_path.as_ref().and_then(|p| p.parent()) {
        std::fs::create_dir_all(d)?;
    }
    let (out, seams, files, presets_list) = (o.out, o.seams, o.files, o.presets);
    let engine = HlsEngine::new(EngineConfig {
        audio_lookahead: 0,
        ..Default::default()
    });
    let load_start = run(Command::new("sysctl").args(["-n", "vm.loadavg"]))
        .map(|r| r.1.trim().to_string())
        .unwrap_or_default();
    let started = cli::utc_stamp();
    let mut rows = Vec::new();
    println!("| file | preset | plan | segs | index ms | seg ms p50 | ffmpeg errs | video pkts | audio | A/V Δ ms | status |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    for f in &files {
        for p in &presets_list {
            let row = validate(&engine, f, p, &out, seams).await?;
            let audio = row["audio"].as_array().map(|v| {
                v.iter()
                    .map(|a| {
                        let seam = a["seams"]["peak_over_p999"]
                            .as_f64()
                            .map(|r| format!(" seam×{r:.2}"))
                            .unwrap_or_default();
                        format!(
                            "{}:{}{}",
                            a["rendition"].as_str().unwrap_or(""),
                            if a["match"] == json!(false) {
                                "MISMATCH"
                            } else {
                                "ok"
                            },
                            seam
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            let avd = row["audio"].as_array().and_then(|v| {
                v.iter()
                    .map(|a| a["av_offset_delta_ms"].as_f64().unwrap_or(0.0).abs())
                    .reduce(f64::max)
            });
            println!(
                "| {} | {} | {} | {} | {:.1} | {:.2} | {} | {}/{} | {} | {} | {} |",
                row["file"].as_str().unwrap_or(""),
                p,
                row["hls_label"].as_str().unwrap_or(""),
                row["segments"],
                row["index_ms"].as_f64().unwrap_or(0.0),
                row["segment_ms_median"].as_f64().unwrap_or(0.0),
                row["ffmpeg_errors"],
                row["video"]["packets"],
                row["video"]["source_packets"],
                audio.unwrap_or_default(),
                avd.map(|v| format!("{v:.2}")).unwrap_or_default(),
                row["status"].as_str().unwrap_or(""),
            );
            rows.push(row);
            // Rewritten after every case: an interrupted run keeps its rows.
            let ok = rows.iter().filter(|r| r["status"] == json!("ok")).count();
            let result = json!({ "tool": "flux-hls", "kind": "validate", "started": started,
                "load_avg_start": load_start, "presets": presets_list, "cases": rows.len(), "ok": ok, "rows": rows });
            if let Some(p) = &json_path {
                std::fs::write(p, serde_json::to_vec_pretty(&result)?)?;
            }
        }
    }
    let load = run(Command::new("sysctl").args(["-n", "vm.loadavg"]))
        .map(|r| r.1.trim().to_string())
        .unwrap_or_default();
    let ok = rows.iter().filter(|r| r["status"] == json!("ok")).count();
    let result = json!({ "tool": "flux-hls", "kind": "validate", "started": started, "finished": cli::utc_stamp(),
        "load_avg_start": load_start, "load_avg": load, "presets": presets_list, "cases": rows.len(), "ok": ok, "rows": rows });
    match &json_path {
        Some(p) => {
            std::fs::write(p, serde_json::to_vec_pretty(&result)?)?;
            eprintln!("wrote {} ({ok}/{} ok)", p.display(), rows.len());
        }
        None => eprintln!("{ok}/{} ok", rows.len()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(a: &[&str]) -> Result<cli::Parsed<Opts>> {
        parse_args(a.iter().map(|s| s.to_string()))
    }

    /// PB-NEW-3: `--help` is help, not an input file; typos are errors.
    #[test]
    fn help_and_unknown_flags() {
        assert!(matches!(parse(&["--help"]).unwrap(), cli::Parsed::Help));
        assert!(matches!(parse(&["a.mkv", "-h"]).unwrap(), cli::Parsed::Help));
        let e = parse(&["--hepl", "a.mkv"]).err().unwrap().to_string();
        assert!(e.contains("unknown option --hepl"), "{e}");
        assert!(parse(&[]).is_err());
        assert!(parse(&["--presets", "chrom", "a.mkv"]).is_err());
        let cli::Parsed::Run(o) = parse(&[
            "--presets",
            "legacy",
            "--no-seams",
            "--force",
            "a.mkv",
            "--",
            "-odd.mkv",
        ])
        .unwrap() else {
            panic!("run")
        };
        assert_eq!(o.presets, vec!["legacy"]);
        assert!(o.force && !o.seams);
        assert_eq!(o.files, vec![PathBuf::from("a.mkv"), PathBuf::from("-odd.mkv")]);
        assert_eq!(o.json, None);
    }

    /// PB-NEW-2: a transcode that loses frames fails, whatever the
    /// source's VFR intervals; one surplus frame per real hole is allowed.
    #[test]
    fn transcode_frame_count_tolerance() {
        // 288 frames at 23.976 with a 59.94 stretch (no holes).
        let fd = 1001.0 / 24000.0;
        let mut pts: Vec<f64> = (0..144).map(|k| k as f64 * fd).collect();
        pts.extend((0..96).map(|k| 144.0 * fd + k as f64 * 1001.0 / 60000.0));
        let t = *pts.last().unwrap();
        pts.extend((1..=48).map(|k| t + k as f64 * fd));
        assert!(!video_count_ok(230, 288, true, &pts, fd), "lost 58 frames");
        assert!(video_count_ok(288, 288, true, &pts, fd));
        assert!(!video_count_ok(289, 288, true, &pts, fd), "no hole to fill");
        // One missing source frame (a 2-frame interval) may be filled.
        let holed: Vec<f64> = (0..10).filter(|&k| k != 5).map(|k| k as f64 * fd).collect();
        assert!(video_count_ok(10, 9, true, &holed, fd));
        assert!(!video_count_ok(10, 9, false, &holed, fd), "a copy must match");
    }
}
