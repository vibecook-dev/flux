//! Artwork for items that have no provider art.
//!
//! - **Frame posters (default, no AI, $0):** pick the best keyframe from the
//!   user's own file, crop a 2:3 poster and a 16:9 backdrop around its most
//!   detailed region, grade, and typeset the title and year deterministically.
//!   Labelled [`ArtSource::Frame`]. See [`frame_posters`].
//! - **Episode thumbnails:** same frame picker inside a spoiler-safe window
//!   (10–50%), 16:9, no text. See [`episode_thumbnail`].
//! - **GenAI posters (opt-in):** [`genai`]: text-free key art from a
//!   [`genai::PosterGenerator`], our own typography on top, provenance
//!   embedded, labelled [`ArtSource::Generated`], under a budget.
//!
//! Pipeline for a frame poster (research/05 §4.1, §5.6):
//!
//! ```text
//! ffprobe (or flux-probe facts) → 12 seek times over 10–80% of the runtime
//! → 12 × ffmpeg keyframe decodes at 480 px (4 in parallel, software decode)
//! → letterbox detection, quality score, near-duplicate removal
//! → best frame re-decoded at full resolution (HDR tone-mapped)
//! → energy-based 2:3 and 16:9 crops → grade + typography → JPEG + XMP + blurhash
//! ```

mod compose;
pub mod crop;
pub mod encode;
mod frames;
pub mod genai;
mod score;
mod tonemap;
pub mod typeset;

use std::path::{Path, PathBuf};
use std::time::Instant;

use flux_core::api::ArtSource;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

pub use crop::Rect;
pub use encode::{encode_jpeg, read_xmp, ArtImage, DigitalSourceType, Provenance};
pub use frames::{Frame, MediaFacts, Tools, Transfer};
pub use score::FrameScore;
pub use typeset::Typesetter;

/// The version of flux's own art: frame posters, backdrops and episode
/// thumbnails, and the typography on generated posters. Bump it whenever
/// the images made from the same file and title change (fonts, colour,
/// layout): flux-server then makes every image of an older version again
/// (`flux_catalog::read::stale_art`). Images made before art was
/// versioned (up to 0.1.0-alpha.2) have none and are made again too.
///
/// - 1: titles in CJK scripts typeset with glyphs (no empty boxes), and
///   Dolby Vision profile 5 frames tone-mapped (no green and magenta cast).
pub const ART_VERSION: u32 = 1;

/// Errors from artwork generation.
#[derive(Debug, thiserror::Error)]
pub enum ArtError {
    #[error("could not run {tool}: {source}")]
    Spawn { tool: String, source: std::io::Error },
    #[error("{tool} failed on {path}: {stderr}")]
    Tool {
        tool: String,
        path: PathBuf,
        stderr: String,
    },
    #[error("{tool} timed out on {path}")]
    Timeout { tool: String, path: PathBuf },
    #[error("could not understand ffprobe output for {path}: {reason}")]
    Probe { path: PathBuf, reason: String },
    #[error("no decodable frame found in {0}")]
    NoFrames(PathBuf),
    #[error("image encoding failed: {0}")]
    Encode(String),
    #[error("image decoding failed: {0}")]
    Decode(String),
    #[error("background task failed: {0}")]
    Join(String),
    #[error("image generator error: {0}")]
    Generator(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("budget: {0}")]
    Budget(#[from] crate::jobs::BudgetError),
    #[error("generative art is disabled for this library")]
    Disabled,
}

/// Frame-poster settings.
#[derive(Debug, Clone)]
pub struct FramePosterOptions {
    pub tools: Tools,
    /// Candidate keyframes per title (default 12).
    pub candidates: usize,
    /// Fraction of the runtime to sample, avoiding logos and credits (default 10–80%).
    pub window: (f64, f64),
    /// Width of the frames used for scoring (default 480).
    pub analysis_width: u32,
    /// Poster width bounds; height is 1.5× (default 600..=1000).
    pub poster_width: (u32, u32),
    /// Backdrop width bounds; height is 9/16 (default 1280..=1920).
    pub backdrop_width: (u32, u32),
    /// Episode thumbnail width (default 640 → 640×360).
    pub thumb_width: u32,
    pub jpeg_quality: u8,
    /// Concurrent ffmpeg processes per title (default 4).
    pub concurrency: usize,
}

impl Default for FramePosterOptions {
    fn default() -> Self {
        FramePosterOptions {
            tools: Tools::default(),
            candidates: 12,
            window: (0.10, 0.80),
            analysis_width: 480,
            poster_width: (600, 1000),
            backdrop_width: (1280, 1920),
            thumb_width: 640,
            jpeg_quality: 86,
            concurrency: 4,
        }
    }
}

/// Where the time went (milliseconds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    pub probe_ms: f64,
    pub harvest_ms: f64,
    pub score_ms: f64,
    pub final_decode_ms: f64,
    pub compose_ms: f64,
    pub total_ms: f64,
}

