//! Prints golden JSON for every flux-core wire type the web client mirrors.
//!
//! `variants!` lists each enum's variants inside an exhaustive `match`, so a
//! variant added to flux-core without updating this file fails to compile, and
//! the TS mirror test then fails until `web/src/api/types.ts` is updated too.

use flux_core::agent::*;
use flux_core::api::*;
use flux_core::*;
use serde::Serialize;
use serde_json::{json, Value};

macro_rules! variants {
    ($ty:ident: $($v:ident),+ $(,)?) => {{
        #[allow(dead_code)]
        fn exhaustive(x: $ty) {
            match x { $($ty::$v)|+ => {} }
        }
        vec![$(serde_json::to_value($ty::$v).expect("serialize")),+]
    }};
}

/// `{ serde_name: display_name }` for an enum with `display_name()`, exhaustive like `variants!`.
macro_rules! display_names {
    ($ty:ident: $($v:ident),+ $(,)?) => {{
        #[allow(dead_code)]
        fn exhaustive(x: $ty) {
            match x { $($ty::$v)|+ => {} }
        }
        let mut m = serde_json::Map::new();
        $(
            let key = serde_json::to_value($ty::$v).expect("serialize");
            m.insert(key.as_str().expect("unit variant").to_string(), json!($ty::$v.display_name()));
        )+
        Value::Object(m)
    }};
}

fn v<T: Serialize>(x: &T) -> Value {
    serde_json::to_value(x).expect("serialize")
}

fn track_video() -> Track {
    Track {
        index: 0,
        id: 1,
        kind: TrackKind::Video,
        codec: Codec::Hevc,
        codec_tag: "V_MPEGH/ISO/HEVC".into(),
        profile: Some("Main 10".into()),
        level: Some(153),
        bit_depth: Some(10),
        width: Some(3840),
        height: Some(2160),
        frame_rate: Some(Rational { num: 24000, den: 1001 }),
        interlaced: Some(false),
        color: Some(ColorInfo { primaries: Some(9), transfer: Some(16), matrix: Some(9), full_range: Some(false) }),
        hdr: Some(HdrInfo {
            format: HdrFormat::Hdr10,
            mastering: Some(MasteringDisplay {
                primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
                white_point: [15635, 16450],
                max_luminance: 10_000_000,
                min_luminance: 50,
            }),
            cll: Some(ContentLightLevel { max_cll: 1000, max_fall: 400 }),
            dovi: Some(DoviConfig {
                profile: 8,
                level: 6,
                rpu_present: true,
                el_present: false,
                bl_present: true,
                bl_signal_compat_id: 1,
            }),
        }),
        channels: None,
        sample_rate: None,
        language: None,
        title: None,
        default: true,
        forced: false,
        hearing_impaired: false,
        bitrate: Some(40_000_000),
        sample_aspect_ratio: Some(Rational { num: 1, den: 1 }),
    }
}

fn track_audio() -> Track {
    Track {
        index: 1,
        id: 2,
        kind: TrackKind::Audio,
        codec: Codec::TrueHd,
        codec_tag: "A_TRUEHD".into(),
        profile: None,
        level: None,
        bit_depth: Some(24),
        width: None,
        height: None,
        frame_rate: None,
        interlaced: None,
        color: None,
        hdr: None,
        channels: Some(8),
        sample_rate: Some(48000),
        language: Some("eng".into()),
        title: Some("TrueHD Atmos 7.1".into()),
        default: true,
        forced: false,
        hearing_impaired: false,
        bitrate: None,
        sample_aspect_ratio: None,
    }
}

fn probe() -> ProbeResult {
    ProbeResult {
        container: Container::Matroska,
        duration_ms: Some(9_960_000),
        bitrate: Some(48_000_000),
        title: Some("Dune Part Two".into()),
        tracks: vec![track_video(), track_audio()],
        chapters: vec![Chapter { start_ms: 0, end_ms: Some(300_000), title: Some("Opening".into()) }],
        attachments: vec![Attachment { name: "font.ttf".into(), mime: "font/ttf".into(), size: 1234 }],
        has_index: true,
        faststart: None,
        needs_frame_sei: false,
        warnings: vec!["example warning".into()],
        probe_version: PROBE_VERSION,
        bytes_read: 131_072,
        reads: 3,
    }
}

