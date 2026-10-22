//! The async playback engine: what `flux-server`'s `/media/{plan}/…` routes call.
//!
//! - [`HlsEngine::prepare`] builds (or loads the cached) playback index of a
//!   file and plans its segments.
//! - Playlists are rendered from the plan without touching media bytes.
//! - Segments are pure functions of (asset, plan recipe, rendition, n): any
//!   segment can be requested cold, in any order (a seek is a GET). Remuxed
//!   and audio-transcoded bytes are identical however they were requested;
//!   video transcodes are equivalent but not byte-identical (see
//!   [`HlsEngine::is_deterministic`]).
//! - Blocking reads run on Tokio's blocking pool behind a read-admission
//!   semaphore; audio transcodes run in supervised ffmpeg children.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use flux_core::{
    AudioOp, ClientCaps, Codec, DeliveryFormat, PlaybackPlan, ProbeResult, SubtitleFile, SubtitleOp,
    SubtitlePlan, TrackKind, VideoOp,
};
use tokio::sync::{OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::asset::{Asset, DEFAULT_INLINE_LIMIT};
use crate::burn::BurnIn;
use crate::cache::LruCache;
use crate::error::{HlsError, Result};
use crate::index::PlaybackIndex;
use crate::planner::{self, PlanPrefs};
use crate::playlist::{self, AudioKey, AudioRecipe, HlsResource, Rendition};
use crate::subtitle;
use crate::transcode::{AudioTarget, AudioTranscoder, TranscodeStats};
use crate::vtranscode::{VideoTarget, VideoTranscodeStats, VideoTranscoder};
use crate::SegmentBody;

/// Engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Target segment duration (segments cut at the first keyframe at/after it).
    pub target_segment: Duration,
    /// Where serialized playback indexes are cached (`None` = memory only).
    pub index_cache_dir: Option<PathBuf>,
    /// ffmpeg binary for audio and video transcodes (`$FLUX_FFMPEG`, else
    /// `PATH`).
    pub ffmpeg: PathBuf,
    /// ffprobe binary, for indexing containers without an index (TS/AVI)
    /// (`$FLUX_FFPROBE`, else next to ffmpeg, else `PATH`).
    pub ffprobe: PathBuf,
    /// The H.264 encoder of video transcodes; `None` detects it
    /// ([`crate::encoders`]). Default: `$FLUX_VIDEO_ENCODER`.
    pub video_encoder: Option<crate::encoders::VideoEncoder>,
    /// The VAAPI render node (`$FLUX_VAAPI_DEVICE`, else
    /// `/dev/dri/renderD128`).
    pub vaapi_device: PathBuf,
    /// Concurrent ffmpeg processes for on-demand transcodes.
    pub max_transcodes: usize,
    /// Byte budget of the transcoded-audio segment cache.
    pub audio_cache_bytes: usize,
    /// Transcoded audio segments prefetched after each request.
    pub audio_lookahead: u32,
    /// ffmpeg processes per on-demand transcoded audio segment.
    pub audio_parallel_parts: u32,
    /// Concurrent encoder runs (sessions) for video transcodes, requests
    /// and prefetch together; 0 picks by the detected encoder
    /// ([`crate::vtranscode::default_max_runs`]: 6 on hardware, 3 for
    /// libx264).
    pub max_video_transcodes: usize,
    /// Byte budget of the transcoded-video segment cache.
    pub video_cache_bytes: usize,
    /// Transcoded video segments kept encoded ahead of the last request.
    pub video_lookahead: u32,
    /// Concurrent blocking segment reads (per-device I/O admission).
    pub max_concurrent_reads: usize,
    /// Segments whose source range fits are served from the parse buffer.
    pub inline_limit: usize,
    /// Signal parameter sets in the sample entry (`avc1`/`hvc1`) and drop
    /// the in-band copies of MPEG-TS video ([`Asset::strip_parameter_sets`];
    /// Matroska/MP4 samples keep theirs). `false`: `avc3`/`hev1`.
    pub strip_parameter_sets: bool,
    /// Byte budget of prepared assets kept in memory (index, expanded MP4
    /// sample tables, segment plan; LRU). An asset in use by a request stays
    /// alive regardless; eviction only means the next `prepare` rebuilds it
    /// (or reloads the cached index).
    pub asset_cache_bytes: usize,
    /// Byte budget of extracted WebVTT subtitle tracks (LRU).
    pub subtitle_cache_bytes: usize,
    /// Target duration of transcoded video segments ("pieces", cut on the
    /// frame grid inside each source keyframe span): short pieces mean a
    /// short wait for the first frame and after a seek.
    pub video_segment_target: Duration,
    /// Transcoded video segments encoded by one ffmpeg run (the encoder
    /// set-up is paid once per run).
    pub video_run_segments: u32,
    /// Byte budget of segments produced ahead by [`HlsEngine::warm`] and kept
    /// for the request that follows (LRU).
    pub warm_cache_bytes: usize,
    /// How long an unused source file keeps its descriptor open
    /// (process-wide, see [`crate::source::idle_timeout`]).
    pub source_idle_timeout: Duration,
    /// Limits on the ffprobe scan that indexes containers flux-hls can't
    /// read itself (AVI, rejected MPEG-TS): a stalled or endless scan is
    /// killed (review ROB-1).
    pub index_scan_limits: crate::index::external::ScanLimits,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let ffmpeg = crate::ffmpeg_tools::default_ffmpeg();
        EngineConfig {
            target_segment: Duration::from_secs(6),
            index_cache_dir: None,
            ffprobe: crate::ffmpeg_tools::default_ffprobe(&ffmpeg),
            ffmpeg,
            video_encoder: crate::encoders::VideoEncoder::from_env(),
            vaapi_device: crate::encoders::vaapi_device_from_env(),
            max_transcodes: 4,
            audio_cache_bytes: 256 << 20,
            audio_lookahead: 2,
            audio_parallel_parts: 2,
            max_video_transcodes: 0,
            video_cache_bytes: 512 << 20,
            video_lookahead: 3,
            max_concurrent_reads: 8,
            inline_limit: DEFAULT_INLINE_LIMIT,
            strip_parameter_sets: true,
            asset_cache_bytes: 256 << 20,
            subtitle_cache_bytes: 32 << 20,
            video_segment_target: Duration::from_secs(2),
            video_run_segments: 4,
            warm_cache_bytes: 64 << 20,
            source_idle_timeout: crate::source::DEFAULT_IDLE_TIMEOUT,
            index_scan_limits: Default::default(),
        }
    }
}

/// A WebVTT document for a response ([`HlsEngine::webvtt_body`]).
pub enum WebVttBody {
    /// Extracted before: the whole document (send it with a length).
    Whole(Arc<str>),
    /// Being extracted: the header, then each cue as it is read (send it
    /// chunked). An `Err` item means the extraction failed part-way.
    Streaming(std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>),
}

impl std::fmt::Debug for WebVttBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebVttBody::Whole(s) => f.debug_tuple("Whole").field(&s.len()).finish(),
            WebVttBody::Streaming(_) => f.write_str("Streaming"),
        }
    }
}

/// A response for one HLS resource.
#[derive(Debug)]
pub struct HlsResponse {
    pub content_type: &'static str,
    /// Segments and init are immutable for a given recipe; playlists are not cached long.
    pub cache_control: &'static str,
    pub body: SegmentBody,
}

