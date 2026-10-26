---
name: flux-metadata
description: Identify films, shows and videos in a flux library and fill in what's missing (titles, original titles, years, descriptions, directors, cast, languages, episode names, ids) by researching the open web, with sources. Use when the user asks to fix, complete, identify, clean up or "track down" information about items in flux, especially rare, foreign, old or obscure titles that metadata databases lack. Needs the flux skill.
---

# Filling in a flux library from the open web

Databases like TMDB cover popular titles well and rare ones badly. You can do better: read what the file itself says, search the open web, cross-check, and write back what you found with the pages that prove it. This skill is that workflow. Read the `flux` skill first for connecting and the rules.

## 1. Survey

```sh
flux status          # metadata_source "none" means flux matched nothing itself: every title is from a file name
flux gaps            # per library: identity, year, overview, poster, backdrop, episode_titles, offline
flux find --gap identity --limit 50
```

Agree the scope with the user before a big run ("the 14 films without a year", not "everything"). Skip `videos` libraries (home videos) unless the user asks: they are personal, and there is nothing on the web about them.

## 2. Read the evidence

`flux get ID` gives, per file: `path` (folder names often hold the real title and year), `parsed` (what flux's name parser read: title, original title, year, season and episodes, edition, release group), `duration_ms`, `tracks` (audio and subtitle languages, the video's size), `container_title` (often the real title, or a release name), `chapters`, `subtitle_files`, and `modified`. For a show, also `folders` and its seasons and episodes as `children`.

Useful signals:

- **Duration** within about 3 minutes of a candidate's runtime is strong evidence; a 90-minute file isn't a 3-hour film.
- **Audio language** is usually the original language (a Japanese film has a `jpn` track, even if dubbed tracks exist).
- **Release names** (`Gisaengchung.2019.1080p…`) are often romanised original titles: search those too.
- **Folder and file names in another script** are the original title: keep it in `original_title`.

## 3. Research

Search the web with the evidence: title variants, year, director, original-language title. Good sources, best first:

- Wikipedia (and the original-language Wikipedia for foreign titles), Wikidata (ids: TMDB P4947, IMDb P345, TVDB P4835)
- National film archives and databases (BFI, Japanese Movie Database, KMDb, filmportal.de, CNC), festival catalogues
- The studio's, distributor's or broadcaster's own pages; for anime: AniDB, MyAnimeList; for music: MusicBrainz, Discogs; Internet Archive for old and public-domain works
- IMDb and Letterboxd to cross-check (cite them only for facts, not text)

Never use TMDB's text. If a page doesn't confirm a value, don't set it.

## 4. Write back

Set what you confirmed, cite every page you used, and summarise the changeset for a person:

```sh
flux edit 42 title="Parasite" original_title="기생충" year=2019 \
  directors="Bong Joon-ho" languages=ko countries=KR genres="Thriller, Drama" \
  release_date=2019-05-30 ids.wikidata=Q61448040 ids.imdb=tt6751668 \
  overview="A poor family schemes its way into the service of a wealthy household, until a secret in the basement turns the arrangement into a fight for survival." \
  --source https://en.wikipedia.org/wiki/Parasite_(2019_film) \
  --source https://www.wikidata.org/wiki/Q61448040 \
  --summary "Identified Gisaengchung.2019 as Bong Joon-ho's Parasite (runtime and Korean audio match)"
```

For many items, write an `items.edit` input and run `flux edit --file edits.json` (one changeset, at most 100 items, each with its own sources):

```json
{
  "summary": "Years and directors for 3 Ghibli films, from Wikipedia",
  "edits": [
    { "item": 12, "set": { "year": 1988, "directors": ["Hayao Miyazaki"] },
      "sources": [{ "url": "https://en.wikipedia.org/wiki/My_Neighbor_Totoro" }] }
  ]
}
```

Field notes:

- `title`: the best-known title in the library's language. `original_title`: the original language and script.
- `overview`: two to five sentences in your own words, no spoilers past the premise.
- `genres` in English and capitalised; `tags` for the user's own groupings ("Studio Ghibli", "Criterion").
- `languages`: ISO 639-1 (`ja`, `ko`, `zh-Hant`); `countries`: ISO 3166-1 alpha-2 (`JP`, `KR`).
- `cast`: top-billed only, in billing order, with the role.
- Ids: set what you're sure of. With a provider configured, flux uses `ids.tmdb` to fetch the rest itself (you won't see that text, and that's fine).
- Episodes: `flux find --parent SHOW --kind episode --gap episode_titles`, then set each episode's `title` (and `air_date`, `overview`) from the broadcaster's or an episode list page.
- Values from an `.nfo` beside the file (`sources.<field>.source = "sidecar"`, provider `nfo`) are withheld from you like TMDB's: replace them only with your own, cited.
- Can't reach flux, but can write to the library's folders? Write a `.flux.json` beside the video instead (flux-organize, "Sidecars"); flux reads it on its next scan.

## 5. Check

```sh
flux edit 42 … --dry-run     # before: what would change, what's refused and why
flux get 42                  # after: values and sources
flux changes                 # your changeset; the user can undo it in the app
```

Once titles and years are right, the files can be named to match (`flux plan`, see flux-organize). If two items turn out to be one film, merge them (`flux merge`); if a file belongs to another episode, fix it (`flux fix`).

If the user disagrees with a change, `flux changes undo ID` restores it. If you're not confident (two films share a title and year, the runtime is off), say so and `--propose` instead of applying.
