//! Segments of an MPEG-TS/M2TS asset, produced from the index alone.
//!
//! - **Video** segment n: the video PES packets that start in the byte range
//!   `[pos(kf_n), pos(kf_n+1))`. Each PES is one access unit (checked at
//!   index time); Annex B becomes 4-byte length prefixes, AUDs and filler
//!   are dropped, and so are parameter sets (they are in `avcC`/`hvcC`).
//!   Timestamps follow the Matroska/MP4 rules: PTS on the frame grid,
//!   DTS = sorted PTS, CTS = PTS + D − DTS.
//! - **Audio** segment n: frames whose presentation time is in
//!   `[T_n, T_n+1)`. Audio is muxed ahead of or behind the video, so the
//!   read window extends past the video range and grows until it provably
//!   holds every such frame: a PES at or before `T_n` at its start, and a
//!   complete PES at or after `T_n+1` at its end.
//!
//! Every segment is a pure function of (index, n): no state is carried
//! between requests.

use bytes::Bytes;

use super::audio::{self as taudio, Framer};
use super::packet::{self, Framing};
use super::pes::{unwrap_near, Assembler, Pes};
use super::video::{self, Flavor};
use super::TsLayout;
use crate::asset::{Asset, SourceFrame};
use crate::codec;
use crate::error::{HlsError, Result};
use crate::fmp4::{self, Fragment, Sample, SampleData};
use crate::index::{ns_to_ticks, ticks_to_ns, IndexTrack};
use crate::segment::Span;
use crate::source::Source;
use crate::SegmentBody;

/// Bytes per read while demuxing a range.
const READ_CHUNK: usize = 4 << 20;

fn malformed(detail: impl Into<String>) -> HlsError {
    HlsError::Malformed {
        container: "MPEG-TS",
        detail: detail.into(),
    }
}

impl TsLayout {
    pub(crate) fn framing(&self) -> Framing {
        Framing::of(self.packet_size)
    }

    /// The offset added to `pid`'s unwrapped timestamps at file offset `pos`
    /// (the epoch it is in).
    pub(crate) fn offset_at(&self, pid: u16, pos: u64) -> i64 {
        self.discontinuities
            .iter()
            .rev()
            .find(|d| d.starts.iter().any(|&(p, at)| p == pid && at <= pos))
            .map(|d| d.offset)
            .unwrap_or(0)
    }

    /// Continuous 90 kHz time of a 33-bit timestamp of `pid` at `pos`,
    /// unwrapped to the value nearest `near` (a continuous time).
    pub(crate) fn corrected(&self, pid: u16, pos: u64, raw: u64, near: i64) -> i64 {
        let o = self.offset_at(pid, pos);
        o + unwrap_near(raw, near - o)
    }

    /// Asset timeline (ns) of a continuous 90 kHz time.
    pub(crate) fn ns(&self, t90: i64) -> i64 {
        ticks_to_ns(t90 - self.origin, 90_000)
    }
}

fn flavor_of(v: &IndexTrack) -> Result<Flavor> {
    match v.codec {
        flux_core::Codec::H264 => Ok(Flavor::H264),
        flux_core::Codec::Hevc => Ok(Flavor::Hevc),
        c => Err(HlsError::Unsupported(format!("{c:?} video from MPEG-TS"))),
    }
}

/// The PES packets of `pid` that start in `[from, to)`; the one still open
/// at `to` is returned separately, `complete` when `to` ends this PID's
/// data there (the file's end, or the next PES of the PID starts at `to`).
fn pes_in(
    src: &Source,
    l: &TsLayout,
    pid: u16,
    from: u64,
    to: u64,
    boundary: bool,
) -> Result<(Vec<Pes>, Option<Pes>)> {
    let mut asm = Assembler::default();
    let mut out = Vec::new();
    packet::scan(src, from, to, l.framing(), READ_CHUNK, |pk| {
        if pk.pid == pid {
            asm.push(pk, &mut out);
        }
        true
    })?;
    let tail = asm.finish(boundary || to >= l.data_end);
    Ok((out, tail))
}

