//! The sans-IO state machines: [`Prober`] (header pass) and
//! [`FrameSeiProber`] (optional second pass over the first video keyframe).
//!
//! Both follow the same protocol: call `start()`, then answer every
//! `Need { offset, len }` with `feed(offset, bytes)` (feeding more than asked
//! is fine and saves reads; feeding less makes the prober ask again) until
//! `Done`. Neither ever touches a file, socket or clock, so the same code runs
//! over `pread`, HTTP range requests, SMB or a Truffle stream.

use flux_core::{
    Codec, ColorInfo, Container, ContentLightLevel, MasteringDisplay, ProbeResult, TrackKind, PROBE_VERSION,
};

use crate::codec::{av1, h264, hevc, VideoCfg};
use crate::error::ProbeError;
use crate::sniff::{sniff, ContainerHint, SNIFF_LEN};
use crate::store::{Ctx, PErr, PResult, Store};
use crate::{avi, hdr, mkv, mp4, ts};

/// Default smallest read the probers ask for: one 64 KiB page. Round trips
/// dominate on HDDs and networks; on a fast local SSD a smaller read size
/// ([`Prober::with_read_size`]) can win (see docs/prototype/probe.md).
pub const MIN_READ: u64 = 64 * 1024;
/// First read for TS-like hints: PAT, PMT and the first PES of every stream.
pub const TS_HEAD: u64 = 256 * 1024;
/// Hard limits: a probe that needs more than this is a pathological file.
pub const MAX_READS: u32 = 64;
/// Hard byte limit across all reads of one pass.
pub const MAX_BYTES: u64 = 64 << 20;

/// What the prober wants next.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Feed the bytes at `[offset, offset + len)` (clipped to the file size).
    Need {
        /// Absolute file offset.
        offset: u64,
        /// Bytes wanted; at least [`MIN_READ`] unless the file ends sooner.
        len: u32,
    },
    /// Probe complete.
    Done(ProbeResult),
}

/// Header-pass output plus the private hint for the second pass.
pub(crate) struct Parsed {
    pub result: ProbeResult,
    pub locator: Option<FrameLocator>,
}

/// Where the first video keyframe lives, as far as the header pass knows.
#[derive(Debug, Clone)]
pub(crate) enum FrameLocator {
    Mkv {
        scan_from: u64,
        segment_end: u64,
        track_number: u64,
        codec: Codec,
        nal_len_size: usize,
    },
    Mp4 {
        stbl: u64,
        stbl_end: u64,
        codec: Codec,
        nal_len_size: usize,
    },
}

/// A result with nothing known yet.
pub(crate) fn empty_result(container: Container) -> ProbeResult {
    ProbeResult {
        container,
        duration_ms: None,
        bitrate: None,
        title: None,
        tracks: Vec::new(),
        chapters: Vec::new(),
        attachments: Vec::new(),
        has_index: false,
        faststart: None,
        needs_frame_sei: false,
        warnings: Vec::new(),
        probe_version: PROBE_VERSION,
        bytes_read: 0,
        reads: 0,
    }
}

/// Shared feed/need bookkeeping for both passes.
#[derive(Debug)]
struct Engine {
    store: Store,
    feeds: u32,
    bytes: u64,
    pending: Option<u64>,
    first_len: u64,
    min_read: u64,
}

impl Engine {
    fn new(store: Store, first_len: u64, min_read: u64) -> Self {
        Engine {
            store,
            feeds: 0,
            bytes: 0,
            pending: None,
            first_len,
            min_read,
        }
    }

    fn feed(&mut self, offset: u64, data: &[u8]) -> Result<(), ProbeError> {
        if let Some(p) = self.pending.take() {
            let covers = offset <= p && p < offset.saturating_add(data.len() as u64);
            if !covers {
                return Err(ProbeError::BadFeed(p));
            }
        }
        if !data.is_empty() {
            self.feeds += 1;
            self.bytes += data.len() as u64;
            self.store.insert(offset, data);
        }
        if self.feeds > MAX_READS || self.bytes > MAX_BYTES {
            return Err(ProbeError::Budget {
                reads: self.feeds,
                bytes: self.bytes,
            });
        }
        Ok(())
    }

