# flux-names: prototype report

**Crate:** `crates/flux-names`. **Contract:** `flux_core::names` (`NameGuess`, `LibraryKind`, `GuessKind`, `ExtraKind`, `ProviderIds`).
**Research:** research/02 §6 and research/05 §3.1–3.4.

## Summary

flux-names turns a library-relative path into a `NameGuess`. A guess carries:

- kind, title and year;
- season, episodes, absolute number and air date;
- edition, part and extra kind;
- provider ids and technical tags;
- a confidence, plus an evidence string for every decision.

The parser is layered as research/02 §6.3 recommends:

1. **Explicit ids.**
2. **A fast path.** An anchored tokenizer grammar, plus folder-layout rules for Plex, Jellyfin and *arr libraries.
3. **The `hunch` 2.0.2 fallback.** It is MIT licensed, a guessit port, and gets sibling context. It runs only when the fast path is unsure.
4. **Post-rules** for the known failure classes.

### Results against the targets

| Target | Result |
|---|---|
| ≥ 99% title+year, synthetic movies | **100%** (3,456 / 3,456) |
| ≥ 99% season+episodes, synthetic episodes | **100%** (10,949 / 10,949) [1] |
| ≥ 33/36 hard cases | **36/36** on all four fields. hunch alone: 32/36. |
| Throughput | **≈ 100k names/s**, single thread (10 µs/name, load avg ≈ 30). About 53× faster than hunch with siblings (533 µs). |

[1] Anime files named `[Group] Show - 035` carry only an absolute number. Mapping that to SxxEyy needs provider data (research/05 §3.4), so I score `absolute_episode` for them. The SxxEyy mapping is reported separately: 0%, by design.

### How honest is 100%?

I read the generator (`tools/corpus/gen-library.py`) and designed against it. So I also scored the parser **out of distribution**, on guessit's own test suite (the fixtures shipped in the hunch crate, 805 cases). I split it in two:

- even-indexed cases are the *dev* half: I inspected their failures while improving the parser;
- odd-indexed cases are *held out*: I only looked at aggregate scores.

On the held-out half, flux matches hunch on episodes but trails it on movies and the miscellaneous set (table below). Note that these fixtures are hunch's own compatibility suite, so they favour hunch.

## What I built

| File | Role |
|---|---|
| `src/lib.rs` | Public API: `parse`, `parse_with`, `ParseOptions`/`Fallback`, `parse_fallback_only`, `is_video_file`, `is_video_ext`, `is_ignored`, `norm_key`. |
| `src/resolve.rs` | Path-level reasoning. Folder roles (season, specials, extras, sample, disc, part, generic), the library-kind prior, and movie-folder vs filename arbitration. Also strong vs weak show folders, obfuscated filenames inside release folders, season-pack folders, bare episode numbers in season folders, and confidence. |
| `src/stem.rs` | The fast-path grammar for one name. Tokenize, find the earliest *anchor* (SxxEyy, NxNN, season/episode words in 7 languages, `Cap.102`, `1of4`, date, anime ` - 12`, first strong tag or `[`), pick the year in the zone before it, then read the title, edition/part/version label, episode title and technical tail. |
| `src/tokens.rs` | Span-preserving tokenizer. Dots separate words, except in "Mr." and "Your Name."; hyphens stay inside words; brackets and stand-alone dashes are tokens. |
| `src/vocab.rs` | Release-tag vocabulary. *Strong* tags never occur in titles; *weak* ones (`Web`, `Opus`, `Extended`, languages) need another tag after them. Also canonical labels, editions and part markers. |
| `src/folders.rs` | Folder classification: `Season 02`/`S02`/`Staffel 2`/`Saison 12 Français`, `Specials`, the Jellyfin and Plex extras folders, `Sample`, `BDMV`/`VIDEO_TS`, `CD1`, and generic folders (`Movies`, `TV`, …). |
| `src/ids.rs` | `[tmdbid-…]`, `{tmdb-…}`, `[imdbid-tt…]`, `{imdb-tt…}`, `[tvdbid-…]`, `{tvdb-…}` (`=` accepted too), and `{edition-X}`. |
| `src/fallback.rs` | The hunch adapter. One shared `Pipeline`, at most 6 video siblings joined to the parent path, a library-kind prior, and the merge policy. |
| `src/rules.rs` | Post-rules: year sanity, dates are air dates, seasons over 50 are rejected unless a folder or an explicit `S2013E14` says otherwise. Fallback output also gets: title number not SxxEyy (`2049`), part words belong to the title, country code belongs to the title. |
| `src/text.rs` | NFC normalisation (NFD names from HFS+), `norm_key`, title-casing of all-lowercase names. |
| `src/eval.rs`, `src/hard_cases.rs` | Manifest, hard-case and guessit-fixture scoring, used by tests and `names-eval`. |
| `src/bin/names-eval.rs` | Accuracy and throughput harness. Writes `flux-names-*.json`. |
| `examples/parse.rs`, `examples/compare.rs` | Debug printer, and dev-half flux-vs-hunch disagreement analysis. |
| `tests/cases.rs`, `tests/media.rs` | 16 behaviour tests covering every claimed layout, plus 3 media tests. |

