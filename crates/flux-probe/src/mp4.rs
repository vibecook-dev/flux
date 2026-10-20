//! MP4 / MOV (ISO/IEC 14496-12, QuickTime) header path.
//!
//! Walks top-level box headers (seeking over `mdat`), then reads only the
//! `moov` boxes a scanner needs: `mvhd`, per-`trak` `tkhd`/`mdhd`/`hdlr`/
//! `stsd` (+ codec config boxes), `edts/elst`, `udta`/`meta` titles and `chpl`
//! chapters. **Sample tables (`stts`/`stsz`/`stco`/…) are never read** during
//! the header pass: a `moov` ≤ 256 KiB is fetched in one read, a larger one
//! piecewise (≈ one read per track, because each `trak` starts after the
//! previous track's tables).

use flux_core::{Chapter, Codec, ColorInfo, Container, ContentLightLevel, Track, TrackKind};

use crate::bytes::{fourcc_str, string, Rd};
use crate::codec::{
    self, apply_audio, apply_video, audio, av1, dovi, h264, hevc, new_track, sei, AudioCfg, VideoCfg,
};
use crate::hdr;
use crate::prober::{empty_result, FrameLocator, Parsed};
use crate::store::{malformed, Ctx, PErr, PResult};

/// `moov` up to this size is read in one go.
const MOOV_WHOLE: u64 = 256 * 1024;
/// A file tail up to this size is read in one go when walking top-level boxes.
const TAIL_WHOLE: u64 = 256 * 1024;
/// Largest leaf box body (`stsd`, `udta` items) we read.
const MAX_LEAF: u64 = 1 << 20;

/// A box header located in the file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BoxHdr {
    pub ty: [u8; 4],
    pub off: u64,
    pub data: u64,
    pub end: u64,
}

impl BoxHdr {
    fn size(&self) -> u64 {
        self.end - self.data
    }
}

/// Read the box header at `off` inside a parent ending at `limit`.
fn box_header(ctx: &Ctx<'_>, off: u64, limit: u64) -> PResult<BoxHdr> {
    if limit.saturating_sub(off) < 8 {
        return malformed(ctx.container, format!("box header at {off} runs past its parent"));
    }
    let b = ctx.bytes(off, (limit - off).min(16))?;
    let mut r = Rd::new(b);
    let size32 = r.be32().unwrap_or(0);
    let ty = r.fourcc().unwrap_or([0; 4]);
    let (data, end) = match size32 {
        0 => (off + 8, limit),
        1 => {
            let Some(large) = r.be64() else {
                return malformed(ctx.container, format!("truncated 64-bit box size at {off}"));
            };
            (off + 16, off.saturating_add(large))
        }
        s => (off + 8, off.saturating_add(u64::from(s))),
    };
    if end < data {
        return malformed(
            ctx.container,
            format!("box {} at {off} has size < header", fourcc_str(ty)),
        );
    }
    Ok(BoxHdr { ty, off, data, end })
}

/// Box headers of the children of `parent` (starting `skip` bytes into its
/// body). In free mode, stops at the first header that is not cached.
fn child_boxes(ctx: &Ctx<'_>, parent: &BoxHdr, skip: u64, paid: bool) -> PResult<Vec<BoxHdr>> {
    let mut out = Vec::new();
    let mut pos = parent.data + skip;
    while pos + 8 <= parent.end && out.len() < 1024 {
        if !paid && ctx.store().get(pos, (parent.end - pos).min(16)).is_none() {
            break;
        }
        let h = box_header(ctx, pos, parent.end)?;
        if h.end > parent.end {
            return malformed(
                ctx.container,
                format!("box {} overruns its parent", fourcc_str(h.ty)),
            );
        }
        out.push(h);
        if h.end <= pos {
            break;
        }
        pos = h.end;
    }
    Ok(out)
}

/// In-memory child boxes of a body slice: (type, body).
pub(crate) fn boxes(b: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut r = Rd::new(b);
    while r.remaining() >= 8 {
        let start = r.pos();
        let (Some(size), Some(ty)) = (r.be32(), r.fourcc()) else {
            break;
        };
        let (hdr, size) = match size {
            0 => (8, (b.len() - start) as u64),
            1 => match r.be64() {
                Some(s) => (16, s),
                None => break,
            },
            s => (8, u64::from(s)),
        };
        let Some(body_len) = size.checked_sub(hdr).and_then(|l| usize::try_from(l).ok()) else {
            break;
        };
        let Some(body) = r.take(body_len) else { break };
        out.push((ty, body));
    }
    out
}

fn find<'a>(kids: &[([u8; 4], &'a [u8])], ty: &[u8; 4]) -> Option<&'a [u8]> {
    kids.iter().find(|(t, _)| t == ty).map(|(_, b)| *b)
}

fn read_body<'a>(ctx: &Ctx<'a>, h: &BoxHdr, max: u64) -> PResult<&'a [u8]> {
    if h.size() > max {
        return malformed(
            ctx.container,
            format!("box {} is {} bytes (limit {max})", fourcc_str(h.ty), h.size()),
        );
    }
    ctx.bytes(h.data, h.size())
}

/// Per-track state collected while parsing.
struct TrakInfo {
    track: Track,
    cfg: VideoCfg,
    known: bool,
    /// Duration in seconds from mdhd (fallback for mvhd).
    duration_s: Option<f64>,
    /// mdhd timescale (for fragment durations).
    timescale: u64,
    stbl: Option<BoxHdr>,
    chapter_refs: Vec<u32>,
}

