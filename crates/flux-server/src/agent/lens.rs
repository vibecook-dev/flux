//! `lens.check`: a lens kit lens checked as the web client checks it
//! (`web/src/lens/kit/runtime.ts`, `validateSpec`), with defaults filled in,
//! plus warnings for what the browser will remove (`sanitizeHTML`,
//! `scopeCSS`), so an agent can fix its lens before anyone sees it. The
//! browser's sanitiser and worker remain the safety boundary.

use std::sync::OnceLock;

use flux_core::agent::{LensCheck, LensInput, LensSpec};
use regex::Regex;
use serde_json::Value;

pub const MAX_NAME: usize = 28;
pub const MAX_BLURB: usize = 160;
pub const MAX_CSS: usize = 14_000;
pub const MAX_ITEM: usize = 4_000;
pub const MAX_SCENE: usize = 4_000;
pub const MAX_LAYOUT: usize = 16_000;

/// Tags the browser removes from `item` and `scene` (runtime.ts `BAD_TAGS`).
const BAD_TAGS: [&str; 39] = [
    "script",
    "iframe",
    "object",
    "embed",
    "link",
    "meta",
    "style",
    "base",
    "form",
    "input",
    "button",
    "textarea",
    "select",
    "option",
    "video",
    "audio",
    "source",
    "track",
    "img",
    "picture",
    "image",
    "use",
    "foreignobject",
    "template",
    "noscript",
    "frame",
    "frameset",
    "portal",
    "a",
    "area",
    "map",
    "canvas",
    "dialog",
    "slot",
    "animate",
    "set",
    "animatemotion",
    "animatetransform",
    "handler",
];

/// The placeholders an item template may use.
const PLACEHOLDERS: [&str; 10] = [
    "title", "year", "meta", "kind", "decade", "seasons", "badges", "index", "number", "show",
];

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("a valid pattern"))
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The id a lens named `name` is kept under on the server: `s-` and the
/// name's slug (runtime.ts `slug`), so adding it again replaces it.
pub fn server_id(name: &str) -> String {
    use unicode_normalization::char::is_combining_mark;
    use unicode_normalization::UnicodeNormalization;
    let folded: String = name
        .to_lowercase()
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .collect();
    let mut slug = String::new();
    for c in folded.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(24).collect();
    let slug = slug.trim_end_matches('-');
    format!("s-{}", if slug.is_empty() { "lens" } else { slug })
}