/// A frame-poster result.
#[derive(Debug, Clone)]
pub struct FramePosters {
    pub poster: ArtImage,
    pub backdrop: ArtImage,
    /// The seek time of the chosen frame.
    pub chosen_at_s: f64,
    /// Every candidate's scores, for explainability.
    pub candidates: Vec<FrameScore>,
    pub facts: MediaFacts,
    pub timings: Timings,
}

/// Candidates harvested from a file: frames with their scores, best first.
#[derive(Debug, Clone)]
pub struct Harvest {
    pub facts: MediaFacts,
    /// (frame, score, active picture area) for non-duplicate candidates, best first.
    pub frames: Vec<(Frame, FrameScore, Rect)>,
    /// All candidate scores including duplicates and rejects.
    pub scores: Vec<FrameScore>,
}

/// Evenly spaced seek times over `window` of the runtime.
pub fn candidate_times(duration_s: f64, n: usize, window: (f64, f64)) -> Vec<f64> {
    let n = n.max(1);
    let (a, b) = (window.0.clamp(0.0, 1.0), window.1.clamp(0.0, 1.0).max(window.0));
    (0..n)
        .map(|i| duration_s * (a + (b - a) * (i as f64 + 0.5) / n as f64))
        .collect()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, ArtError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ArtError::Join(e.to_string()))
}

/// Decode, score and de-duplicate candidate keyframes of `path` within `window`.
pub async fn harvest(
    path: &Path,
    facts: Option<MediaFacts>,
    opts: &FramePosterOptions,
    window: (f64, f64),
    n: usize,
) -> Result<Harvest, ArtError> {
    let facts = match facts {
        Some(f) => f,
        None => MediaFacts::ffprobe(&opts.tools, path).await?,
    };
    let times = candidate_times(facts.duration_s, n, window);
    // Decode the first candidate alone: if hardware decoding had to fall back to
    // software (AV1 on M1), decode the rest in software straight away.
    let mut tools = opts.tools.clone();
    let mut results: Vec<Result<Option<Frame>, ArtError>> = Vec::with_capacity(times.len());
    let first = frames::extract_keyframe(&tools, path, &facts, times[0], opts.analysis_width).await;
    // Likewise, a file whose first candidate needed the decode-everything
    // fallback (AV1 through libdav1d) skips the keyframe-only attempt.
    let mut keyframes_only = true;
    if let Ok(Some(f)) = &first {
        tools.hwaccel &= f.hw;
        keyframes_only = f.keyframes_only;
    }
    results.push(first);
    let tools = &tools;
    results.extend(
        stream::iter(times.into_iter().skip(1))
            .map(|t| async move {
                if keyframes_only {
                    frames::extract_keyframe(tools, path, &facts, t, opts.analysis_width).await
                } else {
                    frames::extract_frame(tools, path, &facts, t, opts.analysis_width, false).await
                }
            })
            .buffered(opts.concurrency.max(1))
            .collect::<Vec<_>>()
            .await,
    );
    let mut got = Vec::new();
    let mut first_err = None;
    for r in results {
        match r {
            Ok(Some(f)) => got.push(f),
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, "candidate frame failed");
                first_err.get_or_insert(e);
            }
        }
    }
    if got.is_empty() {
        return Err(first_err.unwrap_or_else(|| ArtError::NoFrames(path.to_owned())));
    }
    let scored = blocking(move || {
        let mut v: Vec<(Frame, FrameScore, Rect, u64)> = got
            .into_iter()
            .map(|f| {
                let area = crop::active_area(&f.img);
                let s = score::score_frame(&f.img, area, f.at_s);
                let hash = score::average_hash(
                    &image::imageops::crop_imm(&f.img, area.x, area.y, area.w, area.h).to_image(),
                );
                (f, s, area, hash)
            })
            .collect();
        // Best first; then mark near-duplicates of better frames (several seeks
        // land on the same keyframe when GOPs are long).
        v.sort_by(|a, b| {
            b.1.score
                .total_cmp(&a.1.score)
                .then(b.1.sharpness.total_cmp(&a.1.sharpness))
        });
        let mut kept_hashes: Vec<u64> = Vec::new();
        for (_, s, _, h) in &mut v {
            if kept_hashes.iter().any(|k| (k ^ *h).count_ones() <= 3) {
                s.duplicate = true;
            } else {
                kept_hashes.push(*h);
            }
        }
        v
    })
    .await?;
    let scores = scored.iter().map(|(_, s, _, _)| s.clone()).collect();
    let frames = scored
        .into_iter()
        .filter(|(_, s, _, _)| !s.duplicate)
        .map(|(f, s, a, _)| (f, s, a))
        .collect();
    Ok(Harvest {
        facts,
        frames,
        scores,
    })
}

