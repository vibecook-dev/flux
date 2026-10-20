#!/usr/bin/env python3
"""Generate a large, realistically messy media library for scan benchmarks.

Files are APFS clones (clonefile) of a few small seed clips, so a 30k-file
library costs almost no disk. Each clone gets a short format-appropriate
trailer with a unique id, so partial-content hashes (e.g. the OpenSubtitles
hash over the first/last 64 KiB) differ between files, just as real files do.

A ground-truth manifest (manifest.jsonl) records what every path *should*
parse to, so filename parsers and matchers can be scored for accuracy.

Usage:
  tools/corpus/gen-library.py [--out DIR] [--movies N] [--shows N] [--seed S]

--out defaults to $FLUX_TEST_MEDIA/library (the test corpus; see CONTRIBUTING.md).
"""
from __future__ import annotations

import argparse
import ctypes
import ctypes.util
import json
import os
import random
import shutil
import subprocess
import sys
import uuid
from pathlib import Path

DEFAULT_OUT = os.path.join(os.environ["FLUX_TEST_MEDIA"], "library") if os.environ.get("FLUX_TEST_MEDIA") else None

# A slice of real, well-known titles (ground truth for parser/matcher tests).
REAL_MOVIES = [
    ("The Matrix", 1999), ("Inception", 2010), ("Interstellar", 2014), ("Parasite", 2019),
    ("Spirited Away", 2001), ("Amélie", 2001), ("Léon: The Professional", 1994),
    ("2001: A Space Odyssey", 1968), ("Blade Runner 2049", 2017), ("1917", 2019),
    ("Blade Runner", 1982), ("Alien", 1979), ("Aliens", 1986), ("The Thing", 1982),
    ("Heat", 1995), ("Se7en", 1995), ("Fight Club", 1999), ("The Prestige", 2006),
    ("The Dark Knight", 2008), ("Mad Max: Fury Road", 2015), ("Arrival", 2016),
    ("Dune", 2021), ("Dune: Part Two", 2024), ("Oppenheimer", 2023), ("Everything Everywhere All at Once", 2022),
    ("Princess Mononoke", 1997), ("Akira", 1988), ("Ghost in the Shell", 1995), ("Your Name.", 2016),
    ("Oldboy", 2003), ("Memories of Murder", 2003), ("The Handmaiden", 2016), ("Burning", 2018),
    ("Crouching Tiger, Hidden Dragon", 2000), ("In the Mood for Love", 2000), ("Hero", 2002),
    ("City of God", 2002), ("Pan's Labyrinth", 2006), ("Roma", 2018), ("The Lives of Others", 2006),
    ("Das Boot", 1981), ("Run Lola Run", 1998), ("La Haine", 1995), ("The Intouchables", 2011),
    ("Seven Samurai", 1954), ("Rashomon", 1950), ("Ikiru", 1952), ("Tokyo Story", 1953),
    ("Casablanca", 1942), ("Vertigo", 1958), ("Psycho", 1960), ("Rear Window", 1954),
    ("The Godfather", 1972), ("The Godfather Part II", 1974), ("Apocalypse Now", 1979),
    ("Jaws", 1975), ("E.T. the Extra-Terrestrial", 1982), ("Back to the Future", 1985),
    ("Back to the Future Part II", 1989), ("Terminator 2: Judgment Day", 1991), ("Jurassic Park", 1993),
    ("Pulp Fiction", 1994), ("The Shawshank Redemption", 1994), ("Forrest Gump", 1994),
    ("Toy Story", 1995), ("Toy Story 2", 1999), ("Toy Story 3", 2010), ("WALL·E", 2008), ("Up", 2009),
    ("Ratatouille", 2007), ("Coco", 2017), ("Inside Out", 2015), ("Spider-Man: Into the Spider-Verse", 2018),
    ("The Lord of the Rings: The Fellowship of the Ring", 2001), ("The Lord of the Rings: The Two Towers", 2002),
    ("The Lord of the Rings: The Return of the King", 2003), ("Gladiator", 2000), ("Memento", 2000),
    ("No Country for Old Men", 2007), ("There Will Be Blood", 2007), ("Zodiac", 2007), ("The Social Network", 2010),
    ("Drive", 2011), ("Her", 2013), ("Gravity", 2013), ("Whiplash", 2014), ("Ex Machina", 2014),
    ("Moonlight", 2016), ("Get Out", 2017), ("Knives Out", 2019), ("Joker", 2019), ("Tenet", 2020),
    ("Nomadland", 2020), ("The Batman", 2022), ("Top Gun: Maverick", 2022), ("Barbie", 2023),
    ("Past Lives", 2023), ("Poor Things", 2023), ("The Zone of Interest", 2023), ("Anatomy of a Fall", 2023),
    ("Godzilla Minus One", 2023), ("The Boy and the Heron", 2023), ("Furiosa: A Mad Max Saga", 2024),
    ("Civil War", 2024), ("The Substance", 2024), ("Anora", 2024), ("Conclave", 2024), ("Flow", 2024),
    ("King Kong", 1933), ("King Kong", 1976), ("King Kong", 2005), ("Dune", 1984),
    ("Scarface", 1932), ("Scarface", 1983), ("Ocean's Eleven", 1960), ("Ocean's Eleven", 2001),
    ("The Fly", 1958), ("The Fly", 1986), ("Solaris", 1972), ("Solaris", 2002),
    ("M", 1931), ("Up", 2009), ("Her", 2013), ("Us", 2019), ("It", 2017), ("It", 1990),
    ("Nope", 2022), ("Tár", 2022), ("Aftersun", 2022), ("RRR", 2022), ("Drive My Car", 2021),
    ("千と千尋の神隠し", 2001), ("기생충", 2019), ("Le Fabuleux Destin d'Amélie Poulain", 2001),
]