pub(crate) fn parse(ctx: &mut Ctx<'_>) -> PResult<Parsed> {
    ctx.container = Container::Mp4;
    let size = ctx.file_size();
    let mut pos = 0u64;
    let mut moov = None;
    let mut first_mdat = None;
    let mut fragmented = false;
    let mut major = None;
    for _ in 0..4096 {
        if pos + 8 > size {
            break;
        }
        if size - pos <= TAIL_WHOLE {
            // Short tail (typically a moov-at-end): one read for all of it.
            ctx.bytes(pos, size - pos)?;
        }
        let h = box_header(ctx, pos, size)?;
        match &h.ty {
            b"ftyp" => major = ctx.bytes(h.data, h.size().min(4)).ok().map(|b| b.to_vec()),
            b"moov" => moov = Some(h),
            b"mdat" => {
                first_mdat.get_or_insert(h.off);
            }
            b"moof" => fragmented = true,
            _ => {}
        }
        if h.end > size {
            if h.ty == *b"moov" {
                return Err(PErr::Fail(crate::ProbeError::Truncated {
                    container: ctx.container,
                    offset: h.off,
                    needed: h.end - h.off,
                    file_size: size,
                }));
            }
            ctx.warn(format!(
                "{} box runs past end of file (truncated)",
                fourcc_str(h.ty)
            ));
            break;
        }
        if moov.is_some() && (first_mdat.is_some() || fragmented) {
            break;
        }
        if h.end <= pos {
            break;
        }
        pos = h.end;
    }
    if major.as_deref() == Some(b"qt  ") {
        ctx.container = Container::Mov;
    }
    let Some(moov) = moov else {
        return malformed(ctx.container, "no moov box");
    };
    let mut r = empty_result(ctx.container);
    r.faststart = Some(first_mdat.is_none_or(|m| moov.off < m));
    if moov.end - moov.off <= MOOV_WHOLE {
        ctx.bytes(moov.off, moov.end - moov.off)?;
    }

    let mut movie_timescale = 0u64;
    let mut movie_duration = None;
    let mut traks = Vec::new();
    let mut trex: Vec<(u32, u32)> = Vec::new();
    for c in child_boxes(ctx, &moov, 0, true)? {
        match &c.ty {
            b"mvhd" => {
                let b = read_body(ctx, &c, 4096)?;
                if let Some((ts, d)) = mvhd(b) {
                    movie_timescale = ts;
                    movie_duration = d;
                }
            }
            b"trak" => {
                let index = traks.len() as u32;
                if let Some(t) = trak(ctx, &c, index)? {
                    traks.push(t);
                }
            }
            b"mvex" => {
                fragmented = true;
                // mehd: fragment_duration in the movie timescale.
                for m in child_boxes(ctx, &c, 0, true)? {
                    if m.ty == *b"trex" {
                        // track_ID, default_sample_description_index, default_sample_duration
                        let b = read_body(ctx, &m, 64)?;
                        let mut rd = Rd::new(b);
                        rd.skip(4);
                        if let (Some(id), Some(_), Some(dur)) = (rd.be32(), rd.be32(), rd.be32()) {
                            trex.push((id, dur));
                        }
                    }
                    if m.ty == *b"mehd" {
                        let b = read_body(ctx, &m, 64)?;
                        let mut rd = Rd::new(b);
                        let v = rd.u8().unwrap_or(0);
                        rd.skip(3);
                        let d = if v == 1 {
                            rd.be64()
                        } else {
                            rd.be32().map(u64::from)
                        };
                        movie_duration = movie_duration.filter(|d| *d > 0).or(d);
                    }
                }
            }
            b"udta" => {
                let res = udta(ctx, &c);
                if let Some((title, chapters)) = ctx.optional("udta", res)? {
                    r.title = r.title.take().or(title);
                    if !chapters.is_empty() {
                        r.chapters = chapters;
                    }
                }
            }
            b"meta" => {
                let res = meta_title(ctx, &c);
                if let Some(Some(t)) = ctx.optional("meta", res)? {
                    r.title.get_or_insert(t);
                }
            }
            _ => {}
        }
    }

    if let (Some(d), true) = (movie_duration, movie_timescale > 0) {
        if d > 0 && d != u64::from(u32::MAX) && d != u64::MAX {
            r.duration_ms = Some((d as f64 * 1000.0 / movie_timescale as f64).round() as u64);
        }
    }
    if r.duration_ms.is_none() {
        let best = traks.iter().filter_map(|t| t.duration_s).fold(0.0f64, f64::max);
        if best > 0.0 {
            r.duration_ms = Some((best * 1000.0).round() as u64);
        }
    }
    if r.duration_ms.is_none() && fragmented {
        let scales: Vec<(u32, u64)> = traks.iter().map(|t| (t.track.id as u32, t.timescale)).collect();
        let res = fragmented_duration(ctx, &scales, &trex);
        if let Some(d) = ctx.optional("fragment duration", res)? {
            r.duration_ms = d;
        }
    }

    if let Some(last) = r.chapters.last_mut() {
        last.end_ms = last.end_ms.or(r.duration_ms);
    }

    // Chapter text tracks (tref/chap) are not media tracks.
    let chapter_ids: Vec<u32> = traks.iter().flat_map(|t| t.chapter_refs.clone()).collect();
    if !chapter_ids.is_empty() && r.chapters.is_empty() {
        ctx.warn("QuickTime chapter track present but not read (needs sample tables)");
    }
    let mut locator = None;
    for mut t in traks {
        if chapter_ids.contains(&(t.track.id as u32)) && t.track.kind != TrackKind::Video {
            continue;
        }
        hdr::classify(&mut t.track, &t.cfg, t.known);
        if t.track.kind == TrackKind::Video && locator.is_none() {
            r.has_index = !fragmented && t.stbl.is_some();
            if let Some(stbl) = t.stbl {
                locator = Some(FrameLocator::Mp4 {
                    stbl: stbl.off,
                    stbl_end: stbl.end,
                    codec: t.track.codec,
                    nal_len_size: t.cfg.nal_len_size.unwrap_or(4),
                });
            }
        }
        t.track.index = r.tracks.len() as u32;
        r.tracks.push(t.track);
    }
    if fragmented {
        ctx.warn("fragmented MP4: sample tables live in moof boxes");
    }
    Ok(Parsed { result: r, locator })
}

