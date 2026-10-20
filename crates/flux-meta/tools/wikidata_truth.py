#!/usr/bin/env python3
"""Resolve the REAL_MOVIES list of tools/corpus/gen-library.py to TMDB movie ids
via Wikidata (CC0; property P4947 = TMDB movie ID). No TMDB key needed, and no
TMDB content is involved: Wikidata is the AI-safe crosswalk (research/05 §2.4).

Writes tests/fixtures/real_movies_truth.json:
  [{"title", "year", "tmdb_id", "qid", "wd_label", "wd_year"}, ...]
Rows Wikidata cannot resolve get tmdb_id = null and are excluded from accuracy.

Usage: python3 -I crates/flux-meta/tools/wikidata_truth.py
"""
from __future__ import annotations

import ast
import json
import re
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
GEN = REPO / "tools" / "corpus" / "gen-library.py"
OUT = HERE.parent / "tests" / "fixtures" / "real_movies_truth.json"
UA = "flux-meta-prototype/0.1 (https://github.com/vibecook-dev/flux; matcher ground truth)"
LANGS = ["en", "fr", "ja", "ko", "de", "es", "it", "zh", "pt", "sv"]


def real_movies() -> list[tuple[str, int]]:
    src = GEN.read_text(encoding="utf-8")
    m = re.search(r"^REAL_MOVIES = (\[.*?^\])", src, re.S | re.M)
    if not m:
        sys.exit("REAL_MOVIES not found")
    return ast.literal_eval(m.group(1))


def sparql(q: str) -> list[dict]:
    url = "https://query.wikidata.org/sparql?" + urllib.parse.urlencode({"query": q})
    req = urllib.request.Request(url, headers={"Accept": "application/sparql-results+json", "User-Agent": UA})
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return json.load(r)["results"]["bindings"]
        except Exception as e:  # noqa: BLE001 - retry anything transient
            print(f"  retry {attempt}: {e}", file=sys.stderr)
            time.sleep(2 + 3 * attempt)
    return []


def lookup(title: str, year: int) -> dict:
    lit = json.dumps(title)  # SPARQL string literal escaping is JSON-compatible for our inputs
    values = " ".join(f"{lit}@{l}" for l in LANGS)
    q = f"""
SELECT DISTINCT ?item ?tmdb ?enLabel ?date WHERE {{
  VALUES ?l {{ {values} }}
  {{ ?item rdfs:label ?l }} UNION {{ ?item skos:altLabel ?l }}
  ?item wdt:P4947 ?tmdb .
  OPTIONAL {{ ?item wdt:P577 ?date }}
  OPTIONAL {{ ?item rdfs:label ?enLabel FILTER(LANG(?enLabel) = "en") }}
}} LIMIT 50"""
    rows = sparql(q)
    best = None
    for r in rows:
        d = r.get("date", {}).get("value", "")
        y = int(d[:4]) if d[:4].isdigit() else None
        if y is None:
            continue
        delta = abs(y - year)
        if delta > 1:
            continue
        cand = (delta, r["item"]["value"].rsplit("/", 1)[-1], int(r["tmdb"]["value"]),
                r.get("enLabel", {}).get("value"), y)
        if best is None or cand[:1] < best[:1]:
            best = cand
    if best is None:
        return {"title": title, "year": year, "tmdb_id": None, "qid": None, "wd_label": None, "wd_year": None}
    return {"title": title, "year": year, "tmdb_id": best[2], "qid": best[1], "wd_label": best[3], "wd_year": best[4]}


def main() -> None:
    out = []
    seen = set()
    for title, year in real_movies():
        if (title, year) in seen:  # the list repeats a few titles on purpose
            continue
        seen.add((title, year))
        row = lookup(title, year)
        print(f"{title!r} {year} -> {row['tmdb_id']} {row['qid']} {row['wd_label']!r} {row['wd_year']}")
        out.append(row)
        time.sleep(0.4)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(out, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({sum(1 for r in out if r['tmdb_id'])}/{len(out)} resolved)")


if __name__ == "__main__":
    main()