/// Video fragment of segment `span` (copy).
pub(crate) fn video_fragment(asset: &Asset, l: &TsLayout, span: &Span) -> Result<Fragment> {
    let v = asset
        .index
        .video_track()
        .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
    let flavor = flavor_of(v)?;
    let pid = v.id as u16;
    let kf = &asset.index.keyframes;
    let first = kf
        .get(span.kf)
        .ok_or_else(|| malformed(format!("keyframe {}", span.kf)))?;
    let to = span
        .next_kf
        .and_then(|k| kf.get(k))
        .map(|k| k.pos)
        .unwrap_or(l.data_end);
    let (mut pes, tail) = pes_in(asset.source(), l, pid, first.pos, to, true)?;
    pes.extend(tail);
    let strip = asset.strip_parameter_sets;
    let damaged = pes.iter().filter(|p| p.damaged).count();
    if damaged > 0 {
        tracing::debug!(
            damaged,
            n = span.kf,
            "video PES with lost packets (passed on as they are)"
        );
    }
    // (PTS ticks, sample, sync) in decode order.
    let mut frames: Vec<(i64, Bytes, bool)> = Vec::with_capacity(pes.len());
    for p in &pes {
        let Some(raw) = p.header.pts else { continue };
        let t90 = l.corrected(pid, p.pos, raw, first.raw_ts);
        let key = kf.binary_search_by_key(&p.pos, |k| k.pos).is_ok();
        let data = video::to_sample(&p.payload, flavor, strip);
        if data.is_empty() {
            continue; // nothing but delimiters or filler
        }
        frames.push((v.snap_ticks(l.ns(t90)), Bytes::from(data), key));
    }
    if frames.is_empty() {
        return Err(malformed(format!(
            "no video frames found for the segment at {} ns",
            span.start_ns
        )));
    }
    // DTS synthesized from sorted PTS, CTS = PTS + D − DTS ≥ 0 (research/03 §4.4).
    let mut sorted: Vec<i64> = frames.iter().map(|f| f.0).collect();
    sorted.sort_unstable();
    let d = v.ns_to_ticks(asset.index.presentation_offset_ns);
    let lift = (-sorted[0]).max(0);
    let fallback = match v.frame_duration {
        Some(fd) => fd as i64,
        None if sorted.len() > 1 => (sorted[sorted.len() - 1] - sorted[0]) / (sorted.len() as i64 - 1),
        None => 1,
    };
    let samples = frames
        .into_iter()
        .enumerate()
        .map(|(i, (pts, data, sync))| {
            let dts = sorted[i];
            let dur = match sorted.get(i + 1) {
                Some(next) => next - dts,
                None => match v.frame_duration {
                    Some(fd) => fd as i64,
                    None => (span.end_ticks - dts).max(fallback.max(1)),
                },
            };
            Sample {
                duration: dur.clamp(0, u32::MAX as i64) as u32,
                cts: (pts + d - dts).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                sync,
                data: SampleData::Bytes(data),
            }
        })
        .collect();
    Ok(Fragment {
        track_id: Asset::track_id(v),
        base_dts: (sorted[0] + lift) as u64,
        samples,
        video: true,
    })
}

/// Audio frames of track `t` presented in `[t0, t1)` (ns), read around the
/// file range `[from, to)`; `near` is a continuous 90 kHz time close to them.
#[allow(clippy::too_many_arguments)]
fn audio_frames_in(
    src: &Source,
    l: &TsLayout,
    t: &IndexTrack,
    t0: i64,
    t1: i64,
    from: u64,
    to: u64,
    near: i64,
) -> Result<Vec<taudio::Frame>> {
    let framing = taudio::Framing::of(t.codec, &t.codec_tag).ok_or_else(|| {
        HlsError::Unsupported(format!("{:?} audio from MPEG-TS is read by ffmpeg", t.codec))
    })?;
    let pid = t.id as u16;
    // About 1.5 s of the file on each side covers ordinary mux skew.
    let margin = (l.byte_rate.saturating_mul(3) / 2).max(1 << 20);
    let mut lo = if t0 == i64::MIN {
        l.first_packet
    } else {
        from.saturating_sub(margin).max(l.first_packet)
    };
    let mut hi = if t1 == i64::MAX {
        l.data_end
    } else {
        to.saturating_add(margin).min(l.data_end)
    };
    // Windows grow (doubling) until they provably hold every frame; past
    // about 48 s of skew on a side (a track that ended, a long gap) take
    // what is there.
    let limit = margin.saturating_mul(32);
    loop {
        let at_end = hi >= l.data_end;
        let (mut pes, tail) = pes_in(src, l, pid, lo, hi, false)?;
        if at_end {
            pes.extend(tail.filter(|p| p.complete));
        }
        let times: Vec<Option<i64>> = pes
            .iter()
            .map(|p| p.header.pts.map(|raw| l.ns(l.corrected(pid, p.pos, raw, near))))
            .collect();
        let first_t = times.iter().flatten().next().copied();
        let start_ok = lo <= l.first_packet || first_t.is_some_and(|t| t <= t0);
        let end_ok = at_end || times.iter().flatten().any(|&t| t >= t1);
        let lo_done = start_ok || from.saturating_sub(lo) >= limit;
        let hi_done = end_ok || hi.saturating_sub(to) >= limit;
        if lo_done && hi_done {
            if !(start_ok && end_ok) {
                tracing::debug!(pid, t0, t1, "audio window gave up growing (track ended or gap)");
            }
            let mut fr = Framer::new(framing);
            let mut out = Vec::new();
            for (p, t) in pes.iter().zip(&times) {
                fr.push(&p.payload, p.pos, *t, &mut out);
            }
            fr.finish(&mut out);
            out.retain(|f| f.pts_ns >= t0 && f.pts_ns < t1);
            return Ok(out);
        }
        if !lo_done {
            lo = lo
                .saturating_sub(from.saturating_sub(lo).max(margin))
                .max(l.first_packet);
        }
        if !hi_done {
            hi = hi
                .saturating_add(hi.saturating_sub(to).max(margin))
                .min(l.data_end);
        }
    }
}