    /// Turn a parser's minimal need into a read request.
    fn need(&mut self, offset: u64, len: u64) -> Result<(u64, u32), ProbeError> {
        let size = self.store.file_size();
        let min = if self.feeds == 0 {
            self.first_len
        } else {
            self.min_read
        };
        let len = len.max(min).min(size.saturating_sub(offset));
        let len = u32::try_from(len).map_err(|_| ProbeError::Budget {
            reads: self.feeds,
            bytes: len,
        })?;
        if self.feeds >= MAX_READS {
            return Err(ProbeError::Budget {
                reads: self.feeds,
                bytes: self.bytes,
            });
        }
        self.pending = Some(offset);
        Ok((offset, len))
    }
}

/// Sans-IO media header prober.
///
/// ```no_run
/// # use flux_probe::{Prober, Step, ContainerHint};
/// # fn read(_: u64, _: u32) -> Vec<u8> { vec![] }
/// let mut p = Prober::new(1_000_000, ContainerHint::NONE);
/// let mut step = p.start()?;
/// let result = loop {
///     match step {
///         Step::Need { offset, len } => step = p.feed(offset, &read(offset, len))?,
///         Step::Done(r) => break r,
///     }
/// };
/// # Ok::<(), flux_probe::ProbeError>(())
/// ```
#[derive(Debug)]
pub struct Prober {
    eng: Engine,
    locator: Option<FrameLocator>,
    done: bool,
}

impl Prober {
    /// A prober for a file of `file_size` bytes. `hint` only sizes the first read.
    pub fn new(file_size: u64, hint: ContainerHint) -> Self {
        Self::with_read_size(file_size, hint, MIN_READ)
    }

    /// A prober whose reads are at least `read_size` bytes (clamped to
    /// 4 KiB–1 MiB). TS-like hints still read [`TS_HEAD`] first.
    pub fn with_read_size(file_size: u64, hint: ContainerHint, read_size: u64) -> Self {
        let min_read = read_size.clamp(4096, 1 << 20);
        let first = match hint.0 {
            Some(Container::MpegTs | Container::M2ts) => TS_HEAD.max(min_read),
            _ => min_read,
        };
        Prober {
            eng: Engine::new(Store::new(file_size), first, min_read),
            locator: None,
            done: false,
        }
    }

    /// First step (always a `Need` for a non-empty file).
    pub fn start(&mut self) -> Result<Step, ProbeError> {
        self.poll()
    }

    /// Feed bytes at `offset` (normally the answer to the last `Need`).
    pub fn feed(&mut self, offset: u64, data: &[u8]) -> Result<Step, ProbeError> {
        self.eng.feed(offset, data)?;
        self.poll()
    }

    /// Reads and bytes fed so far.
    pub fn io(&self) -> (u32, u64) {
        (self.eng.feeds, self.eng.bytes)
    }

    fn poll(&mut self) -> Result<Step, ProbeError> {
        match parse_all(&self.eng.store, self.eng.feeds) {
            Ok(Parsed { mut result, locator }) => {
                result.reads = self.eng.feeds;
                result.bytes_read = self.eng.bytes;
                self.locator = locator;
                self.done = true;
                Ok(Step::Done(result))
            }
            Err(PErr::Need { offset, len }) => {
                let (offset, len) = self.eng.need(offset, len)?;
                Ok(Step::Need { offset, len })
            }
            Err(PErr::Fail(e)) => Err(e),
        }
    }

    /// After `Done`, the second-pass prober for the main video track's first
    /// keyframe (reusing every byte already fed). `None` when the container or
    /// codec has no keyframe locator (TS parses its first PES in pass one; AVI
    /// cannot carry HDR).
    pub fn into_frame_sei(self) -> Option<FrameSeiProber> {
        if !self.done {
            return None;
        }
        let locator = self.locator?;
        Some(FrameSeiProber {
            eng: Engine::new(self.eng.store, self.eng.min_read, self.eng.min_read),
            locator,
        })
    }
}