/// Server-side segment timing (production time, excluding network).
#[derive(Debug, Clone, Default)]
pub struct EngineStats {
    pub segments: u64,
    pub segment_bytes: u64,
    /// Recent production times in ms (ring of the last 4096).
    pub recent_ms: Vec<f32>,
}

impl EngineStats {
    /// (p50, p99) of recent segment production times, ms.
    pub fn percentiles(&self) -> Option<(f32, f32)> {
        if self.recent_ms.is_empty() {
            return None;
        }
        let mut v = self.recent_ms.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        Some((p(0.5), p(0.99)))
    }
}

/// One-time-initialized shared value (concurrent callers share one build).
type Shared<T> = Arc<OnceCell<T>>;
/// WebVTT per (file identity, subtitle track): the key includes size and
/// mtime like every other cache here, so a file replaced at the same path
/// never gets the old file's cues (PB-3).
type SubtitleCache = Mutex<LruCache<(String, u32), Shared<Arc<str>>>>;
/// A text subtitle track's codec, cache key and cache cell.
type SubtitleSlot = (Codec, (String, u32), Shared<Arc<str>>);
/// A segment produced ahead by `warm`: (asset key, rendition, n).
type WarmKey = (String, Rendition, u32);

/// Memory used by the engine's caches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUsage {
    /// (entries, bytes, evictions so far) of prepared assets.
    pub assets: (usize, usize, u64),
    /// … of WebVTT tracks.
    pub subtitles: (usize, usize, u64),
    /// … of transcoded audio segments.
    pub audio_segments: (usize, usize, u64),
    /// … of transcoded video segments.
    pub video_segments: (usize, usize, u64),
    /// … of segments produced by `warm` for the request that follows.
    pub warmed: (usize, usize, u64),
}

impl CacheUsage {
    /// Bytes held by all the caches.
    pub fn bytes(&self) -> usize {
        self.assets.1 + self.subtitles.1 + self.audio_segments.1 + self.video_segments.1 + self.warmed.1
    }
}

struct Inner {
    cfg: EngineConfig,
    assets: Mutex<LruCache<PathBuf, Shared<Arc<Asset>>>>,
    subs: SubtitleCache,
    warmed: Mutex<LruCache<WarmKey, Shared<Bytes>>>,
    reads: Arc<Semaphore>,
    audio: Arc<AudioTranscoder>,
    video: Arc<VideoTranscoder>,
    stats: Mutex<EngineStats>,
    cancel: CancellationToken,
}

/// The playback engine. Cheap to clone (shared state behind an `Arc`).
#[derive(Clone)]
pub struct HlsEngine {
    inner: Arc<Inner>,
}

fn asset_key(a: &Asset) -> String {
    let s = &a.index.source;
    format!("{}:{}:{}", s.path.display(), s.size, s.mtime_ns)
}

