//! Deterministic poster typography: never let a model draw the title.
//!
//! - **Fonts** (SIL OFL 1.1, bundled in `assets/fonts`): *Fraunces* (variable
//!   display serif) for titles, *Inter* (variable sans) for the year and
//!   subtitles. Titles in scripts those fonts lack (CJK, Hangul, Telugu, …)
//!   fall back to the first system font that covers every character
//!   ([`Typesetter::fallback_paths`]); fonts are never bundled for those.
//! - **Shaping** with `rustybuzz` (HarfBuzz port: GPOS kerning, ligatures,
//!   complex scripts), **rasterising** with `ab_glyph` at the same variation
//!   coordinates.
//! - **Fitting**: the title is broken into 1–4 lines with balanced widths; the
//!   layout with the largest type wins, with a small penalty per extra line. A
//!   `Title: Subtitle` pair is set as a large title over a small tracked subtitle.
//! - **Rendering**: a soft drop shadow (blurred coverage) under warm-white text,
//!   all on top of the gradient scrim drawn by the poster composer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, VariableFont};
use image::RgbImage;
use rustybuzz::ttf_parser::Tag;

static FRAUNCES: &[u8] = include_bytes!("../../assets/fonts/Fraunces-Variable.ttf");
static INTER: &[u8] = include_bytes!("../../assets/fonts/Inter-Variable.ttf");

/// Font bytes: bundled (static) or loaded from disk (shared).
#[derive(Clone)]
enum FontData {
    Static(&'static [u8]),
    Shared(Arc<Vec<u8>>),
}

impl FontData {
    fn bytes(&self) -> &[u8] {
        match self {
            FontData::Static(b) => b,
            FontData::Shared(b) => b,
        }
    }
}

/// A font face plus variation coordinates.
#[derive(Clone)]
pub struct FontSpec {
    data: FontData,
    index: u32,
    variations: Vec<([u8; 4], f32)>,
}

impl std::fmt::Debug for FontSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontSpec")
            .field("index", &self.index)
            .field("variations", &self.variations)
            .finish()
    }
}

impl FontSpec {
    /// The bundled title face: Fraunces at display optical size, semibold, a little soft.
    pub fn title() -> Self {
        FontSpec {
            data: FontData::Static(FRAUNCES),
            index: 0,
            variations: vec![
                (*b"opsz", 144.0),
                (*b"wght", 620.0),
                (*b"SOFT", 30.0),
                (*b"WONK", 0.0),
            ],
        }
    }

    /// The bundled label face: Inter medium.
    pub fn label() -> Self {
        FontSpec {
            data: FontData::Static(INTER),
            index: 0,
            variations: vec![(*b"opsz", 18.0), (*b"wght", 560.0)],
        }
    }

    fn rb_face(&self) -> Option<rustybuzz::Face<'_>> {
        let mut face = rustybuzz::Face::from_slice(self.data.bytes(), self.index)?;
        let vars: Vec<rustybuzz::Variation> = self
            .variations
            .iter()
            .map(|(t, v)| rustybuzz::Variation {
                tag: Tag::from_bytes(t),
                value: *v,
            })
            .collect();
        face.set_variations(&vars);
        Some(face)
    }

    fn ab_font(&self) -> Option<FontRef<'_>> {
        let mut f = FontRef::try_from_slice_and_index(self.data.bytes(), self.index).ok()?;
        for (t, v) in &self.variations {
            f.set_variation(t, *v);
        }
        Some(f)
    }

    /// True when the face has a glyph for every non-whitespace character.
    pub fn covers(&self, text: &str) -> bool {
        self.rb_face().is_some_and(|f| {
            text.chars()
                .filter(|c| !c.is_whitespace())
                .all(|c| f.glyph_index(c).is_some())
        })
    }

    fn coverage_count(&self, text: &str) -> usize {
        self.rb_face().map_or(0, |f| {
            text.chars()
                .filter(|c| !c.is_whitespace() && f.glyph_index(*c).is_some())
                .count()
        })
    }
}