fn parse_all(store: &Store, feeds: u32) -> PResult<Parsed> {
    let size = store.file_size();
    if size == 0 {
        return Err(PErr::Fail(ProbeError::Empty));
    }
    let mut ctx = Ctx::new(store, feeds);
    let head = ctx.bytes_upto(0, SNIFF_LEN)?;
    let Some(container) = sniff(head) else {
        let magic = head
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        return Err(PErr::Fail(ProbeError::UnknownFormat { magic }));
    };
    ctx.container = container;
    let mut parsed = match container {
        Container::Matroska | Container::WebM => mkv::parse(&mut ctx)?,
        Container::Mp4 | Container::Mov => mp4::parse(&mut ctx)?,
        Container::MpegTs | Container::M2ts => ts::parse(&mut ctx, container)?,
        Container::Avi => avi::parse(&mut ctx)?,
        Container::Unknown => return Err(PErr::Fail(ProbeError::UnknownFormat { magic: String::new() })),
    };
    let r = &mut parsed.result;
    r.warnings.append(&mut ctx.warnings);
    r.probe_version = PROBE_VERSION;
    if r.bitrate.is_none() {
        r.bitrate = r
            .duration_ms
            .filter(|d| *d > 0)
            .map(|d| size.saturating_mul(8000) / d);
    }
    r.needs_frame_sei = r.main_video().is_some_and(hdr::needs_frame_sei);
    Ok(parsed)
}

// ---------------------------------------------------------------------------

/// What the second pass learnt from the first video keyframe.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameSei {
    /// SEI 137 / AV1 HDR_MDCV.
    pub mastering: Option<MasteringDisplay>,
    /// SEI 144 / AV1 HDR_CLL.
    pub cll: Option<ContentLightLevel>,
    /// ST 2094-40 T.35 SEI seen.
    pub hdr10plus: bool,
    /// Dolby Vision RPU (HEVC NAL 62 / AV1 T.35) seen.
    pub dovi_rpu: bool,
    /// Colour from in-band parameter sets (fills gaps only).
    pub color: Option<ColorInfo>,
    /// I/O accounting for this pass.
    pub reads: u32,
    /// I/O accounting for this pass.
    pub bytes_read: u64,
}

impl FrameSei {
    /// Fold the findings into the header-pass result (main video track) and
    /// clear `needs_frame_sei`.
    pub fn apply(&self, r: &mut ProbeResult) {
        if let Some(t) = r.tracks.iter_mut().find(|t| t.kind == TrackKind::Video) {
            t.color = hdr::merge_color(t.color, self.color);
            let mut facts = hdr::facts_of(t);
            facts.mastering = facts.mastering.or(self.mastering);
            facts.cll = facts.cll.or(self.cll);
            facts.hdr10plus |= self.hdr10plus;
            facts.dovi_rpu |= self.dovi_rpu && facts.dovi.is_none();
            hdr::classify(t, &facts, true);
        }
        r.needs_frame_sei = false;
        r.reads += self.reads;
        r.bytes_read += self.bytes_read;
    }
}

/// Second-pass step.
#[derive(Debug, Clone, PartialEq)]
pub enum SeiStep {
    /// Feed `[offset, offset + len)`.
    Need {
        /// Absolute file offset.
        offset: u64,
        /// Bytes wanted.
        len: u32,
    },
    /// Done.
    Done(FrameSei),
}

/// Sans-IO second pass: reads only the first video keyframe's non-slice NAL
/// units (MKV: first block of the first cluster; MP4: sample 1 via the first
/// `stsz`/`stco` entries) and parses SEI 137/144, HDR10+ T.35 and DoVi RPU
/// presence. Parse, don't decode.
#[derive(Debug)]
pub struct FrameSeiProber {
    eng: Engine,
    locator: FrameLocator,
}

/// Bytes of a single NAL unit we are willing to look at.
const MAX_NAL: u64 = 64 * 1024;

