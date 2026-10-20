//! Segment plan: keyframe-aligned segment boundaries with exact durations.
//!
//! Cut at the first video keyframe at or after `target` past the segment
//! start (research/03 §4.3). Durations are exact in the video track's
//! timescale, so EXTINF values are known before any segment is produced.

use serde::{Deserialize, Serialize};

use crate::error::{HlsError, Result};
use crate::index::{Layout, PlaybackIndex};

/// One segment of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Index into `PlaybackIndex::keyframes` of the first keyframe.
    pub kf: usize,
    /// Keyframe index the next segment starts at (`None` for the last).
    pub next_kf: Option<usize>,
    /// Presentation start/end in source nanoseconds.
    pub start_ns: i64,
    pub end_ns: i64,
    /// Start/end in video timescale ticks (snapped to the frame grid).
    pub start_ticks: i64,
    pub end_ticks: i64,
    /// Approximate size of the segment's source byte range (bandwidth estimates).
    pub approx_bytes: u64,
}

/// Keyframe-aligned segmentation of one asset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentPlan {
    pub target_ns: i64,
    /// Video timescale the ticks are expressed in.
    pub timescale: u32,
    pub segments: Vec<Span>,
}

impl SegmentPlan {
    /// Plans segments from the index's keyframes.
    pub fn build(idx: &PlaybackIndex, target_ns: i64) -> Result<Self> {
        let v = idx
            .video_track()
            .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
        let kfs = &idx.keyframes;
        if kfs.is_empty() {
            return Err(HlsError::Unsupported("no keyframe index".into()));
        }
        let mut bounds = vec![0usize];
        let mut last = kfs[0].pts_ns;
        for (i, k) in kfs.iter().enumerate().skip(1) {
            if k.pts_ns >= last + target_ns {
                bounds.push(i);
                last = k.pts_ns;
            }
        }
        let frame_ns = v.default_duration_ns.unwrap_or(0) as i64;
        let last_kf = kfs[*bounds.last().unwrap_or(&0)].pts_ns;
        // The last segment ends where the video does (its last frame + that
        // frame's duration), not at the container duration: a subtitle or
        // audio track that outlasts the video would otherwise stretch the
        // last EXTINF past the picture (mkv_fonts.mkv: 4 s vs 2 s).
        let end_ns = match v.extra.end_ns.filter(|&e| e > last_kf) {
            Some(e) => e,
            None => idx.duration_ns.max(last_kf + frame_ns.max(1)),
        };
        let mp4_video = match &idx.layout {
            Layout::Mp4(l) => idx.video.and_then(|vi| l.tracks.get(vi)),
            _ => None,
        };
        // Matroska byte ranges hold every track: take out the others' bytes
        // (their bitrates, from codec headers or measured at index time), so
        // BANDWIDTH counts what the video rendition sends (PB-10).
        let others_bps: u64 = match &idx.layout {
            Layout::Matroska(_) => idx
                .tracks
                .iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != idx.video)
                .filter_map(|(_, t)| t.bitrate)
                .sum(),
            _ => 0,
        };
        let mut segments = Vec::with_capacity(bounds.len());
        for (i, &b) in bounds.iter().enumerate() {
            let next = bounds.get(i + 1).copied();
            let start_ns = kfs[b].pts_ns;
            let seg_end_ns = next.map(|n| kfs[n].pts_ns).unwrap_or(end_ns);
            let approx_bytes = match (&idx.layout, mp4_video) {
                (Layout::Matroska(l), _) => {
                    let s = if b == 0 { l.first_cluster } else { kfs[b].pos };
                    let e = next.map(|n| kfs[n].pos).unwrap_or(l.data_end);
                    let range = e.saturating_sub(s);
                    let secs = (seg_end_ns - start_ns).max(0) as f64 / 1e9;
                    let others = (others_bps as f64 * secs / 8.0) as u64;
                    // Never below a fifth of the range: the estimate of the
                    // other tracks may be high where the video is sparse.
                    range.saturating_sub(others).max(range / 5)
                }
                (Layout::Mp4(_), Some(t)) => {
                    let from = kfs[b].pos as u32;
                    let to = next.map(|n| kfs[n].pos as u32).unwrap_or(t.sample_count);
                    t.bytes_of(from, to)
                }
                _ => 0,
            };
            segments.push(Span {
                kf: b,
                next_kf: next,
                start_ns,
                end_ns: seg_end_ns,
                start_ticks: v.snap_ticks(start_ns),
                end_ticks: v.snap_ticks(seg_end_ns),
                approx_bytes,
            });
        }
        Ok(SegmentPlan {
            target_ns,
            timescale: v.timescale,
            segments,
        })
    }

    /// Number of segments.
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// True when there are no segments.
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Exact duration of segment `n` in seconds.
    pub fn duration_secs(&self, n: usize) -> f64 {
        self.segments
            .get(n)
            .map(|s| (s.end_ticks - s.start_ticks) as f64 / self.timescale as f64)
            .unwrap_or(0.0)
    }

    /// `EXT-X-TARGETDURATION`: ≥ every EXTINF rounded to the nearest integer (RFC 8216 §4.3.3.1).
    pub fn target_duration(&self) -> u64 {
        (0..self.len())
            .map(|n| self.duration_secs(n).round() as u64)
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// Segment covering playlist time `t` (seconds from the first segment).
    pub fn segment_at(&self, t: f64) -> usize {
        let mut acc = 0.0;
        for n in 0..self.len() {
            acc += self.duration_secs(n);
            if t < acc {
                return n;
            }
        }
        self.len().saturating_sub(1)
    }

    /// Peak and average bits per second of the video byte ranges.
    pub fn bandwidth(&self) -> (u64, u64) {
        let mut peak = 0u64;
        let mut bytes = 0u64;
        let mut secs = 0.0;
        for (n, s) in self.segments.iter().enumerate() {
            let d = self.duration_secs(n).max(0.001);
            peak = peak.max((s.approx_bytes as f64 * 8.0 / d) as u64);
            bytes += s.approx_bytes;
            secs += d;
        }
        let avg = if secs > 0.0 {
            (bytes as f64 * 8.0 / secs) as u64
        } else {
            0
        };
        (peak, avg)
    }
}