fn art() -> ArtRef {
    ArtRef {
        url: "/api/v1/art/7".into(),
        width: Some(1000),
        height: Some(1500),
        blurhash: Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".into()),
        source: ArtSource::Provider,
        attribution: Some("TMDB".into()),
        generator: None,
    }
}

fn generated_art() -> ArtRef {
    ArtRef {
        url: "/api/v1/art/9".into(),
        width: Some(1000),
        height: Some(1500),
        blurhash: None,
        source: ArtSource::Generated,
        attribution: None,
        generator: Some("openai/gpt-image-1".into()),
    }
}

fn scan_status() -> ScanStatus {
    ScanStatus {
        state: ScanState::Scanning,
        files_seen: 100,
        files_total: Some(240),
        files_probed: 50,
        items_added: 40,
        items_updated: 2,
        items_removed: 1,
        started_at: Some(1_760_000_000_000),
        finished_at: None,
        last_error: None,
        dirs_unavailable: 1,
    }
}

fn summary() -> ItemSummary {
    ItemSummary {
        id: 7,
        kind: ItemKind::Movie,
        title: "Dune: Part Two".into(),
        parent_title: None,
        year: Some(2024),
        subtitle: None,
        poster: Some(art()),
        backdrop: None,
        progress: Some(Progress { position_ms: 1000, duration_ms: 9_960_000, watched: false, updated_at: 1_760_000_000_000 }),
        badges: vec!["4K".into(), "HDR10".into()],
        child_count: None,
        unwatched_count: None,
        available: true,
        added_at: 1_760_000_000_000,
    }
}

fn episode_summary() -> ItemSummary {
    ItemSummary {
        id: 12,
        kind: ItemKind::Episode,
        title: "Cat's in the Bag...".into(),
        parent_title: Some("Breaking Bad".into()),
        year: Some(2008),
        subtitle: Some("S01E02".into()),
        poster: Some(generated_art()),
        backdrop: None,
        progress: None,
        badges: vec!["1080p".into()],
        child_count: None,
        unwatched_count: None,
        available: false,
        added_at: 1_760_000_000_000,
    }
}

fn show_summary() -> ItemSummary {
    ItemSummary {
        id: 10,
        kind: ItemKind::Show,
        title: "Breaking Bad".into(),
        parent_title: None,
        subtitle: None,
        child_count: Some(5),
        unwatched_count: Some(61),
        badges: vec![],
        progress: None,
        ..summary()
    }
}

fn caps() -> ClientCaps {
    ClientCaps {
        client: "web/chrome-141/macos".into(),
        delivery: vec![DeliveryFormat::ProgressiveFile, DeliveryFormat::HlsFmp4],
        progressive_containers: vec![Container::Mp4, Container::WebM],
        video: vec![VideoCap {
            codec: Codec::Hevc,
            profiles: vec!["main".into(), "main10".into()],
            max_level: Some(153),
            max_bit_depth: Some(10),
            max_width: Some(3840),
            max_height: Some(2160),
            hdr: vec![HdrFormat::Hdr10],
            dovi_profiles: vec![8],
        }],
        audio: vec![AudioCap { codec: Codec::Aac, max_channels: 8 }],
        display_hdr: true,
        subtitles: vec![Codec::WebVtt],
        needs_demuxed_audio: false,
        max_bitrate: None,
    }
}

fn subtitle_file() -> SubtitleFile {
    SubtitleFile {
        track: SUBTITLE_FILE_TRACK_BASE + 4,
        path: "/media/Movies/Dune Part Two (2024)/Dune Part Two (2024).zh-Hans.forced.srt".into(),
        codec: Codec::Subrip,
        language: Some("zh-Hans".into()),
        title: None,
        forced: true,
        hearing_impaired: false,
        size: 48_213,
        mtime_ns: 1_760_000_000_000_000_000,
    }
}

