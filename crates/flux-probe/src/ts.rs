//! MPEG-TS (188-byte packets) and Blu-ray BDAV M2TS (192-byte packets).
//!
//! Two reads: the head (256 KiB; extended once to 1 MiB if the PMT is
//! missing) gives PAT → PMT stream types, descriptors (ISO 639 language,
//! registration, DVB codec descriptors, Dolby Vision) and the first PES of
//! every stream (SPS/VUI/SEI for video, sync-frame headers for audio); the
//! tail (64 KiB, grown to 1 MiB if it holds no PTS) gives the last PTS. Duration = last PTS − first PTS.
//! ffprobe with Jellyfin's arguments reads up to 200 s of stream instead.

use std::collections::HashMap;

use flux_core::{Codec, Container, DoviConfig, Track, TrackKind};

use crate::bytes::annexb_nals;
use crate::codec::{
    apply_audio, apply_video, audio, dovi, h264, hevc, mpegvideo, new_track, AudioCfg, VideoCfg,
};
use crate::prober::{empty_result, Parsed};
use crate::sniff::sync_offset;
use crate::store::{malformed, Ctx, PResult};

const HEAD: u64 = 256 * 1024;
const HEAD_MAX: u64 = 1024 * 1024;
const TAIL: u64 = 64 * 1024;
const TAIL_MAX: u64 = 1024 * 1024;
/// First-PES bytes kept per stream (video needs an SPS; audio a sync frame).
const VIDEO_KEEP: usize = 512 * 1024;
const AUDIO_KEEP: usize = 16 * 1024;
const PTS_WRAP: u64 = 1 << 33;

#[derive(Debug, Default)]
struct Section {
    buf: Vec<u8>,
    done: bool,
}

impl Section {
    /// Feed a packet payload; returns the complete section once.
    fn push(&mut self, pusi: bool, payload: &[u8]) -> Option<Vec<u8>> {
        if self.done {
            return None;
        }
        if pusi {
            let ptr = usize::from(*payload.first()?);
            self.buf = payload.get(1 + ptr..)?.to_vec();
        } else if !self.buf.is_empty() {
            self.buf.extend_from_slice(payload);
        } else {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([*self.buf.get(1)?, *self.buf.get(2)?]) & 0x0fff);
        if self.buf.len() >= 3 + len {
            self.done = true;
            self.buf.truncate(3 + len);
            return Some(std::mem::take(&mut self.buf));
        }
        None
    }
}