REAL_SHOWS = [
    ("Breaking Bad", 2008, [7, 13, 13, 13, 16]), ("Better Call Saul", 2015, [10, 10, 10, 10, 10, 13]),
    ("The Wire", 2002, [13, 12, 12, 13, 10]), ("The Sopranos", 1999, [13, 13, 13, 13, 13, 21]),
    ("Game of Thrones", 2011, [10, 10, 10, 10, 10, 10, 7, 6]), ("Succession", 2018, [10, 10, 9, 10]),
    ("Severance", 2022, [9, 10]), ("The Bear", 2022, [8, 10, 10]), ("Shōgun", 2024, [10]),
    ("Stranger Things", 2016, [8, 9, 8, 9]), ("Dark", 2017, [10, 8, 8]), ("Chernobyl", 2019, [5]),
    ("The Office", 2005, [6, 22, 25, 19, 28, 26, 26, 24, 25]), ("Friends", 1994, [24] * 10),
    ("Seinfeld", 1989, [5, 12, 23, 24, 22, 24, 24, 22, 24]), ("The Simpsons", 1989, [13, 22, 24]),
    ("Doctor Who", 2005, [13, 13, 13, 13]), ("Doctor Who", 1963, [8]), ("Battlestar Galactica", 2004, [13, 20, 20, 20]),
    ("The Expanse", 2015, [10, 13, 13, 10, 10, 6]), ("Fargo", 2014, [10, 10, 10, 11, 10]),
    ("Twin Peaks", 1990, [8, 22]), ("Arcane", 2021, [9, 9]), ("Bluey", 2018, [52, 52, 49]),
    ("Avatar: The Last Airbender", 2005, [20, 20, 21]), ("Cowboy Bebop", 1998, [26]),
    ("Neon Genesis Evangelion", 1995, [26]), ("Frieren: Beyond Journey's End", 2023, [28]),
    ("Attack on Titan", 2013, [25, 12, 22, 30]), ("One Piece", 1999, [61]),
    ("The Last of Us", 2023, [9, 7]), ("House of the Dragon", 2022, [10, 8]), ("Andor", 2022, [12, 12]),
    ("Ted Lasso", 2020, [10, 12, 12]), ("Slow Horses", 2022, [6, 6, 6, 6]), ("Fleabag", 2016, [6, 6]),
]