fn plans() -> Vec<PlaybackPlan> {
    let base = PlaybackPlan {
        delivery: DeliveryFormat::HlsFmp4,
        video_track: Some(0),
        video: VideoOp::Copy { fixes: vec![BitstreamFix::DoviP7To81] },
        audio: vec![AudioPlan { track: 1, op: AudioOp::Transcode { codec: Codec::Aac, channels: 6, bitrate: 640_000 } }],
        subtitles: vec![
            SubtitlePlan { track: 2, op: SubtitleOp::WebVtt, file: None },
            SubtitlePlan { track: 3, op: SubtitleOp::ClientRender, file: None },
            SubtitlePlan { track: SUBTITLE_FILE_TRACK_BASE + 4, op: SubtitleOp::WebVtt, file: Some(subtitle_file()) },
        ],
        demuxed_audio: true,
        reasons: vec![Reason::ContainerUnsupported { container: Container::Matroska }],
        est_cpu_ms_per_min: 120,
    };
    let direct = PlaybackPlan {
        delivery: DeliveryFormat::ProgressiveFile,
        video: VideoOp::Copy { fixes: vec![] },
        audio: vec![AudioPlan { track: 1, op: AudioOp::Copy }],
        subtitles: vec![],
        reasons: vec![],
        demuxed_audio: false,
        ..base.clone()
    };
    let stream = PlaybackPlan {
        audio: vec![AudioPlan { track: 1, op: AudioOp::Copy }],
        subtitles: vec![SubtitlePlan { track: 2, op: SubtitleOp::BurnIn, file: None }],
        ..base.clone()
    };
    let transcode = PlaybackPlan {
        delivery: DeliveryFormat::HlsTs,
        video: VideoOp::Transcode {
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            bitrate: 8_000_000,
            tonemap: true,
            burn_subtitle: Some(3),
            hardware: true,
        },
        ..base.clone()
    };
    vec![direct, stream, base, transcode]
}

fn reasons() -> Vec<Reason> {
    let all = vec![
        Reason::ContainerUnsupported { container: Container::Matroska },
        Reason::VideoCodecUnsupported { codec: Codec::Hevc },
        Reason::VideoProfileUnsupported { codec: Codec::Hevc, profile: "Main 10".into() },
        Reason::VideoLevelTooHigh { codec: Codec::H264, level: 52, max: 51 },
        Reason::VideoBitDepthUnsupported { bit_depth: 10 },
        Reason::VideoResolutionTooHigh { width: 3840, height: 2160 },
        Reason::HdrUnsupported { format: HdrFormat::DolbyVision },
        Reason::DoviProfileUnsupported { profile: 7 },
        Reason::AudioCodecUnsupported { codec: Codec::TrueHd },
        Reason::AudioChannelsTooMany { channels: 8, max: 2 },
        Reason::SubtitleNeedsBurnIn { codec: Codec::Pgs },
        Reason::BitrateTooHigh { bitrate: 60_000_000, max: 20_000_000 },
        Reason::NoIndex { container: Container::Asf },
        Reason::CodecNotPackageable { codec: Codec::TrueHd },
        Reason::ClientQuirk { note: "Samsung 2026 MSE is limited to FHD".into() },
    ];
    // Exhaustiveness guard for Reason.
    for r in &all {
        match r {
            Reason::ContainerUnsupported { .. }
            | Reason::VideoCodecUnsupported { .. }
            | Reason::VideoProfileUnsupported { .. }
            | Reason::VideoLevelTooHigh { .. }
            | Reason::VideoBitDepthUnsupported { .. }
            | Reason::VideoResolutionTooHigh { .. }
            | Reason::HdrUnsupported { .. }
            | Reason::DoviProfileUnsupported { .. }
            | Reason::AudioCodecUnsupported { .. }
            | Reason::AudioChannelsTooMany { .. }
            | Reason::SubtitleNeedsBurnIn { .. }
            | Reason::BitrateTooHigh { .. }
            | Reason::NoIndex { .. }
            | Reason::CodecNotPackageable { .. }
            | Reason::ClientQuirk { .. } => {}
        }
    }
    all
}

fn tailnet_running() -> TailnetInfo {
    TailnetInfo {
        mode: TailnetMode::Embedded,
        state: TailnetState::Running,
        auth_url: None,
        approve_url: None,
        tailnet_name: Some("tail1234.ts.net".into()),
        dns_name: Some("flux.tail1234.ts.net".into()),
        url: Some("https://flux.tail1234.ts.net:8096".into()),
        owner_login: Some("james@example.com".into()),
        key_expiry: None,
        system_tailscale: true,
        last_error: None,
        https: true,
        https_error: None,
    }
}

fn tailnet_login() -> TailnetInfo {
    TailnetInfo {
        state: TailnetState::NeedsLogin,
        auth_url: Some("https://login.tailscale.com/a/abc123".into()),
        tailnet_name: None,
        dns_name: None,
        url: None,
        owner_login: None,
        system_tailscale: false,
        https: false,
        ..tailnet_running()
    }
}

