# flux-meta: prototype report

**Crate:** `crates/flux-meta` (8.4k lines incl. tests). **Contract:** `flux_core::{NameGuess, ProbeResult, ProviderIds}`, `flux_core::api::{ArtRef, ArtSource}`.
**Research:** research/05 (all), research/01 §9.4.

flux-meta covers metadata and artwork for flux:

- `tmdb`: a TMDB client.
- `export_index`: an offline, key-less candidate index built from TMDB's daily ID exports.
- `matcher`: a deterministic, explainable matcher.
- `ai`: an AI hypothesis layer that is *structurally* unable to see TMDB content.
- `art`: frame posters (no AI) and opt-in genAI posters with provenance.
- `jobs`: a budgeted job queue so AI never blocks scan or playback.

No API keys exist yet. Everything is tested offline, against hand-written fixtures and mock servers, and works with bring-your-own keys later.

## Results at a glance

All numbers are from the M1 Max while 8 agents compiled in parallel (load average 45–130). Treat them as upper bounds. Every number can be re-run with one command (see *How to run*).

| What | Result | Target / baseline |
|---|---|---|
| Export index build, movies (1,260,557 rows → 1,027,369 titles) | **1.89 s**, **59.4 MB** index heap | research/05 §8: < 3 s, < 60 MB (the prototype's naïve map was 381 MB) |
| Export index build, TV (234,360 rows) | **0.31 s**, **13.4 MB** | |
| Prefilter lookup (`candidates()`, exact + fuzzy) | **0.63 ms**/title | prototype 0.4–0.5 ms |
| REAL_MOVIES, parser-free, offline export only | **top-1 95/131 = 72.5 %**, recall@10 **77.9 %**, **0 auto-accepts** (so 0 wrong auto-accepts) | research prototype 11/20 = 55 % |
| Matcher throughput | **7.3 µs/match** (136k matches/s, 7.5 candidates each) | |
| Frame poster + backdrop per title (24 files, 12 candidates each) | **p50 0.88 s**, p95 2.9 s (4K HEVC); p50 was 0.64–0.88 s across three runs | |
| Tests | **78 pass**, 2 ignored (live TMDB, live Ollama), clippy clean | |

## What I built

### 1. `tmdb`: TMDB v3 client

- **Auth:** `TmdbAuth::from_env()` reads `TMDB_READ_TOKEN` (v4 read token, sent as `Authorization: Bearer`, preferred so the key never appears in URLs or logs) or `TMDB_API_KEY` (v3 key, sent as the `api_key` query parameter). Secrets are redacted in `Debug`.
- **Endpoints:** `search_movie`, `search_tv`, `movie_details` (`append_to_response=images,external_ids,release_dates`, `include_image_language=en,null`), `tv_details` (`external_ids,images`), `tv_season`, `find` (IMDb/TVDB/Wikidata ids) and `configuration` (`ImageConfiguration::url(size, path)`).
- **Rate limit:** client-side token bucket, default **20 rps** with burst 20 (TMDB's ceiling is ~40 rps).
- **Retries:** 429 honours `Retry-After`. 5xx, timeouts and connect errors back off exponentially with ±25 % jitter. Defaults: 4 retries, 500 ms base. 401 and 404 return typed errors immediately.
- **Disk cache** (`DiskCache`):
  - Files are sharded by xxh3-128 of the request: a one-line header (`fetched_at`, key) plus the raw body.
  - **The maximum age is clamped to 180 days** (`TMDB_MAX_CACHE_AGE`; ToS §1.C forbids caching longer than 6 months). Expired entries miss on read and are deleted; `purge_expired()` sweeps the rest.
  - Cache keys never contain credentials (tested).
- **ToS-aware types:**
  - Every human-readable TMDB string is a `TmdbText`. It has no `Display`, `Deref`, `AsRef<str>` or `Into<String>`, only an explicit, documented `as_str()`.
  - `attribution::{NOTICE, LOGO_GUIDELINES_URL, TERMS_URL, TMDB_TERMS}`. `TMDB_TERMS` is a `TermsProfile` with `ai_allowed: false`, `max_cache_age: 180 d` and `non_commercial_only: true`.
- **Tests:**
  - 11 wiremock tests over 9 hand-written fixtures (`tests/fixtures/tmdb/*.json`, modelled on the documented response shapes). They cover search, details with appends, TV and season, `/find`, configuration, 401/404, 429 with `Retry-After`, retry exhaustion, 5xx recovery, cache hits with no key in the cache files, and rate-limit timing.
  - An `#[ignore]` live test runs when credentials are present.

### 2. `export_index`: offline TMDB ID exports (no key)

- **Verified today:**
  - `https://files.tmdb.org/p/exports/{movie_ids|tv_series_ids}_MM_DD_YYYY.json.gz` answers 200 (28.2 MB / 5.1 MB, `last-modified` 07:19 UTC). Plain `http://` works too.
  - **Before the file is generated, S3 returns 403, not 404.** At 05:59 UTC, `…10_19_2026…` was a 403.
  - `download_latest()` therefore walks back up to *n* days, streams to `*.partial`, renames atomically and never downloads twice. Tested with wiremock and run against the real server: today returned 403, so it fell back to the local 10_18 file.
- **Index layout:**
  - One sorted arena of distinct normalised titles with `u32` offsets.
  - Per-title entry ranges in two columns: TMDB id (`u32`) and **bfloat16 popularity** (`u16`, < 0.4 % error, which is plenty for a prior).
  - A token → title-ordinal postings list. Tokens in more than 20k titles have no postings: they are stop words and useless for candidates.
- **Lookup:** exact lookup is a binary search. It tries the article-toggled variant too (`matrix` ↔ `the matrix`). Fuzzy lookup scores the titles that share the query's two rarest tokens (at most 20k) and keeps those ≥ 0.70. Results rank by similarity, then popularity.
- **Robustness:** the real export contains `"video": null` rows. Nulls are tolerated, and malformed lines are counted and skipped instead of failing the build.
- **Memory:** index heap is exact (from capacities): **59.4 MB**. Process RSS after building both indexes was 181 MB, because macOS malloc keeps the freed build scratch (normalised arena plus row tuples). See *Gaps*.

### 3. `matcher`: deterministic, explainable

`Matcher::match_candidates(&MatchQuery, &[Candidate]) -> MatchResult { decision, ids, confidence, margin, ranked[≤5] { signals, evidence[] }, evidence[] }`

- **Scorer** (research/05 §3.2):
  - movie = 0.40 title + 0.20 year + 0.25 runtime + 0.10 language + 0.05 popularity.
  - tv = 0.40 title + 0.15 year + 0.25 episode-exists + 0.10 language + 0.05 runtime + 0.05 popularity.
  - **Accept** at ≥ 0.85 with a margin ≥ 0.10. **Review** at ≥ 0.60. Otherwise **NoMatch**, which routes to the AI lane.
- **Missing evidence is neutral (0.5)**, so title alone can never auto-accept. A candidate needs a year *and* a runtime or language. This is why the export-only evaluation auto-accepts nothing.
- **Title similarity:**
  - NFKD diacritic folding, for Latin, Greek and Cyrillic only. NFKD would split Hangul into jamo and strip Japanese dakuten (ゴジラ→コシラ); a test caught that.
  - French/Italian elision (`d'Amélie` → `d amelie`); `&` → `and`; roman sequels II–IX become digits; leading English articles are handled.
  - Score: Jaro–Winkler plus a soft token-Dice measure. Typos still share a word; `2` never matches `3`. Different sequel numbers cost 25 %.
- **Runtime** (the evidence Jellyfin ignores):
  - |Δ| ≤ 3 min → 1.0, ≤ 8 % → 0.7, ≤ 15 % → 0.4. With a declared edition, ≤ 35 % → 0.4.
  - Multi-part files are excluded. For TV, the expected runtime is episode runtime × episode count.
- **Language:** probe audio languages are mapped ISO 639-2 → 639-1 (`lang`). A match with `original_language` scores 1.0; a mismatch scores 0.2 (dubs exist).
- **Explicit ids short-circuit:**
  - `[tmdbid-603]` is an Accept at 1.0.
  - An IMDb or TVDB id that no candidate carries returns `Decision::LookupExternalId`, so the caller runs `/find` first.
- **TV:**
  - Season and episode existence comes from `/tv/{id}` seasons. The US vs UK *Office* test picks the show that has an S02E14.
  - The `AbsoluteMapper` hook maps absolute numbers (default `CumulativeSeasons`: *Attack on Titan* 27 → S02E02). TMDB "Absolute" episode groups or AniDB mappings can replace it.
- **Constructors:** `Candidate::from_{export, tmdb_movie, tmdb_movie_details, tmdb_tv, tmdb_tv_details}`.

**Offline evaluation** (`eval`, `tests/export_eval.rs`):

- **Ground truth.** The 132 `REAL_MOVIES` in `tools/corpus/gen-library.py` were resolved to TMDB ids through **Wikidata** (CC0, P4947) by `crates/flux-meta/tools/wikidata_truth.py`. Every id was then cross-checked against the export's `original_title`.
- **Corrections:**
  - Forrest Gump and WALL·E were added by hand.
  - *It* (1990) is **excluded**: TMDB files it as a TV miniseries (tv 19614), and Wikidata's P4947 points at an unrelated Russian film.
- **Method:** "parser-free" means the query is the ground-truth title and year, so this measures the index and the scorer, not filename parsing.

| Outcome (131 titles) | Count | Examples |
|---|---|---|
| Correct top-1 | 95 | 기생충, 千と千尋の神隠し, Le Fabuleux Destin d'Amélie Poulain, Se7en, WALL·E, Toy Story 2 vs 3, 2001: A Space Odyssey, Mad Max: Fury Road |
| **Translated title** (TMDB `original_title` is another language) | 29 | Parasite→기생충, Spirited Away→千と千尋の神隠し, Run Lola Run→Lola rennt, Pan's Labyrinth→El laberinto del fauno, Léon: The Professional→Léon, M→M – Eine Stadt sucht einen Mörder, Flow→Straume |
| **Same title, other film** (remakes; the export has no year) | 6 | King Kong 1933/1976 (2005 wins on popularity), Dune 1984, Scarface 1932, Ocean's Eleven 1960, The Fly 1958 |
| Near title | 1 | Ghost in the Shell (original is "GHOST IN THE SHELL / 攻殻機動隊"; the 2017 remake ranks first, the truth ranks 2nd) |

All 95 correct answers are **Review**, never Accept: the export can't supply year or runtime. All 29 wrong top-1s are Review and 7 are NoMatch. So the prefilter can never cause a silent mismatch. A dangerous near-miss like *Tokyo Story* → *Toy Story* only ever reaches the review queue.

Both failure classes are what research/05 predicted. Translated titles need `/search` (which matches translations), Wikidata aliases, or an AI hypothesis that proposes the original-language title. The AI path is wired for exactly this: `Hypothesis::refine` adds 기생충 to the query, and the matcher then picks 496243 over the decoy (tested in `tests/ai_mock.rs`). Remakes need the year (`/search`) or the probed runtime (`/movie/{id}`).

### 4. `ai`: hypotheses from local evidence only

- **`Hypothesizer` trait:** `fn hypothesize(&self, &LocalEvidence) -> BoxFuture<Result<HypothesisOutput>>`, plus a `descriptor()` giving model, locality, price, and `cost(usage)`.
- **`LocalEvidence`** has private fields and no constructor that takes a string. It can only be built from:
  - a file `Path`;
  - sibling paths (file names only, ≤ 24);
  - a `ProbeResult` (duration, container, video summary, audio and subtitle languages, the container title, track and chapter titles);
  - a `NameGuess`;
  - `OcrText`, which can only be created by running an `OcrEngine` on an `art::Frame`. A `Frame` can only be decoded from a local file.
- **Isolation guarantees:**
  - The `ai` module does not import `tmdb`.
  - Two `compile_fail` doctests pin this down: `LocalEvidence::from_path(&tmdb.overview)` fails with E0308, and a struct literal fails with E0451.
  - The type system cannot stop deliberate laundering (`Path::new(text.as_str())`). `TmdbText::as_str` is the one greppable exit. This is documented with the ToS §1.C reasoning.
- **`Hypothesis` output:**
  - Fields: `{kind: movie|episode|personal|extra|unknown, titles[{title, language, original}], year, season, episodes, absolute_episode, edition, confidence, rationale}`.
  - The **strict JSON schema** is OpenAI strict-mode shaped: every property is required, nulls are expressed as type unions, and no unsupported keywords are used.
  - `validated()` clamps ranges and lengths, ISO-normalises languages, and rejects a movie or episode with no title.
  - The model's confidence is used only to order the review queue. `refine()` feeds the titles back to the matcher, which decides.
- **`OpenAiCompatible`** (chat completions with `response_format: json_schema`, strict) has presets `openai()`, `ollama()`, `lm_studio()` and `vllm()`, and `from_env()` reads `FLUX_AI_BASE_URL`/`FLUX_AI_MODEL`/`FLUX_AI_API_KEY`, or `OPENAI_API_KEY`.
  - It handles `max_tokens` vs `max_completion_tokens`, an optional temperature (OpenAI's reasoning models reject it), Markdown-fenced JSON, refusals and HTTP errors.
  - It also implements `SceneDescriber`, which sends the user's own frames as base64 JPEG `image_url` parts and gets back `{setting, era, mood, palette, genre_guess, key_objects, people_count, time_of_day}`.
- **Tests:** a wiremock chat server asserts the request shape:
  - strict schema with 9 required properties;
  - temperature 0 for local presets;
  - the user message is exactly the local evidence keys (`path`, `siblings`, `probe`, `parse`, `ocr`) and nothing else;
  - bearer auth, cloud cost accounting, refusal, non-JSON output, empty-title rejection and 503 handling.
- **Ollama:** not installed on this machine (`which ollama` finds nothing, there is no Ollama.app or LM Studio, and nothing listens on :11434 or :1234). `tests/ai_mock.rs::ollama_smoke` is `#[ignore]`. Run it with `FLUX_OLLAMA_MODEL=qwen3.5:4b cargo test -p flux-meta --test ai_mock -- --ignored`.

### 5. `art`: artwork

**Frame posters** (`frame_posters(path, title, year, facts?, &opts) -> FramePosters { poster, backdrop, chosen_at_s, candidates[FrameScore], facts, timings }`):

1. **Media facts.** `MediaFacts::from_probe(&ProbeResult)` works with no subprocess. Otherwise one `ffprobe` call reads stream, format and **first-frame** `color_transfer`. The HDR10 test clip signals PQ only in the bitstream, so the stream level says "unknown". SAR (anamorphic DVDs) and ±90° rotation are applied.
2. **Candidates.** 12 seek times spread over 10–80 % of the runtime. Each is one ffmpeg process: `-noaccurate_seek -skip_frame nokey -ss T -i F -frames:v 1`, which decodes **exactly one keyframe** at 480 px. Four run in parallel.
3. **Scoring** (all deterministic, inside the letterbox-free active area):
   - Hard rejects: black, blown out, flat (fade) and mostly uniform.
   - Score: 0.25 exposure (mid-grey mean + 5–95 % spread) + 0.30 sharpness (Laplacian variance) + 0.20 colourfulness (Hasler–Süsstrunk) + 0.15 luma entropy + 0.10 non-uniformity.
   - Near-duplicates (8×8 average hash, ≤ 3 bits) are dropped, because long GOPs make several seeks land on one keyframe.
4. **Crops.** 2:3 and 16:9 crops are placed on the analysis frame with an energy map: Sobel magnitude + chroma distance, a sliding window over prefix sums, and a ≤ 30 % centre bias. The **same keyframe is then re-decoded at full resolution** (≤ 3840) and the crop is scaled up.
5. **HDR.** PQ and HLG frames come out of ffmpeg as 16-bit BT.2020 RGB and are **tone-mapped in Rust**: ST 2084 EOTF, 203-nit reference white, extended Reinhard on luminance (hue-preserving), BT.2020→709, sRGB. The Homebrew ffmpeg has neither zscale nor libplacebo, and servers can't assume them.
6. **Poster finish:**
   - Grade: vignette, gentle top scrim, and a dense bottom scrim that starts above the text block.
   - **Deterministic typography:** *Fraunces* (variable display serif) for titles and *Inter* for the year and subtitles. Both are SIL OFL 1.1, bundled in `crates/flux-meta/assets/fonts` with their licences.
   - Shaping with `rustybuzz` (GPOS kerning, complex scripts); rasterising with `ab_glyph` at the same variation coordinates.
   - Line fitting picks 1–4 lines by balanced-width DP. It penalises lines that end in "of/the/and/de/la…", allows larger type for ≤ 4-character titles, and sets `Title: Subtitle` as a big title over a tracked small-caps subtitle.
   - CJK breaks between characters with abridged kinsoku rules. Typographic quotes (`Grandma’s`). A soft shadow from blurred coverage.
   - Titles the bundled fonts cannot set fall back to the first covering **system** font: Hiragino Sans GB, Apple SD Gothic Neo, Kohinoor Telugu, Arial Unicode, then Noto CJK/DejaVu on Linux. Bold or W6 faces in collections are preferred.
7. **Output.** Baseline JPEG (`jpeg-encoder`, q86) with **XMP provenance**: `Iptc4xmpExt:DigitalSourceType` is `compositeSynthetic` for posters and `digitalCapture` for backdrops and thumbs, plus `flux:artSource="frame"`. Also a blurhash (4×3 or 3×4) and width/height. `ArtImage::to_art_ref(url)` produces the API `ArtRef` with `ArtSource::Frame`.
8. **Sizes.**
   - Poster: 2:3 at the natural crop width, clamped to 600–1000 px. That gives 720×1080 from 1080p, 1000×1500 from 4K, and 600×900 from 360p.
   - Backdrop: 16:9, clamped to 1280–1920 px, **textless** (UIs draw their own scrims).

**Episode thumbnails** (`episode_thumbnail`): candidates come from the spoiler-safe 10–50 % window. The chosen keyframe is re-decoded at 2× the thumbnail width and output as 640×360 with no text.

**Hardware decode is off by default. This was measured, not assumed:**

| Mode | p50 | p95 | Notes |
|---|---|---|---|
| Software (default) | 0.64–0.88 s | 2.4–2.9 s | Three runs; faster on every one of the 24 files |
| VideoToolbox | 1.49 s | 3.4 s | VideoToolbox session setup per process outweighs one keyframe's decode, 4K HEVC included |

With hwaccel on, a failure is retried in software, and the first candidate decides for the rest of the title. AV1 on an M1 has no VideoToolbox decoder, and ffmpeg does *not* fall back on its own.

**GenAI posters (opt-in)** live in `art::genai`.

- **`PosterGenerator` trait**, with two implementations:
  - `OpenAiImages`: `/v1/images/generations`, `gpt-image-1-mini`, medium quality, 1024×1536, $0.015 list price.
  - `HttpImageApi`: generic fal/Replicate/BFL style. It POSTs `{prompt, negative_prompt, width, height, num_images, seed}` plus fixed extras, accepts `images[].url|b64_json`, `data[]` or `output` (string or list), and handles `data:` URIs.
- **`GenAiPosterPipeline::run(path, title, year)`:**
  1. Policy gate: off by default, then `ArtError::Disabled`.
  2. **Budget reservation:** `jobs::Budget`, $/day. When it is exhausted the result is `ArtError::Budget` *before* any HTTP call (tested with `expect(0)`).
  3. Concurrency semaphore.
  4. Optional VLM description of the user's own frames (`SceneDescriber`, local evidence only).
  5. **Deterministic, text-free prompt** from a template plus palette, mood and objects. Denied words (text, logo, title, "in the style of", celebrity…) are dropped from VLM phrases. People appear only as silhouettes. **The title is never in the prompt** (tested).
  6. Generate, then cover-crop to 2:3 and typeset the title with the same typography as frame posters.
  7. XMP `DigitalSourceType = trainedAlgorithmicMedia`, `flux:model`, `flux:promptHash`; `ArtSource::Generated`.
  8. Commit the actual cost.
- **Tests:** OpenAI shape, fal shape (URL download), Replicate shape (data URI), budget and disabled paths, and a VLM run that decodes real frames from `bbb_1080_10s.mkv`, checks the request carries `data:image/jpeg` parts, and checks the description shaped the prompt.

### 6. `jobs`: budgets so AI never blocks

- `JobQueue` lanes, in strict priority order:

  | Lane | Default concurrency |
  |---|---|
  | Playback | 8 |
  | Scan | 4 |
  | Metadata | 4 |
  | Artwork | 2 |
  | Analysis | 2 |
  | Ai | 1 |

- `pause(Lane::Ai)` and `resume()` are for when playback starts. Running jobs see `JobContext.paused`.
- Optional `ai_budget: Arc<Budget>` with $/day and tokens/day caps, reset at the UTC day boundary. A job reserves its estimate before it starts and `commit(actual)` records the real cost; an uncommitted reservation is released on drop.
- A budget-blocked AI job **waits in its lane while lower lanes keep running**, and the dispatcher re-checks every 60 s.
- Idempotency: `JobKey { content_fp, kind, model_id, model_version, prompt_version }`. A key that is queued, running or done is a `Duplicate`, so re-scans never pay twice. Failed keys may be retried. A new model or prompt version means a new key.
- Tests cover priority, dedupe, the paused AI lane next to a running scan lane, a budget hold that survives `resume()`, reservation release and commit, and UTC rollover on an injected clock.

## Public API (for integrators)

```rust
// TMDB
let tmdb = flux_meta::tmdb::TmdbClient::new(TmdbConfig { cache: Some(DiskCache::new(dir, Duration::from_secs(30*86400))), ..TmdbConfig::from_env()? })?;
tmdb.search_movie(&MovieSearch { query, year, ..Default::default() }).await? -> Paged<MovieResult>;
tmdb.movie_details(id).await? -> MovieDetails;   tmdb.tv_details(id) / tv_season(id, n) / find(ext, ExternalSource::Imdb) / configuration()
flux_meta::tmdb::attribution::{NOTICE, LOGO_GUIDELINES_URL, TMDB_TERMS};

// Offline prefilter
let path = export_index::download_latest(&http, dir, ExportKind::Movie, 3).await?;
let idx = ExportIndex::build_from_gz(&path, ExportKind::Movie, BuildOptions::default())?;
idx.candidates("Blade Runner", 10) -> Vec<ExportCandidate>;  idx.lookup_exact(t) -> Vec<Entry>

// Matching
let q = MatchQuery::from_guess(&name_guess, Some(&probe)).extra_titles(hypothesis_titles);
let r: MatchResult = Matcher::default().match_candidates(&q, &candidates);   // r.decision, r.ids, r.confidence, r.ranked[i].evidence

// AI (local evidence only)
let ev = LocalEvidence::from_path(rel_path).with_siblings(sibs).with_probe(&probe).with_parse(&guess);
let out: HypothesisOutput = OpenAiCompatible::new(OpenAiCompatibleConfig::ollama("qwen3.5:4b"))?.hypothesize(&ev).await?;
let q = out.hypothesis.refine(q);   // then match again; the matcher decides

// Art
let fp = art::frame_posters(path, "Title", Some(1999), MediaFacts::from_probe(&probe), &FramePosterOptions::default()).await?;
fp.poster.bytes / fp.poster.to_art_ref(url) /* ArtSource::Frame */ ; fp.backdrop
let (thumb, _) = art::episode_thumbnail(path, facts, &opts).await?;
let gp = GenAiPosterPipeline::new(Arc::new(OpenAiImages::new(key)?), None, budget, GenAiPolicy { enabled: true, ..Default::default() }).run(path, title, year).await?;

// Jobs
let q = JobQueue::new(QueueConfig { ai_budget: Some(Budget::new(BudgetLimits::default())), ..Default::default() }); q.start();
q.submit(Lane::Ai, JobKey::new(fp, "ai_hypothesis").with_model(m, v, ai::PROMPT_VERSION), Cost::usd(0.0005), job);
```

## How to run

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/agent-meta
cargo test -p flux-meta                      # 78 tests; real-media tests skip if the SSD is absent
cargo clippy -p flux-meta --all-targets
cargo run --release -p flux-meta --example meta_bench -- export [--download]   # index build/memory, REAL_MOVIES eval, matcher throughput
cargo run --release -p flux-meta --example meta_bench -- posters [--hwaccel] [--only SUBSTR] [--out DIR]
TMDB_READ_TOKEN=… cargo test -p flux-meta --test tmdb_live -- --ignored
FLUX_OLLAMA_MODEL=qwen3.5:4b cargo test -p flux-meta --test ai_mock -- --ignored
python3 -I crates/flux-meta/tools/wikidata_truth.py   # regenerate the REAL_MOVIES ground truth (then re-apply the 3 manual fixes noted above)
```

Results JSON goes to `$FLUX_WORK/bench/results/`:

- `flux-meta-export-index.json`
- `flux-meta-matcher.json`
- `flux-meta-match-eval.json` (every case, with decision and failure class)
- `flux-meta-frame-posters.json` (per-file timings and every candidate's scores)
- `flux-meta-frame-posters-hw.json` (only when run with `--hwaccel`)

## Gallery

These are generated from the synthetic test media: testsrc2 bars, Mandelbrot, and the real Big Buck Bunny clip in playback-research. Judge layout, typography and cropping, not content. I reviewed every one with the image viewer and iterated on layout (subtitle split, line-end penalties, CJK wrapping, curly quotes, short-title sizing) and on decode speed.

**Contact sheets:**

- `$FLUX_WORK/cache/posters/_sheets/posters.jpg`
- `$FLUX_WORK/cache/posters/_sheets/typography.jpg`
- `$FLUX_WORK/cache/posters/_sheets/backdrops-thumbs.jpg`

**Typography stress set (one Big Buck Bunny frame, 10 titles):**

- `$FLUX_WORK/cache/posters/typography/00.poster.jpg` to `09.poster.jpg`.
- Titles: *M*; *Heat*; *Amélie*; *Mad Max: Fury Road*; *The Lord of the Rings: The Fellowship of the Ring*; *Everything Everywhere All at Once*; 千と千尋の神隠し; 기생충; రౌద్రం రణం రుధిరం; *Grandma's 80th Birthday* (no year).

**Per file:**

- Every file gets `<file>.poster.jpg` and `<file>.backdrop.jpg`.
- The three `show_e0N_with_intro.mkv` files also get `.thumb.jpg`.

Files:

- `$FLUX_WORK/cache/posters/av1_opus.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/av1_opus.webm.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/bbb_1080_10s.mkv.{poster,backdrop}.jpg` (best example: real content)
- `$FLUX_WORK/cache/posters/bbb_1080_10s_30MB_actually_mp4.mp4.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_aac.m2ts.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_aac_faststart.mp4.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_aac_moov_end.mp4.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_dts51.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_flac.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_long_gop.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_multi_audio_subs.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/h264_truehd71.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/hevc_4k_60mbps.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/hevc_main10_hdr10_eac3.mkv.{poster,backdrop}.jpg` (PQ, tone-mapped)
- `$FLUX_WORK/cache/posters/hevc_main10_hdr10_hvc1.mp4.{poster,backdrop}.jpg` (PQ, tone-mapped)
- `$FLUX_WORK/cache/posters/intro_shared.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/long_2h_360p.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/mpeg2_ac3.ts.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/mpeg4_mp3.avi.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/show_e0{1,2,3}_with_intro.mkv.{poster,backdrop,thumb}.jpg`
- `$FLUX_WORK/cache/posters/synth_h264_10min.mkv.{poster,backdrop}.jpg`
- `$FLUX_WORK/cache/posters/synth_hevc10_opengop_10min.mkv.{poster,backdrop}.jpg`

## Known gaps (honest list)

- **No live TMDB or LLM run.**
  - There are no keys and no local model. Every network path is exercised against mocks built from documented shapes.
  - The `/search`-based accuracy (research/05 predicts "high 90s %" on scene names) is **not measured**. The 72.5 % figure is offline-export-only.
- **Translated titles (29/131) and remakes (6/131) can't be fixed offline** with the export alone. They need `/search` (key) or a CC0 Wikidata alias/year index. A Wikidata-backed offline index is the obvious next step and would also be AI-safe.
- **The export index build peak is ~180 MB RSS** (transient; the steady heap is 59.4 MB). Fixes: a two-pass streaming build, or building in a short-lived child process at refresh time. The index is also not persisted: it rebuilds from the `.gz` in ~2 s.
- **Fuzzy lookup is 0.63 ms/title.** Fine for a prefilter that runs once per new file, but an FST plus a trigram index would be faster.
- **Frame posters:**
  - No face detection or aesthetic model yet (research/05 §4.1 step 3–4). Composition is energy-based only.
  - No intro/credits exclusion beyond the 10–80 % window.
  - Seek-based candidates collapse on long-GOP or short files: 11 of 12 were duplicates on most test clips. Using the playback index's keyframe list (Cues/`stss`) would give distinct keyframes directly. See *Proposed core changes*.
  - Dolby Vision profile 5 (IPT-PQ-c2) will decode with wrong colours, because there is no DV reshaping without libplacebo. HLG uses an approximate per-channel OOTF.
- **Output formats:** JPEG only. Lossy WebP needs libwebp (C). The `image` crate's WebP encoder is lossless-only, so I did not add it.
- **Provenance:** XMP/IPTC only. No signed **C2PA** manifest yet; the `c2pa` crate is heavy and needs a signing cert decision. No OCR check that genAI output is text-free, and no NSFW or face-similarity validation (research/05 §5.4 step 6).
- **Fallback fonts:**
  - System CJK fonts are large (Hiragino 23 MB, Apple SD Gothic Neo 55 MB). They are loaded on first use and cached in the shared `Typesetter` for the process lifetime.
  - A footprint-sensitive server should use a per-batch `Typesetter` or add eviction. Without any system CJK font (minimal Linux), CJK titles render with missing glyphs.
- **Jobs:** in-memory only, with no persistence and no cancellation of *running* jobs on pause (cooperative only). The catalog should persist `JobKey` → status.
- **Anime absolute mapping:** only the cumulative-seasons default. TMDB episode groups (type 2) and AniDB/TVDB mappings are not fetched.
- **Licensing:** the bundled fonts are OFL (licences included). The TMDB logo is referenced by URL, not bundled.

## Proposed core changes (not made; flux-core is not mine)

1. **`ProviderIds.wikidata: Option<String>`.** Wikidata Q-ids are the CC0, AI-safe crosswalk (P4947/P4983 ↔ TMDB, P345 ↔ IMDb), and TMDB returns `external_ids.wikidata_id`.
2. **Keyframe hints for artwork.** Add `ProbeResult.keyframes_ms: Option<Vec<u64>>`, or a small core type that flux-hls's playback index can hand to flux-meta. The frame picker would then choose 12 *distinct* keyframes instead of blind seeks: on most test clips 11 of 12 seeks were duplicates.
3. **A shared match-decision type.** `MatchDecision { decision: Accept|Review|NoMatch|LookupExternalId, method: Rule|Ai|User, confidence, margin, evidence, model_id?, prompt_version? }` in core, for the catalog's `match_decision` table and `ItemDetail.match_*`. Today it is `flux_meta::matcher::{Decision, MatchResult}`.
4. **`ArtRef.generator: Option<String>`** (e.g. `"openai/gpt-image-1-mini"`), so the UI's mandatory "AI-generated" badge can name the model. The XMP already carries it.