/// Fragmented MP4 without `mehd`: end time of the last `moof` in the file's
/// tail (`tfdt` base decode time + the `trun` sample durations), per track,
/// in each track's timescale. One tail read; never walks every fragment.
fn fragmented_duration(ctx: &Ctx<'_>, scales: &[(u32, u64)], trex: &[(u32, u32)]) -> PResult<Option<u64>> {
    let size = ctx.file_size();
    let tail_off = size.saturating_sub(TAIL_WHOLE);
    let tail = ctx.bytes(tail_off, size - tail_off)?;
    let Some(moof_off) = last_moof_from_mfra(ctx, tail, tail_off)?.or_else(|| {
        // No mfra: the last "moof" whose first child is `mfhd` in the tail.
        (0..tail.len().saturating_sub(16))
            .rev()
            .find(|&i| &tail[i + 4..i + 8] == b"moof" && &tail[i + 12..i + 16] == b"mfhd")
            .map(|i| tail_off + i as u64)
    }) else {
        return Ok(None);
    };
    let h = box_header(ctx, moof_off, size)?;
    if h.ty != *b"moof" || h.size() > MAX_LEAF {
        return Ok(None);
    }
    let moof = ctx.bytes(h.data, h.size())?;
    let mut best_ms: Option<u64> = None;
    for (ty, traf) in boxes(moof) {
        if &ty != b"traf" {
            continue;
        }
        let kids = boxes(traf);
        let Some(tfhd) = find(&kids, b"tfhd") else {
            continue;
        };
        let mut r = Rd::new(tfhd);
        let flags = r.be32().unwrap_or(0) & 0xff_ffff;
        let Some(track_id) = r.be32() else { continue };
        if flags & 0x1 != 0 {
            r.skip(8);
        }
        if flags & 0x2 != 0 {
            r.skip(4);
        }
        let default_dur = if flags & 0x8 != 0 { r.be32() } else { None }
            .or_else(|| trex.iter().find(|(id, _)| *id == track_id).map(|(_, d)| *d))
            .unwrap_or(0);
        let base = find(&kids, b"tfdt").and_then(|b| {
            let mut r = Rd::new(b);
            let v = r.u8()?;
            r.skip(3)?;
            if v == 1 {
                r.be64()
            } else {
                r.be32().map(u64::from)
            }
        });
        let Some(base) = base else { continue };
        let mut total = 0u64;
        for (_, trun) in kids.iter().filter(|(t, _)| t == b"trun") {
            let mut r = Rd::new(trun);
            let flags = r.be32().unwrap_or(0) & 0xff_ffff;
            let n = r.be32().unwrap_or(0);
            if flags & 0x1 != 0 {
                r.skip(4);
            }
            if flags & 0x4 != 0 {
                r.skip(4);
            }
            if flags & 0x100 == 0 {
                total = total.saturating_add(u64::from(n) * u64::from(default_dur));
                continue;
            }
            let per = 4 * [0x100, 0x200, 0x400, 0x800]
                .iter()
                .filter(|f| flags & **f != 0)
                .count();
            for _ in 0..n.min(1 << 20) {
                let Some(sample) = r.take(per) else { break };
                total = total.saturating_add(u64::from(u32::from_be_bytes([
                    sample[0], sample[1], sample[2], sample[3],
                ])));
            }
        }
        if let Some(&(_, ts)) = scales.iter().find(|(id, _)| *id == track_id) {
            if ts > 0 {
                let ms = (u128::from(base) + u128::from(total)) * 1000 / u128::from(ts);
                let ms = u64::try_from(ms).unwrap_or(u64::MAX);
                best_ms = Some(best_ms.map_or(ms, |b| b.max(ms)));
            }
        }
    }
    Ok(best_ms)
}

/// Highest `moof` offset listed in the `mfra`/`tfra` random-access index,
/// located through the trailing `mfro` box.
fn last_moof_from_mfra(ctx: &Ctx<'_>, tail: &[u8], tail_off: u64) -> PResult<Option<u64>> {
    let size = ctx.file_size();
    let Some(mfro) = tail.len().checked_sub(16).and_then(|i| tail.get(i..)) else {
        return Ok(None);
    };
    if &mfro[4..8] != b"mfro" {
        return Ok(None);
    }
    let mfra_size = u64::from(u32::from_be_bytes([mfro[12], mfro[13], mfro[14], mfro[15]]));
    let Some(mfra_off) = size
        .checked_sub(mfra_size)
        .filter(|o| *o >= tail_off.saturating_sub(MAX_LEAF))
    else {
        return Ok(None);
    };
    let h = box_header(ctx, mfra_off, size)?;
    if h.ty != *b"mfra" || h.size() > MAX_LEAF {
        return Ok(None);
    }
    let mut last = None;
    for (ty, tfra) in boxes(ctx.bytes(h.data, h.size())?) {
        if &ty != b"tfra" {
            continue;
        }
        let mut r = Rd::new(tfra);
        let v = r.u8().unwrap_or(0);
        r.skip(3 + 4); // flags, track_ID
        let sizes = r.be32().unwrap_or(0);
        let n = r.be32().unwrap_or(0);
        let skip = ((sizes >> 4) & 3) as usize + ((sizes >> 2) & 3) as usize + (sizes & 3) as usize + 3;
        for _ in 0..n.min(1 << 20) {
            let off = if v == 1 {
                r.skip(8);
                r.be64()
            } else {
                r.skip(4);
                r.be32().map(u64::from)
            };
            let Some(off) = off else { break };
            if r.skip(skip).is_none() {
                break;
            }
            if off < size {
                last = Some(last.map_or(off, |l: u64| l.max(off)));
            }
        }
    }
    Ok(last)
}

