//! `FIELD=VALUE` and `FIELD:=JSON` for `flux edit`, read the way each field
//! takes its value.

use serde_json::{Map, Value};

/// Fields that take a list of names.
const LISTS: [&str; 7] = [
    "genres",
    "tags",
    "languages",
    "countries",
    "directors",
    "writers",
    "studios",
];

/// The `set` object of an edit, from `FIELD=VALUE` and `FIELD:=JSON`
/// arguments (`ids.PROVIDER=ID` nests under `ids`).
pub fn assignments(args: &[String]) -> Result<Map<String, Value>, String> {
    let mut set = Map::new();
    let mut ids = Map::new();
    for a in args {
        let (field, value) = if let Some((f, raw)) = a.split_once(":=") {
            let v: Value = serde_json::from_str(raw).map_err(|e| format!("{f}:= takes JSON ({e})"))?;
            (f.trim().to_string(), v)
        } else if let Some((f, text)) = a.split_once('=') {
            let f = f.trim().to_string();
            let v = typed(&f, text);
            (f, v)
        } else {
            return Err(format!("{a:?}: write FIELD=VALUE (or FIELD:=JSON)"));
        };
        if field.is_empty() {
            return Err(format!("{a:?} names no field"));
        }
        match field.strip_prefix("ids.") {
            Some(p) => {
                ids.insert(p.to_string(), value);
            }
            None => {
                set.insert(field, value);
            }
        }
    }
    if !ids.is_empty() {
        set.insert("ids".into(), Value::Object(ids));
    }
    Ok(set)
}

/// `text` as `field` takes it.
fn typed(field: &str, text: &str) -> Value {
    let t = text.trim();
    if field == "year" {
        if let Ok(n) = t.parse::<u64>() {
            return Value::from(n);
        }
    }
    if LISTS.contains(&field) {
        return Value::Array(split(t, ',').into_iter().map(Value::String).collect());
    }
    if field == "links" {
        return Value::Array(
            split(t, ',')
                .into_iter()
                .map(|u| serde_json::json!({ "url": u }))
                .collect(),
        );
    }
    if field == "cast" {
        // "Name as Role; Name"
        return Value::Array(
            split(t, ';')
                .into_iter()
                .map(|m| match m.split_once(" as ") {
                    Some((n, r)) => serde_json::json!({ "name": n.trim(), "role": r.trim() }),
                    None => serde_json::json!({ "name": m }),
                })
                .collect(),
        );
    }
    Value::String(text.to_string())
}

fn split(t: &str, sep: char) -> Vec<String> {
    t.split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A summary for an edit given none: what it sets and clears.
pub fn default_summary(set: &Map<String, Value>, clear: &[String]) -> String {
    let mut fields: Vec<String> = set
        .iter()
        .flat_map(|(k, v)| match (k.as_str(), v) {
            ("ids", Value::Object(ids)) => ids.keys().map(|p| format!("the {p} id")).collect::<Vec<_>>(),
            (k, _) => vec![k.replace('_', " ")],
        })
        .collect();
    fields.dedup();
    let mut parts = Vec::new();
    if !fields.is_empty() {
        parts.push(format!("Set {}", fields.join(", ")));
    }
    if !clear.is_empty() {
        parts.push(format!("cleared {}", clear.join(", ")));
    }
    let s = parts.join("; ");
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// A file fix from `KEY=VALUE` arguments (`flux fix`): `kind`, `of`,
/// `title`, `year`, `season`, `episodes` (comma-separated), `extra`,
/// `edition`, `part`.
pub fn fix(values: &[String]) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    for v in values {
        let (k, val) = v
            .split_once('=')
            .ok_or_else(|| format!("{v:?}: give KEY=VALUE"))?;
        let (k, val) = (k.trim(), val.trim());
        let number = |what: &str| -> Result<Value, String> {
            val.parse::<u64>()
                .map(Value::from)
                .map_err(|_| format!("{what} is a number, not {val:?}"))
        };
        let parsed = match k {
            "kind" | "title" | "extra" | "edition" => Value::from(val),
            "of" | "year" | "season" | "part" => number(k)?,
            "episodes" | "episode" => {
                let mut list = Vec::new();
                for e in val.split([',', ' ']).filter(|e| !e.is_empty()) {
                    list.push(
                        e.trim_start_matches(['e', 'E'])
                            .parse::<u64>()
                            .map(Value::from)
                            .map_err(|_| format!("episodes are numbers, not {e:?}"))?,
                    );
                }
                Value::Array(list)
            }
            other => {
                return Err(format!(
                    "{other:?} isn't something a fix says (kind, of, title, year, season, episodes, extra, edition, part)"
                ))
            }
        };
        let key = if k == "episode" { "episodes" } else { k };
        out.insert(key.into(), parsed);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fix_reads_its_keys() {
        let f = fix(&[
            "of=40".into(),
            "season=2".into(),
            "episodes=5,6".into(),
            "kind=episode".into(),
        ])
        .unwrap();
        assert_eq!(
            Value::Object(f),
            serde_json::json!({"of": 40, "season": 2, "episodes": [5, 6], "kind": "episode"})
        );
        assert!(fix(&["year=soon".into()]).unwrap_err().contains("number"));
        assert!(fix(&["colour=red".into()])
            .unwrap_err()
            .contains("isn't something"));
    }
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn values_take_each_fields_shape() {
        let set = assignments(&args(&[
            "title=Spirited Away",
            "year=2001",
            "directors=Hayao Miyazaki",
            "genres=Animation, Fantasy,",
            "cast=Rumi Hiiragi as Chihiro; Miyu Irino",
            "links=https://a.example, https://b.example",
            "ids.tmdb=129",
            "ids.wikidata=Q155653",
            "tags:=[\"Ghibli\"]",
            "overview=A girl, a bathhouse, a spell.",
        ]))
        .unwrap();
        assert_eq!(
            Value::Object(set),
            json!({
                "title": "Spirited Away",
                "year": 2001,
                "directors": ["Hayao Miyazaki"],
                "genres": ["Animation", "Fantasy"],
                "cast": [{"name": "Rumi Hiiragi", "role": "Chihiro"}, {"name": "Miyu Irino"}],
                "links": [{"url": "https://a.example"}, {"url": "https://b.example"}],
                "ids": {"tmdb": "129", "wikidata": "Q155653"},
                "tags": ["Ghibli"],
                "overview": "A girl, a bathhouse, a spell.",
            })
        );
        assert!(assignments(&args(&["title"])).is_err());
        assert!(assignments(&args(&["tags:=[oops"])).is_err());
    }

    #[test]
    fn a_default_summary_names_what_changes() {
        let set = assignments(&args(&["year=2001", "ids.tmdb=129"])).unwrap();
        assert_eq!(
            default_summary(&set, &["tags".into()]),
            "Set the tmdb id, year; cleared tags"
        );
        assert_eq!(default_summary(&Map::new(), &["tags".into()]), "Cleared tags");
    }
}