GROUPS = ["SPARKS", "RARBG", "NTb", "FLUX", "DON", "CtrlHD", "EVO", "YIFY", "TGx", "PSA", "playWEB", "NOGRP"]
SOURCES = ["BluRay", "WEB-DL", "WEBRip", "HDTV", "BDRip", "REMUX", "DVDRip", "AMZN.WEB-DL", "NF.WEB-DL"]
RES = ["480p", "720p", "1080p", "2160p"]
CODECS = ["x264", "x265", "H.264", "HEVC", "AV1", "XviD"]
AUDIO = ["DTS-HD.MA.5.1", "TrueHD.Atmos.7.1", "DDP5.1", "AAC2.0", "AC3", "FLAC2.0", "Opus"]
EDITIONS = ["Director's Cut", "Extended Edition", "Theatrical Cut", "Unrated", "IMAX", "Final Cut", "Remastered"]
WORDS_A = ["Silent", "Crimson", "Hollow", "Last", "Broken", "Golden", "Northern", "Electric", "Distant", "Velvet",
           "Iron", "Paper", "Glass", "Midnight", "Lost", "Burning", "Quiet", "Savage", "Neon", "Wandering"]
WORDS_B = ["Harbor", "Empire", "Garden", "Signal", "Frontier", "Station", "Orchard", "Archive", "Tide", "Mirror",
           "Republic", "Lantern", "Summit", "Kingdom", "Engine", "Meridian", "Harvest", "Circuit", "Desert", "Canal"]

# Seed clips: (key, extension, ffmpeg args)
SEEDS = {
    "h264_mkv": (".mkv", "-c:v libx264 -preset veryfast -g 48 -b:v 2M -c:a aac -b:a 128k"),
    "hevc_mkv": (".mkv", "-c:v libx265 -preset ultrafast -x265-params log-level=error -tag:v hvc1 -b:v 2M -c:a eac3 -b:a 384k"),
    "h264_mp4": (".mp4", "-c:v libx264 -preset veryfast -g 48 -b:v 2M -c:a aac -b:a 128k -movflags +faststart"),
    "h264_mp4_tail": (".mp4", "-c:v libx264 -preset veryfast -g 48 -b:v 2M -c:a aac -b:a 128k"),
    "av1_mkv": (".mkv", "-c:v libsvtav1 -preset 12 -b:v 1M -c:a libopus -b:a 96k"),
    "mpeg4_avi": (".avi", "-c:v mpeg4 -vtag xvid -q:v 5 -c:a libmp3lame -b:a 128k"),
    "h264_ts": (".ts", "-c:v libx264 -preset veryfast -g 48 -b:v 2M -c:a aac -b:a 128k -f mpegts"),
}


def make_seeds(seed_dir: Path) -> dict[str, Path]:
    seed_dir.mkdir(parents=True, exist_ok=True)
    out = {}
    for key, (ext, args) in SEEDS.items():
        p = seed_dir / f"seed_{key}{ext}"
        if not p.exists() or p.stat().st_size == 0:
            size = "1280x720" if "avi" not in key else "720x400"
            cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
                   "-f", "lavfi", "-i", f"testsrc2=size={size}:rate=24",
                   "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
                   "-t", "6", *args.split(), str(p)]
            r = subprocess.run(cmd)
            if r.returncode != 0:
                print(f"WARN: seed {key} failed; skipping it", file=sys.stderr)
                continue
        out[key] = p
    return out


_libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
_clonefile = getattr(_libc, "clonefile", None)