/// Glyphs of one shaped run, in font units.
struct Shaped {
    glyphs: Vec<(u16, f32, f32, f32)>, // (glyph id, x pen, x offset, y offset)
    advance: f32,
    upem: f32,
}

fn shape(spec: &FontSpec, text: &str) -> Option<Shaped> {
    let face = spec.rb_face()?;
    let upem = face.units_per_em() as f32;
    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(text);
    buf.guess_segment_properties();
    let out = rustybuzz::shape(&face, &[], buf);
    let mut pen = 0.0f32;
    let mut glyphs = Vec::with_capacity(out.len());
    for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
        glyphs.push((
            info.glyph_id as u16,
            pen,
            pos.x_offset as f32,
            pos.y_offset as f32,
        ));
        pen += pos.x_advance as f32;
    }
    Some(Shaped {
        glyphs,
        advance: pen,
        upem,
    })
}

/// Width in px of `text` at `size` px with `tracking` em of extra letter spacing.
fn measure(spec: &FontSpec, text: &str, size: f32, tracking: f32) -> f32 {
    shape(spec, text).map_or(0.0, |s| {
        let n = s.glyphs.len().saturating_sub(1) as f32;
        s.advance / s.upem * size + n * tracking * size
    })
}

/// A coverage mask the size of the target image.
struct Mask {
    w: u32,
    h: u32,
    a: Vec<f32>,
}

impl Mask {
    fn new(w: u32, h: u32) -> Self {
        Mask {
            w,
            h,
            a: vec![0.0; (w * h) as usize],
        }
    }
}

/// Draw `text` with its baseline at `y`, starting at `x`.
fn draw_run(mask: &mut Mask, spec: &FontSpec, text: &str, size: f32, tracking: f32, x: f32, y: f32) {
    let (Some(s), Some(font)) = (shape(spec, text), spec.ab_font()) else {
        return;
    };
    let Some(upem) = font.units_per_em() else { return };
    let k = size / s.upem;
    let scale = PxScale::from(size * font.height_unscaled() / upem);
    for (i, &(gid, pen, xo, yo)) in s.glyphs.iter().enumerate() {
        let gx = x + (pen + xo) * k + i as f32 * tracking * size;
        let gy = y - yo * k;
        let glyph = GlyphId(gid).with_scale_and_position(scale, point(gx, gy));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let b = outlined.px_bounds();
            outlined.draw(|px, py, c| {
                let (ix, iy) = (b.min.x as i64 + i64::from(px), b.min.y as i64 + i64::from(py));
                if ix >= 0 && iy >= 0 && (ix as u32) < mask.w && (iy as u32) < mask.h {
                    let idx = (iy as u32 * mask.w + ix as u32) as usize;
                    mask.a[idx] = (mask.a[idx] + c).min(1.0);
                }
            });
        }
    }
}

/// Separable box blur (prefix sums, edge-normalised), `passes` times (3 ≈ Gaussian).
fn blur(src: &[f32], w: usize, h: usize, radius: usize, passes: u32) -> Vec<f32> {
    let mut a = src.to_vec();
    if radius == 0 || w == 0 || h == 0 {
        return a;
    }
    let mut tmp = vec![0f32; a.len()];
    let mut pre = vec![0f32; w.max(h) + 1];
    for _ in 0..passes {
        for y in 0..h {
            for x in 0..w {
                pre[x + 1] = pre[x] + a[y * w + x];
            }
            for x in 0..w {
                let (lo, hi) = (x.saturating_sub(radius), (x + radius + 1).min(w));
                tmp[y * w + x] = (pre[hi] - pre[lo]) / (hi - lo) as f32;
            }
        }
        for x in 0..w {
            for y in 0..h {
                pre[y + 1] = pre[y] + tmp[y * w + x];
            }
            for y in 0..h {
                let (lo, hi) = (y.saturating_sub(radius), (y + radius + 1).min(h));
                a[y * w + x] = (pre[hi] - pre[lo]) / (hi - lo) as f32;
            }
        }
    }
    a
}

