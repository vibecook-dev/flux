# 05 — Metadata providers, AI/agentic metadata, and genAI artwork for flux

*Research date: 2026-10-18. Author: research subagent (topic 05). Status: research + one small local prototype.*

**Confidence legend.** **[V]** = verified this session against a primary source (official docs, source code, model card, or a local measurement). **[S]** = secondary source (blog, forum, aggregator), not confirmed against a primary source. **[I]** = my inference or estimate. All prices are USD list prices as published on the cited page on 2026-10-18.

**Method note.** Official pages, source code (Jellyfin, Jellyfin plugins, Intro Skipper, Trakt, TheTVDB swagger, mflux), Hugging Face model cards, and crates.io were read directly. I also built and ran a small Rust prototype on the SSD: an offline filename-to-TMDB candidate matcher. Cloudflare blocked a few primary sites (fanart.tv, the AniDB wiki, some Plex and OpenAI help pages). For those I fell back to secondary sources, marked [S]. The session's shared web-search budget ran out partway through. After that, verification used direct page fetches and `gh` only.

---

## 0. TL;DR

1. **TMDB is still the right primary provider, but its API Terms are a load-bearing risk for the AI features.** API ToS §1.C prohibits using "the TMDB APIs or TMDB Content in connection with, including for training, a machine learning (ML) or artificial intelligence (AI) based Application." TMDB also reserves "the right… to use the TMDB APIs or TMDB Content in connection with… a machine learning or artificial intelligence based Application" (§1.A). Other terms: 6-month cache cap, mandatory logo and "not endorsed" notice, and non-commercial use only without a written agreement. [V] ([TMDB API ToS](https://www.themoviedb.org/api-terms-of-use), last updated 2023-10-20). **flux must design its AI pipeline so the LLM or VLM never receives TMDB content, and should ask TMDB for written clarification before shipping AI matching.**
2. **Offline-first is feasible.** The daily TMDB ID exports need no API key. As of 2026-10-18 they cover 1,260,557 movies (28 MB gz) and 234,360 TV series (5 MB gz) [V, local download]. They hold only `id`, `original_title`/`original_name`, `popularity`, `adult` and `video`: no year, no translated titles. In my prototype, an offline exact+fuzzy index over the export got **11/20 movie filenames right at top-1** [V, local]. Every failure was a translated title (*Parasite* → 기생충), a parser error (*Blade Runner 2049* parsed as S20E49), or a missing year (*Mulan* 1998 vs 2020). So the export is a fast **candidate prefilter**, not a matcher.
3. **The classic matcher still does most of the work.** It parses (`hunch` 2.0.2, a Rust guessit port, ~229 µs/name steady-state [V, local]), searches TMDB (`/search/movie|tv` plus `/find` for external IDs), and scores on title, year ±1, runtime-vs-probed-duration and language. Jellyfin's scorer is only title-exact 8 / prefix 4 / year±1 2 [V] ([TmdbUtils.cs:42-44, 188-255](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbUtils.cs)). Adding **probed duration** and **audio language** as evidence is an easy, measurable win over Jellyfin.
4. **Use AI for parsing and hypotheses, not adjudication.** The LLM turns messy filename, path and probe evidence into a structured *hypothesis*: canonical title(s) incl. translations, year, type, edition and numbering scheme. Deterministic code then queries the providers and verifies. Cloud cost is negligible: about **$0.0003–$0.0005 per hard item** with gpt-5-nano, Gemini 2.5 Flash-Lite or Claude Haiku 5.5 [V prices, I token counts]. On Apple Silicon a 4B local model takes roughly 3–7 s per item; on an N100 it takes tens of seconds [I, from the [llama.cpp M-series table](https://github.com/ggml-org/llama.cpp/discussions/4167)].
5. **Key handling should follow the OSS precedent, with opt-outs.** Jellyfin ships a built-in TMDB key with a user override ([TmdbUtils.cs:35](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbUtils.cs), [TmdbClientManager.cs:50-52](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbClientManager.cs)). It ships a TVDB *project* key plus an optional **subscriber PIN** ([jellyfin-plugin-tvdb PluginConfiguration.cs:13-24](https://github.com/jellyfin/jellyfin-plugin-tvdb/blob/f1bcc9edf37d5ee65cf3683e89b76226c09a22eb/Jellyfin.Plugin.Tvdb/Configuration/PluginConfiguration.cs)) and a built-in OpenSubtitles key [V]. flux should ship a TMDB project key (with TMDB's blessing), support BYO keys everywhere, and treat TVDB as user-PIN or licensed.
6. **Vision and audio AI ranked by value per watt:** (1) intro/credits detection by cross-episode Chromaprint matching plus black-frame/silence refinement. Intro Skipper's algorithm and defaults are documented below and portable to pure Rust via `rusty-chromaprint`. (2) Deterministic thumbnail selection: scene cuts, blur and exposure rejection, face presence, spoiler window. (3) On-device subtitles: Parakeet v3 (CC-BY-4.0, 25 EU languages), Whisper large-v3-turbo (MIT), or Apple SpeechAnalyzer on macOS 26. (4) Embeddings for semantic search: SigLIP 2 / CLIP on keyframes, EmbeddingGemma or Qwen3-Embedding on plots.
7. **GenAI posters for unmatched titles: default to a *frame poster*.** The best frame plus deterministic typography costs $0 and has no model or legal risk. Offer genAI as opt-in. The cheapest quality cloud options as of 2026-10-18: **FLUX 3 Image Klein API $0.009/1K image**, **Nano Banana 2.1 $0.0336/1K**, **gpt-image-2 medium portrait $0.041**, **gpt-image-1-mini medium portrait $0.015** [V]. Local options on Apple Silicon via mflux 0.23.0: **FLUX.2 [klein] 4B (Apache-2.0)**, **Z-Image-Turbo (Apache-2.0)**, Qwen-Image (Apache-2.0) [V]. Always typeset the title yourself (cosmic-text + tiny-skia/resvg), and label and C2PA-sign the output.
8. **Compliance is now dated, not hypothetical.** The EU AI Act Art. 50 transparency duties apply from 2 Aug 2026. Machine-readable marking for systems already on the market has a grace period to 2 Dec 2026 [S]. OpenAI and Google outputs already carry C2PA and/or SynthID [S]. flux should label every generated asset in the UI, embed C2PA (`c2pa` crate 0.91.3) plus IPTC `trainedAlgorithmicMedia`, and keep generation opt-in.
9. **The architecture rule is that AI never blocks scan or playback.** Use pluggable provider traits, per-field provenance and locks, a job queue with priority classes and **budgets** (concurrency, GPU slots, $/day, tokens/day), and idempotent jobs keyed by content fingerprint + model version. Remote inference can run on another tailnet device through **Truffle egress (RFC 026)**, which is a real dogfooding opportunity ([egress.rs:1-15](https://github.com/vibecook-dev/truffle/blob/d6f66647a1d33af21a83cb7234cc5ddce3c2e7bb/crates/truffle-core/src/egress.rs)).

---

## 1. What flux needs from "metadata"

| Need | Primary source | Fallbacks | Notes |
|---|---|---|---|
| Identity (movie/series/episode IDs) | TMDB | TVDB, IMDb ID via `/find`, Wikidata crosswalk, AniDB/AniList (anime) | Store *all* external IDs; TMDB ID is not the internal key |
| Descriptive text (title, overview, genres, cast) | TMDB (multi-language) | TVmaze (CC BY-SA), TVDB | Localized titles matter for matching |
| Artwork (poster, backdrop, logo, still) | TMDB image CDN | fanart.tv (clearlogo/clearart), TVDB, frame posters, genAI | Logos/clearart are fanart.tv's strength |
| Episode structure & orderings | TMDB (+ episode groups) | TVDB season types | Aired/DVD/absolute/story-arc |
| Segments (intro/recap/credits) | Local analysis | TheIntroDB (community) | See §4.2 |
| Subtitles | Local ASR | OpenSubtitles | ASR is offline and private |
| Ratings / tracking sync | Trakt (user OAuth) | — | Per-user integration, not a catalog source |

---

## 2. Provider landscape (2026)

### 2.1 Summary table

| Provider | Data licence / ToS model | Rate limit | Cost | Attribution | AI/ML restriction | Key model | Confidence |
|---|---|---|---|---|---|---|---|
| **TMDB** API v3 (+v4 auth/lists) | Non-commercial licence; commercial needs written agreement | "somewhere in the 40 requests per second range"; legacy 40/10 s disabled 2019-12-16 | Free (non-commercial) | Logo (less prominent than yours) + "uses TMDB… not endorsed" notice | **Yes**: no use "in connection with… ML or AI based Application"; Chatbots = commercial | API key or Bearer "API Read Access Token" (works for v3 and v4) | [V] |
| **TMDB daily ID exports** | Same ToS | n/a (static files) | Free, **no auth (currently)** | Same | Same | None | [V] |
| **TheTVDB v4** | Licence tiers by company revenue | Not published | Free <$50k/yr revenue (with attribution); $1k/yr to $250k; $10k/yr to $1M; OSS "significant discounts"; or users bring a **subscriber PIN** | "Metadata provided by TheTVDB" + link | Not stated on the API page | `/login` with `apikey` (+`pin` for user-supported keys) → bearer token valid 1 month | [V] |
| **TVmaze** | **CC BY-SA 4.0**, "any purpose" with credit and ShareAlike | "at least 20 calls every 10 seconds per IP" (backend; edge cache exempt) | Free; premium user API | Link back to TVmaze | None stated (but ShareAlike) | None (public) | [V] |
| **OMDb** | **CC BY-NC 4.0** | 1,000/day free | Patreon tiers | CC attribution | NC | Key | [V] |
| **fanart.tv** v3 | Project key + optional personal key | Not verified | Free; VIP | Credit | Not verified | Project key (+ personal key gets newer images faster) | [S] (site blocked) |
| **IMDb datasets** | "personal and non-commercial use" | n/a (daily TSV dumps) | Free | IMDb terms | — | None | [V] |
| **Wikidata** | **CC0** (structured data) | WDQS: ~60 s of query time per 60 s per UA+IP; 60 s/query timeout | Free | None required | **None** | None (send descriptive UA) | [V] data, [S] limits |
| **Trakt** | API terms | `UNAUTHED_API_GET_LIMIT` 500 per 5 min per client+IP; authed GET 500/5 min; POST/PUT/DELETE 1/s | Free; VIP $60/yr (2025) | Trakt branding | Not stated | OAuth client ID | [V] limits, [S] pricing |
| **OpenSubtitles.com** | API key + user login | Jellyfin client limits itself to 5 req/s; download quotas vary (free ~5–20/day, VIP 1,000/day reported) | Free/VIP | Credit | — | API key (Jellyfin ships one) | [V] plugin, [S] quotas |
| **AniList** | Free non-commercial; commercial free below $150/month revenue | 90/min (currently **degraded to 30/min**) | Free | App naming rules | No "backup/data storage", no hoarding | Optional OAuth | [V] |
| **AniDB** | Strict client registration | HTTP API ~1 req/2 s; bans after ~100–250 req/day reported; UDP 1 pkt/4 s long-term | Free | — | — | Registered client name | [S] (wiki blocked) |

Sources: TMDB [rate limiting](https://developer.themoviedb.org/docs/rate-limiting), [API ToS](https://www.themoviedb.org/api-terms-of-use), [auth](https://developer.themoviedb.org/docs/authentication-application), [daily exports](https://developer.themoviedb.org/docs/daily-id-exports); TheTVDB [API information](https://thetvdb.com/api-information), [PIN FAQ](https://support.thetvdb.com/kb/faq.php?id=82), [v4 swagger lines 6-7, 17-30](https://github.com/thetvdb/v4-api/blob/main/docs/swagger.yml); [TVmaze API](https://www.tvmaze.com/api); [OMDb](https://www.omdbapi.com/apikey.aspx); [IMDb datasets](https://data.imdb.com/non-commercial-datasets/); Trakt [rate-limiting guide (updated 2026-10-13)](https://github.com/trakt/trakt-api/blob/master/projects/developer/src/lib/guides/rate-limiting.md) and [VIP price news](https://alternativeto.net/news/2025/2/trakt-tv-has-set-stricter-limits-for-free-users-and-raised-vip-subscription-prices-by-100-/); AniList [rate limiting](https://docs.anilist.co/guide/rate-limiting) and [terms](https://docs.anilist.co/guide/terms-of-use); AniDB community reports ([FileBot forum](https://www.filebot.net/forums/viewtopic.php?p=51839)); fanart.tv community report ([MediaPortal forum](https://forum.team-mediaportal.com/threads/fanart-tv-api-v3.126940/page-2)); WDQS limits as documented in [WikidataQueryServiceR manual](https://wikimedia.r-universe.dev/WikidataQueryServiceR/doc/manual.html).

### 2.2 TMDB in detail

**Licence and ToS clauses that shape flux's design** [V] (live page fetched 2026-10-18; "last updated on: October 20, 2023"):

- **§1.C "Cache, for longer than 6 months, any information obtained through or from TMDB."** Every TMDB-derived row needs `fetched_at`, and a refresh or purge job must run before the 6-month mark. Locally generated derivatives (thumbnails, *your* embeddings of *your* frames) are not TMDB content. A cached TMDB poster is TMDB content.
- **§1.C AI clause** (quoted in the TL;DR) plus §2.A: Chatbots and "interactive query-response system (including LLM…)" uses are classed as commercial. **[I] Implication:** anything that sends TMDB overviews, titles or candidate lists to an LLM, or feeds TMDB posters to an image model, is exposed. Design rules for flux:
  1. **AI-isolated mode (default):** the LLM sees only user-owned evidence: filename, path, probe data, embedded tags, OCR of the user's own frames, transcripts of the user's own audio. It outputs a *hypothesis*. Deterministic, non-AI code queries TMDB and verifies.
  2. **Open-data mode:** where the AI must reason over candidates, give it candidates from **Wikidata (CC0)** or **TVmaze (CC BY-SA)**, not TMDB, then crosswalk the winning Wikidata item to a TMDB ID via P4947/P4983.
  3. **Never** use TMDB images as img2img or reference input for generative models.
  4. Email TMDB (API for Business) for a written interpretation. This is the single most important open question (§10).
- **§3 Attribution:** TMDB logo (less prominent than flux's) plus the exact notice *"This [application] uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB."*
- **§2 Commercial use:** a free OSS server is non-commercial. Any future paid tier, hosted relay or ad revenue needs a TMDB commercial agreement.
- The ToS has **no clause about shipping a key in OSS**. It forbids concealing your application's identity (§1.C), which argues for a *flux-identified* project key over an anonymous shared one. TMDB support threads and Jellyfin's practice (§2.11) suggest a shipped project key is tolerated [I].

**API mechanics** [V]:

- Auth: `api_key` query param **or** `Authorization: Bearer <API Read Access Token>`. Both give the same access on v3 ([docs](https://developer.themoviedb.org/docs/authentication-application)). Prefer the Bearer header so keys stay out of URLs and logs.
- `append_to_response` bundles sub-requests (e.g. `videos,images,external_ids,release_dates`) into **one HTTP request** ([docs](https://developer.themoviedb.org/docs/append-to-response)). This is the main lever for per-title call budgets.
- Change tracking: `/movie/changes`, `/tv/changes`, `/person/changes` give a default 24 h window, extendable to 14 days. Then fetch per-ID field-level changes ([docs](https://developer.themoviedb.org/docs/tracking-content-changes)). A daily changes poll plus the 6-month hard refresh keeps the cache compliant and fresh at about 1 request per changed title.
- Images: `https://image.tmdb.org/t/p/{size}{file_path}`. Take sizes from `/configuration`. Logos may be SVG at `original` ([docs](https://developer.themoviedb.org/docs/image-basics)). Jellyfin exposes size settings per image kind ([PluginConfiguration.cs:106-126](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/Configuration/PluginConfiguration.cs)).
- **Episode groups** (alternative orderings) `GET /3/tv/episode_group/{id}`, types: 1 Original air date, 2 Absolute, 3 DVD, 4 Digital, 5 Story arc, 6 Production, 7 TV ([docs](https://developer.themoviedb.org/reference/tv-episode-group-details)).
- Jellyfin caches TMDB responses in memory for only **1 hour** ([TmdbClientManager.cs:26](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbClientManager.cs)). flux can do much better with a persistent, ToS-bounded cache.

**Daily ID exports: verified locally** [V]. URL pattern `https://files.tmdb.org/p/exports/{movie_ids|tv_series_ids|person_ids|collection_ids|tv_network_ids|keyword_ids|production_company_ids}_MM_DD_YYYY.json.gz`. Files are generated 07:00–08:00 UTC and kept for 3 months. Each is newline-delimited JSON:

```text
movie_ids_10_18_2026.json.gz      28,221,976 bytes   1,260,557 rows  {"adult","id","original_title","popularity","video"}
tv_series_ids_10_18_2026.json.gz   5,123,703 bytes     234,360 rows  {"id","original_name","popularity"}
collection_ids_10_18_2026.json.gz    166,700 bytes
keyword_ids_10_18_2026.json.gz     1,050,414 bytes
```

They are useful for (a) an offline candidate prefilter, (b) knowing which IDs exist (detecting merges and deletions), and (c) popularity priors. They lack year, aliases and translations, so they cannot replace `/search` (§3.3).

### 2.3 TheTVDB v4

- Licensing is by company revenue [V] ([api-information](https://thetvdb.com/api-information)): free under $50k/yr *with attribution*; $1,000/yr for $50k–250k; $10,000/yr for $250k–1M; custom above that. OSS and non-commercial projects get "significant discounts."
- **User-supported keys:** end users subscribe and get a **PIN**. The app sends `apikey` + `pin` to `/login` and receives a bearer token valid for **1 month** [V] (swagger lines 6-7, 19-30). PINs must not be shared ([FAQ](https://support.thetvdb.com/kb/faq.php?id=82)).
- **Season types** for episode listing `GET /series/{id}/episodes/{season-type}`: `default, official, dvd, absolute, alternate, regional` [V] (swagger lines 1807-1842).
- **Role in flux:** optional provider for TV orderings and anime-adjacent series. Ship with "bring your PIN" by default. If flux ever obtains a licensed key, the PIN becomes optional. TheIntroDB recommends TVDB IDs because display orders resolve correctly ([TheIntroDB plugin README](https://github.com/TheIntroDB/jellyfin-plugin)) [V].

### 2.4 TVmaze, OMDb, IMDb, Wikidata

- **TVmaze** [V]: CC BY-SA 4.0. Data "can freely be used for any purpose, as long as TVmaze is properly credited." API output is cached 60 min, and **image URLs are immutable and can be cached indefinitely**. It is the most AI-compatible TV source after Wikidata. ShareAlike applies to *data* you redistribute, not flux's code [I].
- **OMDb** [V]: 1,000 requests/day free, CC BY-NC 4.0. Low value for flux beyond rating snippets; skip by default.
- **IMDb non-commercial datasets** [V]: daily TSVs (`title.basics`, `title.akas`, `title.episode`, `title.ratings`, `title.principals`, `title.crew`, `name.basics`) at `datasets.imdbws.com`, "personal and non-commercial use". `title.akas` (localized titles) and `title.basics.runtimeMinutes` are excellent matching signals. **Recommendation:** an *opt-in* "local IMDb index" for personal servers, never enabled by default and never redistributed.
- **Wikidata** [V]: CC0 structured data, with crosswalk properties verified via the API: **P4947 TMDB movie ID, P4983 TMDB TV series ID, P345 IMDb ID, P4835 TheTVDB series ID, P5646 AniDB anime ID, P8729 AniList anime ID, P2047 duration**. A live SPARQL test resolved TMDB 496243 to Q61448040 "Parasite", IMDb tt6751668, duration 132 (minutes). This is flux's best **AI-safe** candidate source and crosswalk. Send a descriptive User-Agent and respect the query-time budget [S].

### 2.5 Artwork: fanart.tv

fanart.tv is the de-facto source for `clearlogo`, `clearart`, `discart`, `banner` and `characterart`. API v3 needs a project key. A *personal* key reportedly surfaces newly approved images faster (~48 h vs ~1 week) [S] ([MediaPortal forum](https://forum.team-mediaportal.com/threads/fanart-tv-api-v3.126940/page-2)). The site was Cloudflare-blocked for verification; treat its terms as unverified. **Role:** optional artwork provider for logos, which flux's UI can use for premium-looking title treatments.

### 2.6 Trakt

Trakt is a per-user sync integration (scrobble, watched history, ratings), not a catalog source. Limits [V]: authed GET 500/5 min per user, POST/PUT/DELETE 1/s, unauthed GET 500/5 min per client ID + public IP. Premium apps on `apiz.trakt.tv` get higher limits. VIP-only methods return **426**, and account-limit errors return **420** ([vip-methods.md](https://github.com/trakt/trakt-api/blob/master/projects/developer/src/lib/guides/vip-methods.md), [user-account-limits.md](https://github.com/trakt/trakt-api/blob/master/projects/developer/src/lib/guides/user-account-limits.md)). In 2025 Trakt doubled VIP to $60/yr and capped free accounts' lists and connected apps [S].

### 2.7 OpenSubtitles: a matching signal, not just subtitles

- The **OpenSubtitles hash** is file size + the sum of 64-bit little-endian words of the first and last 64 KiB (mod 2^64) ([Jellyfin implementation, OpenSubtitlesRequestHelper.cs:42-53](https://github.com/jellyfin/jellyfin-plugin-opensubtitles/blob/master/Jellyfin.Plugin.OpenSubtitles/OpenSubtitlesHandler/OpenSubtitlesRequestHelper.cs)) [V]. Jellyfin's plugin searches with `moviehash` and `moviehash_match=only` [V]. A hash hit on a release identifies the exact title and returns feature IDs (IMDb/TMDB) [S].
- **Cost:** two 64 KiB reads per file. My Python implementation over 23,618 (mostly sparse) test files on the SSD ran at **4.7 ms/file, CPU-bound** [V, local; Python overhead dominates; Rust would be I/O-bound]. On a NAS HDD expect ~2 seeks per file [I].
- **Recommendation:** compute this hash for every new video during scan. It doubles as a cheap *content fingerprint* for identity (rename/move detection), complementing Mille's (path, size, mtime). Query OpenSubtitles only for unmatched items, rate-limited, with the user's account.
- Jellyfin ships an OpenSubtitles API key ([OpenSubtitlesPlugin.cs:19](https://github.com/jellyfin/jellyfin-plugin-opensubtitles/blob/master/Jellyfin.Plugin.OpenSubtitles/OpenSubtitlesPlugin.cs)) and a client-side fixed-window limiter of **5 requests/second** ([ClientSideRateLimiter.cs](https://github.com/jellyfin/jellyfin-plugin-opensubtitles/blob/master/Jellyfin.Plugin.OpenSubtitles/ClientSideRateLimiter.cs)) [V]. Download quotas for free accounts have changed repeatedly (5, 10, 20 and 200/day all reported) [S].

### 2.8 Anime

Anime breaks western assumptions: absolute numbering, cours, OVAs/specials, and many romanized titles.

- **AniList** [V]: GraphQL at 90 req/min, currently "degraded" to **30 req/min**. Free for non-commercial use and for commercial use under $150/month revenue. Bans "backup/data storage" and "hoarding/mass collection," so flux must fetch on demand and cache narrowly.
- **AniDB** [S]: authoritative episode lists and the daily `anime-titles` dump. The HTTP API is strict: ~1 request per 2 s, bans after a few hundred requests/day, ~24 h cooldown. Use the titles dump at most daily, and API calls only for unmatched shows.
- **Mapping datasets** [V repos]: [Fribb/anime-lists](https://github.com/Fribb/anime-lists) (AniDB/AniList/MAL/TMDB/TVDB ID mapping JSON, updated 2026-10-13), [Anime-Lists/anime-lists](https://github.com/Anime-Lists/anime-lists) (AniDB→TVDB season/episode offsets, updated 2026-10-14), [manami-project/anime-offline-database](https://github.com/manami-project/anime-offline-database). These provide the offline crosswalk. **Licences are unclear** (no SPDX licence on two of them), so fetch at runtime rather than vendoring them [V/I].
- **Shoko Server** (MIT, [repo](https://github.com/ShokoAnime/ShokoServer)) is the reference for hash-based anime identification against AniDB. Study its approach before building anime support.
- **Parser:** `hunch` parsed `[SubsPlease] Frieren - 12 (1080p) [ABCD1234].mkv` as title "Frieren", E=12 [V, local]. The absolute→season mapping then needs AniDB/TVDB data.

### 2.9 Community segment databases

**TheIntroDB** exposes intro, recap, credits and preview timestamps through an API (v4). Its Jellyfin plugin needs Jellyfin 12+ and addresses episodes by TVDB ID with display order, then falls back to TMDB and IMDb ([README](https://github.com/TheIntroDB/jellyfin-plugin)) [V]. Plex shares locally computed credit markers to a cloud DB anonymously by default [S] ([Plex forum](https://forums.plex.tv/t/credits-detection/831048)). **flux option:** read from TheIntroDB when available. Write-back should be opt-in.

### 2.10 How Jellyfin and Plex handle keys and providers

| Project | Provider | How the key is handled | Evidence |
|---|---|---|---|
| Jellyfin 12.x | TMDB | Compile-time constant `TmdbUtils.ApiKey`; used unless the admin sets `TmdbApiKey` in plugin config | [TmdbUtils.cs:35](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbUtils.cs), [TmdbClientManager.cs:50-52](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbClientManager.cs), [PluginConfiguration.cs:16](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/Configuration/PluginConfiguration.cs) [V] |
| Jellyfin TVDB plugin | TheTVDB v4 | Constant `ProjectApiKey` + user `SubscriberPIN` passed to login | [PluginConfiguration.cs:13,22-24](https://github.com/jellyfin/jellyfin-plugin-tvdb/blob/f1bcc9edf37d5ee65cf3683e89b76226c09a22eb/Jellyfin.Plugin.Tvdb/Configuration/PluginConfiguration.cs), [TvdbClientManager.cs:59,82-83](https://github.com/jellyfin/jellyfin-plugin-tvdb/blob/f1bcc9edf37d5ee65cf3683e89b76226c09a22eb/Jellyfin.Plugin.Tvdb/TvdbClientManager.cs) [V] |
| Jellyfin OpenSubtitles plugin | OpenSubtitles.com | Constant `ApiKey` header + user login; 5 req/s client limiter | [OpenSubtitlesPlugin.cs:19](https://github.com/jellyfin/jellyfin-plugin-opensubtitles/blob/master/Jellyfin.Plugin.OpenSubtitles/OpenSubtitlesPlugin.cs), [RequestHandler.cs:45](https://github.com/jellyfin/jellyfin-plugin-opensubtitles/blob/master/Jellyfin.Plugin.OpenSubtitles/OpenSubtitlesHandler/RequestHandler.cs) [V] |
| Plex | Built-in "Plex Movie/Series" agents | Metadata served from Plex's own cloud service, so keys and licences are Plex's problem, not the user's | [S]/[I]: not re-verified this session |
| Plex PMS ≥1.43.0 | **Custom Metadata Providers** (announced 2025-12-09) | Any HTTP service implementing Plex's provider API; an *agent* is an ordered list of providers. Unauthenticated only at launch. Legacy agents to be removed from new PMS releases in 2026 | [Plex announcement](https://forums.plex.tv/t/announcement-custom-metadata-providers/934384) [V] |

**Takeaways for flux:**
1. **Ship a flux project key for TMDB** (identified as flux; ToS §1.C) with a settings field for a personal key. Ask TMDB for consent explicitly; Jellyfin's precedent is not a licence.
2. **No shipped TVDB key** unless licensed: "TVDB (bring your subscriber PIN)".
3. **Plex's provider model is worth copying.** Make flux metadata providers HTTP-pluggable in addition to Rust traits, so community providers (adult, sports, music videos, home video) don't require recompiling. A flux provider could even be Plex-compatible in a later phase [I].

### 2.11 Caching and offline-first strategy

| Data | Store | TTL / refresh | Rationale |
|---|---|---|---|
| TMDB detail JSON (per title, per language) | SQLite table `provider_record(provider, kind, id, lang, etag?, fetched_at, payload_zstd)` | Refresh via daily `/changes` poll; **hard re-fetch or purge before 180 days** | ToS §1.C 6-month cap |
| TMDB images | Content-addressed blob store (BLAKE3) + per-size variants | Same 180-day bound; re-download if `file_path` changes | Images are TMDB content |
| TMDB ID exports | `/cache/tmdb-exports/` + compact index (FST or SQLite FTS5) | Daily, 07:00–08:00 UTC + jitter | Offline prefilter; no key |
| Wikidata crosswalk | SQLite | 30 days | CC0, no cap |
| TVmaze | SQLite + immutable image cache | API 1 h freshness; images forever | Per TVmaze guidance |
| AniDB titles dump | File | ≤ 1 download/day | Ban avoidance |
| Negative results ("no match") | SQLite with evidence hash | 7–30 days exponential backoff | Avoid hammering on junk files |
| AI outputs (hypotheses, captions, embeddings) | SQLite + vector index | Keyed by `(content_fp, model_id, prompt_version)` | Deterministic re-use; invalidate on model change |

**HTTP layer:** `reqwest` + `reqwest-middleware` 0.5.2 + `http-cache-reqwest` 0.16.0 for RFC-7234 caching where providers send cache headers. Use a per-provider `governor` 0.10.4 token bucket (TMDB ~20 rps sustained by default, well under the ~40 ceiling), and `backon` 1.6.0 for 429/`Retry-After` handling [V crate versions, crates.io 2026-10-18].

**Offline behaviour:** a scan must complete with zero network access. Items get `match_state = pending_online` and sort into "Recently added" by filename-derived title, using a frame poster (§5.6). Metadata fills in when the network returns.

**Sharing across a user's devices:** a flux node can publish its *derived, non-provider* assets (segments, thumbnails, embeddings, AI hypotheses) to the user's other nodes through Truffle `SyncedStore` ([synced_store/mod.rs:1-19](https://github.com/vibecook-dev/truffle/blob/d6f66647a1d33af21a83cb7234cc5ddce3c2e7bb/crates/truffle-core/src/synced_store/mod.rs)). Do **not** bulk-replicate TMDB content between users; that edges toward redistribution [I].

---

## 3. Matching

### 3.1 Evidence available per file

| Signal | Cost | Strength | Source |
|---|---|---|---|
| Path + filename tokens (title, year, SxxEyy, edition, group) | ~0.23 ms parse | High for scene-named files; weak for home video | `hunch` [V local] |
| Folder structure (`Movies/Heat (1995)/…`, `Show/Season 02/…`) | ~0 | High | Path rules |
| Sidecar NFO / `.tmdb` / `{tmdb-12345}` tags in names | ~0 | Definitive | Kodi/Jellyfin conventions |
| Embedded container tags (`title`, `date`, `comment`, Matroska `TMDB` tags) | Probe | Medium–high | ffprobe |
| **Probed duration** | Probe (already needed for playback) | High discriminator vs runtime | ffprobe |
| Audio/subtitle languages | Probe | Medium (e.g. Korean audio → *Parasite* 2019, not 2013) | ffprobe |
| Video resolution/HDR/date | Probe | Low (era hint) | ffprobe |
| OpenSubtitles hash | 2×64 KiB read | Definitive when hit | §2.7 |
| Camera-file patterns (`VID_YYYYMMDD_HHMMSS`, `IMG_1234.MOV`, `PXL_`, `DJI_`, `GOPR`, `MVI_`) + QuickTime `creation_time`/GPS | ~0 | Definitive for "personal video" | Rules + probe |
| OCR of title card (first 10 min) | 0.1–1 s/frame | Medium for obscure films | §4.4 |
| ASR snippet (language, spoken title) | seconds | Medium | §4.5 |

### 3.2 Classic algorithm (deterministic, the default)

```
parse(path) → ParsedName{title, alt_titles[], year?, kind, season?, episode?, abs_ep?, part?, edition?, date?}
classify(parsed, probe) → {movie | episode | personal | extra | unknown}
  personal if camera pattern || (no year && no SxxEyy && creation_time present && duration < 3h && no parse confidence)
candidates =  (1) explicit IDs (NFO, {tmdb-…}, oshash hit)            → score 1.0, stop
              (2) offline prefilter (TMDB export, exact+fuzzy)          → ids
              (3) TMDB /search/{movie|tv}?query=…&year=… (+language)   → results (searches alt titles)
              (4) Wikidata label/alias search (multilingual, CC0)       → crosswalk ids
score(c) = 0.40·title_sim(best of title/original_title/alt_titles)
         + 0.20·year_score(|Δ|=0 →1, 1 →0.8, 2 →0.3, else 0)
         + 0.25·runtime_score(probed vs TMDB runtime: |Δ|≤3 min →1, ≤8% →0.7, editions ±15% →0.4, else 0)
         + 0.10·language_score(original_language ∈ audio langs)
         + 0.05·popularity_prior(log)
accept if score ≥ 0.85 and margin to #2 ≥ 0.10
review if 0.60 ≤ score < 0.85 or margin < 0.10
AI fallback if no candidate ≥ 0.60 or parse confidence = Low
```

The weights are a starting point [I]. Tune them on a labelled corpus (Prototype A). Runtime is the evidence Jellyfin ignores ([TmdbUtils.cs:188-255](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbUtils.cs) scores only title and year). It catches remakes (*Mulan* 1998 at 88 min vs 2020 at 115 min [I, typical runtimes]) and lets editions be recognized rather than mismatched. Jellyfin's own comment notes TMDB's `year` "only nudges relevance, it does not filter" [V], so flux must re-rank client-side.

### 3.3 Prototype results: offline candidate generation (measured)

Build: `$FLUX_WORK/proto/metadata-match` (Rust, `hunch` 2.0.2 + `strsim` + `deunicode`; `CARGO_TARGET_DIR` on the SSD). Input: the 2026-10-18 TMDB movie export plus 28 deliberately tricky names. Output saved at `$FLUX_WORK/proto/metadata-match/run1.txt` [V, local, M1 Max]:

| Metric | Value |
|---|---|
| Export rows indexed (non-adult, non-video) | 1,195,508 (1,023,233 distinct normalized titles; 442,650 tokens) |
| Index build from .gz | **1.80 s**, **381 MB RSS** (naïve `HashMap<String,Vec<u32>>`; far too heavy for an N100, so use FST/SQLite FTS5) |
| `hunch` parse, steady state | **229 µs/name** single-threaded (≈23 s per 100k files on one core; parallelize and cache by path+mtime) |
| Lookup (exact or rare-token fuzzy, Jaro-Winkler) | ~0.4–0.5 ms/name |
| **Top-1 correct, 20 movie names with a known answer** | **11/20 (55%)**, offline only, no year and no API |

Failure taxonomy (each row is a real output from the run):

| Input | Offline result | Root cause | What fixes it |
|---|---|---|---|
| `Blade.Runner.2049.2017…` | parsed **S20E49**, matched *Blade Runner* (1982) | Year-like number in title | LLM hypothesis or a "known title with number" rule; `/search` with both readings |
| `Dune Part Two (2024)` | title "Dune" → *Dune* (2021) | Parser dropped "Part Two" | Use the year + runtime gate; LLM restores the full title |
| `Mulan.2020…` | *Mulan* 1998 ranked above 2020 (popularity) | Export has no year | `/search` with year + runtime |
| `Parasite.2019.KOREAN` | *Parasite* (2 unrelated films) | Export has only `original_title` (기생충) | `/search` (matches translations), audio language = ko |
| `Amelie (2001)`, `Spirited Away`, `Godzilla Minus One`, `Ran 1985`, `M 1931` | wrong or none | Same: translated vs original titles | `/search`, Wikidata aliases, IMDb `title.akas` (opt-in) |
| `Star.Wars.Episode.IV.A.New.Hope.1977` | Episodes I/II/III | TMDB original_title is "Star Wars" | `/search` alt titles + year 1977 |
| `Grandma 80th Birthday 2014.mp4` | *Grandma's Boy* (JW 0.896) | Personal video looks like a title | **Personal-video classifier** + score threshold + runtime gate |
| `VID_20190713_154212.mp4` | parsed title "VID", type episode | Camera filename | Camera-pattern rule → personal |
| `Star Wars Despecialized Edition v2.7` | *Star Wars* (1977) top-1 | Fan edit | Correct work, but must become a separate **Edition** ("fan edit") |

**Conclusion [V+I]:** the export prefilter resolves the "easy half" in microseconds with no key. `/search` plus year and runtime verification should take the rest of the scene-named corpus to the high 90s% [I, to be measured in Prototype A]. The LLM is needed for a long tail: parser errors, fan edits, non-English names, personal video.

### 3.4 TV-specific issues

- **Ordering:** the same file `S01E05` means different episodes under aired vs DVD order (classic *Firefly*/*Futurama* problems). Store per series `ordering ∈ {aired, dvd, absolute, story_arc, …}` with the provider's group ID: TMDB episode group types 1–7 [V], TVDB season types `official/dvd/absolute/alternate/regional` [V]. Detect a likely mismatch automatically: if probed durations or embedded episode titles fit a different ordering better, propose a switch in the review UI.
- **Absolute numbering (anime, long-running shows):** `One.Piece.E1071` parsed as E=1071, no season [V local]. Map it via TMDB "Absolute" episode groups or AniDB/TVDB absolute season types.
- **Multi-episode files** (`S01E01-E02`), **specials** (`S00`), **date-based shows** (`Show.2026.10.17`) and **double-length pilots**: the runtime check (episode duration × n) disambiguates.
- **Episode title verification:** `The Office US S02E01 The Dundies` carries an episode title. Fuzzy-match it against the season's episode names to confirm both series identity (US vs UK *The Office*) and ordering [I].

### 3.5 AI-assisted matching

**Where AI adds value (and where it doesn't):**

| Problem | AI value | Notes |
|---|---|---|
| Parser errors (year-in-title, dropped "Part Two", unusual separators) | **High** | LLM sees the raw name + parse and emits corrected fields |
| Non-English / transliterated / fan-translated names | **High** | LLM proposes original-language titles and romanizations for `/search` |
| Fan edits, bootlegs, concert recordings, YouTube rips | **High** | LLM classifies "edition", "extra", "not in DB" |
| Home videos | **Medium** | Classify personal; produce a human title from date, place and OCR ("Grandma's 80th birthday — 2014-07-13") |
| Choosing between near-identical candidates | Low–medium | Deterministic runtime/language gates beat LLM judgment; also the ToS-sensitive step |
| Easy scene-named files | **None** | Never call AI for these (cost, latency, privacy) |

**Agentic loop (AI-isolated mode, ToS-aware):**

```text
Input  (user-owned evidence only):
  raw_path, parsed fields + parse confidence, probe{duration_s, audio_langs, sub_langs, container_title,
  creation_time, resolution}, ocr_title_card?, asr_snippet_lang?, sibling filenames in the folder
Tools offered to the model (all deterministic, flux-side):
  propose_queries(hypotheses[])  -> flux runs TMDB /search + verification; returns ONLY a verdict per hypothesis:
                                      {hypothesis_id, verified: bool, reason_code ∈ {runtime_mismatch,
                                       year_mismatch, no_results, ambiguous, ok}}   ← no TMDB text returned
  wikidata_search(text, lang)      -> CC0 candidates {qid, label, aliases, year, duration, tmdb_id?}
  get_folder_context()             -> sibling names, NFOs, embedded tags
  mark_personal(title, date?)      -> terminal action
  mark_unknown(reason)             -> terminal action → review queue
Budget: ≤ 3 tool rounds, ≤ 4k input tokens, ≤ 400 output tokens, wall clock ≤ 20 s cloud / 60 s local
Output (JSON-schema enforced):
  {kind, hypotheses:[{title, original_title?, year?, alt_titles[], edition?, season?, episode?, ordering?}],
   personal?: {display_title, date?}, confidence: 0..1, rationale_short}
Acceptance: flux accepts only if deterministic verification passed (score ≥ 0.85).
            The model's self-reported confidence is used only for review-queue ordering.
```

Two notes on this design. First, it limits what TMDB data reaches the model to reason codes. Whether even that falls under "in connection with" an AI application is for TMDB to answer (§10). Second, LLM self-reported confidence is poorly calibrated, so **acceptance is decided by the deterministic verifier, never by the model** [I].

**Structured output plumbing:** Ollama enforces a JSON Schema via `format` (and `response_format` on its OpenAI-compatible API) and recommends temperature 0 ([docs](https://docs.ollama.com/capabilities/structured-outputs)) [V]. Generate the schema with `schemars` 1.2.3 and validate with `jsonschema` 0.59.0 before use [V crate versions]. OpenAI, Gemini and Anthropic all offer native structured outputs. Use one Rust abstraction (§6.4) and keep a strict validator on our side regardless of the provider.

**Human-in-the-loop review queue** (core to the UX, not an afterthought):
- Each item shows its evidence, the top-3 candidates with *why* (title sim, year Δ, runtime Δ, language), and the AI hypothesis with rationale.
- One-key actions: accept, pick another, search manually, "personal video", "extra", "ignore".
- User decisions are **field locks** with provenance `user`. A refresh never overwrites them (initial-discussion §"Metadata corrections").
- Learn from corrections: store `(parsed → chosen id)` pairs as per-library rewrite rules, e.g. a release group's naming quirks [I].

### 3.6 Local vs cloud for matching: cost and latency per hard item

Assumptions [I]: single-shot ≈ 1,200 input + 150 output tokens; agent loop (3 rounds) ≈ 3,000 input + 300 output tokens. Prices are list prices verified 2026-10-18 [V]: [OpenAI](https://developers.openai.com/api/docs/pricing), [Gemini](https://ai.google.dev/gemini-api/docs/pricing), [Claude](https://claude.com/pricing).

| Backend | $/M in | $/M out | Single-shot $/item | Agent loop $/item | 1,000 hard items |
|---|---|---|---|---|---|
| OpenAI `gpt-5-nano` | 0.05 | 0.40 | $0.00012 | $0.00027 | **$0.27** |
| Google `gemini-2.5-flash-lite` | 0.10 | 0.40 | $0.00018 | $0.00042 | $0.42 |
| Anthropic Claude Haiku 5.5 (≤100K prompt) | 0.10 | 0.50 | $0.00020 | $0.00045 | $0.45 |
| OpenAI `gpt-5-mini` | 0.25 | 2.00 | $0.00060 | $0.00135 | $1.35 |
| Google `gemini-3.1-flash-lite` | 0.25 | 1.50 | $0.00053 | $0.00120 | $1.20 |
| Local 4B (Qwen3.5-4B / Gemma 4 E4B, Apache-2.0) | — | — | electricity | electricity | ~0 |

**Cost is irrelevant at home-library scale.** If 10% of a 10k-title library needs AI, the whole library costs under $0.50. The real constraints are **privacy** (filenames reveal a lot), **ToS**, and **offline operation**. Hence: **local-first default, cloud BYO-key opt-in.**

Local latency estimates [I, derived from llama.cpp's M-series table: LLaMA-7B Q4_0 on M1 Max 32-core = 530 t/s prompt processing, 61 t/s generation ([discussion #4167](https://github.com/ggml-org/llama.cpp/discussions/4167)), scaled ~1.6–1.75× for a 4B model]:

| Hardware | 4B Q4 prompt t/s | gen t/s | Single-shot | Agent loop | Verdict |
|---|---|---|---|---|---|
| M1 Max (this machine) | ~850–930 | ~95–100 | ~3 s | ~6–7 s | Fine as background |
| M4 Max 40-core | ~1,500 | ~130 | ~2 s | ~4 s | Fine |
| Intel N100 (CPU, ~38 GB/s DDR5 single-channel) | ~30–60 | ~6–10 | ~25–55 s | ~1–2 min | Overnight-only, or offload over the tailnet |
| Raspberry Pi 5 | ~15–30 | ~3–6 | ~1 min | ~3 min | Use cloud or a remote worker |
| RTX 3060/4060-class GPU | 2,000+ | 80–120 | <2 s | ~3 s | Fine |

Small model candidates with verified licences on Hugging Face [V]: **Qwen3.5-4B** (Apache-2.0, 2026-02), **Gemma 4 E4B-it** (Apache-2.0, 2026-03), **Qwen3-VL-4B-Instruct** (Apache-2.0, for title-card OCR and VLM work). One guide recommends disabling Gemma 4 "thinking" for JSON output [S] ([insiderllm](https://insiderllm.com/guides/structured-output-local-llms/)). Current tool releases: Ollama v0.40.x, llama.cpp b117xx, mlx-lm v0.32.2 [V GitHub releases].

**Remote inference over Truffle (dogfooding).** A low-power flux server (N100/NAS) can send AI jobs to the user's Mac or GPU box on the same tailnet. Truffle main already exposes `Node::egress_add` (fixed tailnet target as a loopback listener), `Node::egress_http_proxy` and `Node::loopback` (SOCKS5) ([node.rs:635-691](https://github.com/vibecook-dev/truffle/blob/d6f66647a1d33af21a83cb7234cc5ddce3c2e7bb/crates/truffle-core/src/node.rs), [egress.rs:1-15](https://github.com/vibecook-dev/truffle/blob/d6f66647a1d33af21a83cb7234cc5ddce3c2e7bb/crates/truffle-core/src/egress.rs)). The worker side publishes its Ollama/mflux HTTP port with `Node::proxy` ([node.rs:614](https://github.com/vibecook-dev/truffle/blob/d6f66647a1d33af21a83cb7234cc5ddce3c2e7bb/crates/truffle-core/src/node.rs)), and a `SyncedStore` slice advertises capabilities such as `{models:[…], gpu, queue_depth}`. This needs no port forwarding and keeps data inside the user's tailnet. **Not tested this session** (instructions forbid starting Truffle nodes).

---

## 4. Vision and audio AI for media

### 4.1 Smart thumbnail and backdrop selection

The goal is a good-looking, representative, non-spoiler still for every item and every chapter, without a GPU.

**Deterministic pipeline (default, cheap):**
1. Candidate times: scene cuts via ffmpeg `scdet`, or keyframes only (no decode of non-key frames). Restrict to a **spoiler-safe window**: 8–60% of runtime for movies, 10–50% for episodes. Exclude detected intro/credits segments (§4.2) [I heuristic].
2. Reject frames: black or near-black (`blackframe`/`blackdetect` luma thresholds; Intro Skipper defaults `amount=85%`, `threshold=28` [V]), low-contrast/fades (luma std-dev), **blur** (variance of Laplacian below a threshold, scaled to resolution), letterbox-only frames (crop-detect first).
3. Score: exposure (histogram mid-tones), colourfulness, sharpness, **face presence and size** (rule-of-thirds bonus), dedupe by perceptual hash (`image_hasher` 3.1.1). The ffmpeg `thumbnail` filter (most representative frame per batch of N) is a reasonable baseline [I].
4. Optional ML score: Apple Vision `CalculateImageAestheticsScoresRequest` on macOS 15+ (an `overallScore` plus an `isUtility` flag for "not memorable" frames) ([Apple docs](https://developer.apple.com/documentation/vision/calculateimageaestheticsscoresrequest)) [V availability, S score range]. Or a CLIP-embedding aesthetic head on any platform [I].
5. Keep the top-k per item for backdrops and the top-1 per chapter for chapter thumbnails.

Netflix's AVA system tags every frame with visual metadata (brightness, contrast, sharpness, sensitive-content probability), contextual metadata (faces, characters, shot type) and composition. It then ranks frames on aesthetic, creative and diversity objectives, and human artists finalize [S] ([Netflix TechBlog, "AVA"](https://netflixtechblog.com/ava-the-art-and-science-of-image-discovery-at-netflix-a442f163af6), summarized via [Gigazine](https://gigazine.net/gsc_news/en/20180905-netflix-art-science-image-discovery/)). flux's deterministic pipeline is AVA-lite.

**Face detection options:** Apple Vision (on-device, free on macOS/iOS); YuNet via OpenCV (`opencv` crate 0.101.1); MediaPipe BlazeFace (Apache-2.0). **Avoid InsightFace/SCRFD weights in a distributed product**: their model licences are typically non-commercial [I, verify per model]. Faces are used only for *composition scoring*, never identification.

### 4.2 Intro, recap, credits and preview detection

**State of the art in OSS: Intro Skipper (Jellyfin)** [V, source at commit `1bb2af7`, 2026-10-17, GPL-3.0]:

- **Audio fingerprint matching across episodes of a season.** It runs ffmpeg's `chromaprint` muxer with `-fp_format raw` on the analysis window ([FFmpegService.cs:1307-1308](https://github.com/intro-skipper/intro-skipper/blob/1bb2af7cbad551392490544029e38708e3cd6a0e/IntroSkipper/FFmpeg/FFmpegService.cs)). Each fingerprint point is ≈ **0.1238 s** (4096/11025/3) ([ChromaprintConstants.cs](https://github.com/intro-skipper/intro-skipper/blob/1bb2af7cbad551392490544029e38708e3cd6a0e/IntroSkipper/Data/ChromaprintConstants.cs)).
- **Inverted-index alignment** ([ChromaprintAnalyzer.cs:486-560](https://github.com/intro-skipper/intro-skipper/blob/1bb2af7cbad551392490544029e38708e3cd6a0e/IntroSkipper/Analyzers/ChromaprintAnalyzer.cs)):
  - Build `point → position` maps for both episodes.
  - For every point in A, look for exact matches of `point ± InvertedIndexShift (2)` in B to collect candidate shifts.
  - For each shift, walk both arrays and treat two points as similar when `popcount(a ^ b) ≤ MaximumFingerprintPointDifferences (6)`.
  - Allow gaps up to `MaximumTimeSkip (3.5 s)`. The longest run wins.
- **Defaults** ([PluginConfiguration.cs](https://github.com/intro-skipper/intro-skipper/blob/1bb2af7cbad551392490544029e38708e3cd6a0e/IntroSkipper/Configuration/PluginConfiguration.cs)): analyse the first **25%** of an episode capped at **10 min** (lines 29, 34). Intro 15–120 s; credits 15–450 s, movie credits ≤ 900 s. Black frame `85%/28`. Silence `-50 dB` / `0.33 s` (lines 552-557). Boundaries snap to keyframes and chapters. `MaxParallelism = 2` (line 562), and ffmpeg runs at **BelowNormal** priority.
- **Other analysers:** BlackFrame, Chapter (chapter-name regexes), Subtitle (recap/preview phrases), SponsorBlock-style chapters, AnimePreviewDeriver ([Analyzers/](https://github.com/intro-skipper/intro-skipper/tree/1bb2af7cbad551392490544029e38708e3cd6a0e/IntroSkipper/Analyzers)).
- **Capability probing:** it checks that the ffmpeg build has the `chromaprint` muxer and `silencedetect` before running ([FFmpegService.cs:88-90](https://github.com/intro-skipper/intro-skipper/blob/1bb2af7cbad551392490544029e38708e3cd6a0e/IntroSkipper/FFmpeg/FFmpegService.cs)). Many stock ffmpeg builds lack chromaprint, which is why Jellyfin uses jellyfin-ffmpeg.

**Plex** uses an ML model over on-screen text, black frames and undisclosed signals for credits. It runs locally during maintenance and shares markers to Plex's cloud by default [S] ([Plex forum](https://forums.plex.tv/t/credits-detection/831048)).

**flux design:**
- Fingerprint in-process with **`rusty-chromaprint` 0.3.0** (pure Rust port; no ffmpeg build-flag dependency) on audio flux already decodes for probing. Downmix to mono at 11,025 Hz for the first ~10 min and last ~8 min [V crate; I design].
- **Cost:** decoding 18 min of audio is fast (~1–2 s CPU per episode [I]). Pairwise comparison is O(n²) per season, but with inverted indexes and early exit it is milliseconds per pair [I]. Persist fingerprints (`u32` per 0.124 s ≈ 32 KB per 10 min) so new episodes compare against cached siblings, as Intro Skipper does.
- Refine with `blackdetect`/`silencedetect`, chapter names, subtitle phrases and OCR of "Directed by"/"Executive Producer" text for credits.
- Expose **media segments** (intro/recap/credits/preview/commercial) in the API, aligned with Jellyfin 12's Media Segments concept and TheIntroDB's schema for interop.

### 4.3 Chapter generation

For files without chapters (most TV rips):
- **Cheap:** scene-cut density + silence gaps + intro/credits segments → 5–12 chapters. Name them "Intro / Act 1 / …" and give each a thumbnail from §4.1.
- **AI (opt-in):** a local VLM captions one keyframe per candidate chapter and an LLM writes short names. Alternatively, segment the ASR transcript by topic. Run this only in the AI budget lane; it is valuable for lectures, home videos and concerts, and less so for films [I].

### 4.4 OCR of title cards

Run OCR on frames from the first 10 minutes and the last 3 minutes. It is used for obscure films, home video (date stamps, wedding titles) and credits detection. Options: Apple Vision text recognition (on-device, macOS/iOS); Rust `ocrs` 0.13.1 (on `rten` 0.27.0, pure Rust); `oar-ocr` 0.10.0 (PaddleOCR models via ONNX); a VLM such as Qwen3-VL-4B for stylized text [V crate versions; I suitability]. The output is evidence for the matcher (§3.1), never auto-accepted.

### 4.5 Auto-subtitles (ASR) and translation

| Engine | Licence | Languages | Speed notes | Runs on | Confidence |
|---|---|---|---|---|---|
| **NVIDIA Parakeet TDT 0.6B v3** | **CC-BY-4.0** | 25 European, auto language detection, word timestamps | Open ASR leaderboard RTFx ~3,333 vs Whisper-large-v3-turbo ~350 (GPU); ~10× faster than Turbo on Apple Silicon reported | GPU/CPU via ONNX (`parakeet-rs` 0.4.1, `sherpa-rs`, `transcribe-rs`) | [V model card], [S speed] |
| **Whisper large-v3-turbo** | MIT | ~99 | Workhorse multilingual model | whisper.cpp v1.9.5 (`whisper-rs` 0.16.0), WhisperKit v1.1.1 (Core ML, Swift) | [V] |
| **Apple SpeechAnalyzer / SpeechTranscriber** (macOS 26+) | OS API | Apple-supported locales | Argmax: speed factor ~70 vs Whisper base.en 111 / small.en 35, WER 14.0% vs small.en 12.8% (earnings22). MacStories: 34-min video ~55% faster than Whisper large-v3 | macOS/iOS only (Swift bridge) | [S] ([Argmax](https://www.argmaxinc.com/blog/apple-and-argmax)) |

Sources: [Parakeet v3 card](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3), [speed comparison (vendor)](https://whispernotes.app/blog/parakeet-v3-default-mac-model), [whisper-large-v3-turbo card](https://huggingface.co/openai/whisper-large-v3-turbo), GitHub releases for [whisper.cpp](https://github.com/ggml-org/whisper.cpp/releases) and [WhisperKit](https://github.com/argmaxinc/WhisperKit/releases).

**Translation:** MADLAD-400 (Apache-2.0) [V], Opus-MT/Marian (CC-BY-4.0) [I], an LLM (Gemma 4 / Qwen3.5, Apache-2.0) [V], or Apple's Translation framework on macOS. **NLLB-200 is CC-BY-NC-4.0** [V], so it is not suitable as a default in a product that might go commercial.

**Policy:** ASR is opt-in per library (it is expensive: ~0.1–0.5× realtime on CPU [I]). Generated subtitles are tagged `generated:asr:<model>` and never uploaded anywhere.

### 4.6 Semantic search with embeddings

| Use | Model (licence) | Dim | Rust path | Notes |
|---|---|---|---|---|
| Plot and overview text search ("heist movie set in Lisbon") | **EmbeddingGemma-300M** (Gemma licence), **Qwen3-Embedding-0.6B** (Apache-2.0), multilingual-e5-small | 768 / 1024 / 384 | `fastembed` 7.1.1 (ONNX / candle) | **Embedding TMDB overviews may itself be "in connection with ML"** under TMDB §1.C. Until TMDB clarifies, embed only non-TMDB text: Wikidata/TVmaze descriptions, ASR transcripts, the user's own notes [I] |
| Keyframe visual search ("beach at sunset", "red car") | CLIP ViT-B/32 (MIT), nomic-embed-vision-v1.5 (Apache-2.0), SigLIP 2 base (Apache-2.0) | 512 / 768 / 768 | `fastembed` (CLIP, nomic), `ort` 2.0.0-rc.14, `candle` 0.11.0 | **MobileCLIP2 is `apple-amlr` licensed** (research), so avoid it as a default [V licence] |
| Multimodal unified | Qwen3-VL-Embedding-2B (Apache-2.0) | — | `fastembed` (`qwen3` feature, candle) | Heavier; Apple Silicon / GPU tier |

Model licences verified via Hugging Face API tags [V]; fastembed-rs supported-model list from its [README](https://github.com/Anush008/fastembed-rs) [V].

**Feasibility and storage [I, with one data point]:**
- CLIP ViT-B/32 int8 on a **Raspberry Pi 5 ≈ 95 ms/image** (ExecuTorch/XNNPACK; ~298 ms baseline) per [Arm's model portal](https://developer.arm.com/ai/models/hugging-face/Arm/clip-vit-base-patch32-int8-xnnpack-executorch/clip-vit-b32-int8-pte) [S].
- One keyframe per 10 s means a 2 h film has 720 frames. That is ~70 s of embedding on a Pi 5 and seconds on Apple Silicon/GPU. Storage is 720 × 512 × 2 B (f16) ≈ **0.7 MB per film**; a 2,000-film library is ~1.5 GB.
- Use `sqlite-vec` 0.1.9 (keeps everything in the one SQLite file) or `usearch` 2.26.5 for HNSW at scale [V versions].
- Decode cost dominates, so piggyback on thumbnail extraction and trickplay generation rather than decoding twice [I].

### 4.7 Hardware feasibility matrix

| Workload | N100 / RPi5 | Apple Silicon (M1 Max) | NVIDIA GPU box |
|---|---|---|---|
| Parse + offline prefilter + TMDB match | ✅ (ms) | ✅ | ✅ |
| Thumbnail selection (deterministic) | ✅ (decode-bound; use QSV on N100) | ✅ (VideoToolbox) | ✅ (NVDEC) |
| Chromaprint intros/credits | ✅ (seconds/episode) | ✅ | ✅ |
| Local LLM fallback (4B) | ⚠️ overnight only / remote worker | ✅ 3–7 s/item | ✅ |
| ASR (Parakeet/Whisper-turbo) | ⚠️ slow (≥0.3× realtime, I) | ✅ fast | ✅ very fast |
| Keyframe embeddings | ⚠️ ~0.1 s/frame (Pi 5 int8) | ✅ | ✅ |
| Local genAI posters | ❌ | ✅ 10 s–2 min/image (model-dependent) | ✅ |
| VLM captions/chapters | ❌ (remote) | ✅ | ✅ |

---

## 5. GenAI posters and thumbnails for hard-to-find titles

### 5.1 Scope and defaults

GenAI applies only to items with **no provider artwork**: home videos, obscure or unindexed films, fan edits, concert recordings, YouTube rips, personal projects. The **default for these is a deterministic "frame poster"** (§5.6), with no model or cost. GenAI is an **opt-in** per library or item, with a visible "AI-generated" badge.

### 5.2 Model landscape (verified 2026-10-18)

**Cloud APIs:**

| Model (ID) | Per-image price | Licence/terms of output | Text rendering | Source |
|---|---|---|---|---|
| OpenAI `gpt-image-2` (released 2026-04-21 [S]) | 1024×1536 portrait: **low $0.005, medium $0.041, high $0.165**; 1024² medium $0.053 | OpenAI terms; outputs carry C2PA + SynthID [S] | Much improved; OpenAI says it "can still struggle with precise text placement" | [pricing](https://developers.openai.com/api/docs/pricing), [image guide](https://developers.openai.com/api/docs/guides/image-generation) [V] |
| OpenAI `gpt-image-2.5-sunburst` / `-flare` | Token-priced like gpt-image-2 ($8/M image in, $30/M image out); no per-image table | same | n/a | [pricing](https://developers.openai.com/api/docs/pricing) [V] |
| OpenAI `gpt-image-1-mini` | portrait low $0.006 / **medium $0.015** / high $0.052 | same | Weaker | [V] |
| Google **Nano Banana 2.1** `gemini-nano-banana-2.1` | **$0.0336 (1K)**, $0.0504 (2K); batch $0.0168 | Google terms; SynthID | Good | [Gemini pricing](https://ai.google.dev/gemini-api/docs/pricing) [V] |
| Google Nano Banana 2 `gemini-3.1-flash-image` | $0.067 (1K), $0.101 (2K) | same | Good | [V] |
| Google Nano Banana Pro `gemini-3-pro-image` | $0.134 (1K/2K), $0.24 (4K) | same | Best-in-class [S] | [V] |
| (`gemini-2.5-flash-image` shut down 2026-10-02) | — | — | — | [V] |
| BFL **FLUX 3 Image Klein** (API) | **$0.008 (768²), $0.009 (1K)**, $0.014 (2K) | BFL API terms | "Renders text inside images" | [BFL docs pricing](https://docs.bfl.ai/quick_start/pricing) [V] |
| BFL FLUX.2 [pro]/[flex]/[max] | Per-MP; values not readable this session (third-party snapshot: pro ~$0.03, flex ~$0.05, max ~$0.07) | BFL API terms | Strong | [S] ([inferencehub](https://inferencehub.org/providers/black-forest-labs)) |

**Local (Apple Silicon via mflux 0.23.0, released 2026-10-16; NVIDIA via ComfyUI/diffusers):**

| Model | Params / steps | Weights licence | Notes | Source |
|---|---|---|---|---|
| **FLUX.2 [klein] 4B** | 4B, **4 steps** distilled | **Apache-2.0** | ~15 GB bf16 download (quantize with `-q 4/8`); edit + multi-reference | [HF](https://huggingface.co/black-forest-labs/FLUX.2-klein-4B), [mflux flux2 README](https://github.com/filipstrand/mflux/blob/main/src/mflux/models/flux2/README.md) [V] |
| FLUX.2 [klein] 9B | 9B, 4 steps | FLUX non-commercial ("other") | ~32 GB | [V] |
| **FLUX 3 Image Klein** (2026-10) | 5.6B + Qwen3-VL-4B TE, 16 steps | **FLUX Kommunity Licence v1.0**: non-commercial, plus commercial *output* for "Qualifying Users" (< US$5M revenue) | Strong text, bounding-box layouts, up to 10 reference images | [mflux flux3 README](https://github.com/filipstrand/mflux/blob/main/src/mflux/models/flux3/README.md) [V] |
| **Z-Image-Turbo** (Tongyi/Alibaba) | 6B, **9 steps** | **Apache-2.0** | "Fast, small, very good quality and realism" | [HF](https://huggingface.co/Tongyi-MAI/Z-Image-Turbo) [V] |
| **Qwen-Image** (2025-08) | 20B, ~20 steps | **Apache-2.0** | Strong prompt understanding and text; slow | [HF](https://huggingface.co/Qwen/Qwen-Image) [V] |
| Qwen-Image-2.1 / -Turbo (2026-09/10) | 7.1B + 8B TE; 40 / 8 steps | **Qwen Research Licence** (non-commercial) | M5 Max ~1.5 s/step, ~78 s at 40 steps, peak ~46 GB | [mflux qwen21 README](https://github.com/filipstrand/mflux/blob/main/src/mflux/models/qwen21/README.md) [V] |
| Stable Diffusion 3.5 Large | 8B | Stability Community (other) | Superseded for this use | [V licence tag] |

[I] Local generation times on M1 Max were not measured this session (download budget). Expect FLUX.2-klein-4B (4 steps, q8) and Z-Image-Turbo (9 steps) to land at **~10–40 s per 1024×1536 image** on M1 Max, and Qwen-Image 20B at several minutes. Prototype B measures this.

**Recommended defaults:**
- **Local:** FLUX.2 [klein] 4B for txt2img and img2img/edit from a frame; Z-Image-Turbo as the alternative. Both are Apache-2.0, so redistribution and commercial use are safe.
- **Cloud (BYO key):** FLUX 3 Image Klein API at about $0.009 for best price/performance. Nano Banana 2.1 ($0.034) or gpt-image-2 medium ($0.041) when the user prefers those vendors.

### 5.3 Typography: never trust the model with the title

Even the best 2026 models still mis-render text occasionally (OpenAI's own docs say so [V]). Posters are viewed at thumbnail size, where one wrong glyph looks broken. So:
1. Prompt for a **text-free** composition with negative space reserved for the title, e.g. "lower third empty, no text, no letters, no logos".
2. Run OCR on the output (`ocrs`). If any text is detected, regenerate or inpaint it away. This is cheap insurance [I].
3. Render the title, year and optional tagline **deterministically**: `cosmic-text` 0.19.0 for shaping (CJK, RTL, emoji), `tiny-skia` 0.12.0 / `resvg` 0.48.1 for rasterization, and a set of ~6 bundled OFL fonts with genre-mapped templates (thriller condensed sans, period serif, and so on). Text fitting uses binary search on font size with max 2 lines and a contrast-aware scrim [V crate versions; I design].
4. Fallback: if the user explicitly wants model-rendered typography (Nano Banana Pro / FLUX 3 Klein / gpt-image-2 are good at it), still run OCR verification against the expected title and reject mismatches.

This mirrors what the community already does: **Posterizarr** fetches *textless* artwork from fanart.tv/TMDB/TVDB and overlays its own text with ImageMagick ([README](https://github.com/fscorrupt/Posterizarr)), and Kometa does overlays. (Both v3.3.7 / v2.5.3 as of 2026-10 [V].)

### 5.4 Pipeline

```text
[unmatched item, user opted in]
 1. Frame harvest    → reuse §4.1 candidates (spoiler-safe, sharp, well-exposed), top 8 + face boxes
 2. Consent gate     → if faces present & library.kind=personal: default style = "non-identifying"
                       (silhouettes/landscape/objects; no img2img on faces) unless user opts in per item
 3. Describe (VLM)   → local Qwen3-VL-4B / cloud Flash-Lite: JSON {setting, era, mood, palette[], genre_guess,
                       key_objects[], people_count, time_of_day}; frames are the user's own → no TMDB exposure
 4. Prompt builder   → deterministic template: style preset × genre × palette × composition (2:3, title space),
                       hard negatives ("no text, no logos, no watermark, no real celebrity, not a movie poster of X")
 5. Generate         → txt2img, or img2img/edit from the best frame (strength 0.35–0.6) for "stylized still";
                       N=2–4 variants, seed recorded
 6. Validate         → OCR (no text), NSFW classifier, face-similarity check vs source when consent=no
                       (reject if a recognizable face is reproduced), aesthetic score; pick best
 7. Typeset          → §5.3 deterministic title/year
 8. Provenance       → C2PA manifest (c2pa crate) with actions c2pa.created + digitalSourceType
                       trainedAlgorithmicMedia (or compositeWithTrainedAlgorithmicMedia for img2img),
                       model id/version, prompt hash; IPTC XMP mirror; DB row artifact{kind=poster, ai=true}
 9. Review           → shown as a suggestion with "AI-generated" badge; user accepts/regenerates/reverts
```

### 5.5 Cost per poster

| Path | Generation | VLM description (≈4 frames, ~5k tokens) | Total per accepted poster (assume 3 variants) |
|---|---|---|---|
| Local (FLUX.2 klein 4B / Z-Image-Turbo) | $0 (≈1–2 min of M-series GPU for 3 variants [I]) | $0 (local VLM) | **$0** + electricity |
| FLUX 3 Image Klein API (1K) | 3 × $0.009 = $0.027 | Flash-Lite ≈ $0.0005 | **≈ $0.03** |
| Nano Banana 2.1 (1K) | 3 × $0.0336 = $0.10 | ≈ $0.0005 | **≈ $0.10** |
| gpt-image-2 medium portrait | 3 × $0.041 = $0.123 | ≈ $0.0005 | **≈ $0.12** |
| gpt-image-1-mini medium portrait | 3 × $0.015 = $0.045 | ≈ $0.0005 | **≈ $0.05** |

Prices [V] as in §5.2; token estimate [I]. A user with 200 unmatched items pays about $6 with FLUX 3 Klein API, or nothing locally.

### 5.6 The zero-AI baseline: "frame poster"

Take the best frame (§4.1), crop it to 2:3 around the salient region (faces or high-gradient), apply a gentle grade, blur or vignette the title zone, and typeset the title (§5.3). It is deterministic, instant, free and legally clean, and it already beats a grey placeholder by a wide margin. For personal video, combine 3–4 frames into a collage with date and location (from QuickTime metadata). **Ship this first.** GenAI is an enhancement layer on top [I].

### 5.7 Ethics and legal guardrails

1. **Opt-in only**, per library, with a global kill switch. Cloud generation requires an explicit BYO key and a disclosure that frames are uploaded to the vendor.
2. **Label everything:**
   - UI badge "AI-generated".
   - Embedded **C2PA** manifest via the `c2pa` crate 0.91.3 [V].
   - IPTC `DigitalSourceType` `trainedAlgorithmicMedia` / `compositeWithTrainedAlgorithmicMedia`.
   - DB provenance.
   OpenAI says ChatGPT/API images include C2PA metadata and SynthID watermarks [S] ([OpenAI help](https://help.openai.com/en/articles/8912793-c2pa-in-chatgpt-images)). Google uses SynthID and C2PA in a growing set of tools [S] ([Google blog](https://blog.google/innovation-and-ai/products/making-it-easier-to-understand-how-content-was-created-and-edited/)). flux should **preserve** vendor manifests and add its own ingredient assertions.
3. **EU AI Act Article 50** applies from **2 Aug 2026**. The Digital Omnibus (Reg. (EU) 2026/1744, in force 27 Jul 2026) delayed high-risk duties but **not** Art. 50. Generative systems already on the market have until **2 Dec 2026** for machine-readable marking [S] ([technology.org, 2026-10-13](https://www.technology.org/2026/10/13/eu-ai-act-deadlines-after-the-digital-omnibus-what-applies-from-december-2026-to-august-2028/), [CSA note](https://labs.cloudsecurityalliance.org/research/csa-research-note-eu-ai-act-article50-watermarking-deadline/), [Legalithm](https://www.legalithm.com/en/blog/eu-ai-act-what-applies-2-august-2026)). Whether a self-hosted OSS tool is a "provider" or "deployer" is unsettled. Marking by default makes the question mostly moot [I].
4. **No imitation:**
   - Prompts never include real film titles, studio names, actor names or "in the style of [poster/artist]".
   - Use a deny-list plus an LLM-free template builder.
   - Never feed TMDB/fanart posters as reference images (copyright and TMDB §1.C).
5. **Likeness:**
   - Home videos contain family members.
   - Default to non-identifying compositions. Use img2img on faces only with per-item consent.
   - Run a face-similarity check to *avoid* reproducing recognizable faces when consent is absent.
   - Never generate likenesses of real public figures.
6. **Model licences:** default to Apache-2.0 weights (FLUX.2-klein-4B, Z-Image-Turbo, Qwen-Image). Gate non-commercial weights (FLUX.2-klein-9B, Qwen-Image-2.1, FLUX 3 Image Klein's Kommunity licence) behind a "personal use" acknowledgement, and never bundle them.
7. **Don't publish:** generated art stays local and is excluded from any community sharing (e.g. TheIntroDB-style write-back) unless explicitly shared.

---

## 6. Architecture in Rust

### 6.1 Core traits (sketch)

```rust
/// Identity & descriptive metadata.
#[async_trait::async_trait]
pub trait MetadataProvider: Send + Sync {
    fn id(&self) -> ProviderId;                       // "tmdb", "tvdb", "tvmaze", "wikidata", "anilist", "http:<url>"
    fn capabilities(&self) -> Caps;                   // SEARCH | DETAILS | EPISODES | ORDERINGS | IMAGES | CHANGES | ID_LOOKUP
    fn terms(&self) -> TermsProfile;                  // max_cache_age, ai_allowed: AiUse::{Forbidden, Allowed}, attribution
    async fn search(&self, q: &SearchQuery, cx: &Ctx) -> Result<Vec<Candidate>>;
    async fn details(&self, r: &ProviderRef, langs: &[Lang], cx: &Ctx) -> Result<ProviderRecord>;
    async fn episodes(&self, series: &ProviderRef, ordering: Ordering, cx: &Ctx) -> Result<EpisodeList>;
    async fn changes_since(&self, since: Timestamp, cx: &Ctx) -> Result<Vec<ProviderRef>> { Ok(vec![]) }
}

#[async_trait::async_trait]
pub trait ArtworkProvider: Send + Sync {
    async fn artwork(&self, r: &ProviderRef, kinds: ArtKinds, cx: &Ctx) -> Result<Vec<ArtworkRef>>;
}

/// Deterministic matcher: evidence in, scored decision out. Pure; unit-testable on a corpus.
pub trait Matcher { fn score(&self, ev: &Evidence, c: &Candidate) -> MatchScore; }

/// LLM/VLM backends (local or cloud) behind one interface; schema-constrained.
#[async_trait::async_trait]
pub trait LlmBackend: Send + Sync {
    fn descriptor(&self) -> ModelDescriptor;          // id, version, locality: Local|Remote|Cloud, cost table
    async fn structured<T: JsonSchema + DeserializeOwned>(&self, req: LlmRequest, budget: &Budget) -> Result<T>;
}

#[async_trait::async_trait]
pub trait ImageGenBackend: Send + Sync {
    fn descriptor(&self) -> ModelDescriptor;          // includes weights licence → policy gate
    async fn generate(&self, req: ImageGenRequest, budget: &Budget) -> Result<Vec<GeneratedImage>>;
}

/// Media analysers (thumbnails, chromaprint, OCR, ASR, embeddings) – CPU/GPU bound jobs.
pub trait Analyzer { fn kind(&self) -> AnalyzerKind; fn plan(&self, item: &MediaItem) -> Vec<JobSpec>; }
```

The `TermsProfile.ai_allowed` flag lets the AI job layer **refuse at compile- and run-time** to pass content from an AI-forbidden provider into an `LlmBackend` or `ImageGenBackend`. This makes the TMDB policy enforceable in code, not just documented [I].

**HTTP-pluggable providers:** an `HttpMetadataProvider` implements the same trait against a documented flux provider protocol (JSON over HTTP; endpoints `/match`, `/item/{id}`, `/episodes`). It is inspired by Plex's 2025 Custom Metadata Providers ([announcement](https://forums.plex.tv/t/announcement-custom-metadata-providers/934384)).

### 6.2 Data model additions (to the Work/Edition/Asset/Track/Location/UserState split)

| Table | Key columns | Purpose |
|---|---|---|
| `provider_ref` | work_id, provider, external_id, kind, confidence, source(`match`/`user`/`nfo`/`hash`) | All external IDs for a work |
| `provider_record` | provider, external_id, lang, fetched_at, payload_zstd, etag | ToS-bounded cache |
| `field_value` | work_id, field, value, source_provider, source_ref, locked_by_user, updated_at | **Per-field provenance + locks** |
| `match_decision` | asset_id, evidence_hash, decision, score, margin, method(`rule`/`ai`/`user`), model_id?, prompt_version?, created_at | Audit trail; re-runs only if evidence or model changes |
| `review_item` | asset_id, reason, candidates_json, ai_hypothesis_json, state | HITL queue |
| `content_fp` | asset_id, size, oshash, (optional) blake3_partial | Identity across moves; external hash lookups |
| `segment` | asset_id, kind(intro/recap/credits/preview/commercial), start_ms, end_ms, method, confidence | Skip buttons |
| `derived_artifact` | asset_id/work_id, kind(thumb/backdrop/poster/chapter_thumb/embedding/subtitle), ai_generated, c2pa_manifest_id, model_id, path | Generated assets |
| `ai_job` | id, kind, subject, budget_class, state, cost_estimate, cost_actual, tokens, started/finished | Budget accounting |

### 6.3 Job system: AI never blocks scan or playback

- **Priority lanes** (strict precedence; preemptible by playback):
  1. playback/transcode
  2. scan/probe
  3. metadata (network)
  4. artwork fetch + deterministic thumbnails
  5. analysis (chromaprint, OCR)
  6. **AI lane** (LLM, VLM, ASR, embeddings, genAI)
- **Budgets per lane:** max concurrency, CPU nice/QoS (macOS `QOS_CLASS_UTILITY`/`BACKGROUND`), GPU slot semaphore (one heavy model resident at a time), **$/day and tokens/day caps** for cloud, time-of-day windows ("AI only 01:00–07:00 on N100"), and thermal/battery guards (mflux has `--battery-percentage-stop-limit` [V]).
- **Backpressure:** if a playback session starts, the AI lane pauses new jobs and the GPU semaphore is released after the current step. Intro Skipper similarly runs ffmpeg at BelowNormal priority with parallelism 2 [V].
- **Idempotency:** job key = `(subject content_fp, job kind, model_id, model_version, prompt_version)`. A model upgrade invalidates only that kind of artifact.
- **Crates:** `apalis` 1.0.0 (SQLite-backed jobs) or a bespoke Tokio + SQLite queue (the scan engine probably wants its own) [V version]. `tokio::sync::Semaphore` for slots, `governor` for provider rate limits, `moka` 0.12.16 for in-memory caches [V].

### 6.4 Rust crates (versions verified on crates.io, 2026-10-18)

| Purpose | Crate | Version | Notes |
|---|---|---|---|
| Filename parsing | `hunch` | 2.0.2 | guessit port; 49 properties; ~229 µs/name measured |
| Fuzzy matching | `strsim` 0.11.1; `rapidfuzz` 0.5.0; `nucleo-matcher` 0.3.1 | — | JW/Levenshtein; rapidfuzz is faster for bulk |
| Offline title index | `fst` (FST map) or SQLite FTS5; `tantivy` 0.27.0 for richer search | — | Replace the 381 MB naïve map |
| Transliteration | `deunicode` 1.x | — | Diacritics folding (CJK romanization is crude, so use `/search`) |
| TMDB client | `tmdb-api` 0.9.1 / `tmdb-async` 0.7.2 | — | Low adoption; **write a thin typed client** over `reqwest` |
| HTTP | `reqwest` + `reqwest-middleware` 0.5.2 + `http-cache-reqwest` 0.16.0 + `backon` 1.6.0 + `governor` 0.10.4 | — | Caching, retry, rate limit |
| Multi-provider LLM | **`genai` 0.6.5** (OpenAI, Gemini, Anthropic, Ollama, …); `rig-core` 0.44.0 (agents, tools); `async-openai` 0.42.2 (OpenAI-compatible incl. Ollama/llama-server) | — | Use `genai` or `async-openai` for structured calls; `rig` if the agent tooling earns its weight |
| Ollama native | `ollama-rs` 0.3.6 | — | Or just OpenAI-compatible |
| Local inference in-process | `llama-cpp-2` 0.1.160; `mistralrs` 0.8.1; `candle-core/-transformers` 0.11.0; `ort` 2.0.0-rc.14 (ONNX Runtime 1.31); `mlx-rs` 0.32.0 | — | Prefer **out-of-process** (Ollama/llama-server/mflux) for crash isolation; in-process `ort` for small vision/embedding models |
| Embeddings | `fastembed` 7.1.1 | — | CLIP, nomic-vision, EmbeddingGemma, Qwen3-Embedding |
| Vector store | `sqlite-vec` 0.1.9; `usearch` 2.26.5; `lancedb` 0.40.0 | — | sqlite-vec first |
| ASR | `whisper-rs` 0.16.0; `parakeet-rs` 0.4.1; `sherpa-rs` 0.6.8; `transcribe-rs` 0.3.12 | — | Parakeet for EU languages, Whisper for the rest |
| OCR | `ocrs` 0.13.1 (+`rten` 0.27.0); `oar-ocr` 0.10.0 | — | Apple Vision on macOS via Swift bridge |
| Audio fingerprint | `rusty-chromaprint` 0.3.0 | — | Pure Rust; last release 2024-10 (stable algorithm) |
| Image ops | `image` 0.25.10; `fast_image_resize` 6.1.0; `imageproc` 0.27.0; `image_hasher` 3.1.1; `dssim-core` 3.5.1 | — | Blur (Laplacian), pHash dedupe |
| Typography/raster | `cosmic-text` 0.19.0; `tiny-skia` 0.12.0; `resvg` 0.48.1; `ab_glyph` 0.2.32 | — | Deterministic title rendering |
| Provenance | `c2pa` 0.91.3 | — | Sign/verify manifests |
| Hashing | `blake3` 1.8.7; `xxhash-rust` 0.8.19 | — | Content fingerprints |
| Schema | `schemars` 1.2.3; `jsonschema` 0.59.0 | — | Structured output contracts |
| ffmpeg | `ffmpeg-sidecar` 2.7.0; `ffmpeg-next` 9.0.0 | — | Note: Homebrew ffmpeg currently broken on the dev box |

### 6.5 Mille and Truffle touchpoints

- **Mille** emits `WalkedEntry{path, size, mtime_ms, ctime_ms, …}` with **no content hash** ([walker.rs:78-96](https://github.com/vibecook-dev/mille/blob/c8160d585231e5c6877fefbeb1182d86aca45042/crates/mille-core/src/walker.rs)) [V]. flux's metadata stage should compute `oshash` (+ optional BLAKE3 of head and tail) for new or changed video files and key `match_decision` on it, so renames and moves keep their match without re-querying. Mille's `UnreadDirectory` semantics ("must neither mark its listing complete nor treat the children it already knows as deleted", [walker.rs:98-108](https://github.com/vibecook-dev/mille/blob/c8160d585231e5c6877fefbeb1182d86aca45042/crates/mille-core/src/walker.rs)) map directly onto flux's "unavailable ≠ deleted" rule. Metadata must never be dropped because a NAS was offline.
- **Truffle:**
  - (a) Remote AI workers via egress and proxy (§3.6).
  - (b) `SyncedStore` to share derived artifacts and capability adverts across the user's own nodes (§2.11).
  - (c) `file_transfer` to move generated posters and fingerprints between nodes [V module exists; I usage].
  These exercise Truffle in exactly the "push it to the limit" way the maintainer wants.

---

## 7. Concrete metadata pipeline design

```text
 Mille walker/watcher ──ChangeSet──▶ [S0 Stabilize] file size/mtime stable N s
                                         │
                                         ▼
 [S1 Identify-local]  (CPU, no network, ~1 ms/file)
   hunch parse · folder rules · NFO/{tmdb-…} tags · camera-pattern rule · oshash (2×64KiB)
   → Evidence{…}, kind guess, parse_conf
                                         │
 [S2 Probe] (shared with playback planner) ffprobe/native demux: duration, streams, langs, tags, creation_time
                                         │
                                         ▼
 [S3 Match-offline]  TMDB export prefilter (FST) + Wikidata/IMDb(opt-in) local indexes + prior decisions by oshash
   → if explicit ID / prior decision: ACCEPT
                                         │
 [S4 Match-online]  (network lane, governor-limited)
   TMDB /search (+year, +language) · /find by IMDb/TVDB id · OpenSubtitles moviehash (unmatched only)
   deterministic score (title, year±1, runtime, language, popularity) → ACCEPT ≥0.85 & margin ≥0.10
                                         │ else
                                         ▼
 [S5 AI-hypothesis]  (AI lane, budgeted; local default, cloud BYO)
   LLM sees user-owned evidence only → hypotheses[] → back to S4 verification (max 3 rounds)
   → ACCEPT (verified) | PERSONAL | REVIEW
                                         │
                                         ▼
 [S6 Enrich]  details+append_to_response (credits, images, external_ids, release_dates, episode groups)
   → field_value with provenance; respect user locks; schedule 180-day refresh; daily /changes poll
                                         │
                                         ▼
 [S7 Artwork]  TMDB/fanart images → local cache → resized variants;   no artwork → frame poster
 [S8 Analysis] thumbnails/trickplay/keyframe embeddings (shared decode) · chromaprint segments · OCR
 [S9 Optional AI] ASR subtitles · VLM chapters · genAI poster (opt-in) · semantic index
```

**Service levels [I, targets to validate]:**
- S1–S3 at ≥ 5,000 files/s per core cluster on Apple Silicon. The library must be browsable with local titles before any network call.
- S4 at the provider's rate: TMDB at 20 rps with `append_to_response` is ~1 request per title, so a 10k-movie library is ~10 min cold.
- S5 never runs on a file that S4 matched.
- S7 for the first screen of posters within 2 s of a match, via prioritized fetch of visible items.

**State machine per asset:** `discovered → identified_local → probed → matched{auto|ai|user} | personal | review | unmatched_offline → enriched → artwork_ready`. Plus `unavailable` (location missing), which never regresses metadata.

---

## 8. Prototype plans

### Prototype A: TMDB matching with AI fallback

**Goal:** prove a deterministic matcher with runtime/language evidence beats a Jellyfin-equivalent scorer, and that an AI-isolated hypothesis loop recovers most of the remainder, all without sending TMDB content to the model.

1. **Corpus (no media needed):**
   - 2,000 synthetic filenames generated from TMDB popular and obscure lists in many naming styles: scene, Plex-style folders, foreign-language, year-in-title, sequels with "Part", editions, anime absolute, camera files.
   - Plus the maintainer's real library paths (names only) exported from Mille. Ground truth from known IDs.
   - **Durations:** use TMDB runtime ± noise for synthetic items, and real ffprobe durations once the lead fixes ffmpeg.
2. **Baselines:** (i) Jellyfin `FindBestMatch` re-implemented (title exact 8 / prefix 4 / year±1 2); (ii) flux deterministic scorer (§3.2).
3. **AI fallback:** `genai` 0.6.5 with two backends: local Ollama (Qwen3.5-4B, then Gemma 4 E4B) and BYO cloud (gpt-5-nano or Flash-Lite). JSON Schema via `schemars`. Tools: `propose_queries` (returns verdict codes only), `wikidata_search`, `mark_personal`.
4. **Offline layer:** replace the prototype's naïve map with an `fst` map plus a token→postings index; target < 60 MB RSS and < 3 s build.
5. **Metrics:**
   - top-1 accuracy;
   - **precision of auto-accept (must be ≥ 99.5%)**;
   - auto-accept coverage;
   - review-queue rate;
   - AI calls per 1,000 files;
   - p50/p95 latency per lane;
   - $ per 1,000 files;
   - RSS of the offline index.
6. **Success criteria:**
   - Deterministic scorer ≥ +10 pp top-1 over the Jellyfin baseline on the hard subset.
   - Combined pipeline: auto-accept precision ≥ 99.5% with coverage ≥ 97% on scene-named items and ≥ 85% on the hard subset.
   - AI invoked on ≤ 10% of files.
   - Local AI p95 ≤ 10 s/item on M1 Max.
   - Cloud cost ≤ $0.50 per 1,000 files.
7. **Starting point:** `$FLUX_WORK/proto/metadata-match` (offline layer + hunch already working; see §3.3).

### Prototype B: genAI poster for an unmatched title

**Goal:** given an unmatched video (home video or obscure film), produce a labelled, C2PA-signed 2:3 poster with a correct title in under 2 min locally on M1 Max.

1. **Inputs:** 5 items, including 2 home videos, 1 obscure film, 1 fan edit and 1 concert recording, with frames extracted (ffmpeg via the lead's fixed build, or VideoToolbox through AVFoundation).
2. **Frame harvest:** §4.1 deterministic scorer in Rust (`image`, `imageproc`, Laplacian variance, luma histogram, `image_hasher` dedupe). Faces via Apple Vision (Swift CLI shim) for the prototype.
3. **Describe:** Qwen3-VL-4B via Ollama or mlx-vlm → JSON schema `{setting, era, mood, palette, genre_guess, objects, people_count}`.
4. **Generate:**
   - **Local:** mflux 0.23.0 with `flux2-klein-4b` (`-q 8`), txt2img and edit-from-frame. Compare with Z-Image-Turbo.
   - **Cloud (optional, BYO):** FLUX 3 Image Klein API and Nano Banana 2.1, for a quality/cost comparison.
   - Install mflux with `uv` into a venv on the SSD and point `HF_HOME` at the SSD. The weights are ~8–16 GB quantized, which exceeds this session's 1 GB budget, so this is the lead's call.
5. **Validate and typeset:** `ocrs` to assert no text; `cosmic-text` + `tiny-skia` to render title and year with 3 templates; contrast scrim.
6. **Provenance:** `c2pa` 0.91.3 sign with a self-signed dev cert. Assertions: `c2pa.actions` (created, `digitalSourceType=trainedAlgorithmicMedia`), model and version, prompt hash. Verify with `c2patool`.
7. **Metrics and success criteria:**
   - Wall time per accepted poster: target ≤ 120 s local with 3 variants.
   - OCR text-free rate of raw generations: ≥ 90%.
   - Title rendering correctness: 100% (deterministic).
   - Blinded preference vs the frame-poster baseline on 5 items with 3 raters: genAI preferred in ≥ 60%, or genAI is not worth defaulting.
   - Zero recognizable-face reproductions when consent is off (face-embedding similarity < threshold).
   - C2PA manifest validates.
   - Cost per poster ≤ $0.03 on FLUX 3 Klein API.

---

## 9. Recommendations for flux

| # | Recommendation | Priority | Why |
|---|---|---|---|
| 1 | **Email TMDB** (API for Business) about (a) shipping a flux-identified project key in OSS, (b) the scope of the §1.C AI clause for a local LLM that never sees TMDB content, and (c) embedding overviews for semantic search. Until answered, enforce `TermsProfile.ai_allowed=false` for TMDB in code | **P0** | The AI features' legality hinges on this |
| 2 | Build the **deterministic matcher first** (hunch parse + export prefilter + `/search` + runtime/language scoring + episode orderings), with a labelled corpus and a Jellyfin baseline. Ship with **auto-accept precision ≥ 99.5%** and a first-class review queue | **P0** | Most of the accuracy at none of the risk; directly measurable win over Jellyfin |
| 3 | Compute **oshash per video** at scan, use it as identity glue with Mille's path/mtime, and use OpenSubtitles hash lookups for unmatched items only | **P0** | Cheap (2×64 KiB), survives renames, definitive when it hits |
| 4 | **Offline-first cache** with per-provider `TermsProfile` (TMDB 180-day cap, daily `/changes` poll, immutable TVmaze images, negative-result backoff). Scans must succeed with zero network | **P0** | Speed, resilience, ToS compliance |
| 5 | **Per-field provenance and user locks** in the schema from day one | **P0** | Hard to retrofit; required for HITL and refresh safety |
| 6 | **AI fallback as hypothesis generator** (local default via Ollama/llama-server, BYO cloud via `genai`). The LLM never adjudicates; the deterministic verifier does. Budgets per lane, AI lane preemptible by playback | **P1** | Fixes the long tail (parser errors, foreign titles, fan edits, personal video) cheaply |
| 7 | **Wikidata (CC0) as AI-safe candidate source and crosswalk**; TVmaze (CC BY-SA) as secondary TV source; TVDB as "bring your PIN"; IMDb datasets opt-in for personal use | **P1** | Reduces single-provider and ToS risk |
| 8 | **Intro/credits detection in pure Rust** (`rusty-chromaprint` + Intro Skipper-style inverted index, blackdetect/silencedetect refinement), segments API compatible with Jellyfin Media Segments and TheIntroDB | **P1** | High user value, CPU-cheap, works on N100 |
| 9 | **Deterministic thumbnail and frame-poster engine** (spoiler window, blur/exposure rejection, faces, pHash dedupe, deterministic typography) | **P1** | Every item looks good with zero AI and zero cost |
| 10 | **GenAI posters opt-in**: local FLUX.2-klein-4B / Z-Image-Turbo (Apache-2.0), cloud FLUX 3 Klein API / Nano Banana 2.1 / gpt-image-2 BYO. Always text-free generation + deterministic title, OCR check, C2PA + IPTC labels, likeness guardrails | **P2** | Delightful for the long tail; must be safe and labelled (EU AI Act Art. 50 live since 2026-08-02) |
| 11 | **Remote AI worker over Truffle egress/proxy** with capability adverts in `SyncedStore` | **P2** | Lets N100/NAS servers use the user's Mac/GPU; strong Truffle dogfooding |
| 12 | ASR subtitles (Parakeet v3 for EU languages, Whisper-turbo otherwise, SpeechAnalyzer on macOS 26) and keyframe/plot embeddings (SigLIP 2 / CLIP + Qwen3-Embedding) behind opt-in | **P2** | Valuable but expensive; licence-clean model choices matter (avoid NLLB, MobileCLIP2 as defaults) |
| 13 | HTTP-pluggable provider protocol (à la Plex Custom Metadata Providers) | **P2** | Community providers without recompiling |

---

## 10. Open questions and risks

1. **TMDB AI clause scope.** Does "in connection with… an ML or AI based Application" cover (a) a local LLM that sees only filenames and receives verdict codes derived from TMDB searches; (b) embedding TMDB overviews for local semantic search; (c) VLM captioning of user frames in an app that *also* uses TMDB? Only TMDB can answer. Risk: high; mitigation in §2.2.
2. **Shipping a TMDB key in OSS:** tolerated for Jellyfin (key is in source), but there is no explicit ToS permission. Ask TMDB, and plan for BYO key if refused.
3. **TheTVDB pricing for OSS:** "significant discounts" are unspecified. Is a free licensed key available for a non-revenue OSS project?
4. **fanart.tv and AniDB terms:** primary pages were Cloudflare-blocked this session. Verify manually.
5. **OpenSubtitles API quotas and moviehash response fields:** primary docs are JS-rendered and were not readable. Verify against the live API with a test account.
6. **Actual local model latency/quality on M1 Max and N100** for (a) 4B LLM structured matching and (b) FLUX.2-klein-4B / Z-Image-Turbo poster generation. These were not measured (download budget). Prototypes A and B measure them.
7. **EU AI Act role of self-hosted OSS:** provider vs deployer obligations for an OSS server that calls third-party image APIs or runs local weights. Marking by default reduces exposure; get legal review before any commercial offering.
8. **Anime mapping dataset licences** (Fribb/Anime-Lists show no SPDX licence). Fetching at runtime is likely fine; vendoring needs permission.
9. **Plex's built-in agent data sources** were not re-verified this session.
10. **BFL FLUX.2 [pro]/[flex]/[max] exact per-MP prices:** rendered client-side; only third-party snapshots were available.

---

## 11. Local artefacts produced by this research

- `$FLUX_WORK/proto/metadata-match/`: Rust prototype (`src/main.rs`, `names.txt`, `run1.txt`). Build output is in `$FLUX_WORK/target`.
- `$FLUX_WORK/cache/tmdb-exports/`: `movie_ids_10_18_2026.json.gz` (28 MB) and `tv_series_ids_10_18_2026.json.gz` (5 MB). Total downloads this session ≈ 35 MB plus a few MB of crates.

## 12. Source index (primary unless noted)

- TMDB: [API ToS](https://www.themoviedb.org/api-terms-of-use) · [Terms of Use](https://www.themoviedb.org/terms-of-use) · [rate limiting](https://developer.themoviedb.org/docs/rate-limiting) · [daily exports](https://developer.themoviedb.org/docs/daily-id-exports) · [auth](https://developer.themoviedb.org/docs/authentication-application) · [images](https://developer.themoviedb.org/docs/image-basics) · [changes](https://developer.themoviedb.org/docs/tracking-content-changes) · [append_to_response](https://developer.themoviedb.org/docs/append-to-response) · [episode groups](https://developer.themoviedb.org/reference/tv-episode-group-details)
- TheTVDB: [API info](https://thetvdb.com/api-information) · [PIN FAQ](https://support.thetvdb.com/kb/faq.php?id=82) · [v4 swagger](https://github.com/thetvdb/v4-api/blob/main/docs/swagger.yml)
- [TVmaze API](https://www.tvmaze.com/api) · [OMDb keys](https://www.omdbapi.com/apikey.aspx) · [IMDb datasets](https://data.imdb.com/non-commercial-datasets/) · Wikidata API/SPARQL (live queries) · [WDQS limits (R pkg manual, S)](https://wikimedia.r-universe.dev/WikidataQueryServiceR/doc/manual.html)
- Trakt: [rate limits](https://github.com/trakt/trakt-api/blob/master/projects/developer/src/lib/guides/rate-limiting.md) · [VIP methods](https://github.com/trakt/trakt-api/blob/master/projects/developer/src/lib/guides/vip-methods.md) · [2025 pricing news (S)](https://alternativeto.net/news/2025/2/trakt-tv-has-set-stricter-limits-for-free-users-and-raised-vip-subscription-prices-by-100-/)
- AniList: [rate limiting](https://docs.anilist.co/guide/rate-limiting) · [terms](https://docs.anilist.co/guide/terms-of-use) · AniDB [FileBot reports (S)](https://www.filebot.net/forums/viewtopic.php?p=51839) · [Fribb/anime-lists](https://github.com/Fribb/anime-lists) · [Anime-Lists](https://github.com/Anime-Lists/anime-lists) · [Shoko](https://github.com/ShokoAnime/ShokoServer)
- Jellyfin source @ `08c2ba6`: [TmdbUtils.cs](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbUtils.cs) · [TmdbClientManager.cs](https://github.com/jellyfin/jellyfin/blob/08c2ba6e0b6499122822f1c5b18bc426104538b4/MediaBrowser.Providers/Plugins/Tmdb/TmdbClientManager.cs) · TVDB plugin @ `f1bcc9e` · [OpenSubtitles plugin](https://github.com/jellyfin/jellyfin-plugin-opensubtitles)
- Plex: [Custom Metadata Providers](https://forums.plex.tv/t/announcement-custom-metadata-providers/934384) · [Credits detection (S)](https://forums.plex.tv/t/credits-detection/831048)
- Intro Skipper @ `1bb2af7`: [repo](https://github.com/intro-skipper/intro-skipper) · [TheIntroDB plugin](https://github.com/TheIntroDB/jellyfin-plugin) · [Posterizarr](https://github.com/fscorrupt/Posterizarr)
- Pricing: [OpenAI](https://developers.openai.com/api/docs/pricing) · [OpenAI image guide](https://developers.openai.com/api/docs/guides/image-generation) · [Gemini](https://ai.google.dev/gemini-api/docs/pricing) · [Claude](https://claude.com/pricing) · [BFL docs pricing](https://docs.bfl.ai/quick_start/pricing) · [BFL pricing page](https://bfl.ai/pricing)
- Models: [mflux](https://github.com/filipstrand/mflux) (0.23.0 on PyPI) · HF cards for FLUX.2-klein-4B/9B, flux-3-image-klein, Z-Image-Turbo, Qwen-Image, Qwen-Image-2.1, Parakeet v3, whisper-large-v3-turbo, Qwen3.5-4B, Gemma 4 E4B, Qwen3-VL-4B, SigLIP 2, MobileCLIP2, nomic-embed-vision, EmbeddingGemma, Qwen3-Embedding, NLLB-200, MADLAD-400
- Inference perf: [llama.cpp Apple Silicon table](https://github.com/ggml-org/llama.cpp/discussions/4167) · [Ollama structured outputs](https://docs.ollama.com/capabilities/structured-outputs) · [Arm CLIP on Pi 5 (S)](https://developer.arm.com/ai/models/hugging-face/Arm/clip-vit-base-patch32-int8-xnnpack-executorch/clip-vit-b32-int8-pte) · [Argmax on SpeechAnalyzer (S)](https://www.argmaxinc.com/blog/apple-and-argmax) · [Parakeet vs Whisper (S, vendor)](https://whispernotes.app/blog/parakeet-v3-default-mac-model)
- Vision: [Apple aesthetics request](https://developer.apple.com/documentation/vision/calculateimageaestheticsscoresrequest) · [Netflix AVA](https://netflixtechblog.com/ava-the-art-and-science-of-image-discovery-at-netflix-a442f163af6) (via [Gigazine, S](https://gigazine.net/gsc_news/en/20180905-netflix-art-science-image-discovery/))
- Provenance and regulation: [OpenAI C2PA help (S)](https://help.openai.com/en/articles/8912793-c2pa-in-chatgpt-images) · [Google provenance blog](https://blog.google/innovation-and-ai/products/making-it-easier-to-understand-how-content-was-created-and-edited/) · [EU AI Act after Omnibus (S)](https://www.technology.org/2026/10/13/eu-ai-act-deadlines-after-the-digital-omnibus-what-applies-from-december-2026-to-august-2028/) · [CSA Art. 50 note (S)](https://labs.cloudsecurityalliance.org/research/csa-research-note-eu-ai-act-article50-watermarking-deadline/)
- Local code: Truffle @ `d6f6664` (`crates/truffle-core/src/{egress.rs:1-15, node.rs:614-691, synced_store/mod.rs:1-19, proxy/mod.rs:1-6}`) · Mille @ `c8160d5` (`crates/mille-core/src/walker.rs:78-108`)