/// Copied audio fragment of track `track` for segment `span`.
pub(crate) fn audio_fragment(asset: &Asset, l: &TsLayout, track: u32, span: &Span) -> Result<Fragment> {
    let a = asset
        .index
        .track(track)
        .ok_or_else(|| HlsError::NotFound(format!("track {track}")))?;
    let kf = &asset.index.keyframes;
    let first = kf
        .get(span.kf)
        .ok_or_else(|| malformed(format!("keyframe {}", span.kf)))?;
    let to = span
        .next_kf
        .and_then(|k| kf.get(k))
        .map(|k| k.pos)
        .unwrap_or(l.data_end);
    let t0 = if span.kf == 0 { i64::MIN } else { span.start_ns };
    let t1 = if span.next_kf.is_none() {
        i64::MAX
    } else {
        span.end_ns
    };
    let frames = audio_frames_in(asset.source(), l, a, t0, t1, first.pos, to, first.raw_ts)?;
    let d = a.ns_to_ticks(asset.index.presentation_offset_ns);
    let fd = a.frame_duration.map(|x| x as i64);
    // Each frame at its own timestamp (90 kHz timestamps are sample-exact),
    // so a frame is placed the same way in every segment it could be in.
    let pos: Vec<i64> = frames
        .iter()
        .map(|f| ns_to_ticks(f.pts_ns, a.timescale))
        .collect();
    let samples = frames
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let own = f.samples as i64 * a.timescale as i64 / f.rate.max(1) as i64;
            let dur = match (pos.get(i + 1), fd) {
                (Some(next), Some(fd)) => (next - pos[i]).clamp(1, 4 * fd),
                (Some(next), None) => (next - pos[i]).max(1),
                (None, _) => own.max(1),
            };
            Sample {
                duration: dur as u32,
                cts: 0,
                sync: true,
                data: SampleData::Bytes(f.data),
            }
        })
        .collect();
    Ok(Fragment {
        track_id: Asset::track_id(a),
        base_dts: pos.first().map(|p| p + d).unwrap_or(0).max(0) as u64,
        samples,
        video: false,
    })
}

/// Source audio frames of `track` presented in `[t0, t1)` (ns), for the
/// transcoder: raw AAC (it re-adds ADTS headers), AC-3/E-AC-3 sync frames,
/// MPEG audio frames.
pub(crate) fn source_frames(
    asset: &Asset,
    l: &TsLayout,
    track: u32,
    t0: i64,
    t1: i64,
) -> Result<Vec<SourceFrame>> {
    let a = asset
        .index
        .track(track)
        .ok_or_else(|| HlsError::NotFound(format!("track {track}")))?;
    let kf = &asset.index.keyframes;
    let at = kf.partition_point(|k| k.pts_ns <= t0).saturating_sub(1);
    let from = kf.get(at).map(|k| k.pos).unwrap_or(l.first_packet);
    let to = kf
        .get(kf.partition_point(|k| k.pts_ns < t1))
        .map(|k| k.pos)
        .unwrap_or(l.data_end);
    let near = if t0 == i64::MIN {
        kf.first().map(|k| k.raw_ts).unwrap_or(l.origin)
    } else {
        l.origin + ns_to_ticks(t0, 90_000)
    };
    let frames = audio_frames_in(asset.source(), l, a, t0, t1, from, to, near)?;
    Ok(frames
        .into_iter()
        .map(|f| SourceFrame {
            pts_ns: f.pts_ns,
            data: f.data,
        })
        .collect())
}

