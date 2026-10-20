//! A prepared asset: playback index + segment plan + open source file.
//!
//! Every method here is a pure function of `(asset, track, segment n)`: no
//! per-session state, so any segment can be produced cold, in any order
//! (research/03 §4.1). Methods do blocking I/O; the async engine runs them on
//! Tokio's blocking pool.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use flux_core::{Codec, TrackKind};

use crate::codec::{self, audio, NalFlavor};
use crate::error::{HlsError, Result};
use crate::fmp4::{self, AudioConfig, EntrySpec, Fragment, InitSpec, Sample, SampleData};
use crate::index::mkv::{self, MkvFrame, MkvLayout, SegmentQuery};
use crate::index::mp4::Mp4Samples;
use crate::index::{pack_language, ticks_to_ns, IndexTrack, Layout, PlaybackIndex};
use crate::segment::{SegmentPlan, Span};
use crate::source::{Source, Windows};
use crate::SegmentBody;

/// Segments whose source range is at most this many bytes are served from
/// the parse buffer (zero-copy slices) instead of being re-read from the file.
pub const DEFAULT_INLINE_LIMIT: usize = 16 << 20;

/// One source audio frame, for the transcoder.
#[derive(Debug, Clone)]
pub struct SourceFrame {
    /// Presentation time on the source timeline, ns.
    pub pts_ns: i64,
    pub data: Bytes,
}

/// An indexed, segment-addressable media file.
pub struct Asset {
    pub index: PlaybackIndex,
    pub segments: SegmentPlan,
    source: Arc<Source>,
    /// MP4 only: expanded sample tables, parallel to `index.tracks`.
    mp4: Vec<Mp4Samples>,
    /// See [`DEFAULT_INLINE_LIMIT`].
    pub inline_limit: usize,
    /// Strip in-band VPS/SPS/PPS from keyframes so `hvc1`/`avc1` stay valid.
    pub strip_parameter_sets: bool,
}

impl std::fmt::Debug for Asset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Asset")
            .field("path", &self.index.source.path)
            .field("segments", &self.segments.len())
            .finish()
    }
}

impl Asset {
    /// Opens and indexes a file (blocking).
    pub fn open(path: impl AsRef<Path>, target_ns: i64) -> Result<Self> {
        Self::open_with(path, target_ns, None)
    }

    /// Opens and indexes a file, falling back to an `ffprobe` scan for
    /// containers without an index (blocking).
    pub fn open_with(path: impl AsRef<Path>, target_ns: i64, ffprobe: Option<&Path>) -> Result<Self> {
        Self::open_with_limits(path, target_ns, ffprobe, &Default::default())
    }

    /// [`Asset::open_with`] with explicit limits on the ffprobe scan.
    pub fn open_with_limits(
        path: impl AsRef<Path>,
        target_ns: i64,
        ffprobe: Option<&Path>,
        limits: &crate::index::external::ScanLimits,
    ) -> Result<Self> {
        let src = Source::open(path)?;
        let index = PlaybackIndex::build_with_fallback_limits(&src, ffprobe, limits)?;
        Self::from_parts(index, Arc::new(src), target_ns)
    }

    fn ffmpeg_only(&self) -> Result<()> {
        match self.index.layout {
            Layout::External(_) => Err(HlsError::Unsupported(
                "this container has no index; only ffmpeg transcodes can read it".into(),
            )),
            _ => Ok(()),
        }
    }

    /// Re-opens a file with a cached index; fails if the file changed.
    pub fn with_index(index: PlaybackIndex, target_ns: i64) -> Result<Self> {
        let src = Source::open(&index.source.path)?;
        if src.id() != &index.source {
            return Err(HlsError::NotFound("cached index is stale (file changed)".into()));
        }
        Self::from_parts(index, Arc::new(src), target_ns)
    }

    fn from_parts(index: PlaybackIndex, source: Arc<Source>, target_ns: i64) -> Result<Self> {
        let mut segments = SegmentPlan::build(&index, target_ns)?;
        if let Layout::Ts(l) = &index.layout {
            crate::ts::fill_segment_sizes(&mut segments, &index, l);
        }
        let mp4 = match &index.layout {
            Layout::Mp4(l) => l
                .tracks
                .iter()
                .zip(&index.tracks)
                .map(|(tab, t)| {
                    if t.codec == Codec::Pcm {
                        tab.expand_chunks()
                    } else {
                        tab.expand()
                    }
                })
                .collect(),
            Layout::Matroska(_) | Layout::External(_) | Layout::Ts(_) => Vec::new(),
        };
        Ok(Asset {
            index,
            segments,
            source,
            mp4,
            inline_limit: DEFAULT_INLINE_LIMIT,
            strip_parameter_sets: true,
        })
    }

    /// The open source file.
    pub fn source(&self) -> &Arc<Source> {
        &self.source
    }

    /// Approximate heap size in bytes (index, expanded MP4 sample tables,
    /// segment plan), for the engine's asset cache budget.
    pub fn approx_bytes(&self) -> usize {
        let samples: usize = self.mp4.iter().map(|s| s.len() * 29).sum();
        let tables: usize = match &self.index.layout {
            Layout::Mp4(l) => l
                .tracks
                .iter()
                .map(|t| {
                    t.sizes.len() * 4
                        + t.chunk_offsets.len() * 8
                        + (t.stts.len() + t.ctts.len() + t.stsc.len()) * 16
                        + t.sync.as_ref().map_or(0, |s| s.len() * 4)
                })
                .sum(),
            _ => 0,
        };
        let tracks: usize = self
            .index
            .tracks
            .iter()
            .map(|t| 512 + t.codec_private.len() + t.synth_config.as_ref().map_or(0, |c| c.len()))
            .sum();
        4096 + samples + tables + tracks + self.index.keyframes.len() * 40 + self.segments.len() * 72
    }