fn changeset_summary() -> ChangesetSummary {
    ChangesetSummary {
        id: 3,
        state: ChangesetState::Applied,
        summary: "Identified Stalker (1979) from its Russian container title".into(),
        agent: Some("claude-code".into()),
        by: "james@example.com".into(),
        created_at: 1_760_000_000_000,
        decided_at: Some(1_760_000_000_000),
        decided_by: Some("james@example.com".into()),
        reverted_at: None,
        reverted_by: None,
        item_count: 1,
        change_count: 2,
        moves_files: false,
    }
}

fn wiki() -> Link {
    Link { url: "https://en.wikipedia.org/wiki/Stalker_(1979_film)".into(), title: Some("Wikipedia".into()) }
}

fn changeset() -> Changeset {
    Changeset {
        summary: changeset_summary(),
        changes: vec![
            Change {
                item: 7,
                title: Some("Stalker".into()),
                field: "directors".into(),
                before: None,
                after: Some(json!(["Andrei Tarkovsky"])),
                withheld: None,
                sources: vec![wiki()],
                state: ChangeState::Applied,
            },
            Change {
                item: 7,
                title: Some("Stalker".into()),
                field: "overview".into(),
                before: None,
                after: Some(json!("A guide leads two men into the Zone.")),
                withheld: Some("tmdb".into()),
                sources: vec![wiki()],
                state: ChangeState::Kept,
            },
        ],
    }
}

fn stored_lens() -> StoredLens {
    StoredLens {
        id: "s-departure-board".into(),
        everyone: false,
        added_by: "james@example.com".into(),
        agent: Some("claude-code".into()),
        added_at: 1_760_000_000_000,
        updated_at: 1_760_000_000_000,
        spec: LensSpec {
            name: "Departure board".into(),
            blurb: "Titles on a split-flap board.".into(),
            input: LensInput::Y,
            step: 90.0,
            window: 40,
            animated: false,
            css: ".lens .item { color: var(--text); }".into(),
            scene: String::new(),
            item: "<div class=\"art\"></div>{{title}}".into(),
            layout: "function layout(items, view) { return items.map(() => ({})); }".into(),
        },
    }
}

fn collection() -> CollectionInfo {
    CollectionInfo {
        id: 3,
        name: "Tarkovsky".into(),
        description: Some("Andrei Tarkovsky's features".into()),
        everyone: true,
        rule: Some(CollectionRule { people: vec!["Andrei Tarkovsky".into()], year_from: Some(1960), ..CollectionRule::default() }),
        count: 1,
        cover: vec![7],
        added_by: "james@example.com".into(),
        agent: Some("claude-code".into()),
        sources: vec![wiki()],
        created_at: 1_760_000_000_000,
        updated_at: 1_760_000_000_000,
    }
}

fn agent_item_detail(guess: &NameGuess) -> AgentItemDetail {
    let item = AgentItem {
        id: 7,
        kind: ItemKind::Movie,
        library: 1,
        parent: None,
        title: "Stalker".into(),
        year: Some(1979),
        subtitle: None,
        match_state: MatchState::Parsed,
        gaps: vec![Gap::Backdrop],
        poster: Some(ArtSource::Custom),
        available: true,
        path: Some("/media/Films/Stalker.1979.mkv".into()),
    };
    AgentItemDetail {
        fields: Fields {
            title: Some("Stalker".into()),
            original_title: Some("Сталкер".into()),
            year: Some(1979),
            directors: Some(vec!["Andrei Tarkovsky".into()]),
            cast: Some(vec![CastMember { name: "Alexander Kaidanovsky".into(), role: Some("Stalker".into()) }]),
            languages: Some(vec!["ru".into()]),
            countries: Some(vec!["SU".into()]),
            links: Some(vec![wiki()]),
            ids: Some([("wikidata".to_string(), "Q470613".to_string())].into()),
            ..Fields::default()
        },
        sources: [(
            "directors".to_string(),
            FieldSource {
                source: Source::Agent,
                provider: None,
                agent: Some("claude-code".into()),
                by: Some("james@example.com".into()),
                changeset: Some(3),
                at: Some(1_760_000_000_000),
                sources: vec![wiki()],
            },
        )]
        .into(),
        withheld: [("overview".to_string(), "tmdb".to_string())].into(),
        match_confidence: None,
        season_number: None,
        episode_numbers: vec![],
        files: vec![FileEvidence {
            id: 70,
            path: "Stalker.1979.mkv".into(),
            root: "/media/Films".into(),
            sidecars: vec!["Stalker.1979.nfo".into()],
            fix: Some(FileFix { year: Some(1979), ..FileFix::default() }),
            size: 4_000_000_000,
            modified: Some(1_760_000_000_000),
            available: true,
            container: Some("MKV".into()),
            duration_ms: Some(9_660_000),
            container_title: Some("Сталкер".into()),
            edition: None,
            tracks: vec![TrackEvidence {
                kind: "audio".into(),
                codec: "AAC".into(),
                language: Some("rus".into()),
                title: None,
                width: None,
                height: None,
                fps: None,
                channels: Some(2),
                hdr: None,
                default: true,
                forced: false,
            }],
            chapters: vec![],
            subtitle_files: vec![],
            parsed: Some(guess.clone()),
        }],
        folders: vec![],
        art: vec![AgentArt {
            id: 15,
            kind: ArtKind::Poster,
            source: ArtSource::Custom,
            shown: true,
            pinned: true,
            width: Some(1000),
            height: Some(1500),
            credit: Some("Mosfilm".into()),
            found_at: Some("https://en.wikipedia.org/wiki/Stalker_(1979_film)".into()),
            generator: None,
            url: Some("/api/v1/art/15".into()),
            withheld: None,
        }],
        children: vec![],
        changes: vec![changeset_summary()],
        item,
    }
}