#[derive(Debug, Default)]
struct Es {
    pid: u16,
    stream_type: u8,
    registration: Option<[u8; 4]>,
    language: Option<String>,
    hearing_impaired: bool,
    descriptor_codec: Option<(TrackKind, Codec)>,
    dovi: Option<DoviConfig>,
    first_pts: Option<u64>,
    last_pts: Option<u64>,
    started: bool,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
struct Head {
    pat: Section,
    pmt_pids: Vec<u16>,
    pmts: HashMap<u16, Section>,
    pmt_done: usize,
    program_hdmv: bool,
    streams: Vec<Es>,
}

/// Iterate 188-byte TS packets in `buf` (192-byte M2TS packets carry a
/// 4-byte timecode first), resynchronising on lost sync.
fn packets(buf: &[u8], container: Container) -> Vec<&[u8]> {
    let (stride, lead) = if container == Container::M2ts {
        (192, 4)
    } else {
        (188, 0)
    };
    let mut out = Vec::new();
    let Some(mut pos) = sync_offset(buf, stride, lead) else {
        return out;
    };
    while pos + 188 <= buf.len() {
        if buf[pos] == 0x47 {
            out.push(&buf[pos..pos + 188]);
            pos += stride;
            continue;
        }
        // Lost sync: find the next 0x47 followed by another one a packet later.
        match buf[pos + 1..]
            .windows(stride + 1)
            .position(|w| w[0] == 0x47 && w[stride] == 0x47)
        {
            Some(k) => pos = pos + 1 + k,
            None => break,
        }
    }
    out
}

/// (pid, pusi, payload) of a TS packet.
fn split(p: &[u8]) -> Option<(u16, bool, &[u8])> {
    let pid = u16::from_be_bytes([p[1] & 0x1f, p[2]]);
    let pusi = p[1] & 0x40 != 0;
    let afc = (p[3] >> 4) & 3;
    let payload = match afc {
        1 => p.get(4..)?,
        3 => {
            let al = usize::from(*p.get(4)?);
            p.get(5 + al..)?
        }
        _ => return None,
    };
    Some((pid, pusi, payload))
}

/// PTS and header length of a PES packet start.
fn pes_header(p: &[u8]) -> Option<(Option<u64>, usize)> {
    if p.get(..3)? != [0, 0, 1] {
        return None;
    }
    let sid = *p.get(3)?;
    if matches!(sid, 0xbc | 0xbe | 0xbf | 0xf0 | 0xf1 | 0xf2 | 0xf8 | 0xff) {
        return Some((None, 6));
    }
    let flags = *p.get(7)?;
    let hlen = 9 + usize::from(*p.get(8)?);
    let pts = if flags & 0x80 != 0 {
        let b = p.get(9..14)?;
        Some(
            (u64::from(b[0] >> 1) & 7) << 30
                | u64::from(b[1]) << 22
                | u64::from(b[2] >> 1) << 15
                | u64::from(b[3]) << 7
                | u64::from(b[4] >> 1),
        )
    } else {
        None
    };
    Some((pts, hlen))
}

fn parse_pat(s: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    if s.first() != Some(&0) || s.len() < 12 {
        return out;
    }
    let end = s.len() - 4; // CRC
    let mut i = 8;
    while i + 4 <= end {
        let program = u16::from_be_bytes([s[i], s[i + 1]]);
        let pid = u16::from_be_bytes([s[i + 2] & 0x1f, s[i + 3]]);
        if program != 0 {
            out.push(pid);
        }
        i += 4;
    }
    out
}

fn descriptors(mut d: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    while d.len() >= 2 {
        let (tag, len) = (d[0], usize::from(d[1]));
        let Some(body) = d.get(2..2 + len) else { break };
        out.push((tag, body));
        d = &d[2 + len..];
    }
    out
}

fn parse_pmt(s: &[u8], head: &mut Head) {
    if s.first() != Some(&2) || s.len() < 16 {
        return;
    }
    let end = s.len() - 4;
    let pil = usize::from(u16::from_be_bytes([s[10] & 0x0f, s[11]]));
    let Some(prog_desc) = s.get(12..12 + pil) else {
        return;
    };
    for (tag, body) in descriptors(prog_desc) {
        if tag == 0x05 && body.get(..4) == Some(b"HDMV") {
            head.program_hdmv = true;
        }
    }
    let mut i = 12 + pil;
    while i + 5 <= end {
        let stream_type = s[i];
        let pid = u16::from_be_bytes([s[i + 1] & 0x1f, s[i + 2]]);
        let eil = usize::from(u16::from_be_bytes([s[i + 3] & 0x0f, s[i + 4]]));
        let Some(desc) = s.get(i + 5..i + 5 + eil) else {
            break;
        };
        i += 5 + eil;
        if head.streams.iter().any(|e| e.pid == pid) {
            continue;
        }
        let mut es = Es {
            pid,
            stream_type,
            ..Es::default()
        };
        for (tag, body) in descriptors(desc) {
            match tag {
                0x05 => es.registration = body.get(..4).and_then(|b| b.try_into().ok()),
                0x0a => {
                    if let Some(l) = body.get(..3) {
                        es.language =
                            Some(String::from_utf8_lossy(l).trim().to_string()).filter(|l| !l.is_empty());
                        es.hearing_impaired = body.get(3) == Some(&2);
                    }
                }
                0x59 => {
                    es.descriptor_codec = Some((TrackKind::Subtitle, Codec::DvbSub));
                    if let Some(l) = body.get(..3) {
                        es.language = Some(String::from_utf8_lossy(l).trim().to_string());
                        es.hearing_impaired = body.get(3).is_some_and(|t| (0x20..=0x24).contains(t));
                    }
                }
                0x56 => es.descriptor_codec = Some((TrackKind::Subtitle, Codec::Other)),
                0x6a => es.descriptor_codec = Some((TrackKind::Audio, Codec::Ac3)),
                0x7a => es.descriptor_codec = Some((TrackKind::Audio, Codec::Eac3)),
                0x7b => es.descriptor_codec = Some((TrackKind::Audio, Codec::Dts)),
                0x7c => es.descriptor_codec = Some((TrackKind::Audio, Codec::Aac)),
                0xb0 => es.dovi = dovi::parse_ts_descriptor(body),
                _ => {}
            }
        }
        head.streams.push(es);
    }
}

/// Stream type (+ registration / descriptors) → kind and codec.
fn classify(es: &Es, hdmv: bool) -> (TrackKind, Codec) {
    use Codec::*;
    use TrackKind::*;
    let reg = es.registration.as_ref().map(|r| &r[..]);
    match es.stream_type {
        0x01 => (Video, Other),
        0x02 => (Video, Mpeg2Video),
        0x03 | 0x04 => (Audio, Mp2),
        0x0f | 0x11 => (Audio, Aac),
        0x10 => (Video, Mpeg4Part2),
        0x1b => (Video, H264),
        0x24 => (Video, Hevc),
        0xea => (Video, Vc1),
        0x80 if hdmv => (Audio, Pcm),
        0x81 => (Audio, Ac3),
        0x82 => (Audio, Dts),
        0x83 if hdmv => (Audio, TrueHd),
        0x84 | 0xa1 if hdmv => (Audio, Eac3),
        0x85 | 0x86 | 0xa2 if hdmv => (Audio, Dts),
        0x87 => (Audio, Eac3),
        0x90 => (Subtitle, Pgs),
        0x92 if hdmv => (Subtitle, Other),
        0x06 => match (es.descriptor_codec, reg) {
            (Some(kc), _) => kc,
            (None, Some(b"AC-3")) => (Audio, Ac3),
            (None, Some(b"EAC3")) => (Audio, Eac3),
            (None, Some(b"DTS1" | b"DTS2" | b"DTS3")) => (Audio, Dts),
            (None, Some(b"Opus")) => (Audio, Opus),
            (None, Some(b"BSSD")) => (Audio, Pcm),
            (None, Some(b"HEVC")) => (Video, Hevc),
            _ => (Data, Other), // refined by sniffing the payload
        },
        _ => (Data, Other),
    }
}

/// Streams worth extending the head read for.
fn could_be_av(e: &Es) -> bool {
    matches!(e.stream_type, 0x01..=0x04 | 0x06 | 0x0f | 0x10 | 0x11 | 0x1b | 0x24 | 0x80..=0x87 | 0xa1 | 0xa2 | 0xea)
        && !matches!(e.descriptor_codec, Some((TrackKind::Subtitle, _)))
}

fn scan_head(buf: &[u8], container: Container) -> Head {
    let mut head = Head::default();
    for p in packets(buf, container) {
        let Some((pid, pusi, payload)) = split(p) else {
            continue;
        };
        if pid == 0 {
            if let Some(s) = head.pat.push(pusi, payload) {
                head.pmt_pids = parse_pat(&s);
            }
            continue;
        }
        if head.pmt_pids.contains(&pid) {
            if let Some(s) = head.pmts.entry(pid).or_default().push(pusi, payload) {
                parse_pmt(&s, &mut head);
                head.pmt_done += 1;
            }
            continue;
        }
        let Some(es) = head.streams.iter_mut().find(|e| e.pid == pid) else {
            continue;
        };
        let is_video = matches!(es.stream_type, 0x01 | 0x02 | 0x10 | 0x1b | 0x24 | 0xea);
        let keep = if is_video { VIDEO_KEEP } else { AUDIO_KEEP };
        if pusi {
            if let Some((pts, hlen)) = pes_header(payload) {
                if es.first_pts.is_none() {
                    es.first_pts = pts;
                }
                es.started = true;
                if es.data.len() < keep {
                    es.data.extend_from_slice(payload.get(hlen..).unwrap_or(&[]));
                }
            }
        } else if es.started && es.data.len() < keep {
            es.data.extend_from_slice(payload);
        }
    }
    head
}

fn scan_tail(buf: &[u8], container: Container, streams: &mut [Es]) {
    for p in packets(buf, container) {
        let Some((pid, true, payload)) = split(p) else {
            continue;
        };
        let Some(es) = streams.iter_mut().find(|e| e.pid == pid) else {
            continue;
        };
        if let Some((Some(pts), _)) = pes_header(payload) {
            let pts = match es.first_pts {
                Some(f) if pts < f && f - pts > PTS_WRAP / 2 => pts + PTS_WRAP,
                _ => pts,
            };
            es.last_pts = Some(es.last_pts.map_or(pts, |l| l.max(pts)));
        }
    }
}

pub(crate) fn parse(ctx: &mut Ctx<'_>, container: Container) -> PResult<Parsed> {
    ctx.container = container;
    let size = ctx.file_size();
    let mut head_len = HEAD;
    let mut head = loop {
        let buf = ctx.bytes_upto(0, head_len)?;
        let head = scan_head(buf, container);
        // Complete once every PMT is parsed and every stream that could be
        // audio/video has started a PES (audio PES often trail the video by
        // a second in ffmpeg/Blu-ray muxes).
        let complete = !head.pmt_pids.is_empty()
            && head.pmt_done >= head.pmt_pids.len()
            && head.streams.iter().all(|e| e.started || !could_be_av(e));
        if complete || head_len >= HEAD_MAX || head_len >= size {
            break head;
        }
        head_len = HEAD_MAX;
    };
    if head.streams.is_empty() {
        return malformed(
            container,
            format!("no PAT/PMT in the first {} bytes", head_len.min(size)),
        );
    }
    let hdmv = head.program_hdmv || container == Container::M2ts;

    // Tail: last PTS of each stream. Video PES start every frame, so 64 KiB
    // nearly always holds one; grow once if it does not.
    let mut tail_len = TAIL.min(size);
    loop {
        let tail = ctx.bytes(size - tail_len, tail_len)?;
        for es in &mut head.streams {
            es.last_pts = None;
        }
        scan_tail(tail, container, &mut head.streams);
        let found = head
            .streams
            .iter()
            .any(|e| e.last_pts.is_some() && could_be_av(e));
        if found || tail_len >= TAIL_MAX.min(size) {
            break;
        }
        tail_len = TAIL_MAX.min(size);
    }

    let mut r = empty_result(container);
    let mut first = None::<u64>;
    let mut last = None::<u64>;
    for es in &head.streams {
        let (mut kind, mut codec) = classify(es, hdmv);
        let mut acfg: Option<AudioCfg> = None;
        let mut vcfg: Option<VideoCfg> = None;
        match kind {
            TrackKind::Video => {
                let mut cfg = VideoCfg {
                    dovi: es.dovi,
                    ..VideoCfg::default()
                };
                match codec {
                    Codec::H264 => h264::scan_nals(&annexb_nals(&es.data), &mut cfg),
                    Codec::Hevc => hevc::scan_nals(&annexb_nals(&es.data), &mut cfg),
                    Codec::Mpeg2Video => {
                        if let Some(c) = mpegvideo::parse_mpeg2_es(&es.data) {
                            cfg.absorb(c);
                        }
                    }
                    _ => {}
                }
                if cfg.width.is_none() && matches!(codec, Codec::H264 | Codec::Hevc | Codec::Mpeg2Video) {
                    ctx.warn(format!(
                        "pid {:#x}: no sequence header in the first {} bytes",
                        es.pid, head_len
                    ));
                }
                vcfg = Some(cfg);
            }
            TrackKind::Audio => {
                let d = &es.data;
                acfg = match codec {
                    Codec::Aac => find_sync(d, audio::parse_adts),
                    Codec::Ac3 | Codec::Eac3 => find_sync(d, audio::parse_ac3_frame),
                    Codec::Dts => find_sync(d, audio::parse_dts_frame),
                    Codec::TrueHd => audio::parse_truehd_frame(d),
                    Codec::Mp2 | Codec::Mp3 => find_sync(d, audio::parse_mpa_frame),
                    Codec::Pcm => audio::parse_bd_lpcm(d),
                    _ => None,
                };
            }
            TrackKind::Data if es.stream_type == 0x06 => {
                // Private data with no identifying descriptor: sniff the payload.
                if let Some(a) = find_sync(&es.data, audio::sniff_frame) {
                    kind = TrackKind::Audio;
                    codec = a.codec.unwrap_or(Codec::Other);
                    acfg = Some(a);
                }
            }
            _ => {}
        }
        if kind == TrackKind::Data && es.first_pts.is_none() && es.data.is_empty() {
            // Unidentified data PIDs (SCTE-35, EIT …) are not tracks a player cares about.
            continue;
        }
        let mut t: Track = new_track(
            r.tracks.len() as u32,
            u64::from(es.pid),
            kind,
            codec,
            format!("{:#04x}", es.stream_type),
        );
        t.language = es.language.clone();
        t.hearing_impaired = es.hearing_impaired;
        if let Some(c) = &vcfg {
            apply_video(&mut t, c);
            let known = c.width.is_some() || codec == Codec::Mpeg2Video;
            crate::hdr::classify(&mut t, c, known);
        }
        if let Some(a) = &acfg {
            apply_audio(&mut t, a);
        }
        if matches!(kind, TrackKind::Video | TrackKind::Audio) {
            if let Some(f) = es.first_pts {
                first = Some(first.map_or(f, |x| x.min(f)));
            }
            if let Some(l) = es.last_pts {
                last = Some(last.map_or(l, |x| x.max(l)));
            }
        }
        r.tracks.push(t);
    }
    if let (Some(f), Some(l)) = (first, last) {
        if l > f {
            r.duration_ms = Some((l - f) / 90);
        }
    }
    if r.duration_ms.is_none() {
        ctx.warn("no PTS range found");
    }
    Ok(Parsed {
        result: r,
        locator: None,
    })
}

/// Try `parse` at the first few byte offsets (PES payloads are usually
/// frame-aligned, but not always).
fn find_sync(d: &[u8], parse: impl Fn(&[u8]) -> Option<AudioCfg>) -> Option<AudioCfg> {
    (0..d.len().min(4096)).find_map(|i| parse(&d[i..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pes_pts() {
        // PTS = 0x1_0000_0001 encoded per ISO/IEC 13818-1 2.4.3.7.
        let p = [0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5, 0x29, 0x00, 0x01, 0x00, 0x03];
        let (pts, hlen) = pes_header(&p).unwrap();
        assert_eq!(hlen, 14);
        assert_eq!(pts, Some((1 << 32) | 1));
    }

    #[test]
    fn garbage_sections() {
        let mut s = Section::default();
        assert_eq!(s.push(true, &[0xff]), None);
        assert_eq!(s.push(false, &[1, 2, 3]), None);
        assert!(parse_pat(&[0, 1]).is_empty());
        let mut h = Head::default();
        parse_pmt(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0], &mut h);
        assert!(h.streams.is_empty());
    }
}
