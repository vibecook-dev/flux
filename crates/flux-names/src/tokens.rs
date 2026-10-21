//! Tokenizer for release names and folder names.
//!
//! Splits a stem into words, brackets and stand-alone dashes while keeping
//! byte offsets, so titles can be cut out of the original text (preserving
//! the user's punctuation) instead of being re-joined from tokens.
//!
//! Separator rules, chosen for real release names:
//! - space, `_`, tab: always separators.
//! - `.`: a separator (`The.Matrix.1999`) unless it ends a word and is
//!   followed by whitespace, a closing bracket or the end (`Mr. Robot`,
//!   `Your Name. (2016)`): then it is punctuation that belongs to the word.
//! - `-`: inside a word it is part of the word (`Spider-Man`, `WEB-DL`,
//!   `9-1-1`, `x264-GROUP`, `S01E01-E02`). Standing alone (` - `) it becomes
//!   a [`TokKind::Dash`], the structural separator of Plex/Sonarr names.
//!   Directly after a bracket it starts a word (`[h265]-FLUX` → `-FLUX`).
//! - brackets `[](){}` (and the full-width `【】（）`): tokens of their own.
//! - CJK episode and season markers (`第01集`, `第二季`) are cut out of the
//!   word they are glued to, and words are split where the script changes
//!   between CJK and ASCII letters/digits (`示例4k` → `示例`, `4k`), so the
//!   Latin piece is classified on its own. Hyphenated words are not split
//!   (`x265-某组` keeps its release group).

use std::sync::LazyLock;

use regex::Regex;

use crate::text::is_cjk;

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokKind {
    Word,
    /// `(`, `[` or `{`.
    Open(u8),
    /// `)`, `]` or `}`.
    Close(u8),
    /// A stand-alone `-` (usually ` - `).
    Dash,
}

/// A token with its byte span in the tokenized string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tok {
    pub kind: TokKind,
    pub start: usize,
    pub end: usize,
}

impl Tok {
    pub fn text<'a>(&self, s: &'a str) -> &'a str {
        &s[self.start..self.end]
    }
    pub fn is_word(&self) -> bool {
        self.kind == TokKind::Word
    }
}

/// `第12集`, `第十二话`, `第3季`: a CJK episode or season marker inside a word.
static CJK_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"第(?:[0-9０-９]{1,4}|[零〇一二两兩三四五六七八九十百千]{1,7})[集话話回期季]")
        .expect("valid CJK marker regex")
});

/// Splits `s` into tokens (see the module docs for the rules).
pub(crate) fn tokenize(s: &str) -> Vec<Tok> {
    let toks = split_separators(s);
    if s.is_ascii() {
        toks
    } else {
        refine_cjk(s, toks)
    }
}

/// Opening and closing brackets, full-width ones mapped to their ASCII form.
fn bracket(c: char) -> Option<TokKind> {
    Some(match c {
        '[' | '(' | '{' => TokKind::Open(c as u8),
        ']' | ')' | '}' => TokKind::Close(c as u8),
        '【' => TokKind::Open(b'['),
        '】' => TokKind::Close(b']'),
        '（' => TokKind::Open(b'('),
        '）' => TokKind::Close(b')'),
        _ => return None,
    })
}

/// Cuts CJK markers out of words and splits words at CJK↔ASCII boundaries.
fn refine_cjk(s: &str, toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len() + 4);
    for t in toks {
        let w = t.text(s);
        if !t.is_word() || w.is_ascii() {
            out.push(t);
            continue;
        }
        let mut pos = 0;
        for m in CJK_MARKER_RE.find_iter(w) {
            split_scripts(w, t.start, pos, m.start(), &mut out);
            out.push(Tok {
                kind: TokKind::Word,
                start: t.start + m.start(),
                end: t.start + m.end(),
            });
            pos = m.end();
        }
        split_scripts(w, t.start, pos, w.len(), &mut out);
    }
    out
}

/// Pushes `w[lo..hi]` (at byte offset `base`), split where a CJK character
/// and an ASCII letter or digit touch.
fn split_scripts(w: &str, base: usize, lo: usize, hi: usize, out: &mut Vec<Tok>) {
    if lo >= hi {
        return;
    }
    let piece = &w[lo..hi];
    let mut start = lo;
    if !piece.contains('-') {
        let mut prev: Option<bool> = None;
        for (i, c) in piece.char_indices() {
            let script = if is_cjk(c) {
                Some(true)
            } else if c.is_ascii_alphanumeric() {
                Some(false)
            } else {
                None
            };
            if let (Some(a), Some(b)) = (prev, script) {
                if a != b && lo + i > start {
                    out.push(Tok {
                        kind: TokKind::Word,
                        start: base + start,
                        end: base + lo + i,
                    });
                    start = lo + i;
                }
            }
            prev = script;
        }
    }
    out.push(Tok {
        kind: TokKind::Word,
        start: base + start,
        end: base + hi,
    });
}