/// Typesetter configuration and font fallback.
pub struct Typesetter {
    pub title_font: FontSpec,
    pub label_font: FontSpec,
    /// System fonts tried (in order) for titles the bundled fonts cannot set.
    pub fallback_paths: Vec<PathBuf>,
    /// Text colour (warm white).
    pub color: [u8; 3],
    loaded: Mutex<HashMap<PathBuf, Option<FontSpec>>>,
}

impl std::fmt::Debug for Typesetter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Typesetter")
            .field("fallback_paths", &self.fallback_paths)
            .finish_non_exhaustive()
    }
}

/// Default fallback fonts: common macOS and Linux locations (CJK, Hangul,
/// Indic, wide coverage), or on Windows the fonts it ships in
/// `%WINDIR%\Fonts` ([`windows_fallback_paths`]).
pub fn default_fallback_paths() -> Vec<PathBuf> {
    if cfg!(windows) {
        let windir = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot"));
        return windows_fallback_paths(&windows_fonts_dir(windir));
    }
    [
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/System/Library/Fonts/KohinoorTelugu.ttc",
        "/System/Library/Fonts/Kohinoor.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Bold.ttc",
        "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Fonts Windows ships, one group per script, best first (bold faces for
/// titles; the collections hold a bold face).
const WINDOWS_FONTS: &[&[&str]] = &[
    // Chinese, Simplified and Traditional (and kana): Microsoft YaHei, SimHei, SimSun.
    &["msyhbd.ttc", "msyh.ttc", "simhei.ttf", "simsun.ttc"],
    // Japanese: Yu Gothic, Meiryo, MS Gothic.
    &["YuGothB.ttc", "meiryob.ttc", "meiryo.ttc", "msgothic.ttc"],
    // Korean: Malgun Gothic, Gulim.
    &["malgunbd.ttf", "malgun.ttf", "gulim.ttc"],
    // Indic scripts (Devanagari, Bengali, Tamil, Telugu, …): Nirmala UI.
    &["NirmalaB.ttf", "Nirmala.ttf"],
    // Latin, Greek, Cyrillic, Hebrew, Arabic: Arial, Segoe UI.
    &["arialbd.ttf", "segoeuib.ttf"],
    // Symbols: Segoe UI Symbol.
    &["seguisym.ttf"],
];

/// `%WINDIR%\Fonts`, from the variable's value (`C:\Windows` without one).
pub fn windows_fonts_dir(windir: Option<std::ffi::OsString>) -> PathBuf {
    windir
        .filter(|w| !w.is_empty())
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
        .join("Fonts")
}

/// The Windows fallbacks in `fonts`: the first file of each script's group
/// that exists. One per script, because a title only the last group covers
/// loads every font before it (each stays loaded, some are 15–20 MB).
pub fn windows_fallback_paths(fonts: &Path) -> Vec<PathBuf> {
    WINDOWS_FONTS
        .iter()
        .filter_map(|group| group.iter().map(|f| fonts.join(f)).find(|p| p.is_file()))
        .collect()
}

/// Says once per process (then at debug level) that no font covers every
/// character of `text`: its poster shows boxes for the missing glyphs.
fn note_uncovered(text: &str, tried: &[PathBuf]) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        tracing::debug!(text, "no font covers every character");
    } else {
        tracing::warn!(
            text,
            fallbacks = ?tried,
            "no font covers every character of this title, so frame posters show boxes for the \
             missing ones; install a font for its script (Noto Sans CJK, for example)"
        );
    }
}

impl Default for Typesetter {
    fn default() -> Self {
        Typesetter {
            title_font: FontSpec::title(),
            label_font: FontSpec::label(),
            fallback_paths: default_fallback_paths(),
            color: [246, 241, 231],
            loaded: Mutex::new(HashMap::new()),
        }
    }
}

/// Split `Title: Subtitle` / `Title - Subtitle` (first separator only).
pub(crate) fn split_subtitle(title: &str) -> (&str, Option<&str>) {
    for sep in [": ", " - ", " – ", " — "] {
        if let Some((a, b)) = title.split_once(sep) {
            let (a, b) = (a.trim(), b.trim());
            if !a.is_empty() && !b.is_empty() {
                return (a, Some(b));
            }
        }
    }
    (title.trim(), None)
}

