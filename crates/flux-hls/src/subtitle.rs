//! Text subtitles → WebVTT sidecars: Matroska SRT/ASS/SSA/WebVTT tracks, MP4
//! `tx3g` (mov_text) and `wvtt` tracks, and subtitle files beside the video
//! (`.srt`, `.ass`, `.ssa`, `.vtt`; [`parse_file`]).
//!
//! ASS styling is dropped (lossy, research/03 §7): override blocks `{…}` are
//! removed, drawings (`{\p1}…{\p0}`) too, and `\N` becomes a line break.
//! Clients that can render ASS (JASSUB) should get the original track instead.

use std::fmt::Write as _;

use bytes::Bytes;
use flux_core::Codec;

fn ts(ns: i64) -> String {
    let ms = (ns.max(0) + 500_000) / 1_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// Cue text of one Matroska ASS/SSA block: the 9th comma-separated field
/// (`ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`).
fn ass_text(payload: &str) -> String {
    let text = payload.splitn(9, ',').nth(8).unwrap_or(payload);
    let mut out = String::with_capacity(text.len());
    let mut depth = 0;
    // The current override block, and whether `\p<n>` (n > 0) switched to
    // drawing mode: the text until `\p0` is vector commands, not words.
    let mut block = String::new();
    let mut drawing = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                depth += 1;
                block.clear();
            }
            '}' if depth > 0 => {
                depth -= 1;
                if let Some(on) = drawing_mode(&block) {
                    drawing = on;
                }
            }
            _ if depth > 0 => block.push(c),
            _ if drawing => {}
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out
}

/// The drawing mode an override block sets: `Some(true)` for `\p1`…,
/// `Some(false)` for `\p0`, `None` when it doesn't say (`\pos` and `\pbo`
/// are other tags).
fn drawing_mode(block: &str) -> Option<bool> {
    let mut mode = None;
    let mut rest = block;
    while let Some(i) = rest.find("\\p") {
        rest = &rest[i + 2..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            mode = Some(digits.parse::<u32>().is_ok_and(|n| n > 0));
        }
    }
    mode
}

/// SubRip text with only the tags WebVTT understands (`<i> <b> <u>`).
fn srt_text(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len());
    let mut rest = payload;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('>') else {
            out.push_str(&rest[i..]);
            rest = "";
            break;
        };
        let tag = &rest[i..i + j + 1];
        let name = tag
            .trim_start_matches(['<', '/'])
            .trim_end_matches('>')
            .to_ascii_lowercase();
        if matches!(name.as_str(), "i" | "b" | "u") {
            out.push_str(&tag.to_ascii_lowercase());
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out.replace("-->", "->")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The cue text of one MP4 `tx3g` (3GPP timed text, mov_text) sample, as
/// WebVTT cue text: a 16-bit length, the text (UTF-8, or UTF-16 after a
/// byte-order mark; escaped, it is plain text), then modifier boxes, of
/// which the `styl` ranges become `<b>`, `<i>` and `<u>` (fonts, sizes and
/// colours are dropped). `None` for an empty sample (a gap between cues).
pub fn mov_text_sample(data: &[u8]) -> Option<String> {
    let len = u16::from_be_bytes([*data.first()?, *data.get(1)?]) as usize;
    let end = (2 + len).min(data.len());
    let text = decode_text(data.get(2..end)?);
    if text.trim().is_empty() {
        return None;
    }
    // Per character: bold, italic, underline (face-style flags 1, 2, 4).
    let chars: Vec<char> = text.chars().collect();
    let mut style = vec![0u8; chars.len()];
    for (kind, body) in boxes(&data[end..]) {
        if kind != *b"styl" || body.len() < 2 {
            continue;
        }
        let n = u16::from_be_bytes([body[0], body[1]]) as usize;
        let (records, _) = body[2..].as_chunks::<12>();
        for rec in records.iter().take(n) {
            let from = u16::from_be_bytes([rec[0], rec[1]]) as usize;
            let to = (u16::from_be_bytes([rec[2], rec[3]]) as usize).min(chars.len());
            for f in style.iter_mut().take(to).skip(from) {
                *f |= rec[6] & 7;
            }
        }
    }
    const TAGS: [(u8, &str); 3] = [(1, "b"), (2, "i"), (4, "u")];
    let mut out = String::with_capacity(text.len() + 16);
    let mut open = 0u8;
    for (c, &f) in chars.iter().zip(&style) {
        if f != open {
            for (bit, tag) in TAGS.iter().rev() {
                if open & bit != 0 {
                    out.push_str(&format!("</{tag}>"));
                }
            }
            for (bit, tag) in TAGS {
                if f & bit != 0 {
                    out.push_str(&format!("<{tag}>"));
                }
            }
            open = f;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(*c),
        }
    }
    for (bit, tag) in TAGS.iter().rev() {
        if open & bit != 0 {
            out.push_str(&format!("</{tag}>"));
        }
    }
    Some(out)
}

/// The cue texts of one MP4 `wvtt` (ISO/IEC 14496-30) sample: the `payl`
/// box of each `vttc` box. A `vtte` (empty) sample has none.
pub fn wvtt_sample(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for (kind, body) in boxes(data) {
        if kind == *b"vttc" {
            for (k, b) in boxes(body) {
                if k == *b"payl" {
                    out.push(String::from_utf8_lossy(b).into_owned());
                }
            }
        }
    }
    out
}

/// The (type, body) of each ISO-BMFF box in `data`, stopping at the first
/// malformed one.
fn boxes(mut data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if size < 8 || size > data.len() {
            break;
        }
        out.push(([data[4], data[5], data[6], data[7]], &data[8..size]));
        data = &data[size..];
    }
    out
}

