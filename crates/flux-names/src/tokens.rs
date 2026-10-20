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
//! - brackets `[](){}`: tokens of their own.

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

/// Splits `s` into tokens (see the module docs for the rules).
pub(crate) fn tokenize(s: &str) -> Vec<Tok> {
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
            '[' | '(' | '{' => {
                flush(&mut toks, &mut word_start, i);
                toks.push(Tok {
                    kind: TokKind::Open(c as u8),
                    start: i,
                    end: i + 1,
                });
            }
            ']' | ')' | '}' => {
                flush(&mut toks, &mut word_start, i);
                toks.push(Tok {
                    kind: TokKind::Close(c as u8),
                    start: i,
                    end: i + 1,
                });
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
    fn hyphens_and_dashes() {
        assert_eq!(
            words("Spider-Man - S01E01-E02"),
            ["Spider-Man", "<->", "S01E01-E02"]
        );
        assert_eq!(words("[h265]-FLUX"), ["[", "h265", "]", "-FLUX"]);
        assert_eq!(words("x264-GROUP"), ["x264-GROUP"]);
    }
}