/// (timescale, duration) from `mvhd`.
fn mvhd(b: &[u8]) -> Option<(u64, Option<u64>)> {
    let mut r = Rd::new(b);
    let v = r.u8()?;
    r.skip(3)?;
    if v == 1 {
        r.skip(16)?;
        let ts = u64::from(r.be32()?);
        Some((ts, r.be64()))
    } else {
        r.skip(8)?;
        let ts = u64::from(r.be32()?);
        Some((ts, r.be32().map(u64::from)))
    }
}

fn trak(ctx: &mut Ctx<'_>, trak: &BoxHdr, index: u32) -> PResult<Option<TrakInfo>> {
    let kids = child_boxes(ctx, trak, 0, true)?;
    let mut track_id = 0u64;
    let mut enabled = true;
    let mut tk_size = (None, None);
    let mut handler = *b"    ";
    let mut lang = None;
    let mut mdhd_dur = None;
    let mut stbl = None;
    let mut title = None;
    let mut chapter_refs = Vec::new();
    let mut stsd_body: Option<&[u8]> = None;
    for c in &kids {
        match &c.ty {
            b"tkhd" => {
                let b = read_body(ctx, c, 4096)?;
                if let Some((id, flags, w, h)) = tkhd(b) {
                    track_id = id;
                    enabled = flags & 1 != 0;
                    tk_size = (w, h);
                }
            }
            b"mdia" => {
                for m in child_boxes(ctx, c, 0, true)? {
                    match &m.ty {
                        b"mdhd" => {
                            let b = read_body(ctx, &m, 4096)?;
                            if let Some((l, d)) = mdhd(b) {
                                lang = Some(l);
                                mdhd_dur = d;
                            }
                        }
                        b"hdlr" => {
                            let b = read_body(ctx, &m, 4096)?;
                            if let Some(h) = b.get(8..12) {
                                handler.copy_from_slice(h);
                            }
                        }
                        b"minf" => {
                            for s in child_boxes(ctx, &m, 0, true)? {
                                if s.ty == *b"stbl" {
                                    stbl = Some(s);
                                    // Free walk: only stsd (always first) is read.
                                    for t in child_boxes(ctx, &s, 0, false)? {
                                        if t.ty == *b"stsd" {
                                            stsd_body = Some(read_body(ctx, &t, MAX_LEAF)?);
                                        }
                                    }
                                    if stsd_body.is_none() {
                                        // stsd header was not cached: pay for it.
                                        if let Some(t) = child_boxes(ctx, &s, 0, true)?
                                            .into_iter()
                                            .find(|t| t.ty == *b"stsd")
                                        {
                                            stsd_body = Some(read_body(ctx, &t, MAX_LEAF)?);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            b"udta" => {
                if c.size() <= 64 * 1024 {
                    let b = read_body(ctx, c, 64 * 1024)?;
                    let kids = boxes(b);
                    title = find(&kids, b"name")
                        .map(string)
                        .or_else(|| find(&kids, b"\xa9nam").map(qt_string));
                }
            }
            b"tref" if c.size() <= 4096 => {
                let b = read_body(ctx, c, 4096)?;
                for (ty, refs) in boxes(b) {
                    if &ty == b"chap" {
                        let mut rd = Rd::new(refs);
                        while let Some(id) = rd.be32() {
                            chapter_refs.push(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let (timescale, duration) = mdhd_dur.unwrap_or((0, 0));
    let kind = match &handler {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"sbtl" | b"subt" | b"text" | b"clcp" => TrackKind::Subtitle,
        _ => TrackKind::Data,
    };
    let mut t = new_track(index, track_id, kind, Codec::Other, String::new());
    t.default = enabled;
    t.language = lang;
    t.title = title.filter(|s| !s.is_empty());
    let mut cfg = VideoCfg::default();
    let mut known = false;
    match stsd_body {
        Some(b) => known = sample_entry(ctx, b, &mut t, &mut cfg),
        None => ctx.warn(format!("track {track_id}: no stsd")),
    }
    if kind == TrackKind::Video {
        t.width = t.width.or(tk_size.0);
        t.height = t.height.or(tk_size.1);
    }
    let duration_s = (timescale > 0 && duration > 0 && duration != u64::from(u32::MAX))
        .then(|| duration as f64 / timescale as f64);
    if kind == TrackKind::Video && t.frame_rate.is_none() {
        // stts is only parsed when it is already in the cache (no extra read).
        if let Some(s) = &stbl {
            if let Ok(kids) = child_boxes(ctx, s, 0, false) {
                if let Some(stts) = kids.iter().find(|k| k.ty == *b"stts") {
                    if let Some(b) = ctx.store().get(stts.data, stts.size().min(16)) {
                        let mut rd = Rd::new(b);
                        rd.skip(8);
                        if let (Some(_count), Some(delta)) = (rd.be32(), rd.be32()) {
                            t.frame_rate = codec::rate_from_ticks(timescale, u64::from(delta));
                        }
                    }
                }
            }
        }
    }
    Ok(Some(TrakInfo {
        track: t,
        cfg,
        known,
        duration_s,
        timescale,
        stbl,
        chapter_refs,
    }))
}

/// (track_ID, flags, width, height) from `tkhd`.
fn tkhd(b: &[u8]) -> Option<(u64, u32, Option<u32>, Option<u32>)> {
    let mut r = Rd::new(b);
    let v = r.u8()?;
    let flags = r.be24()?;
    let id = if v == 1 {
        r.skip(16)?;
        let id = r.be32()?;
        r.skip(4 + 8)?;
        id
    } else {
        r.skip(8)?;
        let id = r.be32()?;
        r.skip(4 + 4)?;
        id
    };
    r.skip(8 + 2 + 2 + 2 + 2 + 36)?;
    let w = r.be32().map(|x| x >> 16).filter(|x| *x > 0);
    let h = r.be32().map(|x| x >> 16).filter(|x| *x > 0);
    Some((u64::from(id), flags, w, h))
}

/// (language, (timescale, duration)) from `mdhd`.
fn mdhd(b: &[u8]) -> Option<(String, Option<(u64, u64)>)> {
    let mut r = Rd::new(b);
    let v = r.u8()?;
    r.skip(3)?;
    let (ts, dur) = if v == 1 {
        r.skip(16)?;
        (u64::from(r.be32()?), r.be64()?)
    } else {
        r.skip(8)?;
        (u64::from(r.be32()?), u64::from(r.be32()?))
    };
    let code = r.be16()?;
    Some((mp4_language(code), Some((ts, dur))))
}

/// ISO-639-2/T packed in 3×5 bits; codes < 0x400 are Macintosh language codes
/// (0 = English), 0x7FFF is "unspecified".
fn mp4_language(code: u16) -> String {
    if code == 0x7fff {
        return "und".into();
    }
    if code < 0x400 {
        return match code {
            0 => "eng",
            1 => "fra",
            2 => "deu",
            3 => "ita",
            4 => "nld",
            5 => "swe",
            6 => "spa",
            7 => "dan",
            8 => "por",
            9 => "nor",
            10 => "heb",
            11 => "jpn",
            12 => "ara",
            13 => "fin",
            14 => "ell",
            19 => "zho",
            23 => "kor",
            32 => "rus",
            _ => "und",
        }
        .into();
    }
    let c = |s: u16| char::from(((s & 0x1f) as u8).wrapping_add(0x60));
    [c(code >> 10), c(code >> 5), c(code)].iter().collect()
}

/// QuickTime `©xxx` user-data string: u16 length, u16 language, text.
fn qt_string(b: &[u8]) -> String {
    let mut r = Rd::new(b);
    match r.be16() {
        Some(len) if usize::from(len) + 4 <= b.len() => {
            r.skip(2);
            string(r.take(usize::from(len)).unwrap_or(&[]))
        }
        _ => string(b),
    }
}

/// Parse the first `stsd` sample entry into the track. Returns whether a
/// codec configuration was understood.
fn sample_entry(ctx: &mut Ctx<'_>, stsd: &[u8], t: &mut Track, cfg: &mut VideoCfg) -> bool {
    let mut r = Rd::new(stsd);
    if r.skip(4).is_none() || r.be32().unwrap_or(0) == 0 {
        return false;
    }
    let entries = boxes(r.rest());
    let Some(&(mut ty, body)) = entries.first() else {
        return false;
    };
    if entries.len() > 1 {
        ctx.warn(format!(
            "track {}: {} sample descriptions, using the first",
            t.id,
            entries.len()
        ));
    }
    // Encrypted entries carry the real format in sinf/frma.
    let encrypted = matches!(&ty, b"encv" | b"enca" | b"enct" | b"encs");
    match t.kind {
        TrackKind::Video => {
            let kids = body.get(78..).map(boxes).unwrap_or_default();
            if encrypted {
                if let Some(f) =
                    find(&kids, b"sinf").and_then(|s| find(&boxes(s), b"frma").map(<[u8]>::to_vec))
                {
                    if let Ok(f4) = <[u8; 4]>::try_from(f.as_slice()) {
                        ty = f4;
                    }
                }
                ctx.warn(format!("track {}: encrypted", t.id));
            }
            t.codec_tag = fourcc_str(ty);
            let mut rd = Rd::new(body);
            rd.skip(24);
            t.width = rd.be16().map(u32::from).filter(|w| *w > 0);
            t.height = rd.be16().map(u32::from).filter(|h| *h > 0);
            video_entry(ctx, ty, &kids, t, cfg)
        }
        TrackKind::Audio => {
            if encrypted {
                ctx.warn(format!("track {}: encrypted", t.id));
            }
            t.codec_tag = fourcc_str(ty);
            audio_entry(ty, body, t);
            true
        }
        TrackKind::Subtitle => {
            t.codec_tag = fourcc_str(ty);
            t.codec = match &ty {
                b"tx3g" | b"text" => Codec::MovText,
                b"wvtt" => Codec::WebVtt,
                _ => Codec::Other,
            };
            true
        }
        _ => {
            t.codec_tag = fourcc_str(ty);
            true
        }
    }
}

fn video_entry(
    ctx: &mut Ctx<'_>,
    ty: [u8; 4],
    kids: &[([u8; 4], &[u8])],
    t: &mut Track,
    cfg: &mut VideoCfg,
) -> bool {
    t.codec = match &ty {
        b"avc1" | b"avc2" | b"avc3" | b"avc4" | b"dva1" | b"dvav" => Codec::H264,
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => Codec::Hevc,
        b"av01" | b"dav1" => Codec::Av1,
        b"vp09" => Codec::Vp9,
        b"vp08" => Codec::Vp8,
        b"jpeg" | b"mjpa" | b"mjpb" | b"mjp2" => Codec::Mjpeg,
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => Codec::ProRes,
        b"mp2v" | b"m2v1" => Codec::Mpeg2Video,
        b"vc-1" => Codec::Vc1,
        b"mp4v" => find(kids, b"esds")
            .and_then(esds)
            .map_or(Codec::Other, |e| oti_codec(e.oti)),
        _ => Codec::Other,
    };
    let codec_cfg = match t.codec {
        Codec::H264 => find(kids, b"avcC").and_then(h264::parse_avcc),
        Codec::Hevc => find(kids, b"hvcC").and_then(hevc::parse_hvcc),
        Codec::Av1 => find(kids, b"av1C").and_then(av1::parse_av1c),
        Codec::Vp9 => find(kids, b"vpcC").and_then(vpcc),
        _ => None,
    };
    let known = codec_cfg.is_some();
    if matches!(t.codec, Codec::H264 | Codec::Hevc | Codec::Av1) && !known {
        ctx.warn(format!(
            "track {}: missing/unparsable codec configuration box",
            t.id
        ));
    }
    // Container-level colour and HDR boxes take precedence.
    for (k, b) in kids {
        match k {
            b"colr" => {
                if let Some(c) = colr(b) {
                    t.color = hdr::merge_color(Some(c), t.color);
                }
            }
            b"mdcv" => cfg.mastering = sei::mastering(b),
            b"clli" => {
                let mut r = Rd::new(b);
                if let (Some(max_cll), Some(max_fall)) = (r.be16(), r.be16()) {
                    cfg.cll = Some(ContentLightLevel { max_cll, max_fall });
                }
            }
            b"dvcC" | b"dvvC" | b"dvwC" => cfg.dovi = dovi::parse_config(b),
            b"btrt" => {
                let mut r = Rd::new(b);
                r.skip(8);
                t.bitrate = r.be32().map(u64::from).filter(|b| *b > 0);
            }
            b"fiel" => t.interlaced = b.first().map(|f| *f == 2),
            // PixelAspectRatioBox: hSpacing, vSpacing. Beats the codec's VUI.
            b"pasp" => {
                let mut r = Rd::new(b);
                if let (Some(h), Some(v)) = (r.be32(), r.be32()) {
                    t.sample_aspect_ratio = codec::sar(u64::from(h), u64::from(v));
                }
            }
            _ => {}
        }
    }
    if let Some(c) = codec_cfg {
        apply_video(t, &c);
        cfg.absorb(c);
    }
    known
        || matches!(
            t.codec,
            Codec::Mpeg2Video | Codec::Mpeg4Part2 | Codec::Mjpeg | Codec::Vc1
        )
}

fn audio_entry(ty: [u8; 4], body: &[u8], t: &mut Track) {
    let mut r = Rd::new(body);
    r.skip(8);
    let version = r.be16().unwrap_or(0);
    r.skip(6);
    let mut channels = r.be16().map(|c| c as u8);
    let sample_size = r.be16();
    r.skip(4);
    let mut rate = r.be32().map(|x| x >> 16);
    let child_off = match version {
        1 => 28 + 16,
        2 => {
            // QuickTime v2: f64 sample rate and u32 channel count follow.
            let mut v2 = Rd::new(body.get(28..).unwrap_or(&[]));
            v2.skip(4);
            if let Some(bits) = v2.be64() {
                let sr = f64::from_bits(bits);
                rate = (sr.is_finite() && sr > 0.0 && sr < 1e7).then_some(sr as u32);
            }
            channels = v2.be32().and_then(|c| u8::try_from(c).ok());
            28 + 36
        }
        _ => 28,
    };
    let kids = body.get(child_off..).map(boxes).unwrap_or_default();
    t.channels = channels.filter(|c| *c > 0);
    t.sample_rate = rate.filter(|r| *r > 0);
    t.codec = match &ty {
        b"mp4a" => Codec::Aac, // refined from esds below
        b"ac-3" => Codec::Ac3,
        b"ec-3" => Codec::Eac3,
        b"Opus" => Codec::Opus,
        b"fLaC" => Codec::Flac,
        b"alac" => Codec::Alac,
        b"mlpa" => Codec::TrueHd,
        b"dtsc" | b"dtsh" | b"dtsl" | b"dtse" | b"dtsx" | b"dtsy" => Codec::Dts,
        b".mp3" | b"ms\x00U" => Codec::Mp3,
        b"lpcm" | b"sowt" | b"twos" | b"in24" | b"in32" | b"fl32" | b"fl64" | b"raw " | b"ipcm" | b"fpcm"
        | b"NONE" => {
            t.bit_depth = sample_size.and_then(|s| u8::try_from(s).ok()).filter(|s| *s > 0);
            Codec::Pcm
        }
        _ => Codec::Other,
    };
    // QuickTime nests codec config inside `wave`.
    let wave = find(&kids, b"wave").map(boxes).unwrap_or_default();
    let cfg_box = |name: &[u8; 4]| find(&kids, name).or_else(|| find(&wave, name));
    let acfg: Option<AudioCfg> = match t.codec {
        Codec::Aac => cfg_box(b"esds").and_then(esds).map(|e| {
            let codec = oti_codec(e.oti);
            let mut a = if codec == Codec::Aac {
                e.dsi.and_then(audio::parse_asc).unwrap_or_default()
            } else {
                AudioCfg {
                    codec: Some(codec),
                    ..AudioCfg::default()
                }
            };
            a.codec = Some(codec);
            a.bitrate = Some(u64::from(e.avg_bitrate)).filter(|b| *b > 0);
            a
        }),
        Codec::Ac3 => cfg_box(b"dac3").and_then(audio::parse_dac3),
        Codec::Eac3 => cfg_box(b"dec3").and_then(audio::parse_dec3),
        Codec::Opus => cfg_box(b"dOps").and_then(audio::parse_dops),
        Codec::Flac => cfg_box(b"dfLa").and_then(|b| audio::parse_flac_blocks(b.get(4..)?)),
        Codec::Alac => cfg_box(b"alac").and_then(|b| audio::parse_alac(b.get(4..)?)),
        _ => None,
    };
    if let Some(mut a) = acfg {
        // The config is authoritative for multichannel (sample entries often
        // say 2 channels for 5.1 AC-3/E-AC-3/AAC).
        if a.channels.is_some() {
            t.channels = None;
        }
        if a.sample_rate.is_some() && t.codec != Codec::Opus {
            t.sample_rate = None;
        }
        if t.codec == Codec::Opus {
            a.sample_rate = Some(48000);
            t.sample_rate = None;
        }
        apply_audio(t, &a);
    }
    if let Some(b) = find(&kids, b"btrt") {
        let mut r = Rd::new(b);
        r.skip(8);
        t.bitrate = t.bitrate.or(r.be32().map(u64::from).filter(|b| *b > 0));
    }
}

struct Esds<'a> {
    oti: u8,
    avg_bitrate: u32,
    dsi: Option<&'a [u8]>,
}

/// `esds`: ES_Descriptor → DecoderConfigDescriptor → DecoderSpecificInfo.
fn esds(b: &[u8]) -> Option<Esds<'_>> {
    fn desc_len(r: &mut Rd<'_>) -> Option<usize> {
        let mut len = 0usize;
        for _ in 0..4 {
            let b = r.u8()?;
            len = (len << 7) | usize::from(b & 0x7f);
            if b & 0x80 == 0 {
                break;
            }
        }
        Some(len)
    }
    let mut r = Rd::new(b);
    r.skip(4)?; // version/flags
    if r.u8()? != 0x03 {
        return None;
    }
    desc_len(&mut r)?;
    r.skip(2)?; // ES_ID
    let flags = r.u8()?;
    if flags & 0x80 != 0 {
        r.skip(2)?;
    }
    if flags & 0x40 != 0 {
        let n = r.u8()?;
        r.skip(usize::from(n))?;
    }
    if flags & 0x20 != 0 {
        r.skip(2)?;
    }
    if r.u8()? != 0x04 {
        return None;
    }
    let dcd_len = desc_len(&mut r)?;
    let dcd = r.take(dcd_len.min(r.remaining()))?;
    let mut d = Rd::new(dcd);
    let oti = d.u8()?;
    d.skip(1 + 3 + 4)?;
    let avg_bitrate = d.be32()?;
    let mut dsi = None;
    if d.u8() == Some(0x05) {
        if let Some(l) = desc_len(&mut d) {
            dsi = d.take(l.min(d.remaining()));
        }
    }
    Some(Esds {
        oti,
        avg_bitrate,
        dsi,
    })
}

/// MPEG-4 Systems objectTypeIndication → codec.
fn oti_codec(oti: u8) -> Codec {
    match oti {
        0x20 => Codec::Mpeg4Part2,
        0x21 => Codec::H264,
        0x23 => Codec::Hevc,
        0x40 | 0x66..=0x68 => Codec::Aac,
        0x60..=0x65 => Codec::Mpeg2Video,
        0x69 | 0x6b => Codec::Mp3,
        0x6c => Codec::Mjpeg,
        0xa5 => Codec::Ac3,
        0xa6 => Codec::Eac3,
        0xa9 => Codec::Dts,
        0xdd => Codec::Vorbis,
        _ => Codec::Other,
    }
}

/// `colr`: `nclx` (ISO) or `nclc` (QuickTime, no range flag).
fn colr(b: &[u8]) -> Option<ColorInfo> {
    let mut r = Rd::new(b);
    let ty = r.fourcc()?;
    if &ty != b"nclx" && &ty != b"nclc" {
        return None;
    }
    let p = r.be16()?;
    let tr = r.be16()?;
    let m = r.be16()?;
    let full_range = if &ty == b"nclx" {
        r.u8().map(|f| f & 0x80 != 0)
    } else {
        None
    };
    Some(ColorInfo {
        primaries: u8::try_from(p).ok(),
        transfer: u8::try_from(tr).ok(),
        matrix: u8::try_from(m).ok(),
        full_range,
    })
}

/// `vpcC` (VP codec ISO-BMFF binding §2.2), version 1.
fn vpcc(b: &[u8]) -> Option<VideoCfg> {
    let mut r = Rd::new(b);
    r.skip(4)?;
    let profile = r.u8()?;
    let level = r.u8()?;
    let x = r.u8()?;
    let primaries = r.u8()?;
    let transfer = r.u8()?;
    let matrix = r.u8()?;
    Some(VideoCfg {
        profile: Some(format!("Profile {profile}")),
        level: Some(u16::from(level)),
        bit_depth: Some(x >> 4),
        color: Some(ColorInfo {
            primaries: Some(primaries),
            transfer: Some(transfer),
            matrix: Some(matrix),
            full_range: Some(x & 1 != 0),
        }),
        ..VideoCfg::default()
    })
}

/// moov/udta: title (`meta/ilst/©nam` or QuickTime `©nam`) and Nero `chpl` chapters.
fn udta(ctx: &mut Ctx<'_>, u: &BoxHdr) -> PResult<(Option<String>, Vec<Chapter>)> {
    let mut title = None;
    let mut chapters = Vec::new();
    for c in child_boxes(ctx, u, 0, true)? {
        match &c.ty {
            b"meta" => {
                if let Some(t) = meta_title(ctx, &c)? {
                    title.get_or_insert(t);
                }
            }
            b"\xa9nam" if c.size() <= 4096 => {
                title.get_or_insert(qt_string(read_body(ctx, &c, 4096)?));
            }
            b"chpl" if c.size() <= MAX_LEAF => chapters = chpl(read_body(ctx, &c, MAX_LEAF)?),
            _ => {}
        }
    }
    Ok((title.filter(|t| !t.is_empty()), chapters))
}

/// `meta` → `ilst` → `©nam` → `data`. Handles both the ISO full-box `meta`
/// and QuickTime's plain one; skips cover art without reading it.
fn meta_title(ctx: &mut Ctx<'_>, m: &BoxHdr) -> PResult<Option<String>> {
    let probe = ctx.bytes(m.data, m.size().min(12))?;
    let skip = if probe.get(4..8) == Some(b"hdlr") { 0 } else { 4 };
    for c in child_boxes(ctx, m, skip, true)? {
        if c.ty != *b"ilst" {
            continue;
        }
        for item in child_boxes(ctx, &c, 0, true)? {
            if item.ty == *b"\xa9nam" && item.size() <= 4096 {
                let b = read_body(ctx, &item, 4096)?;
                if let Some(d) = find(&boxes(b), b"data") {
                    return Ok(Some(string(d.get(8..).unwrap_or(&[]))).filter(|s| !s.is_empty()));
                }
            }
        }
    }
    Ok(None)
}

/// Nero chapters: version, flags, [reserved u32 if v1], count u8, then
/// (start in 100 ns u64, title len u8, title).
fn chpl(b: &[u8]) -> Vec<Chapter> {
    let mut out: Vec<Chapter> = Vec::new();
    let mut r = Rd::new(b);
    let Some(v) = r.u8() else { return out };
    r.skip(3);
    if v == 1 {
        r.skip(4);
    }
    let n = r.u8().unwrap_or(0);
    for _ in 0..n {
        let (Some(start), Some(len)) = (r.be64(), r.u8()) else {
            break;
        };
        let Some(t) = r.take(usize::from(len)) else { break };
        out.push(Chapter {
            start_ms: start / 10_000,
            end_ms: None,
            title: Some(string(t)),
        });
    }
    for i in 0..out.len().saturating_sub(1) {
        out[i].end_ms = Some(out[i + 1].start_ms);
    }
    out
}

// ---------------------------------------------------------------------------
// Second pass: the first sample of the video track.

/// Byte range of sample 1 from the first `stsz`/`stz2` and `stco`/`co64`
/// entries (sample 1 always starts chunk 1). Reads a few bytes of each.
pub(crate) fn first_sample(ctx: &Ctx<'_>, stbl: u64, stbl_end: u64) -> PResult<(u64, u64)> {
    let s = box_header(ctx, stbl, stbl_end)?;
    let mut size = None;
    let mut offset = None;
    for c in child_boxes(ctx, &s, 0, true)? {
        match &c.ty {
            b"stsz" => {
                let b = ctx.bytes(c.data, c.size().min(16))?;
                let mut r = Rd::new(b);
                r.skip(4);
                let (fixed, count) = (r.be32().unwrap_or(0), r.be32().unwrap_or(0));
                if count == 0 {
                    return Err(PErr::Fail(crate::ProbeError::Unsupported(
                        "first sample of a fragmented MP4".into(),
                    )));
                }
                size = if fixed != 0 {
                    Some(u64::from(fixed))
                } else {
                    r.be32().map(u64::from)
                };
            }
            b"stz2" => {
                let b = ctx.bytes(c.data, c.size().min(16))?;
                let mut r = Rd::new(b);
                r.skip(7);
                let field = r.u8().unwrap_or(0);
                r.skip(4);
                size = match field {
                    16 => r.be16().map(u64::from),
                    8 => r.u8().map(u64::from),
                    4 => r.u8().map(|v| u64::from(v >> 4)),
                    _ => None,
                };
            }
            b"stco" => {
                let b = ctx.bytes(c.data, c.size().min(12))?;
                let mut r = Rd::new(b);
                r.skip(8);
                offset = r.be32().map(u64::from);
            }
            b"co64" => {
                let b = ctx.bytes(c.data, c.size().min(16))?;
                let mut r = Rd::new(b);
                r.skip(8);
                offset = r.be64();
            }
            _ => {}
        }
    }
    match (offset, size) {
        (Some(o), Some(s)) if o.checked_add(s).is_some_and(|e| e <= ctx.file_size()) => Ok((o, o + s)),
        _ => malformed(ctx.container, "cannot locate the first video sample"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes() {
        assert_eq!(mp4_language(0x55c4), "und");
        assert_eq!(mp4_language(0x15c7), "eng");
        assert_eq!(mp4_language(0), "eng");
    }

    #[test]
    fn box_iter_stops_on_garbage() {
        let b = [
            0, 0, 0, 12, b'f', b'r', b'e', b'e', 1, 2, 3, 4, 0, 0, 0, 99, b'x', b'x', b'x', b'x',
        ];
        let kids = boxes(&b);
        assert_eq!(kids.len(), 1);
        assert_eq!(&kids[0].0, b"free");
    }

    #[test]
    fn chpl_chapters() {
        let mut b = vec![1, 0, 0, 0, 0, 0, 0, 0, 2];
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&[5, b'I', b'n', b't', b'r', b'o']);
        b.extend_from_slice(&(600_000_000u64).to_be_bytes());
        b.extend_from_slice(&[3, b'E', b'n', b'd']);
        let c = chpl(&b);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].start_ms, 60_000);
        assert_eq!(c[0].end_ms, Some(60_000));
        assert_eq!(c[0].title.as_deref(), Some("Intro"));
    }

    /// `hvc1` + `dvvC` (Dolby Vision 8.1) + `colr` nclx PQ + `mdcv`/`clli`.
    #[test]
    fn dovi_and_static_hdr_boxes() {
        let dvvc = [1u8, 0, 0b0001_0000, 0b0011_0101, 0b0001_0000, 0, 0, 0];
        let colr = [b'n', b'c', b'l', b'x', 0, 9, 0, 16, 0, 9, 0];
        let mut mdcv = Vec::new();
        for v in [13250u16, 34500, 7500, 3000, 34000, 16000, 15635, 16450] {
            mdcv.extend_from_slice(&v.to_be_bytes());
        }
        mdcv.extend_from_slice(&10_000_000u32.to_be_bytes());
        mdcv.extend_from_slice(&50u32.to_be_bytes());
        let clli = [0x03, 0xe8, 0x01, 0x90];
        let kids: Vec<([u8; 4], &[u8])> = vec![
            (*b"dvvC", &dvvc[..]),
            (*b"colr", &colr[..]),
            (*b"mdcv", &mdcv[..]),
            (*b"clli", &clli[..]),
        ];
        let store = crate::store::Store::new(0);
        let mut ctx = Ctx::new(&store, 0);
        let mut t = new_track(0, 1, TrackKind::Video, Codec::Other, "hvc1");
        let mut cfg = VideoCfg::default();
        video_entry(&mut ctx, *b"hvc1", &kids, &mut t, &mut cfg);
        hdr::classify(&mut t, &cfg, true);
        assert_eq!(t.codec, Codec::Hevc);
        let h = t.hdr.as_ref().unwrap();
        assert_eq!(h.format, flux_core::HdrFormat::DolbyVision);
        assert_eq!(h.dovi.map(|d| (d.profile, d.bl_signal_compat_id)), Some((8, 1)));
        assert_eq!(h.cll.map(|c| c.max_cll), Some(1000));
        assert_eq!(h.mastering.map(|m| m.min_luminance), Some(50));
        assert!(!hdr::needs_frame_sei(&t));
    }
}