### Public API

```rust
pub fn parse(rel_path: &Path, siblings: &[&str], kind: LibraryKind) -> NameGuess;
pub fn parse_with(rel_path: &Path, siblings: &[&str], kind: LibraryKind, opts: ParseOptions) -> NameGuess;
pub struct ParseOptions { pub fallback: Fallback }          // Fallback::{Never, Auto (default), Always}
pub fn parse_fallback_only(rel_path: &Path, siblings: &[&str], kind: LibraryKind, post_rules: bool) -> NameGuess; // feature "hunch"
pub fn is_ignored(rel_path: &Path) -> bool;                 // hidden, @eaDir, #recycle, Thumbs.db, *.partial~ …
pub fn is_video_file(name: &str) -> bool;  pub fn is_video_ext(ext: &str) -> bool;
pub fn norm_key(s: &str) -> String;                         // title comparison key
pub const FALLBACK_AVAILABLE: bool;
```

How to call it:

- `rel_path` is relative to the library root.
- `siblings` are the file *names* in the same directory. They provide stacking evidence for `part1`/`part2` and title invariance for hunch.
- `parse` never fails and never panics. A no-panic sweep in `tests/media.rs` covers every guessit fixture name and adversarial strings, in all three library kinds.
- Feature `hunch` is on by default. `--no-default-features` gives a regex-only build, and every test passes without it.

### Conventions an integrator needs to know

- **Episodes.** `title` is the show. The show folder wins when it is *strong*: above `Season NN`/`Specials`/`Extras`, or the first folder under a shows-library root. A plain parent folder in a mixed library only fills in when the filename has no title, or when the folder name prefixes the filename title.
- **Anime absolute numbering.** `[Group] Show - 035` sets `absolute_episode = 35`, with `season = None` and `episodes = []`. Inside a `Season N` folder it sets both: `episodes = [35]`, `season = N` and `absolute_episode = 35`.
- **Season packs.** A file named `Show.S03.1080p…` comes back as `Episode` with `season` set, no episodes, and confidence 0.4.
- **Extras.** `title`/`year` are the *owner's* (movie or show). `episode_title` carries the extra's own name ("Making Of"), and `extra` is the kind. Suffix extras (`Alien (1979)-trailer.mkv`) get the owner from the stem.
- **Samples.** `Sample/` folders, and `-sample`/`.sample`/`sample-` names, give `GuessKind::Sample`, with the release title for diagnostics.
- **Editions.**
  - `{edition-X}` wins.
  - Then `Title (Year) - Director's Cut`: known phrases are canonicalised, and other ` - Label` text is kept as a Jellyfin multi-version label (evidence says "version label").
  - Then tail tags (`EXTENDED`, `REMASTERED`, …).
  - Then trailing title words (`Alien DC (1979)`, `Aliens.SE.1986`). "The Final Cut" stays a title.