impl HlsEngine {
    /// Creates an engine.
    pub fn new(cfg: EngineConfig) -> Self {
        let cancel = CancellationToken::new();
        let mut audio = AudioTranscoder::new(
            cfg.ffmpeg.clone(),
            cfg.max_transcodes,
            cfg.audio_cache_bytes,
            cancel.clone(),
        );
        audio.lookahead = cfg.audio_lookahead;
        audio.parallel_parts = cfg.audio_parallel_parts;
        let mut video = VideoTranscoder::new(
            cfg.ffmpeg.clone(),
            cfg.max_video_transcodes,
            cfg.video_cache_bytes,
            cancel.clone(),
        );
        video.lookahead = cfg.video_lookahead;
        video.ffprobe = cfg.ffprobe.clone();
        video.detect.forced = cfg.video_encoder;
        video.detect.vaapi_device = cfg.vaapi_device.clone();
        video.piece_ns = cfg.video_segment_target.as_nanos() as i64;
        video.run_pieces = cfg.video_run_segments.max(1);
        crate::source::set_idle_timeout(cfg.source_idle_timeout);
        HlsEngine {
            inner: Arc::new(Inner {
                reads: Arc::new(Semaphore::new(cfg.max_concurrent_reads.max(1))),
                assets: Mutex::new(LruCache::new(cfg.asset_cache_bytes)),
                subs: Mutex::new(LruCache::new(cfg.subtitle_cache_bytes)),
                warmed: Mutex::new(LruCache::new(cfg.warm_cache_bytes)),
                cfg,
                audio: Arc::new(audio),
                video: Arc::new(video),
                stats: Mutex::new(EngineStats::default()),
                cancel,
            }),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &EngineConfig {
        &self.inner.cfg
    }

    /// What ffmpeg can do and the H.264 encoder video transcodes use,
    /// detected on the first call (or the first video transcode) and cached.
    pub async fn video_caps(&self) -> Arc<crate::encoders::FfmpegCaps> {
        self.inner.video.caps().await
    }

    /// [`Self::video_caps`] if detection has run.
    pub fn video_caps_if_known(&self) -> Option<Arc<crate::encoders::FfmpegCaps>> {
        self.inner.video.caps_if_known()
    }

    /// Cancels background work (prefetch, in-flight transcodes) and kills
    /// the ffprobe index scans still running, so no blocking thread keeps
    /// the runtime from shutting down (review ROB-1). Those scans are
    /// process-wide: this ends every engine's.
    pub fn shutdown(&self) {
        self.inner.cancel.cancel();
        crate::index::external::kill_running();
    }

    fn cache_path(&self, path: &Path) -> Option<PathBuf> {
        let dir = self.inner.cfg.index_cache_dir.as_ref()?;
        let mut h = DefaultHasher::new();
        path.hash(&mut h);
        Some(dir.join(format!("{:016x}.json", h.finish())))
    }

    fn target_ns(&self) -> i64 {
        self.inner.cfg.target_segment.as_nanos() as i64
    }

    fn finish_asset(&self, mut a: Asset) -> Arc<Asset> {
        a.inline_limit = self.inner.cfg.inline_limit;
        a.strip_parameter_sets = self.inner.cfg.strip_parameter_sets;
        Arc::new(a)
    }

    /// Prepares a file for playback: loads the cached index if it matches the
    /// file, otherwise builds it (and caches it). Concurrent calls for the
    /// same file share one build.
    pub async fn prepare(&self, path: impl AsRef<Path>) -> Result<Arc<Asset>> {
        let path = path.as_ref().to_path_buf();
        // A cached asset is reused only while the file is unchanged (size + mtime).
        let current = crate::source::Source::open(&path).map(|s| s.id().clone()).ok();
        let cell = {
            let mut m = self.inner.assets.lock().map_err(|_| HlsError::Cancelled)?;
            let existing = m.get(&path);
            let stale = existing
                .as_ref()
                .and_then(|c| c.get())
                .is_some_and(|a| current.as_ref() != Some(&a.index.source));
            match existing {
                Some(c) if !stale => c,
                _ => {
                    // Weighed once built (below); in-flight entries weigh nothing.
                    let c: Shared<Arc<Asset>> = Arc::new(OnceCell::new());
                    m.put(path.clone(), c.clone(), 0);
                    c
                }
            }
        };
        let key = path.clone();
        let this = self.clone();
        let asset = cell
            .get_or_try_init(|| async move {
                let ffprobe = this.inner.cfg.ffprobe.clone();
                let limits = this.inner.cfg.index_scan_limits;
                let cache = this.cache_path(&path);
                let target = this.target_ns();
                let built = tokio::task::spawn_blocking(move || -> Result<Asset> {
                    if let Some(c) = &cache {
                        if let Ok(b) = std::fs::read(c) {
                            if let Ok(idx) = PlaybackIndex::from_json(&b) {
                                if let Ok(a) = Asset::with_index(idx, target) {
                                    return Ok(a);
                                }
                            }
                        }
                    }
                    let a = Asset::open_with_limits(&path, target, Some(&ffprobe), &limits)?;
                    if let Some(c) = &cache {
                        if let Some(dir) = c.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let tmp = c.with_extension("tmp");
                        if std::fs::write(&tmp, a.index.to_json()?).is_ok() {
                            let _ = std::fs::rename(&tmp, c);
                        }
                    }
                    Ok(a)
                })
                .await
                .map_err(|e| HlsError::Io(std::io::Error::other(e)))??;
                Ok::<_, HlsError>(this.finish_asset(built))
            })
            .await;
        let asset = match asset {
            Ok(a) => a.clone(),
            Err(e) => {
                // Don't keep a failed build around (the cell stays empty).
                if let Ok(mut m) = self.inner.assets.lock() {
                    m.remove(&key);
                }
                return Err(e);
            }
        };
        if let Ok(mut m) = self.inner.assets.lock() {
            m.reweigh(&key, asset.approx_bytes());
        }
        Ok(asset)
    }

    /// Prepares from an index the caller cached (e.g. in the catalog).
    pub async fn prepare_with_index(&self, index: PlaybackIndex) -> Result<Arc<Asset>> {
        let target = self.target_ns();
        let a = tokio::task::spawn_blocking(move || Asset::with_index(index, target))
            .await
            .map_err(|e| HlsError::Io(std::io::Error::other(e)))??;
        Ok(self.finish_asset(a))
    }

    /// The asset as a probe result (for the planner).
    pub fn probe(&self, asset: &Asset) -> ProbeResult {
        asset.index.to_probe_result()
    }

    /// Plans playback of `asset` for a client. Removes renditions the engine
    /// cannot produce (e.g. copy of an audio codec with no configuration).
    ///
    /// It also applies what only the index knows ([`planner::IndexFacts`]):
    /// open-GOP video on a client that seeks only to IDR pictures
    /// ([`planner::Quirks::idr_random_access_only`]), H.264 Baseline video
    /// that uses slice groups, arbitrary slice order or redundant pictures.
    pub fn plan(&self, asset: &Asset, caps: &ClientCaps, prefs: &PlanPrefs) -> PlaybackPlan {
        let probe = asset.index.to_probe_result();
        let facts = asset
            .index
            .video_track()
            .map(|v| planner::IndexFacts {
                open_gop: v.open_gop,
                baseline_tools: v.extra.baseline_tools,
            })
            .unwrap_or_default();
        self.finalize_plan(asset, planner::plan_with_index_facts(&probe, caps, prefs, facts))
    }

    /// Adapts a plan made elsewhere (e.g. `planner::plan` on flux-probe's
    /// `ProbeResult`) to what this engine can produce for `asset`: copies it
    /// cannot package become stereo AAC, unproducible renditions are dropped,
    /// duplicates removed.
    pub fn finalize_plan(&self, asset: &Asset, plan: PlaybackPlan) -> PlaybackPlan {
        // A copy the engine cannot package (e.g. a TS/AVI source, or a codec
        // with no fMP4 sample entry) becomes stereo AAC when ffmpeg can read it.
        //
        // `VideoOp::Transcode::hardware` stays the planner's "hardware
        // first": the plan (and its id) must not depend on when encoder
        // detection ran. `video_caps` says which encoder is used.
        let mut plan = planner::finalize_with(
            plan,
            |track| asset.audio_copy_entry(track).is_ok(),
            |track| crate::transcode::can_transcode(asset, track),
        );
        // WebVTT only of tracks whose cues the engine reads (files are read
        // whole when asked for): never offer a rendition that can't be served.
        plan.subtitles.retain(|s| {
            s.op != SubtitleOp::WebVtt || s.file.is_some() || asset.text_subtitle_readable(s.track)
        });
        plan
    }

    /// The subtitle `plan` burns into its video, if any: the plan's
    /// `BurnIn` entry for `VideoOp::Transcode::burn_subtitle`, a picture
    /// format (a file of one, or such a track of the asset).
    fn burn_in(asset: &Asset, plan: &PlaybackPlan) -> Result<Option<BurnIn>> {
        let VideoOp::Transcode {
            burn_subtitle: Some(track),
            ..
        } = &plan.video
        else {
            return Ok(None);
        };
        let entry = plan
            .subtitles
            .iter()
            .find(|s| s.track == *track && s.op == SubtitleOp::BurnIn);
        let unsupported = || HlsError::Unsupported(format!("subtitle {track} can't be drawn into the video"));
        match entry {
            Some(SubtitlePlan { file: Some(f), .. }) if f.codec.is_image_subtitle() => Ok(Some(BurnIn {
                path: f.path.clone().into(),
                stream: 0,
                codec: f.codec,
                embedded: false,
                size: f.size,
                mtime_ns: f.mtime_ns,
            })),
            Some(SubtitlePlan { file: None, .. }) => {
                let t = asset
                    .index
                    .track(*track)
                    .filter(|t| t.kind == TrackKind::Subtitle && t.codec.is_image_subtitle())
                    .ok_or_else(unsupported)?;
                Ok(Some(BurnIn {
                    path: asset.index.source.path.clone(),
                    stream: t.index,
                    codec: t.codec,
                    embedded: true,
                    size: 0,
                    mtime_ns: 0,
                }))
            }
            _ => Err(unsupported()),
        }
    }

    /// The video transcode of `plan` (with the subtitle it burns in), if it
    /// transcodes.
    fn video_target(&self, asset: &Asset, plan: &PlaybackPlan) -> Result<Option<VideoTarget>> {
        let Some(mut t) = VideoTarget::from_op(&plan.video) else {
            return Ok(None);
        };
        if let Some(b) = Self::burn_in(asset, plan)? {
            t.burn = Some(self.inner.video.burn_key(b));
        }
        Ok(Some(t))
    }

    fn check_rendition(&self, asset: &Asset, plan: &PlaybackPlan, r: &Rendition) -> Result<()> {
        if plan.delivery != DeliveryFormat::HlsFmp4 {
            return Err(HlsError::NotFound("plan is not HLS".into()));
        }
        match r {
            Rendition::Video => Self::burn_in(asset, plan).map(|_| ()),
            Rendition::Audio(k) => {
                if plan.audio.iter().any(|a| AudioKey::from_plan(a) == *k) {
                    Ok(())
                } else {
                    Err(HlsError::NotFound(format!(
                        "audio rendition {} not in plan",
                        k.path()
                    )))
                }
            }
            Rendition::Subtitle(t) => {
                let ok = plan.subtitles.iter().any(|s| {
                    s.track == *t
                        && s.op == SubtitleOp::WebVtt
                        && match &s.file {
                            Some(f) => f.codec.is_text_subtitle(),
                            None => asset.text_subtitle_readable(*t),
                        }
                });
                if ok {
                    Ok(())
                } else {
                    Err(HlsError::NotFound(format!("subtitle {t} not in plan")))
                }
            }
        }
    }

    /// I-frame playlists exist for HLS plans that copy indexed video.
    fn check_iframes(&self, asset: &Asset, plan: &PlaybackPlan) -> Result<()> {
        let copy = matches!(plan.video, VideoOp::Copy { .. });
        if plan.delivery == DeliveryFormat::HlsFmp4 && copy && asset.index.remuxable() {
            Ok(())
        } else {
            Err(HlsError::NotFound("no I-frame playlist for this plan".into()))
        }
    }

    /// The multivariant playlist.
    pub fn master_playlist(&self, asset: &Asset, plan: &PlaybackPlan) -> Result<String> {
        if plan.delivery != DeliveryFormat::HlsFmp4 {
            return Err(HlsError::NotFound("plan is not HLS".into()));
        }
        Ok(playlist::master_playlist(asset, plan))
    }

    /// A media playlist.
    pub fn media_playlist(&self, asset: &Asset, plan: &PlaybackPlan, r: &Rendition) -> Result<String> {
        self.check_rendition(asset, plan, r)?;
        Ok(match r {
            Rendition::Subtitle(t) => playlist::subtitle_playlist(asset, *t),
            Rendition::Video if VideoTarget::from_op(&plan.video).is_some() => {
                playlist::pieces_playlist(&self.inner.video.pieces(asset), asset.segments.timescale)
            }
            _ => playlist::media_playlist(asset),
        })
    }

    /// The media segment of a rendition that covers playlist time `t_secs`
    /// (for prefetch URLs and resume positions). Transcoded video has its own
    /// shorter segments, so `v/{n}.m4s` of a transcode plan is not remux
    /// segment n: use this instead of `asset.segments.segment_at`.
    pub fn segment_at(&self, asset: &Asset, plan: &PlaybackPlan, r: &Rendition, t_secs: f64) -> u32 {
        match r {
            Rendition::Video if VideoTarget::from_op(&plan.video).is_some() => {
                let pieces = self.inner.video.pieces(asset);
                let ts = asset.segments.timescale.max(1) as f64;
                let origin = pieces.first().map(|p| p.start_ticks).unwrap_or(0);
                let t = origin + (t_secs * ts) as i64;
                pieces
                    .partition_point(|p| p.end_ticks <= t)
                    .min(pieces.len().saturating_sub(1)) as u32
            }
            _ => asset.segments.segment_at(t_secs) as u32,
        }
    }

    /// Number of media segments of a rendition (transcoded video is cut
    /// into shorter pieces than the remux segments; subtitles have none).
    pub fn segment_count(&self, asset: &Asset, plan: &PlaybackPlan, r: &Rendition) -> usize {
        match r {
            Rendition::Video if VideoTarget::from_op(&plan.video).is_some() => {
                self.inner.video.pieces(asset).len()
            }
            Rendition::Subtitle(_) => 0,
            _ => asset.segments.len(),
        }
    }

    fn target_of(plan: &PlaybackPlan, k: &AudioKey) -> Option<AudioTarget> {
        let a = plan.audio.iter().find(|a| AudioKey::from_plan(a) == *k)?;
        match &a.op {
            AudioOp::Copy => None,
            AudioOp::Transcode {
                codec,
                channels,
                bitrate,
            } => Some(AudioTarget {
                codec: if *codec == Codec::Eac3 {
                    Codec::Eac3
                } else {
                    Codec::Aac
                },
                channels: *channels,
                bitrate: *bitrate as u32,
            }),
        }
    }

    /// The init segment of a rendition.
    pub async fn init_segment(
        &self,
        asset: &Arc<Asset>,
        plan: &PlaybackPlan,
        r: &Rendition,
    ) -> Result<Bytes> {
        self.check_rendition(asset, plan, r)?;
        if let (Rendition::Video, Some(t)) = (r, self.video_target(asset, plan)?) {
            let init = self
                .inner
                .video
                .init_segment(&asset_key(asset), asset.clone(), t)
                .await?;
            return Ok(Bytes::from(init));
        }
        let asset = asset.clone();
        let r = *r;
        let target = match r {
            Rendition::Audio(k) => Self::target_of(plan, &k),
            _ => None,
        };
        let b = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            match r {
                Rendition::Video => asset.video_init(),
                Rendition::Audio(k) => match target {
                    None => asset.audio_copy_init(k.track),
                    Some(t) => AudioTranscoder::init_segment(&asset, k.track, t),
                },
                Rendition::Subtitle(_) => Err(HlsError::NotFound("subtitles have no init segment".into())),
            }
        })
        .await
        .map_err(|e| HlsError::Io(std::io::Error::other(e)))??;
        Ok(Bytes::from(b))
    }

