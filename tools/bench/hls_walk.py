"""The HLS client walk shared by jellyfin_playback_bench.py and flux_playback_bench.py.

Exactly what the original Jellyfin harness timed, with demuxed audio added for
servers that serve audio as separate renditions (flux):

  TTFS  = GET master -> GET first variant -> GET its init (EXT-X-MAP) -> GET segment 0
          then, if the master names a default audio rendition with a URI:
          GET audio playlist -> GET its init -> GET its segment 0 (sequential,
          so the audio cost is added in full, never overlapped).
          Jellyfin muxes audio into its video segments, so it has no second leg.
  Seeks = for 600, 3600, 5100, then 30 s: GET the video segment covering the
          target (+ the audio segment covering it, by the audio playlist's own
          EXTINF durations), never fetched before in this session. A seek the
          server answers with an HTTP error is recorded (`error`) and not timed.

Every request is a fresh urllib connection (as the original harness), from the
same machine (loopback): server-side latency, not network.
"""
from __future__ import annotations

import re
import time
import urllib.error
import urllib.parse
import urllib.request

SEEK_TARGETS_S = [600, 3600, 5100, 30]  # 10 min, 60 min, 85 min, back to 30 s


def timed_get(url: str, headers: dict, timeout=120) -> tuple[float, int, bytes, dict]:
    t0 = time.perf_counter()
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        body = r.read()
        return time.perf_counter() - t0, r.status, body, dict(r.headers)


def playlist_lines(text: str) -> list[str]:
    return [l.strip() for l in text.splitlines() if l.strip() and not l.startswith("#")]


def segment_durations(text: str) -> list[float]:
    return [float(m) for m in re.findall(r"#EXTINF:([0-9.]+)", text)]


def default_audio_uri(master: str) -> str | None:
    """URI of the default demuxed audio rendition (EXT-X-MEDIA TYPE=AUDIO), if any."""
    medias = [l for l in master.splitlines() if l.startswith("#EXT-X-MEDIA:") and "TYPE=AUDIO" in l and "URI=" in l]
    pick = next((l for l in medias if "DEFAULT=YES" in l), medias[0] if medias else None)
    if not pick:
        return None
    m = re.search(r'URI="([^"]+)"', pick)
    return m.group(1) if m else None


def starts_of(durs: list[float]) -> tuple[list[float], float]:
    starts, cum = [], 0.0
    for d in durs:
        starts.append(cum)
        cum += d
    return starts, cum


def covering(starts: list[float], target: float) -> int:
    return max(i for i, s in enumerate(starts) if s <= target)


def server_ms(h: dict) -> float | None:
    v = h.get("X-Flux-Ms") or h.get("x-flux-ms")
    try:
        return float(v) if v is not None else None
    except ValueError:
        return None


def walk(master_url: str, headers: dict) -> dict:
    """TTFS and cold seeks for one HLS session. Returns the per-file entry."""
    entry: dict = {}
    t_start = time.perf_counter()
    _, _, master, _ = timed_get(master_url, headers)
    mtext = master.decode()
    variant_url = urllib.parse.urljoin(master_url, playlist_lines(mtext)[0])
    _, _, variant, _ = timed_get(variant_url, headers)
    vtext = variant.decode()
    segs = playlist_lines(vtext)
    durs = segment_durations(vtext)
    init = re.search(r'#EXT-X-MAP:URI="([^"]+)"', vtext)
    if init:
        timed_get(urllib.parse.urljoin(variant_url, init.group(1)), headers)
    seg0_s, _, seg0, seg0_h = timed_get(urllib.parse.urljoin(variant_url, segs[0]), headers)
    entry["ttfs_video_s"] = round(time.perf_counter() - t_start, 4)
    entry["seg0_bytes"] = len(seg0)
    entry["seg0_server_ms"] = server_ms(seg0_h)
    entry["segments"] = len(segs)
    entry["seg_dur_avg_s"] = round(sum(durs) / len(durs), 3) if durs else None

    audio_rel = default_audio_uri(mtext)
    a_url = a_segs = a_starts = None
    if audio_rel:
        a_url = urllib.parse.urljoin(master_url, audio_rel)
        t_a = time.perf_counter()
        _, _, ap, _ = timed_get(a_url, headers)
        atext = ap.decode()
        a_segs = playlist_lines(atext)
        a_starts, _ = starts_of(segment_durations(atext))
        a_init = re.search(r'#EXT-X-MAP:URI="([^"]+)"', atext)
        if a_init:
            timed_get(urllib.parse.urljoin(a_url, a_init.group(1)), headers)
        _, _, a0, a0_h = timed_get(urllib.parse.urljoin(a_url, a_segs[0]), headers)
        entry["audio_leg_s"] = round(time.perf_counter() - t_a, 4)
        entry["audio_seg0_bytes"] = len(a0)
        entry["audio_seg0_server_ms"] = server_ms(a0_h)
        entry["audio_rendition"] = audio_rel
    entry["ttfs_s"] = round(time.perf_counter() - t_start, 4)
    entry["demuxed_audio"] = bool(audio_rel)

    starts, cum = starts_of(durs)
    seeks = []
    for target in SEEK_TARGETS_S:
        if target >= cum:
            continue
        idx = covering(starts, target)
        try:
            dt, status, body, h = timed_get(urllib.parse.urljoin(variant_url, segs[idx]), headers)
        except urllib.error.HTTPError as e:
            # A segment the playlist lists but the server cannot serve (Jellyfin: the
            # last, partial segment of a short file once its ffmpeg run has ended,
            # "transcoding quit before we got there"). Recorded, not timed.
            seeks.append({"target_s": target, "segment": idx, "error": f"HTTP {e.code}",
                          "segments_listed": len(segs), "playlist_end_s": round(cum, 3)})
            continue
        s = {"target_s": target, "segment": idx, "video_s": round(dt, 4), "bytes": len(body),
             "video_server_ms": server_ms(h)}
        total = dt
        if a_url and a_segs and a_starts:
            aidx = covering(a_starts, target)
            adt, _, abody, ah = timed_get(urllib.parse.urljoin(a_url, a_segs[aidx]), headers)
            s.update({"audio_segment": aidx, "audio_s": round(adt, 4), "audio_bytes": len(abody),
                      "audio_server_ms": server_ms(ah)})
            total += adt
        s["latency_s"] = round(total, 4)
        seeks.append(s)
    entry["seeks"] = seeks
    return entry