    fn track(&self, index: u32) -> Result<(usize, &IndexTrack)> {
        self.index
            .tracks
            .iter()
            .enumerate()
            .find(|(_, t)| t.index == index)
            .ok_or_else(|| HlsError::NotFound(format!("track {index}")))
    }

    fn video(&self) -> Result<(usize, &IndexTrack)> {
        let vi = self
            .index
            .video
            .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
        Ok((vi, &self.index.tracks[vi]))
    }

    fn span(&self, n: usize) -> Result<Span> {
        self.segments
            .segments
            .get(n)
            .copied()
            .ok_or_else(|| HlsError::NotFound(format!("segment {n}")))
    }

    /// Output track ID for a source track (stable across renditions).
    pub fn track_id(t: &IndexTrack) -> u32 {
        t.index + 1
    }

    fn strips(&self, v: &IndexTrack) -> bool {
        self.strip_parameter_sets && v.inband_parameter_sets && matches!(v.codec, Codec::H264 | Codec::Hevc)
    }

    // ------------------------------------------------------------- init segments

    /// The video sample entry, as written to the init segment.
    pub fn video_entry(&self) -> Result<EntrySpec> {
        let (_, v) = self.video()?;
        let keep_inband = v.inband_parameter_sets && !self.strips(v);
        let dv_profile = v.hdr.as_ref().and_then(|h| h.dovi).map(|d| d.profile);
        let (fourcc, config_box): ([u8; 4], [u8; 4]) = match v.codec {
            Codec::H264 => (if keep_inband { *b"avc3" } else { *b"avc1" }, *b"avcC"),
            Codec::Hevc => match (dv_profile, keep_inband) {
                (Some(5), false) => (*b"dvh1", *b"hvcC"),
                (Some(5), true) => (*b"dvhe", *b"hvcC"),
                (_, true) => (*b"hev1", *b"hvcC"),
                _ => (*b"hvc1", *b"hvcC"),
            },
            Codec::Av1 => (*b"av01", *b"av1C"),
            c => {
                return Err(HlsError::Unsupported(format!(
                    "{c:?} video cannot be remuxed to fMP4"
                )))
            }
        };
        // Profile 7 has an enhancement layer no fMP4 client can use: play the
        // HDR10 base layer and drop the DV record (a real 7→8.1 rewrite is a gap).
        let dovi = v.dovi.clone().filter(|_| dv_profile != Some(7));
        Ok(EntrySpec::Video {
            fourcc,
            config_box,
            config: v.codec_private.clone(),
            width: v.width.unwrap_or(0) as u16,
            height: v.height.unwrap_or(0) as u16,
            color: v.color,
            mastering: v.hdr.as_ref().and_then(|h| h.mastering),
            cll: v.hdr.as_ref().and_then(|h| h.cll),
            dovi,
            // Anamorphic sources keep their shape; portrait phone video its
            // rotation (the copy carries the source's matrix, PB-9).
            pasp: v.extra.sar,
            matrix: v.extra.matrix,
        })
    }

    /// Init segment of the video rendition.
    pub fn video_init(&self) -> Result<Vec<u8>> {
        let (_, v) = self.video()?;
        let spec = InitSpec {
            track_id: Self::track_id(v),
            timescale: v.timescale,
            language: pack_language(None),
            entry: self.video_entry()?,
        };
        Ok(fmp4::init_segment(&[spec]))
    }

    /// The sample entry of an audio track copied as-is.
    pub fn audio_copy_entry(&self, track: u32) -> Result<EntrySpec> {
        self.ffmpeg_only()?;
        let (_, a) = self.track(track)?;
        if a.kind != TrackKind::Audio {
            return Err(HlsError::NotFound(format!("track {track} is not audio")));
        }
        if let Layout::Ts(_) = self.index.layout {
            crate::ts::check_copy(a)?;
        }
        let channels = a.channels.unwrap_or(2) as u16;
        let sample_rate = a.timescale;
        let missing = || HlsError::Unsupported(format!("track {track}: missing {:?} configuration", a.codec));
        let (fourcc, config) = match a.codec {
            Codec::Aac => {
                let asc = a.codec_private.clone();
                let ch = audio::AacConfig::parse(&asc)
                    .map(|c| c.channel_config as u16)
                    .filter(|&c| c > 0);
                return Ok(EntrySpec::Audio {
                    fourcc: *b"mp4a",
                    channels: ch.unwrap_or(channels),
                    sample_rate,
                    config: AudioConfig::Esds {
                        object_type: 0x40,
                        dsi: asc,
                        avg_bitrate: a.bitrate.unwrap_or(0) as u32,
                    },
                });
            }
            Codec::Mp3 => (
                *b"mp4a",
                AudioConfig::Esds {
                    object_type: 0x6B,
                    dsi: Vec::new(),
                    avg_bitrate: a.bitrate.unwrap_or(0) as u32,
                },
            ),
            Codec::Ac3 => (
                *b"ac-3",
                AudioConfig::Dac3(a.synth_config.clone().ok_or_else(missing)?),
            ),
            Codec::Eac3 => (
                *b"ec-3",
                AudioConfig::Dec3(a.synth_config.clone().ok_or_else(missing)?),
            ),
            Codec::Opus => {
                let head = audio::OpusHead::parse(&a.codec_private).ok_or_else(missing)?;
                (*b"Opus", AudioConfig::Dops(head.dops()))
            }
            Codec::Flac => (
                *b"fLaC",
                AudioConfig::Dfla(a.synth_config.clone().ok_or_else(missing)?),
            ),
            c => {
                return Err(HlsError::Unsupported(format!(
                    "{c:?} audio cannot be copied into fMP4"
                )))
            }
        };
        Ok(EntrySpec::Audio {
            fourcc,
            channels,
            sample_rate,
            config,
        })
    }