    /// Media segment `n` of a rendition.
    pub async fn media_segment(
        &self,
        asset: &Arc<Asset>,
        plan: &PlaybackPlan,
        r: &Rendition,
        n: u32,
    ) -> Result<SegmentBody> {
        self.check_rendition(asset, plan, r)?;
        let n = n as usize;
        let video_target = self.video_target(asset, plan)?;
        let transcoded_video = matches!(r, Rendition::Video) && video_target.is_some();
        if !transcoded_video && n >= asset.segments.len() {
            return Err(HlsError::NotFound(format!("segment {n}")));
        }
        let t0 = Instant::now();
        let body = match *r {
            Rendition::Video if video_target.is_some() => {
                let t = video_target.ok_or_else(|| HlsError::NotFound("recipe".into()))?;
                self.inner
                    .video
                    .segment(&asset_key(asset), asset.clone(), t, n)
                    .await?
            }
            Rendition::Audio(k) if k.recipe != AudioRecipe::Copy => {
                let target = Self::target_of(plan, &k).ok_or_else(|| HlsError::NotFound("recipe".into()))?;
                self.inner
                    .audio
                    .segment(&asset_key(asset), asset.clone(), k.track, target, n)
                    .await?
            }
            r => {
                // Produced ahead by `warm`: served from memory (or awaited
                // while `warm` is still producing it).
                let key: WarmKey = (asset_key(asset), r, n as u32);
                let warmed = self.inner.warmed.lock().ok().and_then(|mut m| m.get(&key));
                match warmed {
                    Some(cell) => {
                        let b = cell
                            .get_or_try_init(|| self.copy_segment_bytes(asset, r, n))
                            .await?
                            .clone();
                        // Served: the request it was produced for came.
                        // Keeping it would hold up to `warm_cache_bytes`
                        // (a 4K segment is ~45 MB) for a repeat that a
                        // stream from the source serves as well.
                        if let Ok(mut m) = self.inner.warmed.lock() {
                            m.remove(&key);
                        }
                        SegmentBody::from_bytes(b)
                    }
                    None => self.copy_segment(asset, r, n).await?,
                }
            }
        };
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        if let Ok(mut s) = self.inner.stats.lock() {
            s.segments += 1;
            s.segment_bytes += body.len();
            if s.recent_ms.len() >= 4096 {
                s.recent_ms.remove(0);
            }
            s.recent_ms.push(ms);
        }
        Ok(body)
    }