fn main() {
    let enums = json!({
        "Container": variants!(Container: Matroska, WebM, Mp4, Mov, MpegTs, M2ts, Avi, Asf, Flv, RealMedia, MpegPs, Unknown),
        "TrackKind": variants!(TrackKind: Video, Audio, Subtitle, Attachment, Data),
        "Codec": variants!(Codec: H264, Hevc, Av1, Vp9, Vp8, Mpeg2Video, Mpeg4Part2, Vc1, ProRes, Mjpeg,
            Mpeg1Video, Wmv1, Wmv2, Msmpeg4, Vp6, Flv1, RealVideo,
            Aac, Ac3, Eac3, TrueHd, Dts, Flac, Opus, Vorbis, Mp3, Mp2, Pcm, Alac, Wma, WmaPro, RealAudio,
            Subrip, Ass, Ssa, WebVtt, MovText, Pgs, VobSub, DvbSub, Other),
        "HdrFormat": variants!(HdrFormat: Sdr, Hdr10, Hdr10Plus, Hlg, DolbyVision),
        "LibraryKind": variants!(LibraryKind: Movies, Shows, Mixed, Videos),
        "GuessKind": variants!(GuessKind: Movie, Episode, Extra, Sample, Unknown),
        "ExtraKind": variants!(ExtraKind: Trailer, Featurette, BehindTheScenes, DeletedScene, Interview, Short, Scene, Other),
        "DeliveryFormat": variants!(DeliveryFormat: ProgressiveFile, HlsFmp4, HlsTs),
        "BitstreamFix": variants!(BitstreamFix: DoviP7To81, StripInbandParameterSets),
        "Role": variants!(Role: Owner, Admin, Viewer, Kid, Guest),
        "ScanState": variants!(ScanState: Idle, Scanning, Watching, Unavailable, Error),
        "ItemKind": variants!(ItemKind: Movie, Show, Season, Episode, Extra),
        "ArtSource": variants!(ArtSource: Provider, Local, Frame, Generated, Custom),
        "ItemSort": variants!(ItemSort: Title, TitleDesc, Year, YearDesc, Added, AddedDesc),
        "FolderSort": variants!(FolderSort: Title, TitleDesc, Date, DateDesc),
        "ItemFilter": variants!(ItemFilter: Unwatched, InProgress, Uhd, Hdr),
        "TailnetMode": variants!(TailnetMode: Off, Embedded, System),
        "TailnetState": variants!(TailnetState: Off, Starting, NeedsLogin, NeedsApproval, Running, Unavailable, Error),
        "MetadataSource": variants!(MetadataSource: None, Tmdb, Exports),
        // The agent contract (flux_core::agent; web/src/api/agent.ts).
        "Gap": variants!(Gap: Identity, Year, Overview, Poster, Backdrop, EpisodeTitles, Offline),
        "MatchState": variants!(MatchState: Unmatched, Parsed, Matched, Locked),
        "Source": variants!(Source: File, Provider, User, Agent, Sidecar),
        "ChangesetState": variants!(ChangesetState: Proposed, Applied, Reverted, Rejected),
        "ChangeState": variants!(ChangeState: Pending, Applied, Skipped, Reverted, Kept),
        "ArtKind": variants!(ArtKind: Poster, Backdrop, Thumb, Logo),
        "ArtOrigin": variants!(ArtOrigin: Found, Made, Generated),
        "EditMode": variants!(EditMode: Apply, Propose),
        "ErrorCode": variants!(ErrorCode: Invalid, NotFound, Forbidden, Conflict, Unavailable, TooLarge, Internal),
        "LensInput": variants!(LensInput: X, Y, None),
        "Effect": variants!(Effect: Read, Write, Act),
        "FixKind": variants!(FixKind: Movie, Episode, Extra, Ignore),
        "StepKind": variants!(StepKind: NewFolder, RenameFolder, MoveVideo, MoveCompanion, RemoveFolder),
        "NamingScheme": variants!(NamingScheme: Standard),
    });

    // Display names used by Reason::explain (web/src/api/explain.ts mirrors them).
    let display = json!({
        "Codec": display_names!(Codec: H264, Hevc, Av1, Vp9, Vp8, Mpeg2Video, Mpeg4Part2, Vc1, ProRes, Mjpeg,
            Mpeg1Video, Wmv1, Wmv2, Msmpeg4, Vp6, Flv1, RealVideo,
            Aac, Ac3, Eac3, TrueHd, Dts, Flac, Opus, Vorbis, Mp3, Mp2, Pcm, Alac, Wma, WmaPro, RealAudio,
            Subrip, Ass, Ssa, WebVtt, MovText, Pgs, VobSub, DvbSub, Other),
        "Container": display_names!(Container: Matroska, WebM, Mp4, Mov, MpegTs, M2ts, Avi, Asf, Flv, RealMedia, MpegPs, Unknown),
        "HdrFormat": display_names!(HdrFormat: Sdr, Hdr10, Hdr10Plus, Hlg, DolbyVision),
    });

    let reasons = reasons();
    let reason_explain: Vec<Value> = reasons.iter().map(|r| json!([v(r), r.explain()])).collect();
    let plan_labels: Vec<Value> = plans().iter().map(|p| json!([v(p), p.label()])).collect();

    let detail = ItemDetail {
        summary: summary(),
        overview: Some("Paul Atreides unites with the Fremen.".into()),
        genres: vec!["Science Fiction".into()],
        runtime_ms: Some(9_960_000),
        assets: vec![AssetInfo {
            id: 70,
            path: "/media/Movies/Dune Part Two (2024)/Dune Part Two (2024).mkv".into(),
            size: 60_000_000_000,
            container: Container::Matroska,
            duration_ms: Some(9_960_000),
            edition: Some("IMAX".into()),
            label: "4K HEVC HDR10 · TrueHD 7.1".into(),
            hdr: Some(HdrFormat::Hdr10),
            available: true,
            probe: Some(probe()),
            subtitle_files: vec![
                subtitle_file(),
                SubtitleFile {
                    track: SUBTITLE_FILE_TRACK_BASE + 5,
                    path: "/media/Movies/Dune Part Two (2024)/Subs/Dune Part Two (2024).Commentary.sup".into(),
                    codec: Codec::Pgs,
                    language: None,
                    title: Some("Commentary".into()),
                    forced: false,
                    hearing_impaired: false,
                    size: 9_000_000,
                    mtime_ns: 1_760_000_000_000_000_000,
                },
            ],
        }],
        show_id: None,
        season_number: None,
        episode_numbers: vec![],
        external_ids: vec![("tmdb".into(), "693134".into())],
        match_confidence: Some(0.92),
        match_evidence: vec!["fast path: Title (Year)/".into()],
    };

    let play = PlayResponse {
        session_id: "s1".into(),
        asset_id: 70,
        plan: plans()[2].clone(),
        url: "/media/p1/master.m3u8".into(),
        explain: reasons.iter().take(2).map(Reason::explain).collect(),
        start_ms: 0,
        duration_ms: Some(9_960_000),
        prefetch: vec!["/media/p1/v/init.mp4".into()],
        audio: vec![
            AudioRef {
                rendition: "1-eac36".into(),
                track: 1,
                label: "TrueHD Atmos 7.1 (E-AC-3 5.1)".into(),
                language: Some("eng".into()),
                codec: Codec::Eac3,
                channels: 6,
                default: true,
            },
            AudioRef {
                rendition: "1-aac2".into(),
                track: 1,
                label: "TrueHD Atmos 7.1 (AAC Stereo)".into(),
                language: Some("eng".into()),
                codec: Codec::Aac,
                channels: 2,
                default: false,
            },
        ],
        subtitles: vec![SubtitleRef {
            track: 2,
            label: "English".into(),
            language: Some("eng".into()),
            url: "/media/p1/s/2.vtt".into(),
            forced: false,
        }],
    };
    let progress = ProgressReport {
        position_ms: 1000,
        paused: false,
        ttff_ms: Some(240),
        last_seek_ms: Some(80),
        stalls: Some(0),
        dropped_frames: Some(3),
    };

    let name_guess = NameGuess {
        kind: GuessKind::Episode,
        title: Some("Breaking Bad".into()),
        original_title: None,
        year: Some(2008),
        season: Some(1),
        episodes: vec![1, 2],
        absolute_episode: Some(1),
        air_date: None,
        episode_title: None,
        edition: None,
        part: None,
        resolution: Some("1080p".into()),
        source: Some("WEB-DL".into()),
        video_codec: Some("x264".into()),
        release_group: Some("NTb".into()),
        extra: Some(ExtraKind::Featurette),
        ids: ProviderIds { tmdb: Some(1396), imdb: Some("tt0903747".into()), tvdb: None },
        confidence: 0.9,
        evidence: vec!["fast path".into()],
    };

    let samples = json!({
        "ServerInfo": v(&ServerInfo {
            name: "flux".into(),
            version: "0.1.0".into(),
            you: Some(Identity {
                login_name: "james@example.com".into(),
                display_name: "James".into(),
                profile_pic_url: None,
                node_name: Some("macbook".into()),
                role: Role::Owner,
                tags: vec![],
            }),
            path: "tailnet-direct".into(),
            tailnet_name: Some("flux.tail1234.ts.net".into()),
            metadata: MetadataInfo { source: MetadataSource::Tmdb },
            https_url: Some("https://flux.tail1234.ts.net:8096".into()),
        }),
        "CreateLibrary": v(&CreateLibrary { name: "Movies".into(), kind: LibraryKind::Movies, roots: vec!["/m".into()] }),
        "Library": v(&Library {
            id: 1,
            name: "Movies".into(),
            kind: LibraryKind::Movies,
            roots: vec![
                RootStatus { path: "/m".into(), available: true, last_seen: Some(1_760_000_000_000) },
                RootStatus { path: "/Volumes/NAS/Movies".into(), available: false, last_seen: None },
            ],
            item_count: 10,
            scan: scan_status(),
        }),
        "PageItemSummary": v(&Page {
            items: vec![summary(), show_summary(), episode_summary()],
            next: Some("c1".into()),
            total: Some(3),
        }),
        "ItemsParams": v(&ItemsParams {
            cursor: Some("c1".into()),
            limit: Some(100),
            sort: Some(ItemSort::AddedDesc),
            filter: Some(ItemFilter::Uhd),
        }),
        "FolderParams": v(&FolderParams {
            folder: Some("3/2019/Trip".into()),
            cursor: Some("t696d672030303030303031.12".into()),
            limit: Some(100),
            sort: Some(FolderSort::DateDesc),
        }),
        "FolderPage": v(&FolderPage {
            at: FolderRef { folder: "3/2019/Trip".into(), name: "Trip".into() },
            parents: vec![
                FolderRef { folder: String::new(), name: "Home videos".into() },
                FolderRef { folder: "3/2019".into(), name: "2019".into() },
            ],
            folders: vec![FolderEntry { folder: "3/2019/Trip/Day 2".into(), name: "Day 2".into(), item_count: 4 }],
            items: vec![ItemSummary {
                title: "IMG 0001".into(),
                year: None,
                poster: None,
                progress: None,
                badges: vec![],
                ..summary()
            }],
            next: Some("e1760000000000~696d672030303030303031.12".into()),
            total: 2,
        }),
        "ItemDetail": v(&detail),
        "Home": v(&Home {
            continue_watching: vec![summary(), episode_summary()],
            recently_added: vec![],
            rows: vec![HomeRow { title: "Movies".into(), library_id: Some(1), items: vec![summary()] }],
        }),
        "PlayRequest": v(&PlayRequest {
            item_id: 7,
            asset_id: Some(70),
            caps: caps(),
            audio_track: Some(1),
            subtitle_track: None,
            start_ms: None,
        }),
        "PlayResponse": v(&play),
        "PlanPreview": v(&PlanPreview::new(70, plans()[2].clone())),
        "MarkWatched": v(&MarkWatched { watched: true }),
        "TailnetInfo": v(&tailnet_running()),
        "TailnetConnect": v(&TailnetConnect { mode: TailnetMode::System, https: Some(true) }),
        "ProgressReport": v(&progress),
        "SessionDiagnostics": v(&SessionDiagnostics {
            session_id: "s1".into(),
            plan: plans()[0].clone(),
            explain: vec![],
            path: "lan".into(),
            segments_served: 3,
            bytes_served: 1_000_000,
            segment_ms_p50: Some(1.5),
            segment_ms_p95: Some(3.0),
            client: Some(progress.clone()),
        }),
        "Events": [
            v(&Event::ScanProgress { library_id: 1, status: scan_status() }),
            v(&Event::ItemsChanged { library_id: 1, added: vec![1], updated: vec![], removed: vec![2] }),
            v(&Event::SourceAvailability { library_id: 1, root: "/m".into(), available: false }),
            v(&Event::Tailnet { info: tailnet_login() }),
            v(&Event::Changes { changeset: changeset_summary() }),
            v(&Event::Lenses),
            v(&Event::Collections),
        ],
        "AgentItemDetail": v(&agent_item_detail(&name_guess)),
        "ChangesListOutput": v(&ChangesListOutput { changesets: vec![changeset_summary()], next: None, proposed: 1 }),
        "Changeset": v(&changeset()),
        "Decided": v(&Decided {
            changeset: changeset_summary(),
            done: 2,
            not_done: vec![NotDone { item: 7, field: "year".into(), reason: "it changed again since".into() }],
        }),
        "Lenses": v(&Lenses { lenses: vec![stored_lens()] }),
        "FixOutput": v(&FixOutput {
            changeset: Some(changeset_summary()),
            files: vec![FixResult {
                file: 71,
                path: "Stalker (2).mkv".into(),
                before: FilePlace { item: Some(8), title: Some("Stalker (2)".into()) },
                after: Some(FilePlace { item: Some(7), title: Some("Stalker".into()) }),
                fix: Some(FileFix { of: Some(7), ..FileFix::default() }),
                unchanged: None,
            }],
        }),
        "PlanOutput": v(&PlanOutput {
            changeset: Some(changeset_summary()),
            steps: vec![
                PlanStep { step: StepKind::NewFolder, file: None, item: Some(7), root: "/media/Films".into(), from: None, to: Some("Stalker (1979)".into()) },
                PlanStep { step: StepKind::MoveVideo, file: Some(70), item: Some(7), root: "/media/Films".into(), from: Some("Stalker.1979.mkv".into()), to: Some("Stalker (1979)/Stalker (1979).mkv".into()) },
            ],
            skipped: vec![PlanSkip { file: Some(72), item: Some(9), path: Some("x.mkv".into()), reason: "its drive doesn't answer".into() }],
        }),
        "Collections": v(&Collections { collections: vec![collection()] }),
        "CollectionItems": v(&CollectionItems {
            collection: collection(),
            items: vec![agent_item_detail(&name_guess).item],
            notes: [("7".to_string(), "Tarkovsky's last Soviet film".to_string())].into(),
            next: None,
            total: 1,
        }),
        "ErrorBody": v(&ErrorBody { error: OpError { code: ErrorCode::Conflict, message: "changeset 3 is reverted".into() } }),
        "Manifest": v(&Manifest {
            contract: CONTRACT.into(),
            server: ServerAbout { name: "flux".into(), version: "0.1.0".into() },
            ops: vec![],
            mcp: "/mcp".into(),
            skills: vec![SkillInfo { name: "flux".into(), description: "Work with a flux media server.".into(), url: "/api/v1/agent/skills/flux".into() }],
            schemas: vec!["lens".into()],
            policy: Policy { withheld: vec!["tmdb".into()], rules: vec!["Cite.".into()] },
        }),
        "NameGuess": v(&name_guess),
    });

    let out = json!({
        "enums": enums,
        "display": display,
        "reasons": reason_explain,
        "plan_labels": plan_labels,
        "samples": samples,
    });
    println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
}