/// Text of a subtitle file or sample: UTF-8 (with or without a byte-order
/// mark) or UTF-16 with one. Other bytes are decoded lossily as UTF-8
/// (legacy code pages such as GBK or Big5 aren't converted).
pub fn decode_text(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let (pairs, _) = b.as_chunks::<2>();
        let units: Vec<u16> = pairs
            .iter()
            .map(|&c| {
                if le {
                    u16::from_le_bytes(c)
                } else {
                    u16::from_be_bytes(c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// A clock time `[h:]mm:ss[.,]fff` (SubRip, WebVTT, ASS `h:mm:ss.cc`) in ns.
fn clock_ns(s: &str) -> Option<i64> {
    let s = s.trim();
    let mut parts: Vec<&str> = s.split(':').collect();
    let last = parts.pop()?;
    let (sec, frac) = match last.split_once(['.', ',']) {
        Some((a, b)) => (a, b),
        None => (last, ""),
    };
    let num = |x: &str| -> Option<i64> {
        let x = x.trim();
        if x.is_empty() || !x.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        x.parse().ok()
    };
    let (h, m) = match parts.as_slice() {
        [m] => (0, num(m)?),
        [h, m] => (num(h)?, num(m)?),
        _ => return None,
    };
    let sec = num(sec)?;
    let frac_ns = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(9).collect();
        num(&digits)? * 10i64.pow(9 - digits.len() as u32)
    };
    Some(((h * 60 + m) * 60 + sec) * 1_000_000_000 + frac_ns)
}

/// The cues of a subtitle **file** (`codec` from its extension: SubRip,
/// ASS/SSA or WebVTT) as (start ns, end ns, payload), in file order, with
/// payloads in the form [`WebVttWriter::cue`] takes for that codec (an ASS
/// `Dialogue` line becomes a Matroska-style block payload). Anything else
/// in the file (styles, comments, notes) is skipped; a malformed cue is
/// dropped, never the whole file.
pub fn parse_file(codec: Codec, text: &str) -> Vec<(i64, i64, Bytes)> {
    let text = text.trim_start_matches('\u{feff}');
    match codec {
        Codec::Ass | Codec::Ssa => parse_ass(text),
        Codec::Subrip | Codec::WebVtt => parse_timed_blocks(text),
        _ => Vec::new(),
    }
}

/// SubRip and WebVTT: a `start --> end` line (after an optional number or
/// identifier), then text lines until a blank line or the next timing line.
fn parse_timed_blocks(text: &str) -> Vec<(i64, i64, Bytes)> {
    let timing = |line: &str| -> Option<(i64, i64)> {
        let (a, b) = line.split_once("-->")?;
        // WebVTT cue settings follow the end time.
        let b = b.split_whitespace().next()?;
        Some((clock_ns(a)?, clock_ns(b)?))
    };
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((start, end)) = timing(lines[i]) else {
            i += 1;
            continue;
        };
        i += 1;
        let mut body: Vec<&str> = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && timing(lines[i]).is_none() {
            body.push(lines[i]);
            i += 1;
        }
        // No blank line before the next cue: its number came along.
        let next_cue = i < lines.len() && timing(lines[i]).is_some();
        if next_cue
            && body.len() > 1
            && body
                .last()
                .is_some_and(|l| l.trim().bytes().all(|b| b.is_ascii_digit()))
        {
            body.pop();
        }
        if end > start && !body.is_empty() {
            out.push((start, end, Bytes::from(body.join("\n"))));
        }
    }
    out
}

/// ASS/SSA `[Events]`: each `Dialogue:` line split by the section's
/// `Format:` (`Start`, `End`, and `Text` last, which may hold commas).
fn parse_ass(text: &str) -> Vec<(i64, i64, Bytes)> {
    // ASS's default field order, used when no Format line comes first.
    let mut fields = 10;
    let (mut start_at, mut end_at) = (1, 2);
    let mut in_events = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("format") {
            let names: Vec<String> = value.split(',').map(|f| f.trim().to_ascii_lowercase()).collect();
            if let (Some(s), Some(e), Some(t)) = (
                names.iter().position(|n| n == "start"),
                names.iter().position(|n| n == "end"),
                names.iter().position(|n| n == "text"),
            ) {
                if t == names.len() - 1 {
                    (fields, start_at, end_at) = (names.len(), s, e);
                }
            }
        } else if key.trim().eq_ignore_ascii_case("dialogue") {
            let parts: Vec<&str> = value.splitn(fields, ',').collect();
            if parts.len() < fields {
                continue;
            }
            let (Some(start), Some(end)) = (clock_ns(parts[start_at]), clock_ns(parts[end_at])) else {
                continue;
            };
            if end <= start {
                continue;
            }
            // The Matroska block layout `ass_text` reads: text after 8 commas.
            let payload = format!(",,,,,,,,{}", parts[fields - 1]);
            out.push((start, end, Bytes::from(payload)));
        }
    }
    out
}

/// Builds a WebVTT document. `offset_ns` is the presentation offset the
/// segments carry (`PlaybackIndex::presentation_offset_ns`): it goes into
/// `X-TIMESTAMP-MAP` so cue times stay on the source timeline.
pub fn to_webvtt(codec: Codec, cues: &[(i64, i64, Bytes)], offset_ns: i64) -> String {
    let mut w = WebVttWriter::new(codec);
    let mut s = WebVttWriter::header(offset_ns);
    let mut sorted: Vec<&(i64, i64, Bytes)> = cues.iter().collect();
    sorted.sort_by_key(|c| c.0);
    for (start, end, payload) in sorted {
        if let Some(c) = w.cue(*start, *end, payload) {
            s.push_str(&c);
        }
    }
    s
}

/// WebVTT written incrementally: the header, then each cue as it is read
/// (`Asset::for_each_subtitle_block`), so a response can stream subtitles
/// instead of waiting for the whole track (PB-7). [`to_webvtt`] is this
/// over a sorted list.
#[derive(Debug, Clone)]
pub struct WebVttWriter {
    codec: Codec,
    n: usize,
}

impl WebVttWriter {
    /// A writer for cues of a `codec` track.
    pub fn new(codec: Codec) -> Self {
        WebVttWriter { codec, n: 0 }
    }

    /// The document header (see [`to_webvtt`] for `offset_ns`).
    pub fn header(offset_ns: i64) -> String {
        let mpegts = (offset_ns as i128 * 90_000 / 1_000_000_000) as i64;
        format!("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{mpegts},LOCAL:00:00:00.000\n\n")
    }

    /// The text of the next cue, or `None` when it is empty after cleaning.
    pub fn cue(&mut self, start: i64, end: i64, payload: &[u8]) -> Option<String> {
        self.n += 1;
        let raw = String::from_utf8_lossy(payload);
        let text = match self.codec {
            Codec::Ass | Codec::Ssa => escape(&ass_text(&raw)),
            Codec::Subrip => srt_text(&raw),
            // `mov_text_sample` made it cue text already (escaped, with tags).
            _ => raw.replace("-->", "->"),
        };
        let text = text.trim().replace("\r\n", "\n").replace("\n\n", "\n");
        if text.is_empty() {
            return None;
        }
        let mut s = String::with_capacity(text.len() + 40);
        let _ = write!(
            s,
            "{}\n{} --> {}\n{}\n\n",
            self.n,
            ts(start),
            ts(end.max(start + 1)),
            text
        );
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ass_override_tags_stripped() {
        assert_eq!(
            ass_text("0,0,Sign,,0,0,0,,{\\pos(960,200)\\fad(300,300)}A typeset sign"),
            "A typeset sign"
        );
        assert_eq!(
            ass_text("1,0,Default,,0,0,0,,{\\i1}Italic{\\i0} and\\Nmore"),
            "Italic and\nmore"
        );
    }

    #[test]
    fn srt_tags_filtered() {
        assert_eq!(
            srt_text("This has <i>italics</i> and <font color=\"red\">red</font>"),
            "This has <i>italics</i> and red"
        );
    }

    #[test]
    fn ass_drawings_dropped() {
        assert_eq!(
            ass_text(",,,,,,,,{\\pos(10,10)\\p1}m 0 0 l 100 0 100 100{\\p0}Sign text"),
            "Sign text"
        );
        // `\pos` and `\pbo` are not drawing mode.
        assert_eq!(ass_text(",,,,,,,,{\\pos(1,2)\\pbo0}Hello"), "Hello");
    }

    #[test]
    fn mov_text_samples() {
        // 16-bit length, text (plain: escaped), then modifier boxes.
        let mut s = vec![0, 11];
        s.extend_from_slice(b"Hello <you>");
        s.extend_from_slice(&[0, 0, 0, 12, b'h', b'c', b'l', b'r', 0, 0, 0, 0]);
        assert_eq!(mov_text_sample(&s).as_deref(), Some("Hello &lt;you&gt;"));
        // A `styl` box: characters 0..4 italic, 2..7 bold.
        let text = "Well, & so";
        let mut s = vec![0, text.len() as u8];
        s.extend_from_slice(text.as_bytes());
        let mut styl = vec![0, 0, 0, 34, b's', b't', b'y', b'l', 0, 2];
        styl.extend_from_slice(&[0, 0, 0, 4, 0, 1, 2, 18, 255, 255, 255, 255]);
        styl.extend_from_slice(&[0, 2, 0, 7, 0, 1, 1, 18, 255, 255, 255, 255]);
        s.extend_from_slice(&styl);
        assert_eq!(
            mov_text_sample(&s).as_deref(),
            Some("<i>We</i><b><i>ll</i></b><b>, &amp;</b> so")
        );
        // An empty sample is a gap between cues.
        assert_eq!(mov_text_sample(&[0, 0]), None);
        assert_eq!(mov_text_sample(&[]), None);
        // UTF-16 with a byte-order mark.
        let mut u = vec![0, 4, 0xFE, 0xFF];
        u.extend_from_slice(&[0x4E, 0x2D]); // 中
        assert_eq!(mov_text_sample(&u).as_deref(), Some("中"));
        // The cue writer takes it as cue text.
        let mut w = WebVttWriter::new(Codec::MovText);
        let cue = w.cue(0, 1_000_000_000, b"<i>a &lt; b</i>").unwrap();
        assert!(cue.contains("\n<i>a &lt; b</i>\n"), "{cue}");
    }

    #[test]
    fn wvtt_samples() {
        let payl = |t: &str| {
            let mut b = ((8 + t.len()) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(b"payl");
            b.extend_from_slice(t.as_bytes());
            b
        };
        let vttc = |inner: Vec<u8>| {
            let mut b = ((8 + inner.len()) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(b"vttc");
            b.extend_from_slice(&inner);
            b
        };
        let mut sample = vttc(payl("<i>One</i>"));
        sample.extend(vttc(payl("Two")));
        assert_eq!(wvtt_sample(&sample), vec!["<i>One</i>", "Two"]);
        assert!(wvtt_sample(&[0, 0, 0, 8, b'v', b't', b't', b'e']).is_empty());
    }

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFabc"), "abc");
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("中文".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_text(&le), "中文");
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("ab".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode_text(&be), "ab");
        assert_eq!(decode_text("示例".as_bytes()), "示例");
    }

    #[test]
    fn clock_times() {
        assert_eq!(clock_ns("00:00:01,500"), Some(1_500_000_000));
        assert_eq!(clock_ns("01:02:03.004"), Some(3_723_004_000_000));
        assert_eq!(clock_ns("0:00:38.12"), Some(38_120_000_000)); // ASS centiseconds
        assert_eq!(clock_ns("02:03.250"), Some(123_250_000_000)); // WebVTT mm:ss.ttt
        assert_eq!(clock_ns("x:00:01"), None);
        assert_eq!(clock_ns(""), None);
    }

    #[test]
    fn srt_files() {
        let srt = "1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>First</i> line\r\nsecond line\r\n\r\n\
                   2\n00:00:03,000 --> 00:00:04,000\nNo blank line follows\n3\n00:00:05,000 --> 00:00:06,000\n第三句\n";
        let cues = parse_file(Codec::Subrip, srt);
        assert_eq!(cues.len(), 3, "{cues:?}");
        assert_eq!((cues[0].0, cues[0].1), (1_000_000_000, 2_500_000_000));
        assert_eq!(&cues[0].2[..], "<i>First</i> line\nsecond line".as_bytes());
        assert_eq!(&cues[1].2[..], b"No blank line follows");
        assert_eq!(&cues[2].2[..], "第三句".as_bytes());
        let vtt = to_webvtt(Codec::Subrip, &cues, 0);
        assert!(vtt.contains("00:00:05.000 --> 00:00:06.000\n第三句"), "{vtt}");
    }

    #[test]
    fn vtt_files() {
        let vtt = "WEBVTT - a title\n\nNOTE a comment\nspanning lines\n\nSTYLE\n::cue { color: red }\n\n\
                   intro\n00:01.000 --> 00:02.000 align:start line:0\n<v Bob>Hi</v>\n\n00:00:03.000 --> 00:00:04.000\nBye\n";
        let cues = parse_file(Codec::WebVtt, vtt);
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!((cues[0].0, cues[0].1), (1_000_000_000, 2_000_000_000));
        assert_eq!(&cues[0].2[..], b"<v Bob>Hi</v>");
    }

    #[test]
    fn ass_files() {
        let ass = "\u{feff}[Script Info]\nTitle: 示例\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n\
                   [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                   Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,not shown\n\
                   Dialogue: 0,0:00:38.12,0:00:41.50,Default,,0,0,0,,{\\fad(200,200)}示例字幕, with a comma\\N第二行\n\
                   Dialogue: 0,0:01:18.00,0:01:17.00,Default,,0,0,0,,ends before it starts\n";
        let cues = parse_file(Codec::Ass, ass);
        assert_eq!(cues.len(), 1, "{cues:?}");
        assert_eq!((cues[0].0, cues[0].1), (38_120_000_000, 41_500_000_000));
        let doc = to_webvtt(Codec::Ass, &cues, 0);
        assert!(
            doc.contains("00:00:38.120 --> 00:00:41.500\n示例字幕, with a comma\n第二行"),
            "{doc}"
        );
        // SSA v4 orders its fields differently; the Format line says how.
        let ssa =
            "[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                   Dialogue: Marked=0,0:00:01.00,0:00:02.00,Default,NTP,0000,0000,0000,!Effect,Old style\n";
        let cues = parse_file(Codec::Ssa, ssa);
        assert_eq!(cues.len(), 1);
        assert_eq!(&cues[0].2[..], b",,,,,,,,Old style");
        assert!(parse_file(Codec::Pgs, ass).is_empty());
    }

    #[test]
    fn document() {
        let cues = vec![(
            1_000_000_000,
            4_000_000_000,
            Bytes::from_static(b"Hello from flux."),
        )];
        let v = to_webvtt(Codec::Subrip, &cues, 83_416_667);
        assert!(v.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:7507,LOCAL:00:00:00.000\n"));
        assert!(v.contains("00:00:01.000 --> 00:00:04.000\nHello from flux."));
    }
}