    /// Init segment of a copied audio rendition.
    pub fn audio_copy_init(&self, track: u32) -> Result<Vec<u8>> {
        let (_, a) = self.track(track)?;
        let spec = InitSpec {
            track_id: Self::track_id(a),
            timescale: a.timescale,
            language: pack_language(a.language.as_deref()),
            entry: self.audio_copy_entry(track)?,
        };
        Ok(fmp4::init_segment(&[spec]))
    }

    // ------------------------------------------------------------- media segments

    fn mkv_layout(&self) -> Option<&MkvLayout> {
        match &self.index.layout {
            Layout::Matroska(l) => Some(l),
            Layout::Mp4(_) | Layout::External(_) | Layout::Ts(_) => None,
        }
    }

    fn mkv_bounds(&self, l: &MkvLayout, span: &Span) -> (u64, u64) {
        let kf = &self.index.keyframes;
        let start = if span.kf == 0 {
            l.first_cluster
        } else {
            kf[span.kf].pos
        };
        let end = span.next_kf.map(|k| kf[k].pos).unwrap_or(l.data_end);
        (start, end)
    }

    /// Video media segment `n` (copy).
    pub fn video_segment(&self, n: usize) -> Result<SegmentBody> {
        let span = self.span(n)?;
        let (vi, v) = self.video()?;
        let frag = match &self.index.layout {
            Layout::Matroska(l) => {
                let kf = &self.index.keyframes;
                let (start_pos, end_pos) = self.mkv_bounds(l, &span);
                // Small segments: one read, payloads sliced from memory. Large
                // ones (4K remux): header-only walk with small windows, payloads
                // streamed later as file ranges (≈1.2× reads, bounded memory).
                let large = end_pos.saturating_sub(start_pos) > self.inline_limit as u64;
                let q = SegmentQuery {
                    video: Some(v.id),
                    video_start: kf[span.kf].raw_ts,
                    video_end: span.next_kf.map(|k| kf[k].raw_ts),
                    others: &[],
                    t_start: 0,
                    t_end: 0,
                    start_pos,
                    end_pos,
                    chunk: large.then_some(128 << 10),
                    retain: !large,
                    capture: false,
                };
                let (frames, mut win) = mkv::read_segment(&self.source, l, &q)?;
                self.mkv_video_fragment(v, l, &span, &frames, &mut win)?
            }
            Layout::Mp4(_) => self.mp4_video_fragment(vi, v, &span)?,
            Layout::Ts(l) => crate::ts::video_fragment(self, l, &span)?,
            Layout::External(_) => return self.ffmpeg_only().map(|_| SegmentBody::new()),
        };
        self.checked(fmp4::media_segment(n as u32 + 1, &[frag]))
    }

