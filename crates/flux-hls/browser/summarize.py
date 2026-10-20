#!/usr/bin/env python3
"""Summarize flux-hls browser check results (run.mjs JSON) as Markdown.

    python3 crates/flux-hls/browser/summarize.py RESULT.json [RESULT2.json ...]
"""
import json
import statistics
import sys


def med(xs):
    xs = [x for x in xs if x is not None]
    return round(statistics.median(xs), 1) if xs else None


def p95(xs):
    xs = sorted(x for x in xs if x is not None)
    return round(xs[min(len(xs) - 1, round(0.95 * (len(xs) - 1)))], 1) if xs else None


for path in sys.argv[1:]:
    d = json.load(open(path))
    cases = d["cases"]
    ok = sum(1 for c in cases if c.get("ok"))
    print(f"### {d['browser']} — {path.split('/')[-1]}")
    print(f"{d.get('user_agent','')}; load {d.get('load_avg_1m_start')}→{d.get('load_avg_1m_end')}; **{ok}/{len(cases)} ok**")
    print()
    print("| file | preset | plan | TTFF ms | seeks (first frame) ms | drift p50/p95 ms | dropped/decoded | audio switches | subs | I-frames | notes |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for c in cases:
        seeks = "/".join(str(s.get("firstFrameMs")) for s in c.get("seeks", []))
        dr = c.get("drift") or {}
        q = (c.get("quality") or {}).get("final") or {}
        sw = c.get("audioSwitch")
        sw_s = (
            f"{sum(1 for x in sw['switches'] if x['ok'])}/{len(sw['switches'])}" if sw else "—"
        )
        st = c.get("subtitles")
        st_s = (f"ok ({st.get('firstCueErrorMs')} ms)" if st.get("ok") else "FAIL") if st else "—"
        ifr = c.get("iframes")
        if_s = ("ok" if ifr.get("ok") else "FAIL") if ifr else "—"
        notes = "; ".join((c.get("problems") or []) + (c.get("warnings") or []))[:140]
        nonfatal = [e["details"] for e in c.get("hlsErrors", []) if not e.get("fatal")]
        if nonfatal:
            notes = (notes + "; " if notes else "") + "hls.js non-fatal: " + ",".join(sorted(set(nonfatal)))
        print(
            f"| {c['file']} | {c['preset']} | {c.get('label','')} | {c.get('ttffMs')} | {seeks} | "
            f"{dr.get('p50', 0):.1f}/{dr.get('p95', 0):.1f} | {q.get('dropped')}/{q.get('decoded')} | {sw_s} | {st_s} | {if_s} | {notes} |"
        )
    ttff = [c.get("ttffMs") for c in cases]
    seeks = [s.get("firstFrameMs") for c in cases for s in c.get("seeks", [])]
    print()
    print(
        f"Medians: TTFF {med(ttff)} ms (p95 {p95(ttff)}), seek to first frame {med(seeks)} ms (p95 {p95(seeks)}), "
        f"{sum(len(c.get('seeks', [])) for c in cases)} seeks."
    )
    print()