/// Break `words` into `n` lines minimising the widest line (DP; titles are short).
/// `end_penalty[j]` is added to a line that ends after word `j` (unless it is
/// the last word), so lines avoid ending on "of", "the", "and"….
fn balanced_lines(widths: &[f32], space: f32, n: usize, end_penalty: &[f32]) -> Option<(Vec<usize>, f32)> {
    let m = widths.len();
    if n == 0 || n > m {
        return None;
    }
    let line_w = |i: usize, j: usize| widths[i..j].iter().sum::<f32>() + space * (j - i - 1) as f32;
    let cost = |i: usize, j: usize| {
        line_w(i, j)
            + if j < m {
                end_penalty.get(j - 1).copied().unwrap_or(0.0)
            } else {
                0.0
            }
    };
    // best[k][j] = minimal max cost setting words[..j] in k lines; prev for reconstruction.
    let mut best = vec![vec![f32::INFINITY; m + 1]; n + 1];
    let mut prev = vec![vec![0usize; m + 1]; n + 1];
    best[0][0] = 0.0;
    for k in 1..=n {
        for j in k..=m {
            for i in (k - 1)..j {
                let v = best[k - 1][i].max(cost(i, j));
                if v < best[k][j] {
                    best[k][j] = v;
                    prev[k][j] = i;
                }
            }
        }
    }
    let mut breaks = Vec::with_capacity(n);
    let mut j = m;
    for k in (1..=n).rev() {
        breaks.push(j);
        j = prev[k][j];
    }
    breaks.reverse();
    // Report the true widest line (without penalties) for sizing.
    let mut start = 0;
    let mut widest = 0f32;
    for &e in &breaks {
        widest = widest.max(line_w(start, e));
        start = e;
    }
    Some((breaks, widest))
}

/// Words a line should not end on (English, French, Spanish, German, Italian).
const WEAK_LINE_ENDS: &[&str] = &[
    "a", "an", "the", "of", "and", "or", "in", "on", "at", "to", "for", "by", "with", "from", "de", "la",
    "le", "les", "du", "des", "el", "los", "las", "y", "et", "der", "die", "das", "und", "il", "lo", "di",
    "&",
];

/// True for scripts written without spaces between words (Han, kana).
fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF66..=0xFF9F)
}

/// Characters that must not start a line in Japanese/Chinese (kinsoku shori, abridged).
fn no_line_start(c: char) -> bool {
    "、。，．・ー！？）」』】〕〉》っゃゅょぁぃぅぇぉッャュョァィゥェォ…‥".contains(c)
}

/// Break units for line wrapping: words for spaced scripts, characters for
/// CJK (with kinsoku: closing punctuation and small kana stick to the previous
/// character). Returns `(units, joiner)`.
fn break_units(text: &str) -> (Vec<String>, &'static str) {
    let cjk = text.chars().filter(|&c| is_cjk(c)).count();
    if cjk * 2 >= text.chars().filter(|c| !c.is_whitespace()).count().max(1) && !text.contains(' ') {
        let mut units: Vec<String> = Vec::new();
        for c in text.chars() {
            match units.last_mut() {
                Some(last)
                    if no_line_start(c) || !is_cjk(c) && last.chars().last().is_some_and(|l| !is_cjk(l)) =>
                {
                    last.push(c)
                }
                _ => units.push(c.to_string()),
            }
        }
        (units, "")
    } else {
        let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        (
            if words.is_empty() {
                vec![text.to_owned()]
            } else {
                words
            },
            " ",
        )
    }
}

/// Typographic quotes and apostrophes for display (`Grandma's` → `Grandma’s`).
pub fn display_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut open_double = true;
    let mut prev: Option<char> = None;
    for c in s.chars() {
        match c {
            '\'' => out.push(if prev.is_none_or(char::is_whitespace) {
                '‘'
            } else {
                '’'
            }),
            '"' => {
                out.push(if open_double { '“' } else { '”' });
                open_double = !open_double;
            }
            _ => out.push(c),
        }
        prev = Some(c);
    }
    out
}