    /// Attaches the source to a body whose file ranges were all checked to
    /// lie inside the file. A truncated file (an interrupted copy whose
    /// `moov` still describes the full movie) fails here, before a response
    /// with a `Content-Length` it could not fill is started (ROB-5). So does
    /// a source that can't be reopened as the file it was opened as (deleted,
    /// replaced, or a re-pointed link; SEC-2).
    fn checked(&self, body: SegmentBody) -> Result<SegmentBody> {
        let len = self.source.len();
        if body
            .parts()
            .iter()
            .any(|p| matches!(p, crate::body::Part::File { .. }))
        {
            self.source.ensure_open()?;
        }
        for p in body.parts() {
            if let crate::body::Part::File { offset, len: l } = p {
                if offset.checked_add(*l).is_none_or(|end| end > len) {
                    return Err(HlsError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "{}: samples at {offset}+{l} lie past the end of the file ({len} bytes); is it truncated?",
                            self.index.source.path.display()
                        ),
                    )));
                }
            }
        }
        Ok(body.with_source(self.source.clone()))
    }

    fn sample_data(
        &self,
        t: &IndexTrack,
        win: &mut Windows<'_>,
        inline: bool,
        off: u64,
        len: u32,
    ) -> SampleData {
        let prefix = t.header_strip.as_ref().filter(|p| !p.is_empty());
        match (inline, prefix) {
            (true, None) => win
                .slice(off, len as usize)
                .map(SampleData::Bytes)
                .unwrap_or(SampleData::File { offset: off, len }),
            (true, Some(p)) => match win.slice(off, len as usize) {
                Some(b) => {
                    let mut v = Vec::with_capacity(p.len() + b.len());
                    v.extend_from_slice(p);
                    v.extend_from_slice(&b);
                    SampleData::Bytes(Bytes::from(v))
                }
                None => SampleData::Prefixed {
                    prefix: Bytes::from(p.clone()),
                    offset: off,
                    len,
                },
            },
            (false, None) => SampleData::File { offset: off, len },
            (false, Some(p)) => SampleData::Prefixed {
                prefix: Bytes::from(p.clone()),
                offset: off,
                len,
            },
        }
    }

    /// First `n` bytes of a frame (captured payload or through the window).
    fn frame_head(&self, f: &MkvFrame, win: &mut Windows<'_>, n: usize) -> Result<Vec<u8>> {
        match &f.data {
            Some(d) => Ok(d[..n.min(d.len())].to_vec()),
            None => Ok(win.get(f.offset, n)?.to_vec()),
        }
    }

    /// A captured payload with Matroska header-stripping bytes re-prepended.
    fn with_prefix(&self, t: &IndexTrack, d: &Bytes) -> Bytes {
        match t.header_strip.as_ref().filter(|p| !p.is_empty()) {
            None => d.clone(),
            Some(p) => {
                let mut v = Vec::with_capacity(p.len() + d.len());
                v.extend_from_slice(p);
                v.extend_from_slice(d);
                Bytes::from(v)
            }
        }
    }

    fn sample_bytes(&self, t: &IndexTrack, win: &mut Windows<'_>, off: u64, len: u32) -> Result<Vec<u8>> {
        let mut v = t.header_strip.clone().unwrap_or_default();
        match win.slice(off, len as usize) {
            Some(b) => v.extend_from_slice(&b),
            None => v.extend_from_slice(&self.source.read_at(off, len as usize)?),
        }
        Ok(v)
    }

    fn strip_keyframe(&self, v: &IndexTrack, bytes: &[u8]) -> Bytes {
        let (flavor, ls) = match v.codec {
            Codec::H264 => (
                NalFlavor::H264,
                codec::h264::AvcConfig::parse(&v.codec_private).map(|c| c.length_size),
            ),
            _ => (
                NalFlavor::Hevc,
                codec::hevc::HevcConfig::parse(&v.codec_private).map(|c| c.length_size),
            ),
        };
        Bytes::from(codec::strip_parameter_sets(
            bytes,
            ls.unwrap_or(4) as usize,
            flavor,
        ))
    }

    /// MKV video: DTS synthesized from sorted PTS, CTS = PTS + D − DTS ≥ 0
    /// (research/03 §4.4 "Timestamps").
    fn mkv_video_fragment(
        &self,
        v: &IndexTrack,
        l: &MkvLayout,
        span: &Span,
        frames: &[MkvFrame],
        win: &mut Windows<'_>,
    ) -> Result<Fragment> {
        if frames.is_empty() {
            return Err(HlsError::mkv(format!(
                "no video frames found for segment at {} ns",
                span.start_ns
            )));
        }
        let inline = win.retains() && win.bytes_read() <= self.inline_limit as u64;
        let pts: Vec<i64> = frames.iter().map(|f| v.snap_ticks(l.ts_ns(f.ts))).collect();
        let mut sorted = pts.clone();
        sorted.sort_unstable();
        let d = v.ns_to_ticks(self.index.presentation_offset_ns);
        // Decode times must be ≥ 0; with a negative first PTS (rare) shift the
        // whole track and accept signed composition offsets.
        let lift = (-sorted[0]).max(0);
        let fallback_dur = v.frame_duration.map(|x| x as i64).unwrap_or_else(|| {
            let n = sorted.len() as i64;
            if n > 1 {
                (sorted[sorted.len() - 1] - sorted[0]) / (n - 1)
            } else {
                1
            }
        });
        let strip = self.strips(v);
        let mut samples = Vec::with_capacity(frames.len());
        let last_ns = frames.iter().map(|f| l.ts_ns(f.ts)).max().unwrap_or(span.end_ns);
        for (i, f) in frames.iter().enumerate() {
            let dts = sorted[i];
            let dur = match sorted.get(i + 1) {
                Some(next) => next - dts,
                // The last frame: one frame on the grid; off the grid (a
                // VFR stretch, PB-1) it lasts until the next segment starts.
                None => match v.frame_duration {
                    Some(fd) if v.grid_point(last_ns).is_some() => fd as i64,
                    Some(_) => (span.end_ticks - dts).max(1),
                    None => (span.end_ticks - dts).max(fallback_dur.max(1)),
                },
            };
            let cts = pts[i] + d - dts;
            let data = if strip && f.key {
                let raw = self.sample_bytes(v, win, f.offset, f.len)?;
                SampleData::Bytes(self.strip_keyframe(v, &raw))
            } else {
                self.sample_data(v, win, inline, f.offset, f.len)
            };
            samples.push(Sample {
                duration: dur.clamp(0, u32::MAX as i64) as u32,
                cts: cts.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                sync: f.key,
                data,
            });
        }
        Ok(Fragment {
            track_id: Self::track_id(v),
            base_dts: (sorted[0] + lift) as u64,
            samples,
            video: true,
        })
    }

    fn mp4_video_fragment(&self, vi: usize, v: &IndexTrack, span: &Span) -> Result<Fragment> {
        let Layout::Mp4(l) = &self.index.layout else {
            return Err(HlsError::mp4("layout"));
        };
        let tab = &l.tracks[vi];
        let s = &self.mp4[vi];
        let kf = &self.index.keyframes;
        let from = kf[span.kf].pos as usize;
        let to = span.next_kf.map(|k| kf[k].pos as usize).unwrap_or(s.len());
        if from >= to || to > s.len() {
            return Err(HlsError::mp4(format!("bad sample range {from}..{to}")));
        }
        let d = v.ns_to_ticks(self.index.presentation_offset_ns);
        let strip = self.strips(v);
        let mut samples = Vec::with_capacity(to - from);
        for i in from..to {
            let dur = if i + 1 < s.len() {
                s.dts[i + 1] - s.dts[i]
            } else {
                v.frame_duration.unwrap_or(1) as i64
            };
            let cts = s.cts[i] - tab.edit_shift + d;
            let data = if strip && s.sync[i] {
                let raw = self.source.read_at(s.offset[i], s.size[i] as usize)?;
                SampleData::Bytes(self.strip_keyframe(v, &raw))
            } else {
                SampleData::File {
                    offset: s.offset[i],
                    len: s.size[i],
                }
            };
            samples.push(Sample {
                duration: dur.max(0) as u32,
                cts: cts.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                sync: s.sync[i],
                data,
            });
        }
        Ok(Fragment {
            track_id: Self::track_id(v),
            base_dts: s.dts[from].max(0) as u64,
            samples,
            video: true,
        })
    }

    /// Copied audio media segment `n` for source track `track`: the frames
    /// whose PTS lies in the video segment's [T_k, T_{k+1}).
    pub fn audio_copy_segment(&self, track: u32, n: usize) -> Result<SegmentBody> {
        let span = self.span(n)?;
        let (ai, a) = self.track(track)?;
        let last = span.next_kf.is_none();
        let frag = match &self.index.layout {
            Layout::Matroska(l) => {
                let kf = &self.index.keyframes;
                let (start_pos, end_pos) = self.mkv_bounds(l, &span);
                let others = [a.id];
                // Partition by presentation time (block time − CodecDelay).
                let delay = (a.codec_delay_ns / l.timestamp_scale.max(1)) as i64;
                let q = SegmentQuery {
                    video: None,
                    video_start: 0,
                    video_end: None,
                    others: &others,
                    t_start: if span.kf == 0 {
                        i64::MIN
                    } else {
                        kf[span.kf].raw_ts + delay
                    },
                    t_end: span.next_kf.map(|k| kf[k].raw_ts + delay).unwrap_or(i64::MAX),
                    start_pos,
                    end_pos,
                    chunk: Some(64 << 10),
                    // Audio payloads are copied during the walk; video payloads
                    // are skipped, so memory stays at one small window.
                    retain: false,
                    capture: true,
                };
                let (frames, mut win) = mkv::read_segment(&self.source, l, &q)?;
                self.mkv_audio_fragment(a, l, &frames, &mut win)?
            }
            Layout::External(_) => return self.ffmpeg_only().map(|_| SegmentBody::new()),
            Layout::Ts(l) => crate::ts::audio_fragment(self, l, track, &span)?,
            Layout::Mp4(l) => {
                let tab = &l.tracks[ai];
                let s = &self.mp4[ai];
                let t0 = if span.kf == 0 { i64::MIN } else { span.start_ns };
                let t1 = if last { i64::MAX } else { span.end_ns };
                let pts_ns = |i: usize| ticks_to_ns(s.dts[i] - tab.edit_shift, tab.timescale);
                let from = lower_bound(s.len(), |i| pts_ns(i) < t0);
                let to = lower_bound(s.len(), |i| pts_ns(i) < t1);
                let d = a.ns_to_ticks(self.index.presentation_offset_ns);
                let mut samples = Vec::with_capacity(to.saturating_sub(from));
                for i in from..to {
                    let dur = if i + 1 < s.len() {
                        s.dts[i + 1] - s.dts[i]
                    } else {
                        a.frame_duration.unwrap_or(1024) as i64
                    };
                    samples.push(Sample {
                        duration: dur.max(0) as u32,
                        cts: 0,
                        sync: true,
                        data: SampleData::File {
                            offset: s.offset[i],
                            len: s.size[i],
                        },
                    });
                }
                let base = s
                    .dts
                    .get(from)
                    .map(|&x| x - tab.edit_shift + d)
                    .unwrap_or(0)
                    .max(0) as u64;
                Fragment {
                    track_id: Self::track_id(a),
                    base_dts: base,
                    samples,
                    video: false,
                }
            }
        };
        self.checked(fmp4::media_segment(n as u32 + 1, &[frag]))
    }

    /// MKV audio: frames whose PTS lies in the segment, placed on the track's
    /// frame grid (see [`grid_positions`]).
    fn mkv_audio_fragment(
        &self,
        a: &IndexTrack,
        l: &MkvLayout,
        frames: &[MkvFrame],
        win: &mut Windows<'_>,
    ) -> Result<Fragment> {
        let inline = win.retains() && win.bytes_read() <= self.inline_limit as u64;
        let d = a.ns_to_ticks(self.index.presentation_offset_ns);
        let fd = a.frame_duration.map(|x| x as i64);
        let pts_ns: Vec<i64> = frames
            .iter()
            .map(|f| {
                let lace_ns = fd
                    .map(|fd| ticks_to_ns(fd * f.lace as i64, a.timescale))
                    .unwrap_or(0);
                l.ts_ns(f.ts) - a.codec_delay_ns as i64 + lace_ns
            })
            .collect();
        let positions = match (fd, a.first_pts_ns) {
            (Some(fd), Some(origin)) if fd > 0 => {
                grid_positions(&pts_ns, a.timescale, a.ns_to_ticks(origin), fd)
            }
            _ => pts_ns.iter().map(|&p| a.ns_to_ticks(p)).collect(),
        };
        let mut samples = Vec::with_capacity(frames.len());
        for (i, f) in frames.iter().enumerate() {
            let codec_dur = match a.codec {
                Codec::Opus => {
                    let b = self.frame_head(f, win, 1)?;
                    audio::opus_packet_samples(&b).map(|x| x as i64).or(fd)
                }
                Codec::Flac if fd.is_none() => {
                    let b = self.frame_head(f, win, 16)?;
                    audio::flac_frame_samples(&b).map(|x| x as i64)
                }
                _ => fd,
            };
            // Within a segment, a frame lasts until the next one starts (this
            // absorbs the source's own discontinuities); the last frame lasts
            // its codec frame length.
            let dur = match (positions.get(i + 1), codec_dur) {
                (Some(next), Some(cd)) if fd.is_some() => (next - positions[i]).clamp(1, 4 * cd),
                (_, Some(cd)) => cd,
                (Some(next), None) => (next - positions[i]).max(1),
                (None, None) => 1024,
            };
            samples.push(Sample {
                duration: dur as u32,
                cts: 0,
                sync: true,
                data: match &f.data {
                    Some(d) => SampleData::Bytes(self.with_prefix(a, d)),
                    None => self.sample_data(a, win, inline, f.offset, f.len),
                },
            });
        }
        Ok(Fragment {
            track_id: Self::track_id(a),
            base_dts: positions.first().map(|p| p + d).unwrap_or(0).max(0) as u64,
            samples,
            video: false,
        })
    }

    /// Source audio frames of `track` with PTS in `[t0_ns, t1_ns)`, in order
    /// (for the transcoder).
    pub fn audio_frames(&self, track: u32, t0_ns: i64, t1_ns: i64) -> Result<Vec<SourceFrame>> {
        let (ai, a) = self.track(track)?;
        match &self.index.layout {
            Layout::Matroska(l) => {
                let kf = &self.index.keyframes;
                // Start at a cluster a little before t0 (muxers interleave by
                // decode time, so audio can trail the video keyframe).
                let start_k = kf
                    .partition_point(|k| k.pts_ns <= t0_ns - 1_000_000_000)
                    .saturating_sub(1);
                let start_pos = if start_k == 0 {
                    l.first_cluster
                } else {
                    kf[start_k].pos
                };
                let end_k = kf.partition_point(|k| k.pts_ns < t1_ns);
                let end_pos = kf.get(end_k).map(|k| k.pos).unwrap_or(l.data_end);
                let ts_scale = l.timestamp_scale.max(1) as i64;
                let delay = a.codec_delay_ns as i64;
                let others = [a.id];
                let q = SegmentQuery {
                    video: None,
                    video_start: 0,
                    video_end: None,
                    others: &others,
                    t_start: (t0_ns + delay).div_euclid(ts_scale),
                    t_end: if t1_ns == i64::MAX {
                        i64::MAX
                    } else {
                        (t1_ns + delay + ts_scale - 1).div_euclid(ts_scale)
                    },
                    start_pos,
                    end_pos,
                    chunk: Some(256 << 10),
                    // Audio payloads are copied during the walk; video payloads
                    // are skipped, so memory stays at one small window.
                    retain: false,
                    capture: true,
                };
                let (frames, mut win) = mkv::read_segment(&self.source, l, &q)?;
                let fd = a.frame_duration.map(|x| x as i64);
                let mut out = Vec::with_capacity(frames.len());
                for f in &frames {
                    let lace_ns = fd
                        .map(|fd| ticks_to_ns(fd * f.lace as i64, a.timescale))
                        .unwrap_or(0);
                    let data = match &f.data {
                        Some(d) => self.with_prefix(a, d),
                        None => Bytes::from(self.sample_bytes(a, &mut win, f.offset, f.len)?),
                    };
                    out.push(SourceFrame {
                        pts_ns: l.ts_ns(f.ts) - a.codec_delay_ns as i64 + lace_ns,
                        data,
                    });
                }
                Ok(out)
            }
            Layout::External(_) => self.ffmpeg_only().map(|_| Vec::new()),
            Layout::Ts(l) => crate::ts::source_frames(self, l, track, t0_ns, t1_ns),
            Layout::Mp4(l) => {
                let tab = &l.tracks[ai];
                let s = &self.mp4[ai];
                let pts_ns = |i: usize| ticks_to_ns(s.dts[i] - tab.edit_shift, tab.timescale);
                let from = lower_bound(s.len(), |i| pts_ns(i) < t0_ns);
                let to = lower_bound(s.len(), |i| pts_ns(i) < t1_ns);
                let mut out = Vec::with_capacity(to.saturating_sub(from));
                if a.codec == Codec::Pcm {
                    // A PCM "sample" is one sample frame (a few bytes): read
                    // contiguous runs at once and hand them over as frames.
                    let mut i = from;
                    while i < to {
                        let start = s.offset[i];
                        let mut end = start + s.size[i] as u64;
                        let mut j = i + 1;
                        while j < to && s.offset[j] == end && end - start < (256 << 10) {
                            end += s.size[j] as u64;
                            j += 1;
                        }
                        out.push(SourceFrame {
                            pts_ns: pts_ns(i),
                            data: Bytes::from(self.source.read_at(start, (end - start) as usize)?),
                        });
                        i = j;
                    }
                    return Ok(out);
                }
                for i in from..to {
                    let data = Bytes::from(self.source.read_at(s.offset[i], s.size[i] as usize)?);
                    out.push(SourceFrame {
                        pts_ns: pts_ns(i),
                        data,
                    });
                }
                Ok(out)
            }
        }
    }

    /// I-frame segment `n` (for `EXT-X-I-FRAMES-ONLY` playlists): keyframe
    /// `n` alone, shown until the next keyframe, on the copied video
    /// rendition's timeline (so it shares that rendition's init segment).
    pub fn keyframe_segment(&self, n: usize) -> Result<SegmentBody> {
        let (vi, v) = self.video()?;
        let kfs = &self.index.keyframes;
        let kf = kfs
            .get(n)
            .ok_or_else(|| HlsError::NotFound(format!("keyframe {n}")))?;
        let pts_ticks = |k: &crate::index::Keyframe| match &self.index.layout {
            Layout::Mp4(_) => k.raw_ts,
            _ => v.snap_ticks(k.pts_ns),
        };
        let (off, len) = match &self.index.layout {
            Layout::Matroska(l) => {
                let from = if n == 0 { l.first_cluster } else { kf.pos };
                mkv::keyframe_range(&self.source, l, kf, v.id, from)?
                    .ok_or_else(|| HlsError::mkv(format!("keyframe {n} not found in its cluster")))?
            }
            Layout::Mp4(_) => {
                let s = &self.mp4[vi];
                let i = kf.pos as usize;
                (
                    *s.offset.get(i).ok_or_else(|| HlsError::mp4("keyframe sample"))?,
                    s.size[i],
                )
            }
            Layout::Ts(l) => return crate::ts::keyframe_segment(self, l, n),
            Layout::External(_) => return self.ffmpeg_only().map(|_| SegmentBody::new()),
        };
        let mut win = Windows::streaming(&self.source, 4096);
        let raw = self.sample_bytes(v, &mut win, off, len)?;
        let mut data = if self.strips(v) {
            self.strip_keyframe(v, &raw).to_vec()
        } else {
            raw
        };
        if v.codec == Codec::Hevc {
            // CRA → BLA_W_LP (same slice header syntax, one header byte): each
            // I-frame then starts its own coded video sequence. Played back to
            // back, CRA pictures 10 s apart derive their POC from each other
            // across an lsb wrap, and decoders output them out of order
            // (ffmpeg swapped every pair). An end-of-sequence NAL instead made
            // ffmpeg drop all but one frame.
            let ls = codec::hevc::HevcConfig::parse(&v.codec_private)
                .map(|c| c.length_size as usize)
                .unwrap_or(4);
            codec::cra_to_bla(&mut data, ls);
        }
        let data = Bytes::from(data);
        let start = pts_ticks(kf);
        let end = kfs
            .get(n + 1)
            .map(pts_ticks)
            .unwrap_or(start + v.frame_duration.unwrap_or(1) as i64);
        let d = v.ns_to_ticks(self.index.presentation_offset_ns);
        let frag = Fragment {
            track_id: Self::track_id(v),
            base_dts: (start + d).max(0) as u64,
            samples: vec![Sample {
                duration: (end - start).clamp(1, u32::MAX as i64) as u32,
                cts: 0,
                sync: true,
                data: SampleData::Bytes(data),
            }],
            video: true,
        };
        Ok(fmp4::media_segment(n as u32 + 1, &[frag]))
    }

    /// Presentation times of the video frames that remux segment `n`
    /// decodes, in video ticks exactly as the remux places them (snapped to
    /// the frame grid when on it), sorted. Read from the index alone: the
    /// MP4 sample table, or Matroska block headers (no payloads). `None`
    /// for containers only ffmpeg reads.
    ///
    /// The video transcoder counts and times its output frames with these
    /// (PB-2), so variable-frame-rate sources and tracks without a
    /// DefaultDuration keep their real frame count and timing.
    pub fn video_frame_ticks(&self, n: usize) -> Result<Option<Vec<i64>>> {
        let span = self.span(n)?;
        let (vi, v) = self.video()?;
        let mut t: Vec<i64> = match &self.index.layout {
            Layout::Matroska(l) => {
                let kf = &self.index.keyframes;
                let (start_pos, end_pos) = self.mkv_bounds(l, &span);
                let q = SegmentQuery {
                    video: Some(v.id),
                    video_start: kf[span.kf].raw_ts,
                    video_end: span.next_kf.map(|k| kf[k].raw_ts),
                    others: &[],
                    t_start: 0,
                    t_end: 0,
                    start_pos,
                    end_pos,
                    chunk: Some(64 << 10),
                    retain: false,
                    capture: false,
                };
                let (frames, _) = mkv::read_segment(&self.source, l, &q)?;
                frames.iter().map(|f| v.snap_ticks(l.ts_ns(f.ts))).collect()
            }
            Layout::Mp4(l) => {
                let tab = &l.tracks[vi];
                let s = &self.mp4[vi];
                let kf = &self.index.keyframes;
                let from = kf[span.kf].pos as usize;
                let to = span.next_kf.map(|k| kf[k].pos as usize).unwrap_or(s.len());
                (from..to.min(s.len())).map(|i| s.pts(i, tab)).collect()
            }
            // TS: constant-rate video is exact on the frame grid (the TS
            // index only claims a frame duration when every frame sits on
            // it), so the predicted counts are used.
            Layout::Ts(_) | Layout::External(_) => return Ok(None),
        };
        t.sort_unstable();
        Ok(Some(t))
    }

    /// Text subtitle cues of `track` as (start ns, end ns, payload)
    /// (Matroska only; see [`Asset::for_each_subtitle_block`]).
    pub fn subtitle_blocks(&self, track: u32) -> Result<Vec<(i64, i64, Bytes)>> {
        let (_, t) = self.track(track)?;
        let l = self
            .mkv_layout()
            .ok_or_else(|| HlsError::Unsupported("subtitles: Matroska only".into()))?;
        mkv::read_subtitles(&self.source, l, t.id)
    }

    /// Hands the text subtitle cues of `track` to `f` as they are read, in
    /// time order, so a response can stream them (the first cue needs a
    /// few reads, not the whole file); `f` returns `false` to stop. Reads
    /// only the track's cue points when the Cues index it, otherwise walks
    /// block headers without reading other tracks' payloads (PB-7).
    pub fn for_each_subtitle_block(
        &self,
        track: u32,
        f: &mut dyn FnMut((i64, i64, Bytes)) -> bool,
    ) -> Result<mkv::SubtitleSource> {
        let (_, t) = self.track(track)?;
        let l = self
            .mkv_layout()
            .ok_or_else(|| HlsError::Unsupported("subtitles: Matroska only".into()))?;
        mkv::read_subtitles_each(&self.source, l, t.id, f)
    }
}