/// The separator pass: words, brackets and stand-alone dashes.
fn split_separators(s: &str) -> Vec<Tok> {
    let mut toks = Vec::with_capacity(16);
    let mut word_start: Option<usize> = None;
    let bytes = s.as_bytes();
    let flush = |toks: &mut Vec<Tok>, word_start: &mut Option<usize>, end: usize| {
        if let Some(st) = word_start.take() {
            if end > st {
                toks.push(Tok {
                    kind: TokKind::Word,
                    start: st,
                    end,
                });
            }
        }
    };
    for (i, c) in s.char_indices() {
        match c {
            ' ' | '_' | '\t' => flush(&mut toks, &mut word_start, i),
            '.' => {
                let next = bytes.get(i + 1).copied();
                let ends_word = matches!(next, None | Some(b' ') | Some(b')') | Some(b']') | Some(b'}'));
                if word_start.is_some() && ends_word {
                    // Keep the dot as part of the word ("Mr.", "Name.").
                    continue;
                }
                flush(&mut toks, &mut word_start, i);
            }
            '[' | '(' | '{' | ']' | ')' | '}' | '【' | '】' | '（' | '）' => {
                flush(&mut toks, &mut word_start, i);
                if let Some(kind) = bracket(c) {
                    toks.push(Tok {
                        kind,
                        start: i,
                        end: i + c.len_utf8(),
                    });
                }
            }
            '-' => {
                if word_start.is_some() {
                    continue;
                }
                let next = bytes.get(i + 1).copied();
                let after_bracket =
                    matches!(toks.last(), Some(Tok { kind: TokKind::Close(_), end, .. }) if *end == i);
                if after_bracket && next.is_some_and(|b| b.is_ascii_alphanumeric()) {
                    // "[h265]-FLUX": the release group glued to a bracket.
                    word_start = Some(i);
                } else {
                    toks.push(Tok {
                        kind: TokKind::Dash,
                        start: i,
                        end: i + 1,
                    });
                }
            }
            _ => {
                if word_start.is_none() {
                    word_start = Some(i);
                }
            }
        }
    }
    flush(&mut toks, &mut word_start, s.len());
    toks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        tokenize(s)
            .iter()
            .map(|t| match t.kind {
                TokKind::Word => t.text(s).to_string(),
                TokKind::Dash => "<->".into(),
                TokKind::Open(c) | TokKind::Close(c) => (c as char).to_string(),
            })
            .collect()
    }

    #[test]
    fn dotted_names_split_on_dots() {
        assert_eq!(words("The.Matrix.1999.1080p"), ["The", "Matrix", "1999", "1080p"]);
        assert_eq!(words("S.W.A.T.2017"), ["S", "W", "A", "T", "2017"]);
    }

    #[test]
    fn abbreviation_dots_stay_in_words() {
        assert_eq!(words("Mr. Robot"), ["Mr.", "Robot"]);
        assert_eq!(words("Your Name. (2016)"), ["Your", "Name.", "(", "2016", ")"]);
    }

    #[test]
    fn cjk_and_latin_split_where_the_script_changes() {
        assert_eq!(words("示例片4k"), ["示例片", "4k"]);
        assert_eq!(words("示例剧.S01E01中英字幕"), ["示例剧", "S01E01", "中英字幕"]);
        assert_eq!(words("X示例2.Film"), ["X", "示例", "2", "Film"]);
        // A hyphenated word keeps its release group whole.
        assert_eq!(words("x265-某字幕组"), ["x265-某字幕组"]);
        // Punctuation between the scripts is not a split point.
        assert_eq!(words("示例300：副题"), ["示例", "300：副题"]);
    }

    #[test]
    fn cjk_markers_are_cut_out() {
        assert_eq!(words("示例剧_第01集"), ["示例剧", "第01集"]);
        assert_eq!(words("示例剧第十二集"), ["示例剧", "第十二集"]);
        assert_eq!(
            words("示例剧第2季第3话标题"),
            ["示例剧", "第2季", "第3话", "标题"]
        );
    }

    #[test]
    fn full_width_brackets() {
        assert_eq!(
            words("【某站】示例（2019）"),
            ["[", "某站", "]", "示例", "(", "2019", ")"]
        );
        let s = "【某站】示例";
        let t = tokenize(s);
        assert_eq!(t[3].text(s), "示例");
    }

    #[test]
    fn hyphens_and_dashes() {
        assert_eq!(
            words("Spider-Man - S01E01-E02"),
            ["Spider-Man", "<->", "S01E01-E02"]
        );
        assert_eq!(words("[h265]-FLUX"), ["[", "h265", "]", "-FLUX"]);
        assert_eq!(words("x264-GROUP"), ["x264-GROUP"]);
    }
}