/// Check `raw` as a lens.
pub fn check(raw: &Value) -> LensCheck {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let Some(r) = raw.as_object() else {
        return LensCheck {
            ok: false,
            errors: vec!["A lens is a JSON object with a name, css, item and layout.".into()],
            warnings,
            lens: None,
            id: None,
        };
    };
    let text = |k: &str| -> String {
        match r.get(k) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        }
    };
    for k in r.keys() {
        if ![
            "name", "blurb", "input", "step", "window", "animated", "css", "scene", "item", "layout", "id",
        ]
        .contains(&k.as_str())
        {
            warnings.push(format!("{k:?} isn't part of a lens and is ignored."));
        }
    }
    let layout = text("layout");
    static LAYOUT_FN: OnceLock<Regex> = OnceLock::new();
    static IMPORT: OnceLock<Regex> = OnceLock::new();
    if layout.trim().is_empty() {
        errors.push(
            "This lens has no layout. It needs \"layout\": \"function layout(items, view) { … }\".".into(),
        );
    } else if chars(&layout) > MAX_LAYOUT {
        errors.push(format!(
            "The layout code is longer than {MAX_LAYOUT} characters. Make it simpler."
        ));
    } else if !re(
        &LAYOUT_FN,
        r"function\s+layout\s*\(|(?:const|let|var)\s+layout\s*=",
    )
    .is_match(&layout)
    {
        errors.push("The layout code must define a function called layout(items, view).".into());
    } else if re(&IMPORT, r"\bimport\b").is_match(&layout) {
        errors.push(
            "The layout code can't load other code (import). Everything it uses must be in the lens.".into(),
        );
    }
    for (word, why) in [
        ("fetch", "has no network"),
        ("XMLHttpRequest", "has no network"),
        ("WebSocket", "has no network"),
        ("localStorage", "has no storage"),
        ("document", "has no DOM"),
        ("window", "has no window"),
        ("eval", "can't run eval"),
    ] {
        if Regex::new(&format!(r"\b{word}\b")).is_ok_and(|w| w.is_match(&layout)) {
            warnings.push(format!(
                "The layout uses {word}, but it runs in a locked-down worker that {why}."
            ));
        }
    }
    let css = text("css");
    if chars(&css) > MAX_CSS {
        errors.push(format!("The CSS is longer than {MAX_CSS} characters."));
    }
    let name_raw = text("name");
    let name = cut(name_raw.trim(), MAX_NAME);
    let name = if name.is_empty() {
        warnings.push("The lens has no name: it will be called \"My lens\".".into());
        "My lens".to_string()
    } else {
        if chars(name_raw.trim()) > MAX_NAME {
            warnings.push(format!("The name is cut to {MAX_NAME} characters: {name:?}."));
        }
        name
    };
    let input = match r.get("input").and_then(Value::as_str) {
        Some("y") => LensInput::Y,
        Some("none") => LensInput::None,
        Some("x") | None => LensInput::X,
        Some(other) => {
            warnings.push(format!("input {other:?} isn't x, y or none: x it is."));
            LensInput::X
        }
    };
    let num = |k: &str| {
        r.get(k)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n != 0.0)
    };
    let step = num("step").unwrap_or(160.0).clamp(30.0, 600.0);
    if num("step").is_some_and(|s| !(30.0..=600.0).contains(&s)) {
        warnings.push(format!("step is kept between 30 and 600 pixels: {step}."));
    }
    let window = num("window").map(f64::round).unwrap_or(40.0).clamp(8.0, 150.0) as u32;
    if num("window").is_some_and(|w| !(8.0..=150.0).contains(&w.round())) {
        warnings.push(format!("window is kept between 8 and 150 titles: {window}."));
    }
    let blurb_raw = text("blurb");
    let blurb = cut(blurb_raw.trim(), MAX_BLURB);
    let blurb = if blurb.is_empty() {
        format!("A lens called {name}.")
    } else {
        blurb
    };
    let mut item = text("item");
    if chars(&item) > MAX_ITEM {
        warnings.push(format!("The item template is cut to {MAX_ITEM} characters."));
        item = cut(&item, MAX_ITEM);
    }
    if item.trim().is_empty() {
        item = "<div class=\"art\"></div>".into();
    } else if !item.contains("class=\"art\"") && !item.contains("class='art'") {
        warnings.push(
            "The item template has no <div class=\"art\"></div>: titles show without their posters.".into(),
        );
    }
    let mut scene = text("scene");
    if chars(&scene) > MAX_SCENE {
        warnings.push(format!("The scene is cut to {MAX_SCENE} characters."));
        scene = cut(&scene, MAX_SCENE);
    }
    html_warnings("item", &item, &mut warnings);
    html_warnings("scene", &scene, &mut warnings);
    css_warnings(&css, &mut warnings);
    static PLACEHOLDER: OnceLock<Regex> = OnceLock::new();
    for m in re(&PLACEHOLDER, r"\{\{\s*([A-Za-z_]+)\s*\}\}").captures_iter(&item) {
        let p = &m[1];
        if !PLACEHOLDERS.contains(&p) {
            warnings.push(format!(
                "{{{{{p}}}}} isn't a placeholder flux fills in (it has {}).",
                PLACEHOLDERS.join(", ")
            ));
        }
    }
    let ok = errors.is_empty();
    let lens = ok.then(|| LensSpec {
        name: name.clone(),
        blurb,
        input,
        step,
        window,
        animated: r.get("animated") == Some(&Value::Bool(true)),
        css,
        scene,
        item,
        layout,
    });
    LensCheck {
        ok,
        errors,
        warnings,
        id: ok.then(|| server_id(&name)),
        lens,
    }
}

fn html_warnings(part: &str, html: &str, w: &mut Vec<String>) {
    static TAG: OnceLock<Regex> = OnceLock::new();
    static ATTR: OnceLock<Regex> = OnceLock::new();
    let mut tags: Vec<String> = Vec::new();
    for m in re(&TAG, r"<\s*([A-Za-z][A-Za-z0-9:-]*)").captures_iter(html) {
        let t = m[1].to_ascii_lowercase();
        if BAD_TAGS.contains(&t.as_str()) && !tags.contains(&t) {
            tags.push(t);
        }
    }
    if !tags.is_empty() {
        w.push(format!(
            "flux removes <{}> from the {part}: only plain markup, classes and simple SVG shapes stay (the poster goes in <div class=\"art\">).",
            tags.join(">, <")
        ));
    }
    let mut attrs: Vec<String> = Vec::new();
    for m in re(&ATTR, r#"\s([A-Za-z][A-Za-z0-9:-]*)\s*="#).captures_iter(html) {
        let a = m[1].to_ascii_lowercase();
        let kept = matches!(
            a.as_str(),
            "class"
                | "role"
                | "title"
                | "viewbox"
                | "d"
                | "cx"
                | "cy"
                | "r"
                | "rx"
                | "ry"
                | "x"
                | "y"
                | "x1"
                | "y1"
                | "x2"
                | "y2"
                | "width"
                | "height"
                | "points"
                | "fill"
                | "stroke"
                | "stroke-width"
                | "stroke-linecap"
                | "stroke-linejoin"
                | "opacity"
                | "fill-opacity"
                | "stroke-opacity"
                | "transform"
                | "preserveaspectratio"
        ) || a.starts_with("aria-")
            || a.starts_with("data-");
        if !kept && !attrs.contains(&a) {
            attrs.push(a);
        }
    }
    if !attrs.is_empty() {
        w.push(format!(
            "flux removes these attributes from the {part}: {} (style belongs in the lens CSS; no handlers or links).",
            attrs.join(", ")
        ));
    }
    let lower = html.to_ascii_lowercase().replace(char::is_whitespace, "");
    if lower.contains("javascript:") || lower.contains("url(") {
        w.push(format!(
            "The {part} refers to javascript: or url(), which flux removes."
        ));
    }
}