/// I-frame segment `n` (keyframe `n` alone), on the copied rendition's timeline.
pub(crate) fn keyframe_segment(asset: &Asset, l: &TsLayout, n: usize) -> Result<SegmentBody> {
    let v = asset
        .index
        .video_track()
        .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
    let flavor = flavor_of(v)?;
    let pid = v.id as u16;
    let kfs = &asset.index.keyframes;
    let kf = kfs
        .get(n)
        .ok_or_else(|| HlsError::NotFound(format!("keyframe {n}")))?;
    let to = kfs.get(n + 1).map(|k| k.pos).unwrap_or(l.data_end);
    // Only the keyframe's own PES: stop at the next video PES start.
    let mut asm = Assembler::default();
    let mut out: Vec<Pes> = Vec::new();
    packet::scan(asset.source(), kf.pos, to, l.framing(), 1 << 20, |pk| {
        if pk.pid == pid {
            asm.push(pk, &mut out);
        }
        out.is_empty()
    })?;
    let pes = out
        .into_iter()
        .next()
        .or_else(|| asm.finish(true))
        .ok_or_else(|| malformed(format!("keyframe {n}: no PES")))?;
    let mut data = video::to_sample(&pes.payload, flavor, asset.strip_parameter_sets);
    if flavor == Flavor::Hevc {
        // CRA → BLA_W_LP: each I-frame starts its own coded video sequence
        // (see `Asset::keyframe_segment`).
        codec::cra_to_bla(&mut data, 4);
    }
    let start = v.snap_ticks(kf.pts_ns);
    let end = kfs
        .get(n + 1)
        .map(|k| v.snap_ticks(k.pts_ns))
        .unwrap_or(start + v.frame_duration.unwrap_or(1) as i64);
    let d = v.ns_to_ticks(asset.index.presentation_offset_ns);
    let frag = Fragment {
        track_id: Asset::track_id(v),
        base_dts: (start + d).max(0) as u64,
        samples: vec![Sample {
            duration: (end - start).clamp(1, u32::MAX as i64) as u32,
            cts: 0,
            sync: true,
            data: SampleData::Bytes(Bytes::from(data)),
        }],
        video: true,
    };
    Ok(fmp4::media_segment(n as u32 + 1, &[frag]))
}

/// MPEG-TS for ffmpeg to transcode the video from keyframe `from` without
/// seeking: the file's first PAT and PMT packets, then every packet of the
/// video PID from that keyframe's PES up to keyframe `to` (exclusive; `None`
/// = the end), as 188-byte packets. Decoding starts on the keyframe and the
/// stream's start time is its PTS; leading pictures of an open GOP come out
/// with negative times, for `trim` to drop.
///
/// ffmpeg's own `-ss` into a TS file seeks by DTS to any PES, not to a
/// keyframe: HEVC then fails on missing references and loses frames (an HDR10
/// M2TS gave 95 of 143 frames in its last 6 s; an open-GOP HEVC TS exited
/// with errors). `chunk` receives the bytes in order (about 1 MB at a time).
pub fn video_input(
    asset: &Asset,
    from: usize,
    to: Option<usize>,
    mut chunk: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<()> {
    let crate::index::Layout::Ts(l) = &asset.index.layout else {
        return Err(HlsError::Unsupported("not an MPEG-TS asset".into()));
    };
    let v = asset
        .index
        .video_track()
        .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
    let pid = v.id as u16;
    let kfs = &asset.index.keyframes;
    let start = kfs
        .get(from)
        .ok_or_else(|| HlsError::NotFound(format!("keyframe {from}")))?
        .pos;
    let end = to.and_then(|k| kfs.get(k)).map(|k| k.pos).unwrap_or(l.data_end);
    let f = l.framing();
    // The program tables, from the start of the file.
    let mut psi = Vec::new();
    let (mut pat, mut pmt) = (super::psi::Section::default(), super::psi::Section::default());
    let (mut pmt_pid, mut have_pmt) = (None::<u16>, false);
    packet::scan(
        asset.source(),
        l.first_packet,
        l.data_end.min(l.first_packet + (4 << 20)),
        f,
        1 << 20,
        |pk| {
            if pk.pid == 0 && pmt_pid.is_none() {
                psi.extend_from_slice(pk.raw);
                if let Some(s) = pat.push(pk) {
                    pmt_pid = super::psi::parse_pat(&s).first().copied();
                }
            } else if Some(pk.pid) == pmt_pid {
                psi.extend_from_slice(pk.raw);
                if let Some(s) = pmt.push(pk) {
                    have_pmt = super::psi::parse_pmt(&s)
                        .is_some_and(|(_, _, _, es)| es.iter().any(|e| e.pid == pid));
                }
            }
            !have_pmt
        },
    )?;
    if !have_pmt {
        return Err(malformed("no PAT/PMT for the video PID"));
    }
    chunk(&psi)?;
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut err = None;
    packet::scan(asset.source(), start, end, f, READ_CHUNK, |pk| {
        if pk.pid == pid {
            buf.extend_from_slice(pk.raw);
            if buf.len() >= 1 << 20 {
                if let Err(e) = chunk(&buf) {
                    err = Some(e);
                    return false;
                }
                buf.clear();
            }
        }
        true
    })?;
    if let Some(e) = err {
        return Err(e.into());
    }
    if !buf.is_empty() {
        chunk(&buf)?;
    }
    Ok(())
}