    /// A copied (remuxed) segment, streamed from the source.
    async fn copy_segment(&self, asset: &Arc<Asset>, r: Rendition, n: usize) -> Result<SegmentBody> {
        let _permit = self
            .inner
            .reads
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| HlsError::Cancelled)?;
        let asset = asset.clone();
        tokio::task::spawn_blocking(move || match r {
            Rendition::Video => asset.video_segment(n),
            Rendition::Audio(k) => asset.audio_copy_segment(k.track, n),
            Rendition::Subtitle(_) => Err(HlsError::NotFound("subtitles are one WebVTT file".into())),
        })
        .await
        .map_err(|e| HlsError::Io(std::io::Error::other(e)))?
    }

    /// A copied segment read entirely into memory (what `warm` keeps).
    async fn copy_segment_bytes(&self, asset: &Arc<Asset>, r: Rendition, n: usize) -> Result<Bytes> {
        let body = self.copy_segment(asset, r, n).await?;
        let _permit = self
            .inner
            .reads
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| HlsError::Cancelled)?;
        tokio::task::spawn_blocking(move || body.read_all().map(Bytes::from))
            .await
            .map_err(|e| HlsError::Io(std::io::Error::other(e)))?
            .map_err(HlsError::Io)
    }

    /// Produces copied segment `n` of `r` into the warm cache.
    async fn warm_copy(&self, asset: &Arc<Asset>, r: Rendition, n: usize) -> Result<()> {
        let key: WarmKey = (asset_key(asset), r, n as u32);
        let cell = {
            let mut m = self.inner.warmed.lock().map_err(|_| HlsError::Cancelled)?;
            match m.get(&key) {
                Some(c) => c,
                None => {
                    // Weighed once built; in-flight entries weigh nothing.
                    let c: Shared<Bytes> = Arc::new(OnceCell::new());
                    m.put(key.clone(), c.clone(), 0);
                    c
                }
            }
        };
        let len = cell
            .get_or_try_init(|| self.copy_segment_bytes(asset, r, n))
            .await?
            .len();
        if let Ok(mut m) = self.inner.warmed.lock() {
            m.reweigh(&key, len);
        }
        Ok(())
    }

    /// The renditions [`HlsEngine::warm`] produces: the video and the audio
    /// the player starts with, i.e. every rendition of the plan's first
    /// audio track (e.g. its E-AC-3 and AAC versions on an Apple TV, one of
    /// which the device picks). Other tracks are produced when selected:
    /// warming them cost ~0.3 s of ffmpeg CPU per transcoded rendition per
    /// play, for audio nobody hears (PB-6).
    pub fn warm_renditions(plan: &PlaybackPlan) -> Vec<Rendition> {
        let mut out = vec![Rendition::Video];
        if let Some(first) = plan.audio.first() {
            out.extend(
                plan.audio
                    .iter()
                    .filter(|a| a.track == first.track)
                    .map(|a| Rendition::Audio(AudioKey::from_plan(a))),
            );
        }
        out
    }

    /// Predictive prefetch (research/08 idea E): starts producing the
    /// segments of the video and the default audio
    /// ([`HlsEngine::warm_renditions`]) that cover playlist time `t_secs`
    /// ([`HlsEngine::segment_at`]), in the background. Call it when a play
    /// request is planned (or a detail page opens), so that the first
    /// segments are in flight or ready before the player asks: transcoded
    /// segments land in the transcoders' caches, remuxed ones in a small
    /// warm cache that the following request is served from
    /// ([`EngineConfig::warm_cache_bytes`]) instead of being produced twice.
    /// Errors are logged, not returned.
    pub fn warm(&self, asset: &Arc<Asset>, plan: &PlaybackPlan, t_secs: f64) {
        if plan.delivery != DeliveryFormat::HlsFmp4 {
            return;
        }
        let start = asset.segments.segment_at(t_secs) as u32;
        for r in Self::warm_renditions(plan) {
            if self.check_rendition(asset, plan, &r).is_err() {
                continue;
            }
            let this = self.clone();
            let asset = asset.clone();
            let plan = plan.clone();
            let cancel = self.inner.cancel.clone();
            tokio::spawn(async move {
                let work = async {
                    match (r, this.video_target(&asset, &plan)?) {
                        (Rendition::Video, Some(t)) => {
                            // Start the encoder run at the piece the player
                            // asks for first: the one covering `t_secs`, which
                            // may start in the remux segment before (a
                            // converted video's pieces straddle keyframes).
                            // The init waits for its avcC.
                            let p = this.segment_at(&asset, &plan, &Rendition::Video, t_secs);
                            this.inner
                                .video
                                .warm(&asset_key(&asset), asset.clone(), t, p as usize);
                            Ok(())
                        }
                        (Rendition::Audio(k), _) if k.recipe != AudioRecipe::Copy => {
                            this.media_segment(&asset, &plan, &r, start).await.map(|_| ())
                        }
                        _ if (start as usize) < asset.segments.len() => {
                            this.warm_copy(&asset, r, start as usize).await
                        }
                        _ => Ok(()),
                    }
                };
                tokio::select! {
                    res = work => if let Err(e) = res { tracing::debug!(%e, "warm failed") },
                    _ = cancel.cancelled() => {}
                }
            });
        }
    }

    /// The cache cell of WebVTT document `key`.
    fn subtitle_slot(&self, key: &(String, u32)) -> Result<Shared<Arc<str>>> {
        let mut m = self.inner.subs.lock().map_err(|_| HlsError::Cancelled)?;
        Ok(match m.get(key) {
            Some(c) => c,
            None => {
                let c: Shared<Arc<str>> = Arc::new(OnceCell::new());
                m.put(key.clone(), c.clone(), 0);
                c
            }
        })
    }

    /// The codec of text subtitle track `track`, its cache key and cell.
    /// Fails for a track whose cues the engine can't read
    /// ([`Asset::text_subtitle_readable`]), before anything is sent.
    fn subtitle_cell(&self, asset: &Asset, track: u32) -> Result<SubtitleSlot> {
        let t = asset
            .index
            .track(track)
            .filter(|t| t.kind == TrackKind::Subtitle && t.codec.is_text_subtitle());
        let codec = t
            .map(|t| t.codec)
            .ok_or_else(|| HlsError::NotFound(format!("text subtitle track {track}")))?;
        if !asset.text_subtitle_readable(track) {
            return Err(HlsError::Unsupported(format!(
                "{codec} subtitles of this file can't be converted to WebVTT"
            )));
        }
        let key = (asset_key(asset), track);
        let cell = self.subtitle_slot(&key)?;
        Ok((codec, key, cell))
    }

    /// WebVTT of a text subtitle track (extracted once per file, then cached).
    pub async fn webvtt(&self, asset: &Arc<Asset>, track: u32) -> Result<Arc<str>> {
        let (codec, key, cell) = self.subtitle_cell(asset, track)?;
        let weigh_key = key.clone();
        let asset = asset.clone();
        let reads = self.inner.reads.clone();
        let vtt = cell
            .get_or_try_init(|| async move {
                // The extraction is a blocking read like any segment's.
                let _permit = reads.acquire_owned().await.map_err(|_| HlsError::Cancelled)?;
                tokio::task::spawn_blocking(move || -> Result<Arc<str>> {
                    let cues = asset.subtitle_blocks(track)?;
                    Ok(Arc::from(subtitle::to_webvtt(
                        codec,
                        &cues,
                        asset.index.presentation_offset_ns,
                    )))
                })
                .await
                .map_err(|e| HlsError::Io(std::io::Error::other(e)))?
            })
            .await?
            .clone();
        if let Ok(mut m) = self.inner.subs.lock() {
            m.reweigh(&weigh_key, vtt.len() + 64);
        }
        Ok(vtt)
    }

    /// WebVTT of a text subtitle **file** beside `asset`'s video (a
    /// sidecar: SubRip, ASS/SSA or WebVTT), read and converted whole, then
    /// cached. Its cues are on the video's timeline (0 = the video's
    /// start), like a track's. A file that has changed size since the plan
    /// was made is refused (start playback again); a file larger than
    /// 32 MiB is not a subtitle file.
    pub async fn file_webvtt(&self, asset: &Arc<Asset>, file: &SubtitleFile) -> Result<Arc<str>> {
        const MAX_BYTES: u64 = 32 << 20;
        if !file.codec.is_text_subtitle() {
            return Err(HlsError::NotFound(format!("text subtitle {}", file.track)));
        }
        let key = (
            format!(
                "{}|{}:{}:{}",
                asset_key(asset),
                file.path,
                file.size,
                file.mtime_ns
            ),
            file.track,
        );
        let cell = self.subtitle_slot(&key)?;
        let (asset, file) = (asset.clone(), file.clone());
        let vtt = cell
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || -> Result<Arc<str>> {
                    let len = std::fs::metadata(&file.path)?.len();
                    if len != file.size {
                        return Err(HlsError::NotFound(
                            "the subtitle file changed since playback started; start it again".into(),
                        ));
                    }
                    if len > MAX_BYTES {
                        return Err(HlsError::Unsupported(format!("a {len}-byte subtitle file")));
                    }
                    let bytes = std::fs::read(&file.path)?;
                    let cues = subtitle::parse_file(file.codec, &subtitle::decode_text(&bytes));
                    Ok(Arc::from(subtitle::to_webvtt(
                        file.codec,
                        &cues,
                        asset.index.presentation_offset_ns,
                    )))
                })
                .await
                .map_err(|e| HlsError::Io(std::io::Error::other(e)))?
            })
            .await?
            .clone();
        if let Ok(mut m) = self.inner.subs.lock() {
            m.reweigh(&key, vtt.len() + 64);
        }
        Ok(vtt)
    }

    /// WebVTT of subtitle rendition `track` of `plan` for a response: a
    /// subtitle file of the plan ([`HlsEngine::file_webvtt`]), else a text
    /// track of `asset` ([`HlsEngine::webvtt_body`]). 404 when an HLS plan
    /// has no such rendition; a direct-play plan serves its subtitle files
    /// and any readable text track (the `<track>` elements of the page).
    pub async fn plan_webvtt_body(
        &self,
        asset: &Arc<Asset>,
        plan: &PlaybackPlan,
        track: u32,
    ) -> Result<WebVttBody> {
        if !plan.is_direct() {
            self.check_rendition(asset, plan, &Rendition::Subtitle(track))?;
        }
        let file = plan
            .subtitles
            .iter()
            .find(|s| s.track == track && s.op == SubtitleOp::WebVtt)
            .and_then(|s| s.file.as_ref());
        match file {
            Some(f) => Ok(WebVttBody::Whole(self.file_webvtt(asset, f).await?)),
            None => self.webvtt_body(asset, track).await,
        }
    }

    /// WebVTT of a text subtitle track for a response: the cached document
    /// when the track was extracted before, else the cues **as they are
    /// read** (review PB-7): the response starts with the first cue, and
    /// each cue follows right after its block is read, so the first cue
    /// doesn't wait for a walk of a Cue-less file. A completed extraction is
    /// cached (later requests get [`WebVttBody::Whole`], byte for byte the
    /// same); one the client abandons stops reading.
    ///
    /// Nothing is sent until the track has been read up to its first cue
    /// (or its end): a track that can't be read at all is an error, never a
    /// `200` that breaks off (only a read failing later, a file cut short
    /// mid-way, still can).
    pub async fn webvtt_body(&self, asset: &Arc<Asset>, track: u32) -> Result<WebVttBody> {
        let (codec, key, cell) = self.subtitle_cell(asset, track)?;
        if let Some(v) = cell.get() {
            return Ok(WebVttBody::Whole(v.clone()));
        }
        let permit = self
            .inner
            .reads
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| HlsError::Cancelled)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(16);
        let asset = asset.clone();
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let header = subtitle::WebVttWriter::header(asset.index.presentation_offset_ns);
            let mut doc = header.clone();
            // Sent with the first cue (or alone at the end).
            let mut pending = Some(header);
            let mut w = subtitle::WebVttWriter::new(codec);
            let mut open = true;
            let res = asset.for_each_subtitle_block(track, &mut |(start, end, payload)| {
                if let Some(cue) = w.cue(start, end, &payload) {
                    doc.push_str(&cue);
                    let chunk = match pending.take() {
                        Some(h) => h + &cue,
                        None => cue,
                    };
                    open = tx.blocking_send(Ok(Bytes::from(chunk))).is_ok();
                }
                open
            });
            match res {
                Ok(_) if open => {
                    if let Some(h) = pending.take() {
                        let _ = tx.blocking_send(Ok(Bytes::from(h)));
                    }
                    let doc: Arc<str> = Arc::from(doc);
                    let len = doc.len();
                    if cell.set(doc).is_ok() {
                        if let Ok(mut m) = this.inner.subs.lock() {
                            m.reweigh(&key, len + 64);
                        }
                    }
                }
                Ok(_) => {} // the client went away: nothing complete to keep
                Err(e) => {
                    let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                }
            }
        });
        let first = match rx.recv().await {
            Some(Ok(b)) => b,
            Some(Err(e)) => return Err(HlsError::Io(e)),
            None => return Err(HlsError::Cancelled),
        };
        let rest =
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
        Ok(WebVttBody::Streaming(Box::pin(futures::StreamExt::chain(
            futures::stream::once(async move { Ok(first) }),
            rest,
        ))))
    }

    /// Serves any resource of a plan (router helper for `flux-server`).
    pub async fn serve(
        &self,
        asset: &Arc<Asset>,
        plan: &PlaybackPlan,
        res: HlsResource,
    ) -> Result<HlsResponse> {
        const PLAYLIST: &str = "application/vnd.apple.mpegurl";
        const SHORT: &str = "public, max-age=10";
        const IMMUTABLE: &str = "public, max-age=31536000, immutable";
        Ok(match res {
            HlsResource::Master => HlsResponse {
                content_type: PLAYLIST,
                cache_control: SHORT,
                body: SegmentBody::from_bytes(self.master_playlist(asset, plan)?),
            },
            HlsResource::Playlist(r) => HlsResponse {
                content_type: PLAYLIST,
                cache_control: SHORT,
                body: SegmentBody::from_bytes(self.media_playlist(asset, plan, &r)?),
            },
            HlsResource::Init(r) => HlsResponse {
                content_type: if matches!(r, Rendition::Video) {
                    "video/mp4"
                } else {
                    "audio/mp4"
                },
                cache_control: IMMUTABLE,
                body: SegmentBody::from_bytes(self.init_segment(asset, plan, &r).await?),
            },
            HlsResource::Segment(r, n) => HlsResponse {
                content_type: if matches!(r, Rendition::Video) {
                    "video/iso.segment"
                } else {
                    "audio/iso.segment"
                },
                cache_control: IMMUTABLE,
                body: self.media_segment(asset, plan, &r, n).await?,
            },
            HlsResource::IFramePlaylist => {
                self.check_iframes(asset, plan)?;
                HlsResponse {
                    content_type: PLAYLIST,
                    cache_control: SHORT,
                    body: SegmentBody::from_bytes(playlist::iframe_playlist(asset)),
                }
            }
            HlsResource::IFrame(n) => {
                self.check_iframes(asset, plan)?;
                let asset = asset.clone();
                let body = tokio::task::spawn_blocking(move || asset.keyframe_segment(n as usize))
                    .await
                    .map_err(|e| HlsError::Io(std::io::Error::other(e)))??;
                HlsResponse {
                    content_type: "video/iso.segment",
                    cache_control: IMMUTABLE,
                    body,
                }
            }
            HlsResource::WebVtt(t) => {
                self.check_rendition(asset, plan, &Rendition::Subtitle(t))?;
                let file = plan
                    .subtitles
                    .iter()
                    .find(|s| s.track == t && s.op == SubtitleOp::WebVtt)
                    .and_then(|s| s.file.as_ref());
                let vtt = match file {
                    Some(f) => self.file_webvtt(asset, f).await?,
                    None => self.webvtt(asset, t).await?,
                };
                HlsResponse {
                    content_type: "text/vtt; charset=utf-8",
                    cache_control: IMMUTABLE,
                    body: SegmentBody::from_bytes(Bytes::from(vtt.to_string())),
                }
            }
        })
    }

    /// Segment timing statistics.
    pub fn stats(&self) -> EngineStats {
        self.inner.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Video transcoder statistics.
    pub fn video_transcode_stats(&self) -> VideoTranscodeStats {
        self.inner.video.stats()
    }

    /// Audio transcoder statistics.
    pub fn transcode_stats(&self) -> TranscodeStats {
        self.inner.audio.stats()
    }

    /// Memory held by the engine's caches (all LRU, byte-budgeted by
    /// [`EngineConfig`]).
    pub fn cache_usage(&self) -> CacheUsage {
        CacheUsage {
            assets: self.inner.assets.lock().map(|m| m.usage()).unwrap_or_default(),
            subtitles: self.inner.subs.lock().map(|m| m.usage()).unwrap_or_default(),
            audio_segments: self.inner.audio.cache_usage(),
            video_segments: self.inner.video.cache_usage(),
            warmed: self.inner.warmed.lock().map(|m| m.usage()).unwrap_or_default(),
        }
    }

    /// Drops every cached segment, WebVTT track and prepared asset, and
    /// returns what they held. For a server whose playback went idle: the
    /// caches keep up to their budgets (about 1.1 GB by default) until
    /// something evicts them, which on an idle server never happens.
    ///
    /// Nothing breaks: a request in flight keeps the asset it holds (an
    /// `Arc`), a transcode still running puts its pieces back, and the next
    /// request rebuilds what it needs (an index from the catalog, a piece by
    /// encoding it again). It costs that rebuild, so call it only after
    /// minutes without media requests: during playback the caches are what
    /// make seeks back and prefetched pieces instant.
    pub fn trim(&self) -> CacheUsage {
        let before = self.cache_usage();
        if let Ok(mut m) = self.inner.assets.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.inner.subs.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.inner.warmed.lock() {
            m.clear();
        }
        self.inner.audio.clear_cache();
        self.inner.video.clear_cache();
        before
    }

    /// True when every response for `res` under `plan` has the same bytes,
    /// however and in whatever order it was requested, so that a strong
    /// validator (`ETag: "…"`, `If-Range`) is correct for it.
    ///
    /// - Playlists, remuxed segments, init segments of copies and of audio
    ///   transcodes, and WebVTT are pure functions of (asset, recipe, n).
    /// - Transcoded audio is too: on-demand and prefetched segments are
    ///   encoded with the same partition into parallel parts by the same
    ///   deterministic encoder (PB-11).
    /// - Transcoded video is not: a piece's bytes depend on the encoder run
    ///   that produced it (rate control carries over from the run's earlier
    ///   pieces, whichever encoder it is, and hardware encoders are not
    ///   bit-exact across sessions). Its timing, frame count and IDR
    ///   positions are exact for every encoder, so the pieces are
    ///   interchangeable, but a server must label them with a weak
    ///   validator (`ETag: W/"…"`), which `If-Range` never matches.
    pub fn is_deterministic(plan: &PlaybackPlan, res: &HlsResource) -> bool {
        let video_transcode = VideoTarget::from_op(&plan.video).is_some();
        !matches!(
            res,
            HlsResource::Init(Rendition::Video) | HlsResource::Segment(Rendition::Video, _)
                if video_transcode
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets;

    /// A feature clip of the corpus (`$FLUX_TEST_MEDIA/features`).
    fn media(name: &str) -> Option<PathBuf> {
        flux_test_media::media(&format!("features/{name}"))
    }

    fn ffmpeg() -> bool {
        EngineConfig::default().ffmpeg.is_file()
    }

    async fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// PB-3: a film replaced at the same path (new size/mtime, other subtitle
    /// timing) gets its own WebVTT, not the cached cues of the old file.
    #[tokio::test]
    async fn webvtt_cache_follows_the_file_identity() {
        let Some(src) = media("h264_multi_audio_subs.mkv") else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("flux-pb3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Movie (2020).mkv");
        let _ = std::fs::remove_file(&path);
        flux_test_media::clone_file(&src, &path).unwrap();
        let engine = HlsEngine::new(EngineConfig::default());
        let a1 = engine
            .prepare_with_index(PlaybackIndex::build(&path).unwrap())
            .await
            .unwrap();
        let old = engine.webvtt(&a1, 3).await.unwrap();

        // "Replaced": a new identity, and cues that sit 5 s later.
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(std::time::SystemTime::now() + Duration::from_secs(10))
            .unwrap();
        drop(f);
        let mut idx = PlaybackIndex::build(&path).unwrap();
        assert_ne!(idx.source, a1.index.source);
        idx.presentation_offset_ns += 5_000_000_000;
        let a2 = engine.prepare_with_index(idx).await.unwrap();
        let truth = HlsEngine::new(EngineConfig::default())
            .webvtt(&a2, 3)
            .await
            .unwrap();
        assert_ne!(truth, old);
        assert_eq!(engine.webvtt(&a2, 3).await.unwrap(), truth, "stale WebVTT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An idle server gives the caches' memory back: `trim` drops the
    /// prepared asset, the warmed segments and the transcoded audio, and
    /// playback goes on afterwards (the next request makes its segment
    /// again, the same bytes).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trim_empties_the_caches_and_playback_goes_on() {
        let Some(src) = flux_test_media::clip(flux_test_media::Clip::Ac3Eac3BurstsMkv) else {
            return;
        };
        let (Some(ffmpeg), Some(ffprobe)) = (flux_test_media::ffmpeg(), flux_test_media::ffprobe()) else {
            return;
        };
        let engine = HlsEngine::new(EngineConfig {
            ffmpeg,
            ffprobe,
            // No prefetch: nothing lands in a cache after the requests.
            audio_lookahead: 0,
            ..EngineConfig::default()
        });
        let asset = engine.prepare(&src).await.unwrap();
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &PlanPrefs::default());
        // AC-3 isn't Chrome's: the first audio rendition is AAC, transcoded.
        assert!(matches!(plan.audio[0].op, AudioOp::Transcode { .. }), "{plan:?}");
        let r = Rendition::Audio(AudioKey::from_plan(&plan.audio[0]));
        let first = engine
            .media_segment(&asset, &plan, &r, 0)
            .await
            .unwrap()
            .read_all()
            .unwrap();
        // Segment 1 (it starts where segment 0 ends): the copied video into
        // the warm cache, the AAC into the audio transcoder's.
        engine.warm(&asset, &plan, asset.segments.duration_secs(0));
        assert!(
            wait_for(|| {
                let u = engine.cache_usage();
                u.warmed.0 == 1 && u.audio_segments.0 == 2
            })
            .await
        );
        let used = engine.cache_usage();
        assert!(used.assets.0 == 1 && used.bytes() > 0, "{used:?}");

        assert_eq!(engine.trim().bytes(), used.bytes());
        let after = engine.cache_usage();
        assert_eq!(after.bytes(), 0, "{after:?}");
        assert_eq!(
            (after.assets.0, after.audio_segments.0, after.warmed.0),
            (0, 0, 0)
        );
        // Not evictions: the counters say what the budgets did.
        assert_eq!(after.audio_segments.2, used.audio_segments.2);

        let again = engine
            .media_segment(&asset, &plan, &r, 0)
            .await
            .unwrap()
            .read_all()
            .unwrap();
        assert_eq!(again, first);
        engine.prepare(&src).await.unwrap();
        assert_eq!(engine.cache_usage().assets.0, 1, "prepared again");
    }

    /// PB-6: `warm` produces the video and the first audio track only, not
    /// every rendition of the plan (each transcoded one cost ffmpeg runs).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warm_produces_only_the_default_audio() {
        let Some(p) = media("h264_multi_audio_subs.mkv") else {
            return;
        };
        let engine = HlsEngine::new(EngineConfig::default());
        let asset = engine.prepare(&p).await.unwrap();
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &PlanPrefs::default());
        // AAC copy of track 1, AC-3 track 2 as AAC.
        assert_eq!(plan.audio[0].op, AudioOp::Copy, "{plan:?}");
        assert!(plan
            .audio
            .iter()
            .any(|a| a.track != plan.audio[0].track && a.op != AudioOp::Copy));
        assert_eq!(
            HlsEngine::warm_renditions(&plan),
            vec![
                Rendition::Video,
                Rendition::Audio(AudioKey::from_plan(&plan.audio[0]))
            ]
        );
        engine.warm(&asset, &plan, 0.0);
        assert!(wait_for(|| engine.cache_usage().warmed.0 == 2 && engine.cache_usage().warmed.1 > 0).await);
        // Give a stray transcode time to show up.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            engine.transcode_stats().segments,
            0,
            "warm transcoded audio nobody plays"
        );
    }

    /// `warm` keeps what it produced: the request that follows is served
    /// from memory instead of producing (and reading) the segment again,
    /// and the entry is released once served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warm_keeps_the_segments_it_produced() {
        let Some(p) = media("h264_multi_audio_subs.mkv") else {
            return;
        };
        let engine = HlsEngine::new(EngineConfig::default());
        let asset = engine.prepare(&p).await.unwrap();
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &PlanPrefs::default());
        engine.warm(&asset, &plan, asset.segments.duration_secs(0));
        assert!(wait_for(|| engine.cache_usage().warmed.0 == 2 && engine.cache_usage().warmed.1 > 0).await);
        let before = asset.source().io_stats();
        let v = engine
            .media_segment(&asset, &plan, &Rendition::Video, 1)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        let k = AudioKey::from_plan(&plan.audio[0]);
        let a = engine
            .media_segment(&asset, &plan, &Rendition::Audio(k), 1)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        assert_eq!(
            asset.source().io_stats(),
            before,
            "served without reading the source"
        );
        assert_eq!(v.to_vec(), asset.video_segment(1).unwrap().read_all().unwrap());
        assert_eq!(
            a.to_vec(),
            asset.audio_copy_segment(k.track, 1).unwrap().read_all().unwrap()
        );
        // Served once, then dropped: memory isn't held for a repeat.
        assert_eq!(
            engine.cache_usage().warmed.0,
            0,
            "served segments left the warm cache"
        );
        assert_eq!(engine.cache_usage().warmed.1, 0);
    }

    /// PB-11: a transcoded audio segment has the same bytes whether it was
    /// prefetched (played into) or requested on a seek.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transcoded_audio_bytes_do_not_depend_on_prefetch() {
        let Some(p) = media("h264_truehd71.mkv") else {
            return;
        };
        if !ffmpeg() {
            return;
        }
        let key = |plan: &PlaybackPlan| {
            plan.audio
                .iter()
                .map(AudioKey::from_plan)
                .find(|k| k.recipe != AudioRecipe::Copy)
                .expect("an AAC rendition")
        };
        let seek_engine = HlsEngine::new(EngineConfig {
            audio_lookahead: 0,
            video_lookahead: 0,
            ..Default::default()
        });
        let a1 = seek_engine.prepare(&p).await.unwrap();
        let plan1 = seek_engine.plan(&a1, &presets::chrome_desktop(), &PlanPrefs::default());
        let seek = seek_engine
            .media_segment(&a1, &plan1, &Rendition::Audio(key(&plan1)), 2)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        // Play: segment 1 on demand prefetches 2, which then comes from the cache.
        let play_engine = HlsEngine::new(EngineConfig {
            audio_lookahead: 1,
            video_lookahead: 0,
            ..Default::default()
        });
        let a2 = play_engine.prepare(&p).await.unwrap();
        let plan2 = play_engine.plan(&a2, &presets::chrome_desktop(), &PlanPrefs::default());
        let k2 = key(&plan2);
        play_engine
            .media_segment(&a2, &plan2, &Rendition::Audio(k2), 1)
            .await
            .unwrap();
        assert!(wait_for(|| play_engine.transcode_stats().prefetched > 0).await);
        let played = play_engine
            .media_segment(&a2, &plan2, &Rendition::Audio(k2), 2)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        assert_eq!(seek, played, "audio segment 2 differs between prefetch and seek");
    }

    /// PB-11: video transcodes are labelled for weak validators; everything
    /// else is byte-deterministic.
    #[test]
    fn only_video_transcodes_need_weak_validators() {
        let copy = PlaybackPlan {
            delivery: DeliveryFormat::HlsFmp4,
            video_track: Some(0),
            video: VideoOp::Copy { fixes: vec![] },
            audio: vec![],
            subtitles: vec![],
            demuxed_audio: true,
            reasons: vec![],
            est_cpu_ms_per_min: 0,
        };
        let transcode = PlaybackPlan {
            video: VideoOp::Transcode {
                codec: Codec::H264,
                width: 1920,
                height: 1080,
                bitrate: 8_000_000,
                tonemap: false,
                burn_subtitle: None,
                hardware: true,
            },
            ..copy.clone()
        };
        let audio = HlsResource::Segment(
            Rendition::Audio(AudioKey {
                track: 1,
                recipe: AudioRecipe::Aac { channels: 2 },
            }),
            3,
        );
        for r in [
            HlsResource::Init(Rendition::Video),
            HlsResource::Segment(Rendition::Video, 3),
        ] {
            assert!(HlsEngine::is_deterministic(&copy, &r));
            assert!(!HlsEngine::is_deterministic(&transcode, &r));
        }
        for r in [audio, HlsResource::Master, HlsResource::IFrame(2)] {
            assert!(HlsEngine::is_deterministic(&transcode, &r));
        }
    }
}