/// First index in `0..n` for which `pred` is false (`pred` must be monotonic).
fn lower_bound(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Places constant-duration audio frames on their frame grid (ticks).
///
/// Matroska timestamps are rounded to 1 ms, so each frame is snapped to the
/// grid `origin + k·frame`. Concatenated files can shift the grid phase at a
/// join (e.g. −3 ms, then −14 ms in the corpus's `show_e0*` episodes), so the
/// frames are split into regions at phase jumps (> 1.5 ms) and each region is
/// placed on its own phase: the mean residual, quantized to 0.5 ms so that
/// independently produced segments of the same region agree exactly.
/// Unshifted streams (residual ≈ 0) land exactly on the global grid.
fn grid_positions(pts_ns: &[i64], timescale: u32, origin: i64, frame: i64) -> Vec<i64> {
    let ts = timescale as f64;
    let raw: Vec<f64> = pts_ns
        .iter()
        .map(|&p| p as f64 * ts / 1e9 - origin as f64)
        .collect();
    let jump = 0.0015 * ts;
    let q = 0.0005 * ts;
    let mut out = Vec::with_capacity(raw.len());
    let mut start = 0usize;
    while start < raw.len() {
        // Residual of a frame vs the global grid, unwrapped relative to the
        // region's first frame.
        let k0 = (raw[start] / frame as f64).round();
        let r0 = raw[start] - k0 * frame as f64;
        let mut end = start + 1;
        while end < raw.len() {
            let k = ((raw[end] - r0) / frame as f64).round();
            let r = raw[end] - k * frame as f64;
            if (r - r0).abs() > jump {
                break;
            }
            end += 1;
        }
        let mean = (start..end)
            .map(|i| {
                let k = ((raw[i] - r0) / frame as f64).round();
                raw[i] - k * frame as f64
            })
            .sum::<f64>()
            / (end - start) as f64;
        let phase = if mean.abs() <= 2.0 * q {
            0.0
        } else {
            (mean / q).round() * q
        };
        for &x in &raw[start..end] {
            let k = ((x - phase) / frame as f64).round() as i64;
            out.push(origin + k * frame + phase.round() as i64);
        }
        start = end;
    }
    out
}

#[cfg(test)]
mod grid_tests {
    use super::grid_positions;

    #[test]
    fn ms_rounded_frames_snap_to_exact_grid() {
        // AAC @48 kHz: true times k·21.333 ms, stored truncated to ms.
        let pts: Vec<i64> = (0..20)
            .map(|k| (k as f64 * 1024.0 / 48.0).floor() as i64 * 1_000_000)
            .collect();
        let pos = grid_positions(&pts, 48_000, 0, 1024);
        assert_eq!(pos, (0..20).map(|k| k * 1024).collect::<Vec<_>>());
    }

    #[test]
    fn phase_jump_gets_its_own_region() {
        // 10 frames on the grid, then 10 frames shifted by −14 ms (a concat join).
        let mut pts: Vec<i64> = (0..10)
            .map(|k| (k as f64 * 1024.0 / 48.0).round() as i64 * 1_000_000)
            .collect();
        pts.extend((10..20).map(|k| ((k as f64 * 1024.0 / 48.0) - 14.0).round() as i64 * 1_000_000));
        let pos = grid_positions(&pts, 48_000, 0, 1024);
        // Inside each region the spacing is exactly one frame.
        assert!(pos[..10].windows(2).all(|w| w[1] - w[0] == 1024));
        assert!(pos[10..].windows(2).all(|w| w[1] - w[0] == 1024));
        // The second region keeps the shifted phase (−14 ms ≡ +7.33 ms, quantized to 0.5 ms).
        let phase = (pos[10] % 1024 + 1024) % 1024;
        assert!((phase - 360).abs() <= 24, "phase {phase}");
        // Any sub-range of the second region yields the same positions.
        assert_eq!(grid_positions(&pts[13..], 48_000, 0, 1024), pos[13..].to_vec());
    }
}
