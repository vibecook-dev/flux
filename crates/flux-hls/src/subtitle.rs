//! Text subtitles (Matroska SRT/ASS/SSA/WebVTT tracks) → WebVTT sidecars.
//!
//! ASS styling is dropped (lossy, research/03 §7): override blocks `{…}` are
//! removed and `\N` becomes a line break. Clients that can render ASS
//! (JASSUB) should get the original track instead.

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
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
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