fn even(x: u32) -> u32 {
    x & !1
}

/// Frame poster (2:3, typeset) and backdrop (16:9, textless) for a movie or show.
pub async fn frame_posters(
    path: &Path,
    title: &str,
    year: Option<u16>,
    facts: Option<MediaFacts>,
    opts: &FramePosterOptions,
) -> Result<FramePosters, ArtError> {
    let t0 = Instant::now();
    let facts = match facts {
        Some(f) => f,
        None => MediaFacts::ffprobe(&opts.tools, path).await?,
    };
    let probe_ms = t0.elapsed().as_secs_f64() * 1e3;
    let t1 = Instant::now();
    let h = harvest(path, Some(facts), opts, opts.window, opts.candidates).await?;
    let harvest_ms = t1.elapsed().as_secs_f64() * 1e3;
    let (best, best_score, analysis_area) = h
        .frames
        .first()
        .cloned()
        .ok_or_else(|| ArtError::NoFrames(path.to_owned()))?;
    if best_score.reject.is_some() {
        tracing::info!(path = %path.display(), reason = ?best_score.reject, "every candidate was rejected; using the least bad");
    }

    // Decide crops on the analysis frame (cheap), then re-decode the same keyframe at full size.
    let analysis = best.img.clone();
    let crops = blocking(move || {
        (
            crop::best_crop(&analysis, analysis_area, 2.0 / 3.0),
            crop::best_crop(&analysis, analysis_area, 16.0 / 9.0),
        )
    })
    .await?;
    let t2 = Instant::now();
    let full_w = facts.width.min(3840);
    let tools = Tools {
        hwaccel: opts.tools.hwaccel && best.hw,
        ..opts.tools.clone()
    };
    let full = frames::extract_keyframe(&tools, path, &facts, best.at_s, full_w)
        .await?
        .ok_or_else(|| ArtError::NoFrames(path.to_owned()))?;
    let final_decode_ms = t2.elapsed().as_secs_f64() * 1e3;

    let t3 = Instant::now();
    let (aw, ah) = best.img.dimensions();
    let title_s = title.to_owned();
    let o = opts.clone();
    let (poster, backdrop) = blocking(move || -> Result<(ArtImage, ArtImage), ArtError> {
        let (fw, fh) = full.img.dimensions();
        let (sx, sy) = (f64::from(fw) / f64::from(aw), f64::from(fh) / f64::from(ah));
        let pc = crops.0.scale(sx, sy, fw, fh);
        let bc = crops.1.scale(sx, sy, fw, fh);
        let pw = even(pc.w.clamp(o.poster_width.0, o.poster_width.1));
        let poster_img = compose::render_poster(
            &full.img,
            pc,
            pw,
            pw * 3 / 2,
            &title_s,
            year,
            Typesetter::shared(),
        );
        let bw = even(bc.w.clamp(o.backdrop_width.0, o.backdrop_width.1));
        let backdrop_img = compose::crop_resize(&full.img, bc, bw, even(bw * 9 / 16));
        let desc = format!(
            "Frame poster for \"{title_s}\": a frame from the video at {:.1}s with typeset title",
            full.at_s
        );
        let poster = encode::encode_jpeg(&poster_img, o.jpeg_quality, &Provenance::frame(desc))?;
        let bd = Provenance {
            digital_source_type: DigitalSourceType::DigitalCapture,
            ..Provenance::frame(format!(
                "Backdrop for \"{title_s}\": a frame from the video at {:.1}s",
                full.at_s
            ))
        };
        let backdrop = encode::encode_jpeg(&backdrop_img, o.jpeg_quality, &bd)?;
        Ok((poster, backdrop))
    })
    .await??;
    let compose_ms = t3.elapsed().as_secs_f64() * 1e3;
    Ok(FramePosters {
        poster,
        backdrop,
        chosen_at_s: best.at_s,
        candidates: h.scores,
        facts,
        timings: Timings {
            probe_ms,
            harvest_ms,
            score_ms: 0.0,
            final_decode_ms,
            compose_ms,
            total_ms: t0.elapsed().as_secs_f64() * 1e3,
        },
    })
}