/// One typeset line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlacedLine {
    pub text: String,
    pub size: f32,
    pub tracking: f32,
    pub x: f32,
    pub baseline: f32,
    pub label: bool,
    pub alpha: f32,
}

/// Where the text block starts (for the gradient), and its lines.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PosterText {
    pub top: f32,
    pub lines: Vec<PlacedLine>,
    pub title_font_fallback: bool,
}

impl Typesetter {
    /// A process-wide default typesetter (fonts parsed lazily).
    pub fn shared() -> &'static Typesetter {
        static TS: OnceLock<Typesetter> = OnceLock::new();
        TS.get_or_init(Typesetter::default)
    }

    fn load_fallback(&self, path: &Path) -> Option<FontSpec> {
        let mut cache = self.loaded.lock().ok()?;
        if let Some(spec) = cache.get(path) {
            return spec.clone();
        }
        let spec = std::fs::read(path).ok().map(|bytes| {
            let bytes = Arc::new(bytes);
            // In collections prefer a bold/semibold face (W6, Bold, SemiBold).
            let n = rustybuzz::ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
            let index = (0..n)
                .find(|&i| {
                    rustybuzz::ttf_parser::Face::parse(&bytes, i)
                        .ok()
                        .is_some_and(|f| {
                            f.names().into_iter().any(|nm| {
                                nm.name_id == rustybuzz::ttf_parser::name_id::FULL_NAME
                                    && nm.to_string().is_some_and(|s| {
                                        s.contains("W6") || s.contains("Bold") || s.contains("SemiBold")
                                    })
                            })
                        })
                })
                .unwrap_or(0);
            FontSpec {
                data: FontData::Shared(bytes),
                index,
                variations: vec![(*b"wght", 700.0)],
            }
        });
        cache.insert(path.to_owned(), spec.clone());
        spec
    }

    /// The font to set `text` in: the preferred one if it covers the text,
    /// else the first covering fallback, else whichever covers most.
    fn pick_font(&self, preferred: &FontSpec, text: &str) -> (FontSpec, bool) {
        if preferred.covers(text) {
            return (preferred.clone(), false);
        }
        let mut best: Option<(usize, FontSpec)> = None;
        for p in &self.fallback_paths {
            if let Some(spec) = self.load_fallback(p) {
                if spec.covers(text) {
                    return (spec, true);
                }
                let c = spec.coverage_count(text);
                if best.as_ref().is_none_or(|(bc, _)| c > *bc) {
                    best = Some((c, spec));
                }
            }
        }
        note_uncovered(text, &self.fallback_paths);
        match best {
            Some((c, spec)) if c > preferred.coverage_count(text) => (spec, true),
            _ => (preferred.clone(), false),
        }
    }

    /// Lay out a poster's title block for a `w×h` poster.
    pub(crate) fn layout_poster(&self, w: u32, h: u32, title: &str, year: Option<u16>) -> PosterText {
        let (wf, hf) = (w as f32, h as f32);
        let avail = wf * 0.84;
        let (main, sub) = split_subtitle(title);
        let (title_font, fell_back) = self.pick_font(&self.title_font, main);
        let label_for = |t: &str| self.pick_font(&self.label_font, t).0;
        let title_tracking = if fell_back { 0.02 } else { -0.01 };
        let lh = 1.04f32;

        // Title: choose the line count giving the largest type.
        let (words, joiner) = break_units(main);
        let unit = |t: &str| measure(&title_font, t, 1.0, title_tracking);
        let widths: Vec<f32> = words.iter().map(|wd| unit(wd)).collect();
        let space = if joiner.is_empty() {
            0.0
        } else {
            unit("a b") - unit("ab")
        };
        let penalties: Vec<f32> = words
            .iter()
            .map(|w| {
                if WEAK_LINE_ENDS.contains(&w.to_lowercase().as_str()) {
                    0.9
                } else {
                    0.0
                }
            })
            .collect();
        // Very short titles ("M", "Up", "Her") may go bigger than the general cap.
        let short = main.chars().count() <= 4;
        let (s_max, max_block) = (wf * if short { 0.21 } else { 0.165 }, hf * 0.30);
        let mut chosen: Option<(f32, Vec<usize>)> = None;
        let mut chosen_obj = f32::MIN;
        let max_lines = if joiner.is_empty() { 3 } else { 4 };
        for n in 1..=words.len().min(max_lines) {
            let Some((breaks, maxw)) = balanced_lines(&widths, space, n, &penalties) else {
                continue;
            };
            let mut size = s_max.min(avail / maxw.max(1e-3));
            size = size.min(max_block / (n as f32 * lh));
            let obj = size * (1.0 - 0.07 * (n as f32 - 1.0));
            if obj > chosen_obj {
                chosen_obj = obj;
                chosen = Some((size, breaks));
            }
        }
        let (title_size, breaks) = chosen.unwrap_or((wf * 0.08, vec![words.len()]));
        let mut title_lines = Vec::new();
        let mut start = 0;
        for &end in &breaks {
            title_lines.push(words[start..end].join(joiner));
            start = end;
        }

        // Subtitle: small, uppercase, tracked; ≤ 2 balanced lines.
        let sub_tracking = 0.14;
        let mut sub_lines: Vec<(String, f32, FontSpec)> = Vec::new();
        if let Some(sub) = sub {
            let upper = sub.to_uppercase();
            let font = label_for(&upper);
            let size = (title_size * 0.34).clamp(wf * 0.030, wf * 0.046);
            let sw: Vec<&str> = upper.split_whitespace().collect();
            let unit_w: Vec<f32> = sw.iter().map(|t| measure(&font, t, 1.0, sub_tracking)).collect();
            let sp = measure(&font, "A B", 1.0, sub_tracking) - measure(&font, "AB", 1.0, sub_tracking);
            let one = balanced_lines(&unit_w, sp, 1, &[]).map_or(f32::MAX, |(_, mw)| mw * size);
            let (n, size) = if one <= avail || sw.len() < 2 {
                (1, size.min(avail / (one / size).max(1e-3)))
            } else {
                let two = balanced_lines(&unit_w, sp, 2, &[]).map_or(f32::MAX, |(_, mw)| mw);
                (2, size.min(avail / two.max(1e-3)))
            };
            if let Some((br, _)) = balanced_lines(&unit_w, sp, n, &[]) {
                let mut s0 = 0;
                for &e in &br {
                    sub_lines.push((sw[s0..e].join(" "), size, font.clone()));
                    s0 = e;
                }
            }
        }

        // Stack from the bottom up.
        let mut lines = Vec::new();
        let mut y = hf * (1.0 - 0.062);
        let year_size = (wf * 0.033).max(12.0);
        let year_tracking = 0.32;
        if let Some(yv) = year {
            let t = yv.to_string();
            let tw = measure(&self.label_font, &t, year_size, year_tracking);
            lines.push(PlacedLine {
                text: t,
                size: year_size,
                tracking: year_tracking,
                x: (wf - tw) / 2.0 + year_tracking * year_size / 2.0,
                baseline: y,
                label: true,
                alpha: 0.82,
            });
            y -= year_size * 2.1;
        }
        for (text, size, font) in sub_lines.iter().rev() {
            let tw = measure(font, text, *size, sub_tracking);
            lines.push(PlacedLine {
                text: text.clone(),
                size: *size,
                tracking: sub_tracking,
                x: (wf - tw) / 2.0 + sub_tracking * size / 2.0,
                baseline: y,
                label: true,
                alpha: 0.92,
            });
            y -= size * 1.45;
        }
        if !sub_lines.is_empty() {
            y -= title_size * 0.12;
        } else if year.is_some() {
            y -= title_size * 0.05;
        }
        // Descenders of the last title line need room above the labels.
        y -= title_size * 0.18;
        for text in title_lines.iter().rev() {
            let tw = measure(&title_font, text, title_size, title_tracking);
            lines.push(PlacedLine {
                text: text.clone(),
                size: title_size,
                tracking: title_tracking,
                x: (wf - tw) / 2.0,
                baseline: y,
                label: false,
                alpha: 1.0,
            });
            y -= title_size * lh;
        }
        let top = y + title_size * (lh - 0.75);
        lines.reverse();
        PosterText {
            top,
            lines,
            title_font_fallback: fell_back,
        }
    }

    /// Draw a laid-out text block onto `img` with a soft shadow.
    pub(crate) fn draw(&self, img: &mut RgbImage, text: &PosterText, title: &str) {
        let (w, h) = img.dimensions();
        let (main, _) = split_subtitle(title);
        let (title_font, _) = self.pick_font(&self.title_font, main);
        // coverage: for the shadow; paint: coverage × line opacity, for the text itself.
        let mut coverage = Mask::new(w, h);
        let mut paint = vec![0f32; (w * h) as usize];
        for l in &text.lines {
            let font = if l.label {
                self.pick_font(&self.label_font, &l.text).0
            } else {
                title_font.clone()
            };
            let mut m = Mask::new(w, h);
            draw_run(&mut m, &font, &l.text, l.size, l.tracking, l.x, l.baseline);
            for ((c, p), &v) in coverage.a.iter_mut().zip(paint.iter_mut()).zip(&m.a) {
                *c = (*c + v).min(1.0);
                *p = (*p + v * l.alpha).min(1.0);
            }
        }
        // Soft shadow: blurred coverage, offset down a little, 55% black.
        let radius = (w as f32 * 0.006).round().max(2.0) as usize;
        let shadow = blur(&coverage.a, w as usize, h as usize, radius, 3);
        let dy = (w as f32 * 0.003).round() as u32;
        for y in 0..h {
            for x in 0..w {
                let s = shadow[(y.saturating_sub(dy) * w + x) as usize] * 0.55;
                let a = paint[(y * w + x) as usize];
                if s <= 0.0 && a <= 0.0 {
                    continue;
                }
                let p = img.get_pixel_mut(x, y);
                for (c, &tc) in self.color.iter().enumerate() {
                    let base = f32::from(p.0[c]) * (1.0 - s);
                    p.0[c] = (base * (1.0 - a) + f32::from(tc) * a).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_subtitles() {
        assert_eq!(
            split_subtitle("Mad Max: Fury Road"),
            ("Mad Max", Some("Fury Road"))
        );
        assert_eq!(split_subtitle("Heat"), ("Heat", None));
        assert_eq!(
            split_subtitle("Star Wars: Episode IV - A New Hope"),
            ("Star Wars", Some("Episode IV - A New Hope"))
        );
    }

    #[test]
    fn balanced_breaks() {
        let (b, w) = balanced_lines(&[3.0, 1.0, 1.0, 3.0], 0.5, 2, &[]).unwrap();
        assert_eq!(b, vec![2, 4]);
        assert!((w - 4.5).abs() < 1e-6);
        assert!(balanced_lines(&[1.0], 0.5, 2, &[]).is_none());
        // "The Lord of | the Rings" → "The Lord | of the Rings" once "of" is a weak line end.
        let widths = [1.6, 2.0, 0.9, 1.4, 2.4];
        let pen = [0.0, 0.0, 0.9, 0.9, 0.0];
        let (plain, _) = balanced_lines(&widths, 0.3, 2, &[]).unwrap();
        let (nice, _) = balanced_lines(&widths, 0.3, 2, &pen).unwrap();
        assert_eq!(plain, vec![3, 5]);
        assert_eq!(nice, vec![2, 5]);
    }

    #[test]
    fn cjk_breaks_between_characters_with_kinsoku() {
        let (u, j) = break_units("千と千尋の神隠し");
        assert_eq!((u.len(), j), (8, ""));
        let (u, _) = break_units("ゴジラ-1.0");
        assert!(u.iter().any(|x| x.contains("-1.0") || x == "ラ-1.0"), "{u:?}");
        let (u, _) = break_units("ちょっと");
        assert!(
            u.iter().all(|x| !x.starts_with('ょ') && !x.starts_with('っ')),
            "{u:?}"
        );
        let (u, j) = break_units("The Matrix");
        assert_eq!((u.len(), j), (2, " "));
    }

    #[test]
    fn smart_quotes() {
        assert_eq!(display_text("Grandma's 80th"), "Grandma’s 80th");
        assert_eq!(display_text("'Salem's Lot"), "‘Salem’s Lot");
        assert_eq!(display_text("The \"Best\" Film"), "The “Best” Film");
    }

    #[test]
    fn bundled_fonts_shape_and_cover_latin() {
        let t = FontSpec::title();
        assert!(t.covers("Amélie — Léon, Ça va? 1999"));
        assert!(!t.covers("기생충"));
        let wide = measure(&t, "WWWW", 100.0, 0.0);
        let narrow = measure(&t, "iiii", 100.0, 0.0);
        assert!(wide > narrow * 2.0, "{wide} {narrow}");
        // Kerning via GPOS: "AV" is narrower than "A" + "V".
        let av = measure(&t, "AV", 100.0, 0.0);
        assert!(av < measure(&t, "A", 100.0, 0.0) + measure(&t, "V", 100.0, 0.0));
    }

    /// Windows had no fallbacks, so CJK titles were drawn as boxes.
    #[test]
    fn windows_fallbacks_take_one_font_per_script_from_the_fonts_dir() {
        assert_eq!(
            windows_fonts_dir(Some(r"D:\Win".into())),
            Path::new(r"D:\Win").join("Fonts")
        );
        assert_eq!(windows_fonts_dir(None), Path::new(r"C:\Windows").join("Fonts"));
        let dir = tempfile::tempdir().unwrap();
        for f in [
            "msyh.ttc",
            "simsun.ttc",
            "malgunbd.ttf",
            "malgun.ttf",
            "seguisym.ttf",
        ] {
            std::fs::write(dir.path().join(f), b"").unwrap();
        }
        let names: Vec<String> = windows_fallback_paths(dir.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["msyh.ttc", "malgunbd.ttf", "seguisym.ttf"]);
        assert!(windows_fallback_paths(&dir.path().join("none")).is_empty());
    }

    /// The system fallbacks set Chinese and Korean titles on the desktop
    /// systems that always ship such fonts (Linux may have none).
    #[test]
    fn system_fonts_set_cjk_titles() {
        let ts = Typesetter::default();
        for title in ["示例剧 第二季", "예시 드라마"] {
            let (font, fell_back) = ts.pick_font(&ts.title_font, title);
            if cfg!(any(windows, target_os = "macos")) {
                assert!(
                    fell_back && font.covers(title),
                    "{title}: {:?}",
                    ts.fallback_paths
                );
            } else if !fell_back {
                eprintln!("skipping {title}: no system font covers it");
            }
        }
    }

    #[test]
    fn layout_fits_inside_the_poster() {
        let ts = Typesetter::default();
        for title in [
            "M",
            "Heat",
            "The Lord of the Rings: The Fellowship of the Ring",
            "Everything Everywhere All at Once",
        ] {
            let pt = ts.layout_poster(800, 1200, title, Some(2001));
            assert!(
                pt.top > 1200.0 * 0.45,
                "{title}: block starts too high ({})",
                pt.top
            );
            for l in &pt.lines {
                let font = if l.label {
                    ts.label_font.clone()
                } else {
                    ts.title_font.clone()
                };
                let w = measure(&font, &l.text, l.size, l.tracking);
                assert!(l.x >= 0.0 && l.x + w <= 800.0 + 1.0, "{title}: {l:?} width {w}");
            }
        }
    }

    #[test]
    fn draws_pixels() {
        let ts = Typesetter::default();
        let mut img = RgbImage::from_pixel(400, 600, image::Rgb([20, 20, 30]));
        let pt = ts.layout_poster(400, 600, "Heat", Some(1995));
        ts.draw(&mut img, &pt, "Heat");
        let bright = img.pixels().filter(|p| p.0[0] > 200).count();
        assert!(bright > 500, "{bright}");
    }
}