- **Titles keep punctuation** for space-separated names ("Mr. Robot", "Your Name.", "Léon - The Professional"). Dotted scene names are re-spaced, with acronyms restored ("S.W.A.T.", "E.T."). `Simpsons, The` becomes "The Simpsons". All-lowercase names are title-cased.
- **Deliberate differences from guessit** (TMDB titles keep these words):
  - "Part Two/Part II/Part 1" stays in the title.
  - "Title - Subtitle" is not split into an alternative title.
  - "The Office (US)", "The Office US" and "Shark Tank (AU)" keep the country.
- **Mixed libraries need show evidence** (parser v3). Without a TV folder (`TV/`, `Anime/`…) or a season/specials folder around the file:
  - an explicit marker (`S01E02`, `1x02`, `Season 1 Episode 2`, `第2集`) is enough;
  - anything only the fallback (hunch) reads as an episode, and a weaker fast-path marker (a date after a title, `E12`, `Title - 07`) in a name with nothing of a release about it (no technical tags, no release group), needs another video in the folder of one numbered series with it: named alike up to the number read as its episode, which differs (`Clip 01` beside `Clip 02`, `Beach_Day_2019-05-01` beside `Beach_Day_2019-05-04`; not `Site_ep1_4711` beside `Site_ep1_4712`, where only a running id differs). Alone, the file is a movie titled after its whole name. A single clip is not a show of one episode, while a release (`Show.2024.03.15.720p.WEB-GRP`, `[Group] Show - 12`) is still an episode on its own;
  - a date with a time of day after it (`VID_20190501_123456`, `2019-05-01 12-34-56`, `Screen Recording 2019-05-01 at 12.34.56`), or with nothing before it (`2019-05-01.mp4`), is a recording: a movie titled after the whole name, never a daily show filed under the year as its season;
  - when a new file arrives, the scanner re-parses the files beside it that share its series key (`flux_names::series_key`), as it does for multi-part stacks; a re-parse that reads a file as before writes nothing;
  - names compare in NFC, so a decomposed name (macOS writes Hangul and accents that way) makes a series with its siblings in either form;
  - `Other`/`Others` hold extras only right under a folder named as a film or show (`Some Film (2010)`, an id tag) or in a Movies or TV folder; anywhere else they are ordinary folders (they stay extras folders in movie and show libraries).