/// A 16:9 episode thumbnail (no text) from the spoiler-safe 10–50% window.
pub async fn episode_thumbnail(
    path: &Path,
    facts: Option<MediaFacts>,
    opts: &FramePosterOptions,
) -> Result<(ArtImage, Vec<FrameScore>), ArtError> {
    let h = harvest(path, facts, opts, (0.10, 0.50), opts.candidates.min(8)).await?;
    let (best, _, area) = h
        .frames
        .first()
        .cloned()
        .ok_or_else(|| ArtError::NoFrames(path.to_owned()))?;
    let analysis = best.img.clone();
    let crop = blocking(move || crop::best_crop(&analysis, area, 16.0 / 9.0)).await?;
    // Re-decode the chosen keyframe at 2× the thumbnail width (the 480 px analysis
    // frame would be upscaled); skip that when the analysis frame is already big enough.
    let want = opts.thumb_width.saturating_mul(2).min(h.facts.width);
    let src = if want > best.img.width() {
        let tools = Tools {
            hwaccel: opts.tools.hwaccel && best.hw,
            ..opts.tools.clone()
        };
        frames::extract_keyframe(&tools, path, &h.facts, best.at_s, want)
            .await?
            .unwrap_or_else(|| best.clone())
    } else {
        best.clone()
    };
    let o = opts.clone();
    let (aw, ah) = best.img.dimensions();
    let img = blocking(move || -> Result<ArtImage, ArtError> {
        let (fw, fh) = src.img.dimensions();
        let c = crop.scale(
            f64::from(fw) / f64::from(aw),
            f64::from(fh) / f64::from(ah),
            fw,
            fh,
        );
        let tw = even(o.thumb_width.min(c.w.max(2)).max(2));
        let thumb = compose::crop_resize(&src.img, c, tw, even(tw * 9 / 16));
        let p = Provenance {
            source: ArtSource::Frame,
            digital_source_type: DigitalSourceType::DigitalCapture,
            model: None,
            prompt_hash: None,
            description: format!("Episode thumbnail: frame at {:.1}s", src.at_s),
        };
        encode::encode_jpeg(&thumb, o.jpeg_quality, &p)
    })
    .await??;
    Ok((img, h.scores))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_times_cover_the_window() {
        let t = candidate_times(100.0, 12, (0.10, 0.80));
        assert_eq!(t.len(), 12);
        assert!(t[0] > 10.0 && t[11] < 80.0);
        assert!(t.windows(2).all(|w| w[1] > w[0]));
    }
}
