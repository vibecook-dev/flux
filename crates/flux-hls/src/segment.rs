//! Segment plan: keyframe-aligned segment boundaries with exact durations.
//!
//! Cut at the first video keyframe at or after `target` past the segment
//! start (research/03 §4.3). Durations are exact in the video track's
//! timescale, so EXTINF values are known before any segment is produced.
//!
//! Containers only ffmpeg reads (`Layout::External`) have no keyframe list:
//! their segments are `target` long on the frame grid of the rate their
//! video is transcoded to, from 0 to the end of the video (else of the
//! file). Nothing learned later changes them, so a playlist a player holds
//! stays valid.

use serde::{Deserialize, Serialize};

use crate::error::{HlsError, Result};
use crate::index::{Layout, PlaybackIndex};

/// One segment of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Index into `PlaybackIndex::keyframes` of the first keyframe. (Time
    /// grid of a `Layout::External` index: the segment's number.)
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
    /// Plans segments from the index's keyframes (a time grid for
    /// `Layout::External`).
    pub fn build(idx: &PlaybackIndex, target_ns: i64) -> Result<Self> {
        let v = idx
            .video_track()
            .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
        if let Layout::External(l) = &idx.layout {
            let parts: Vec<i64> = l.parts.iter().map(|p| p.start_ns).collect();
            let end = v.extra.end_ns.filter(|&e| e > 0).unwrap_or(idx.duration_ns);
            return Self::time_grid(v, end, target_ns, &parts);
        }
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

    /// Segments of `target_ns` (rounded to whole frames) from 0 to the
    /// first frame boundary at or after `end_ns`, on the frame grid of `v`
    /// (its timescale and frame duration), starting anew at each of
    /// `parts` (the starts of an MPEG-PS file's joined recordings, on the
    /// grid): no segment, and so no piece or audio encode, spans a join.
    fn time_grid(v: &crate::index::IndexTrack, end_ns: i64, target_ns: i64, parts: &[i64]) -> Result<Self> {
        if end_ns <= 0 {
            return Err(HlsError::Unsupported("the file's duration is unknown".into()));
        }
        let ts = v.timescale.max(1);
        let fd = v.frame_duration.filter(|&f| f > 0).map_or(1, i64::from);
        let whole = |ticks: i64| ((ticks + fd / 2) / fd).max(1) * fd;
        let step = whole(crate::index::ns_to_ticks(target_ns, ts));
        let end = (crate::index::ns_to_ticks(end_ns, ts) + fd - 1) / fd * fd;
        let mut starts: Vec<i64> = parts
            .iter()
            .map(|&p| crate::index::ns_to_ticks(p, ts))
            .filter(|&p| p > 0 && p < end)
            .collect();
        starts.push(end);
        let mut bounds = vec![0i64];
        for stop in starts {
            let from = *bounds.last().unwrap_or(&0);
            let mut a = from + step;
            while a < stop {
                bounds.push(a);
                a += step;
            }
            bounds.push(stop);
        }
        bounds.dedup();
        let n = bounds.len() - 1;
        let segments = (0..n)
            .map(|k| {
                let (a, b) = (bounds[k], bounds[k + 1]);
                Span {
                    kf: k,
                    next_kf: (k + 1 < n).then_some(k + 1),
                    start_ns: crate::index::ticks_to_ns(a, ts),
                    end_ns: crate::index::ticks_to_ns(b, ts),
                    start_ticks: a,
                    end_ticks: b,
                    approx_bytes: 0,
                }
            })
            .collect();
        Ok(SegmentPlan {
            target_ns,
            timescale: ts,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::external::{frame_timebase, ExternalLayout};
    use crate::index::{IndexMethod, IndexStats};
    use crate::source::SourceId;

    fn external(rate: flux_core::Rational, duration_ns: i64) -> PlaybackIndex {
        let (timescale, fd) = frame_timebase(rate);
        let mut v = crate::index::tests::test_track();
        v.timescale = timescale;
        v.frame_duration = Some(fd);
        v.frame_rate = Some(rate);
        PlaybackIndex {
            version: crate::index::INDEX_VERSION,
            source: SourceId {
                path: "x.wmv".into(),
                size: 1,
                mtime_ns: 0,
            },
            container: flux_core::Container::Asf,
            duration_ns,
            tracks: vec![v],
            video: Some(0),
            keyframes: Vec::new(),
            presentation_offset_ns: 0,
            method: IndexMethod::HeaderProbe,
            layout: Layout::External(ExternalLayout {
                start_time_ns: 0,
                seek_margin_ns: 500_000_000,
                parts: Vec::new(),
            }),
            stats: IndexStats::default(),
            warnings: Vec::new(),
        }
    }

    /// No keyframe list: 6 s segments on the frame grid, the last one
    /// ending at the first frame boundary at or after the end.
    #[test]
    fn index_less_files_get_a_time_grid() {
        let r = |num, den| flux_core::Rational { num, den };
        // 25 fps, 20.01 s: 6, 6, 6, 2.04 s.
        let p = SegmentPlan::build(&external(r(25, 1), 20_010_000_000), 6_000_000_000).unwrap();
        let d: Vec<f64> = (0..p.len()).map(|n| p.duration_secs(n)).collect();
        assert_eq!(d, vec![6.0, 6.0, 6.0, 2.04]);
        assert_eq!(p.segments[1].start_ns, 6_000_000_000);
        assert_eq!(p.segments[3].next_kf, None);
        assert_eq!(p.segments[2].next_kf, Some(3));
        // 29.97 fps: whole frames, 180 per segment (6.006 s).
        let p = SegmentPlan::build(&external(r(30000, 1001), 60_000_000_000), 6_000_000_000).unwrap();
        assert_eq!(p.segments[0].end_ticks, 180 * 1001);
        let total: i64 = p.segments.iter().map(|s| s.end_ticks - s.start_ticks).sum();
        assert_eq!(total % 1001, 0);
        assert!(total * 1_000_000_000 / 30_000 >= 60_000_000_000);
        // No duration: refused, not a zero-length playlist.
        assert!(SegmentPlan::build(&external(r(25, 1), 0), 6_000_000_000).is_err());
        // Joined recordings: segments start anew at the join (10 s).
        let mut idx = external(r(25, 1), 18_000_000_000);
        if let Layout::External(l) = &mut idx.layout {
            l.parts = vec![
                crate::index::external::ExternalPart {
                    offset: 0,
                    start_ns: 0,
                    clock_ns: 0,
                },
                crate::index::external::ExternalPart {
                    offset: 1 << 20,
                    start_ns: 10_000_000_000,
                    clock_ns: 500_000_000,
                },
            ];
        }
        let p = SegmentPlan::build(&idx, 6_000_000_000).unwrap();
        let starts: Vec<i64> = p.segments.iter().map(|s| s.start_ns / 1_000_000_000).collect();
        assert_eq!(starts, vec![0, 6, 10, 16]);
        assert_eq!(p.segments.last().unwrap().end_ns, 18_000_000_000);
    }
}