- **Videos libraries** (`LibraryKind::Videos`): every file is a `Movie` titled after its name (`_`, and `.` in a name without spaces, read as spaces), with no year, ids, extras, samples, episodes or fallback. Only a true multi-part stack, a number after `cd`, `disc`, `disk`, `dvd`, `part` or `pt` (`Clip part1` + `Clip part2` side by side, Plex's stacking words), sets `part` and titles the parts by their common name. A last letter does not stack (`Interview_A` and `Interview_B` are camera angles or takes, each a video).
- **Confidence scale.**
  - 0.95: explicit id, or canonical `Title (Year)` agreeing with its folder.
  - 0.9: `Title (Year)`.
  - 0.85: `Title.Year` or SxxEyy.
  - 0.75–0.8: anime absolute, packed number, date.
  - 0.65: bare episode number.
  - < 0.5 (fallback runs, then the AI matching queue): no year and no tags, a trailing ambiguous number, a season-pack file, or junk names. Two parsers agreeing on a bare title adds only +0.05, so the result stays below 0.5.

### Example parses (with evidence)

```
Blade Runner 2049 (2017)/Blade Runner 2049 (2017) - 2160p.mkv   [Movies]
  Movie "Blade Runner 2049" 2017 res=2160p conf=0.95  ["movie folder agrees", "fast path: Title (Year)"]
2001.a.space.odyssey.1968.ts   [Movies]
  Movie "2001 A Space Odyssey" 1968 conf=0.80  ["kept year-like 2001 in the title (a later year wins)", "fast path: Title.Year"]
Knives Out (2019)/Featurettes/Making Of.mkv   [Movies]
  Extra(Featurette) "Knives Out" 2019 episode_title="Making Of" conf=0.85  ["extra: Featurettes folder"]
Breaking Bad (2008)/[HorribleSubs] Breaking Bad - 035 [1080p][0695A0BA].mp4   [Shows]
  Episode "Breaking Bad" 2008 abs=35 group=HorribleSubs conf=0.85  ["episode marker: anime absolute number", "show from folder 'Breaking Bad (2008)'"]
The.Daily.Show.2024.03.15.Guest.Name.720p.WEB.h264.mkv   [Mixed]
  Episode "The Daily Show" air_date=2024-03-15 ep_title="Guest Name" conf=0.80  ["episode marker: air date", "show from filename"]
movies/James_Bond-f17-Goldeneye.mkv   [Mixed]
  Movie "Goldeneye" conf=0.30  ["fast path: title only", "fallback found structure the fast path missed", "fallback: hunch (guessit port), low confidence"]
Some Random Name.mkv   [Movies]
  Movie "Some Random Name" conf=0.45  ["fast path: title only", "fallback agrees on the title"]   → AI matching queue
```

## How to run

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/agent-names
cargo test   -p flux-names                         # 48 tests (28 unit, 16 behaviour, 3 media, 1 doc)
cargo test   -p flux-names --no-default-features   # same suite without hunch
cargo clippy -p flux-names --all-targets           # clean (also with --no-default-features)
cargo run --release -p flux-names --bin names-eval # accuracy + guessit + throughput → bench/results/flux-names-*.json
cargo run --release -p flux-names --bin names-eval -- --no-bench --failures   # + dev-half failure listing
cargo run --release -p flux-names --example parse -- --shows "Show/Season 1/Show 1x03.mkv"
cargo run --release -p flux-names --example compare -- ~/.cargo/registry/src/*/hunch-2.0.2/tests/fixtures
```

The media tests (`tests/media.rs`) need the SSD and print a skip message without it. They cover:

- the tiny-library targets from its manifest;
- a walk of the real `library-tiny/` tree on disk, checking every video file against the manifest;
- the no-panic sweep.

## Results

All numbers below are from one run on 2026-10-18 at 23:47. Load average was 27.5 / 34.9 / 37.1, with 8 agents compiling. Results files are in `$FLUX_WORK/bench/results/`:

- `flux-names-accuracy-library.json`
- `flux-names-accuracy-library-tiny.json`
- `flux-names-hard-cases.json`
- `flux-names-guessit.json`
- `flux-names-throughput.json`

### Synthetic library (16,671 files; 14,760 media files scored)

Paths are relative to `Movies/` (a movie library) or `TV/` (a shows library). Siblings come from the manifest's directory listing.

| kind | field | flux (default) | flux fast path only | hunch alone* |
|---|---|---|---|---|
| movie (3,456) | title+year | **100%** | 100% | 72.7% |
| | title / year | 100% / 100% | 100% / 100% | 99.1% / 73.6% |
| | edition / part / tmdb id | 100% / 100% / 100% | same | 95.7% / 97.7% / 95.8% |
| | resolution / release group | 100% / 100% | same | 100% / 100% |
| episode (10,949) | season+episodes | **100%** | 100% | 90.1% |
| | show title | 100% | 100% | 99.5% |
| | year (6,353 with year in path) | 100% | 100% | 0.9% |
| | anime absolute → SxxEyy mapping | 0% (by design) | 0% | 0% |
| extra (94) | kind + owner title + year | 100% | 100% | 0% |
| sample (261) | kind | 100% | 100% | 0% |
| non-media (1,911) | skipped by `is_ignored` / extension | 100% | — | — |

\* hunch alone means `hunch_with_context` on the same relative path, no post-rules. It does not use folder conventions: show-folder years, extras folders, `Sample/`.

**Per generator layout**, all fields, 100% everywhere:

- movies: clean 1,079; clean_versions 448; edition 150; flat 305; lowercase_flat 272; multipart 164; scene 891; tmdbid 147;
- episodes: clean 4,996; scene 3,414; x_style 676; anime 1,085; flat_season 694; specials 84;
- extras 94 and samples 261.

The fallback ran on **0** of the 14,760 files, and no file was wrong with confidence ≥ 0.5.

**Failure classes on the synthetic manifest:** one, by design. Anime absolute numbers aren't mapped to season and episode, because that needs TMDB/TVDB absolute episode groups (1,085 files).

### The 36 hard cases (research/02 §6.2)

| parser | all 4 fields | title | year | season | episodes |
|---|---|---|---|---|---|
| **flux-names** (default, and fast path only) | **36/36** | 36 | 36 | 36 | 36 |
| hunch 2.0.2 alone | 32/36 | 33 | 35 | 35 | 34 |
| hunch + flux post-rules | 35/36 | 36 | 35 | 36 | 35 |

The hunch-alone numbers reproduce research/02 §6.2 exactly. The post-rules fix hunch's Blade Runner `S20E49`, "Dune Part Two", the Daily Show date and "The Office US".

### Out of distribution: guessit's test suite

Fixtures: `movies.yml` (200 cases), `episodes.yml` (481 cases) and `various.yml` (124 cases). Each field is scored only when the case specifies it. "all" means every specified field among title, year, season and episodes is correct.

| file / half | flux (mixed library) | flux (library kind known) | flux fast path only | hunch alone |
|---|---|---|---|---|
| movies, dev | 93.0% | 93.0% | 90.0% | 91.0% |
| movies, **held-out** | **86.0%** | **88.0%** | 76.0% | **91.0%** |
| episodes, dev | 88.0% | 86.3% | 81.7% | 83.0% |
| episodes, **held-out** | **83.3%** | **84.6%** | 79.2% | **84.6%** |
| various, dev | 88.7% | 88.7% | 80.6% | 88.7% |
| various, **held-out** | **79.0%** | **80.6%** | 71.0% | **93.5%** |

Held-out title accuracy:

- movies: 86 / 88% vs hunch 91%;
- episodes: 87.8 / 90.3% vs hunch 88.2%;
- various: 82.3 / 83.9% vs hunch 95.2%.

The flux columns include the fallback. The layered design is what closes most of the gap: the fast path alone scores 76% on held-out movies.

**Remaining failure classes** (from the dev half, plus categories I expect in the held-out half):

1. **Convention differences, deliberate.** guessit splits `Title - Subtitle` and `Title--Subtitle` into alternative titles ("OSS 117", "Queen", "Yowamushi Pedal"). It treats `Part III` as a part, and drops `(US)`/`(AU)` and `UK` from show names.
2. **Unknown release vocabulary.** Words we don't know end up in titles; hunch has a larger vocabulary (`BTSportHD`, `Mbps`, `SPLIT SCENES`).
3. **Leading junk.** Scene group prefixes (`ulshd-the.right.stuff`), date prefixes (`09.03.08.The.Doors`), leading episode numbers (`003. Show - Ep`, `[Group] 06 Title`), leading-year titles (`2009.shoot.fruit.chan`).
4. **Numbers without context in a mixed library.** `Show.Name.10.720p` and `Show Name 13-16` stay in the title unless the library is a shows library, the name has a fansub group, or a TV broadcast tag follows.
5. **guessit special forms.** `-fNN-` film numbering, `-xNN-` bonus numbering, `Eyes.Of.Dawn.1991.E01` (year as season).
6. **Fixture artifacts.** Names without real extensions (`Show.Name.S01.E02.E03`, where `E03` reads as an extension), and `/` inside brackets (`[720p/mkv]`).

### Throughput (single thread, 14,760 media paths of the full library, median of 5 reps)

| mode | names/s | µs/name |
|---|---|---|
| fast path only | 99,698 | 10.0 |
| default (fallback only when unsure) | 99,608 | 10.0 |
| fallback always (fast + hunch + merge) | 2,007 | 498 |
| hunch alone (with ≤ 6 siblings) | 1,875 | 533 |
| hard cases, fast path only (no folders) | 90,271 | 11.1 |
| hard cases, fallback always | 6,208 | 161 |

This was measured under load average ≈ 30, so expect lower µs on a quiet machine. To reproduce: `cargo run --release -p flux-names --bin names-eval`.

In practice:
- 100k new files cost about 1 CPU-second on the fast path. hunch on everything would cost about 50 s.
- The default mode only pays for hunch on files below confidence 0.5. That was 0% of the synthetic library and 19% of guessit's deliberately odd names (150 of 805: movies 28%, episodes 15%, various 19%). The share in real libraries is unknown.

## Known gaps

- **Generalisation trails hunch on movies and odd names** (held-out 86–88% vs 91%, and 79–81% vs 93.5%). On episodes it is at parity. The remaining classes are listed above. Some are conventions I chose on purpose; vocabulary and leading-junk cases are open work.
- **The synthetic manifest is not evidence of real-world accuracy.** I tuned against its generator. The guessit split is the honest number, and the maintainer's real library paths (research/02 §6.3 step 6) would be a better one.
- **Anime absolute → season/episode** needs provider absolute episode groups. That belongs to flux-meta.
- **Season-pack files** (`Show.S03.1080p.mkv`) are `Episode` with a season and no episodes, at confidence 0.4. The scanner should treat them as "needs review".
- **Ambiguous year-like titles** keep one reading. "Space 1999" yields title "Space", year 1999 (its show name is "Space: 1999"). There is no field yet for the alternative reading; see the proposed core changes.
- **`:` can't be restored** from ` - ` in filenames ("Léon - The Professional"). The matcher must compare with `norm_key`. Lowercase dotted acronyms (`s.w.a.t.`) do get restored; other casing doesn't.
- **No personal-video or camera-file classification** (`VID_20190713_154212.mp4`; research/05 §3.1). `GuessKind` has no variant for it, so those parse as low-confidence movie titles.
- **Per-file cost repeats folder parses.** Every file in a season folder re-parses the same show folder (still about 10 µs total). The scanner could cache parsed folders by directory if this ever matters.
- **hunch merge policy is heuristic.** hunch's shorter title is taken when the fast path is weak, hunch's title is contained in ours, the difference isn't just hunch's alternative-title split, and hunch didn't misread a second container. hunch's release-group guess can still be odd (`Goldeneye` above).
- **`MAX_YEAR = 2035`** is a constant, chosen for deterministic parsing. Bump it together with `PARSER_VERSION`.
- **DD.MM.YYYY dates** are not recognised, because they are ambiguous with MM.DD.
- **Evaluation code ships in the library.** `eval` (serde_json) and `hard_cases` live there so that tests and the binary share them. They could move behind a feature.

## Proposed core changes (flux-core)

1. `NameGuess.alt_titles: Vec<String>`. This would hold readings the parser chose against, so the matcher can search them all:
   - parenthesised alternative titles, such as "(The Prestige)", which are currently dropped and only mentioned in evidence;
   - hunch's ` - ` alternative title;
   - the other reading of year-like titles ("Space 1999" vs "Space", 1999).
2. `NameGuess.label: Option<String>` (or `extra_title`). This would carry the extra's own name ("Making Of") and Jellyfin multi-version labels (`Title (Year) - Bob's Cut`). Today the extra's name goes in `episode_title` and the version label in `edition`.
3. `GuessKind::Personal` for camera and home-video files (research/05 §3.1), plus a camera-pattern rule (`VID_`, `PXL_`, `DJI_`, `GOPR`, `IMG_`).
4. Document in `names.rs`:
   - anime-absolute semantics: `episodes` is empty and `season` is `None` unless a season folder exists;
   - season-pack semantics: `Episode` with a season and no episodes.
5. Consider structured evidence: an `Evidence { rule: &'static str, detail: String }` pair instead of free-form strings, so the UI and metrics can group decisions by rule.