def clone(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    if _clonefile is not None:
        if _clonefile(str(src).encode(), str(dst).encode(), 0) == 0:
            return
    shutil.copyfile(src, dst)


def unique_trailer(ext: str, uid: bytes) -> bytes:
    """A trailer that parsers ignore but that makes the file's tail unique."""
    if ext in (".mp4", ".m4v", ".mov"):
        payload = b"flux" + uid
        return (8 + len(payload)).to_bytes(4, "big") + b"free" + payload  # MP4 'free' box
    if ext in (".mkv", ".webm"):
        payload = uid
        return b"\xEC" + bytes([0x80 | len(payload)]) + payload  # EBML Void element
    if ext in (".ts", ".m2ts"):
        pkt = bytearray(b"\x47\x1F\xFF\x10" + b"\xFF" * 184)  # null packet, PID 0x1FFF
        pkt[4:4 + len(uid)] = uid
        return bytes(pkt)
    if ext == ".avi":
        return b"JUNK" + len(uid).to_bytes(4, "little") + uid
    return uid


def write_media(root: Path, rel: str, seed: Path, manifest: list, truth: dict) -> None:
    dst = root / rel
    clone(seed, dst)
    with open(dst, "ab") as f:
        f.write(unique_trailer(dst.suffix.lower(), uuid.uuid4().bytes))
    manifest.append({"path": rel, **truth})


def write_aux(root: Path, rel: str, content: bytes, manifest: list, kind: str) -> None:
    dst = root / rel
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_bytes(content)
    manifest.append({"path": rel, "kind": kind})


def dotted(s: str) -> str:
    keep = "".join(c if c.isalnum() or c in " -'" else " " for c in s)
    return ".".join(keep.replace("'", "").split())


def safe(s: str) -> str:
    return s.replace("/", "-").replace(":", " -").replace("?", "").replace("*", "").replace('"', "")


def synth_title(rng: random.Random) -> str:
    t = f"The {rng.choice(WORDS_A)} {rng.choice(WORDS_B)}" if rng.random() < 0.5 else f"{rng.choice(WORDS_A)} {rng.choice(WORDS_B)}"
    if rng.random() < 0.15:
        t += f" {rng.choice(['II', '2', '3', 'Part Two', 'Returns', 'Reloaded'])}"
    return t


def gen_movie(root: Path, rng: random.Random, seeds: dict, title: str, year: int, manifest: list) -> None:
    seed_key = rng.choice(list(seeds))
    seed = seeds[seed_key]
    ext = seed.suffix
    style = rng.choices(
        ["clean", "clean_versions", "edition", "scene", "flat", "tmdbid", "multipart", "lowercase_flat"],
        weights=[30, 8, 5, 30, 10, 5, 3, 9])[0]
    truth = {"kind": "movie", "title": title, "year": year, "seed": seed_key}
    st = safe(title)
    if style == "clean":
        d = f"Movies/{st} ({year})"
        write_media(root, f"{d}/{st} ({year}){ext}", seed, manifest, truth)
        if rng.random() < 0.4:
            write_aux(root, f"{d}/{st} ({year}).en.srt", b"1\n00:00:01,000 --> 00:00:02,000\nhi\n", manifest, "subtitle")
        if rng.random() < 0.3:
            write_aux(root, f"{d}/poster.jpg", b"\xFF\xD8\xFF\xE0fake", manifest, "artwork")
        if rng.random() < 0.1:
            write_media(root, f"{d}/Featurettes/Making Of{ext}", seed, manifest, {"kind": "extra", "parent_title": title, "year": year, "seed": seed_key})
    elif style == "clean_versions":
        d = f"Movies/{st} ({year})"
        for res in rng.sample(["2160p", "1080p", "720p"], 2):
            write_media(root, f"{d}/{st} ({year}) - {res}{ext}", seed, manifest, {**truth, "resolution": res})
    elif style == "edition":
        ed = rng.choice(EDITIONS)
        d = f"Movies/{st} ({year})"
        write_media(root, f"{d}/{st} ({year}) {{edition-{ed}}}{ext}", seed, manifest, {**truth, "edition": ed})
        write_media(root, f"{d}/{st} ({year}){ext}", seed, manifest, truth)
    elif style == "scene":
        res, src, codec, grp = rng.choice(RES), rng.choice(SOURCES), rng.choice(CODECS), rng.choice(GROUPS)
        aud = rng.choice(AUDIO) + "." if rng.random() < 0.5 else ""
        name = f"{dotted(title)}.{year}.{res}.{src}.{aud}{codec}-{grp}"
        d = f"Movies/{name}"
        write_media(root, f"{d}/{name}{ext}", seed, manifest, {**truth, "resolution": res, "group": grp})
        write_aux(root, f"{d}/{name}.nfo", b"NFO file\n", manifest, "nfo")
        if rng.random() < 0.3:
            write_media(root, f"{d}/Sample/{name.lower()}-sample{ext}", seed, manifest, {"kind": "sample", "seed": seed_key})
        if rng.random() < 0.3:
            write_aux(root, f"{d}/Subs/English.srt", b"1\n00:00:01,000 --> 00:00:02,000\nhi\n", manifest, "subtitle")
    elif style == "flat":
        write_media(root, f"Movies/{st} {year}{ext}", seed, manifest, truth)
    elif style == "lowercase_flat":
        write_media(root, f"Movies/{dotted(title).lower()}.{year}{ext}", seed, manifest, truth)
    elif style == "tmdbid":
        tid = rng.randint(100, 999999)
        d = f"Movies/{st} ({year}) [tmdbid-{tid}]"
        write_media(root, f"{d}/{st} ({year}) [tmdbid-{tid}]{ext}", seed, manifest, {**truth, "tmdb_id": tid})
    elif style == "multipart":
        d = f"Movies/{st} ({year})"
        for part in (1, 2):
            write_media(root, f"{d}/{st} ({year}) - cd{part}{ext}", seed, manifest, {**truth, "part": part})


def gen_show(root: Path, rng: random.Random, seeds: dict, show: str, year: int, seasons: list[int], manifest: list) -> None:
    seed_key = rng.choice(list(seeds))
    seed = seeds[seed_key]
    ext = seed.suffix
    style = rng.choices(["clean", "scene", "x_style", "anime", "flat_season"], weights=[45, 30, 8, 10, 7])[0]
    ss = safe(show)
    show_dir = f"TV/{ss} ({year})" if rng.random() < 0.6 else f"TV/{ss}"
    for s_idx, n_eps in enumerate(seasons, start=1):
        season_dir = rng.choice([f"Season {s_idx:02d}", f"Season {s_idx}", f"S{s_idx:02d}"])
        ep = 1
        while ep <= n_eps:
            multi = style in ("clean", "scene") and ep < n_eps and rng.random() < 0.04
            eps = [ep, ep + 1] if multi else [ep]
            truth = {"kind": "episode", "show": show, "year": year, "season": s_idx, "episodes": eps, "seed": seed_key}
            if style == "clean":
                code = f"S{s_idx:02d}E{ep:02d}" + (f"-E{ep + 1:02d}" if multi else "")
                rel = f"{show_dir}/{season_dir}/{ss} - {code} - Episode {ep}{ext}"
            elif style == "scene":
                code = f"S{s_idx:02d}E{ep:02d}" + (f"E{ep + 1:02d}" if multi else "")
                rel = f"{show_dir}/{season_dir}/{dotted(show)}.{code}.{rng.choice(RES)}.{rng.choice(SOURCES)}.{rng.choice(CODECS)}-{rng.choice(GROUPS)}{ext}"
            elif style == "x_style":
                rel = f"{show_dir}/{season_dir}/{ss} {s_idx}x{ep:02d}{ext}"
            elif style == "anime":
                absolute = sum(seasons[: s_idx - 1]) + ep
                crc = f"{rng.getrandbits(32):08X}"
                rel = f"{show_dir}/[{rng.choice(['SubsPlease', 'Erai-raws', 'HorribleSubs'])}] {ss} - {absolute:03d} [1080p][{crc}]{ext}"
                truth["absolute"] = absolute
            else:  # flat_season
                rel = f"{show_dir}/{ss}.S{s_idx:02d}E{ep:02d}{ext}"
            write_media(root, rel, seed, manifest, truth)
            ep += len(eps)
    if rng.random() < 0.3:
        write_media(root, f"{show_dir}/Specials/{ss} - S00E01 - Special{ext}", seed, manifest,
                    {"kind": "episode", "show": show, "year": year, "season": 0, "episodes": [1], "seed": seed_key})
    if rng.random() < 0.2:
        write_aux(root, f"{show_dir}/tvshow.nfo", b"<tvshow/>", manifest, "nfo")


def gen_junk(root: Path, rng: random.Random, manifest: list) -> None:
    for d in ["Movies", "TV"]:
        write_aux(root, f"{d}/.DS_Store", b"\0" * 64, manifest, "junk")
        write_aux(root, f"{d}/@eaDir/SYNOINDEX_MEDIA_INFO", b"junk", manifest, "junk")
        write_aux(root, f"{d}/Thumbs.db", b"junk", manifest, "junk")
    write_aux(root, "Movies/incomplete.download.mkv.partial~", b"\x1aE\xdf\xa3", manifest, "junk")
    write_aux(root, "Movies/.hidden.mkv", b"\x1aE\xdf\xa3", manifest, "junk")
    write_aux(root, "Movies/readme.txt", b"hello", manifest, "junk")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", default=DEFAULT_OUT, help="output dir (default: $FLUX_TEST_MEDIA/library)")
    ap.add_argument("--movies", type=int, default=3000, help="number of movies (real titles first, then synthetic)")
    ap.add_argument("--shows", type=int, default=300, help="number of shows (real first, then synthetic)")
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--force", action="store_true", help="delete an existing output dir first")
    args = ap.parse_args()
    if args.out is None:
        sys.exit("pass --out DIR, or set FLUX_TEST_MEDIA to the test corpus directory (see CONTRIBUTING.md)")

    out = Path(args.out)
    if out.exists() and any(out.iterdir()):
        if not args.force:
            sys.exit(f"{out} is not empty; pass --force to regenerate")
        shutil.rmtree(out)
    rng = random.Random(args.seed)
    seeds = make_seeds(out.parent / "seeds")
    manifest: list[dict] = []

    movies = list(REAL_MOVIES)
    seen = set(movies)
    while len(movies) < args.movies:
        m = (synth_title(rng), rng.randint(1930, 2026))
        if m not in seen:
            seen.add(m)
            movies.append(m)
    for title, year in movies[: args.movies]:
        gen_movie(out, rng, seeds, title, year, manifest)

    shows = list(REAL_SHOWS)
    names = {s[0] for s in shows}
    while len(shows) < args.shows:
        name = synth_title(rng)
        if name in names:
            continue
        names.add(name)
        shows.append((name, rng.randint(1980, 2026), [rng.randint(6, 13) for _ in range(rng.randint(1, 6))]))
    for show, year, seasons in shows[: args.shows]:
        gen_show(out, rng, seeds, show, year, seasons, manifest)

    gen_junk(out, rng, manifest)

    # Kept outside the library so scanners never see it.
    with open(out.parent / f"{out.name}.manifest.jsonl", "w") as f:
        for row in manifest:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
    kinds: dict[str, int] = {}
    for row in manifest:
        kinds[row["kind"]] = kinds.get(row["kind"], 0) + 1
    print(json.dumps({"out": str(out), "files": len(manifest), "by_kind": kinds}, indent=2))


if __name__ == "__main__":
    main()