fn css_warnings(css: &str, w: &mut Vec<String>) {
    let lower = css.to_ascii_lowercase();
    for (needle, what) in [
        ("@import", "@import"),
        (
            "@font-face",
            "@font-face (use var(--font-body), var(--font-mono) or var(--font-cond))",
        ),
        ("url(", "url() (draw with gradients instead)"),
        ("image-set(", "image-set()"),
        ("expression(", "expression()"),
        ("-moz-binding", "-moz-binding"),
        ("behavior:", "behavior"),
    ] {
        if lower.contains(needle) {
            w.push(format!("flux removes {what} from the CSS."));
        }
    }
    static ROOT: OnceLock<Regex> = OnceLock::new();
    if re(&ROOT, r"(?m)(^|[}\s,])(:root|html|body)\s*[{,]").is_match(&lower) {
        w.push("CSS rules for :root, html or body are dropped: style .lens and what's inside it.".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({
            "name": "Departure board",
            "blurb": "Titles on a split-flap board.",
            "input": "y",
            "step": 90,
            "window": 30,
            "css": ".lens .item { color: var(--text); }",
            "item": "<div class=\"art\"></div><b class=\"t\">{{title}}</b>",
            "layout": "function layout(items, view) { return items.map(it => ({ y: (it.i - view.pos) * 90 })); }"
        })
    }

    #[test]
    fn a_good_lens_passes_with_its_values() {
        let c = check(&good());
        assert!(c.ok, "{:?}", c.errors);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let l = c.lens.unwrap();
        assert_eq!(
            (l.input, l.step, l.window, l.animated),
            (LensInput::Y, 90.0, 30, false)
        );
        assert_eq!(c.id.as_deref(), Some("s-departure-board"));
    }

    #[test]
    fn errors_are_what_must_change() {
        let mut v = good();
        v["layout"] = json!("const f = 1;");
        assert!(check(&v).errors[0].contains("function called layout"));
        v["layout"] = json!("function layout(items) { import('x'); }");
        assert!(check(&v).errors[0].contains("import"));
        v["layout"] = json!("");
        assert!(!check(&v).ok);
        assert!(!check(&json!([1])).ok);
        let mut v = good();
        v["css"] = json!("x".repeat(MAX_CSS + 1));
        assert!(!check(&v).ok);
    }

    #[test]
    fn warnings_say_what_the_browser_removes() {
        let mut v = good();
        v["item"] = json!("<img src=\"x.png\"><div class=\"t\" style=\"color:red\" onclick=\"go()\">{{title}} {{rating}}</div>");
        v["css"] = json!("@import 'x.css'; body { margin: 0 } .lens .t { background: url(a.png) }");
        v["layout"] = json!("function layout(items, view) { fetch('/api'); return []; }");
        v["step"] = json!(1000);
        v["extra"] = json!(1);
        let c = check(&v);
        assert!(c.ok);
        let all = c.warnings.join("\n");
        for needle in [
            "<img>",
            "style",
            "onclick",
            "{{rating}}",
            "@import",
            "url()",
            "body",
            "fetch",
            "no <div class=\"art\">",
            "step is kept",
            "\"extra\"",
        ] {
            assert!(all.contains(needle), "{needle} in:\n{all}");
        }
        assert_eq!(c.lens.unwrap().step, 600.0);
    }

    #[test]
    fn defaults_and_ids() {
        let c = check(&json!({ "layout": "function layout(items, view) { return []; }" }));
        assert!(c.ok);
        let l = c.lens.unwrap();
        assert_eq!((l.name.as_str(), l.step, l.window), ("My lens", 160.0, 40));
        assert_eq!(l.item, "<div class=\"art\"></div>");
        assert_eq!(server_id("Café Noir — 1950s"), "s-cafe-noir-1950s");
        assert_eq!(server_id("東京"), "s-lens");
    }
}