impl FrameSeiProber {
    /// First step.
    pub fn start(&mut self) -> Result<SeiStep, ProbeError> {
        self.poll()
    }

    /// Feed bytes at `offset`.
    pub fn feed(&mut self, offset: u64, data: &[u8]) -> Result<SeiStep, ProbeError> {
        self.eng.feed(offset, data)?;
        self.poll()
    }

    fn poll(&mut self) -> Result<SeiStep, ProbeError> {
        match frame_sei(&self.eng.store, self.eng.feeds, &self.locator) {
            Ok(mut f) => {
                f.reads = self.eng.feeds;
                f.bytes_read = self.eng.bytes;
                Ok(SeiStep::Done(f))
            }
            Err(PErr::Need { offset, len }) => {
                let (offset, len) = self.eng.need(offset, len)?;
                Ok(SeiStep::Need { offset, len })
            }
            Err(PErr::Fail(e)) => Err(e),
        }
    }
}

fn frame_sei(store: &Store, feeds: u32, loc: &FrameLocator) -> PResult<FrameSei> {
    let mut ctx = Ctx::new(store, feeds);
    let (start, end, codec, nls) = match *loc {
        FrameLocator::Mkv {
            scan_from,
            segment_end,
            track_number,
            codec,
            nal_len_size,
        } => {
            ctx.container = Container::Matroska;
            let (s, e) = mkv::first_video_frame(&ctx, scan_from, segment_end, track_number)?;
            (s, e, codec, nal_len_size)
        }
        FrameLocator::Mp4 {
            stbl,
            stbl_end,
            codec,
            nal_len_size,
        } => {
            ctx.container = Container::Mp4;
            let (s, e) = mp4::first_sample(&ctx, stbl, stbl_end)?;
            (s, e, codec, nal_len_size)
        }
    };
    let mut cfg = VideoCfg::default();
    match codec {
        Codec::H264 | Codec::Hevc => walk_nals(&ctx, start, end, nls, codec, &mut cfg)?,
        Codec::Av1 => {
            let b = ctx.bytes(start, (end - start).min(MAX_NAL))?;
            av1::scan_obus(b, &mut cfg);
        }
        _ => {
            return Err(PErr::Fail(ProbeError::Unsupported(format!(
                "frame SEI for {codec:?}"
            ))))
        }
    }
    Ok(FrameSei {
        mastering: cfg.mastering,
        cll: cfg.cll,
        hdr10plus: cfg.hdr10plus,
        dovi_rpu: cfg.dovi_rpu,
        color: cfg.color,
        reads: 0,
        bytes_read: 0,
    })
}

/// Hop over the length-prefixed NAL units of one access unit: non-VCL units
/// are read and scanned, slice data is skipped by its length (never read),
/// so a trailing Dolby Vision RPU costs at most one extra small read.
fn walk_nals(
    ctx: &Ctx<'_>,
    start: u64,
    end: u64,
    nls: usize,
    codec: Codec,
    cfg: &mut VideoCfg,
) -> PResult<()> {
    let hdr_len = if codec == Codec::Hevc { 2 } else { 1 };
    let mut pos = start;
    for _ in 0..256 {
        if pos + nls as u64 + hdr_len > end {
            break;
        }
        let lb = ctx.bytes(pos, nls as u64)?;
        let len = lb.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b));
        let nal_start = pos + nls as u64;
        let nal_end = nal_start.saturating_add(len);
        if len < hdr_len || nal_end > end {
            break;
        }
        let head = ctx.bytes(nal_start, hdr_len)?;
        let vcl = if codec == Codec::Hevc {
            hevc::is_vcl(head)
        } else {
            h264::is_vcl(head)
        };
        if !vcl {
            let nal = ctx.bytes(nal_start, len.min(MAX_NAL))?;
            if codec == Codec::Hevc {
                hevc::scan_nals(&[nal], cfg);
            } else {
                h264::scan_nals(&[nal], cfg);
            }
        } else if codec == Codec::H264 {
            // H.264 has no RPU; SEI always precedes the first slice.
            break;
        }
        pos = nal_end;
    }
    Ok(())
}
