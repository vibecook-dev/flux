# flux for agents

flux has no AI of its own, and doesn't need one: the agent you already use (Claude Code, Codex, Claude Desktop, …) can read the open web, look at images and write code. For a rare film, a fan edit, a festival short, a concert recording or an old TV special, the web knows far more than any metadata database, and an agent can dig it out. So flux's job is to be the best tool for that agent:

- say exactly what it knows about each file, and how it knows it;
- take changes safely;
- show you what every agent did, and let you undo it.

This document is the design. [Plan](#plan) at the end says what is built.

## Try it

1. **Connect your agent.** For Claude Code (or any MCP client) on the server's own machine: `claude mcp add --transport http flux http://localhost:8096/mcp`; across the tailnet use the server's tailnet address. For agents that work in a terminal: `flux skills install`, then `flux status`. The app's **Agents & changes** screen (the corner menu) shows both commands for your server.
2. **Ask.** "What's missing in my flux library?" "Identify the films flux couldn't, from their files and the web, and fill in what you can, with sources." "Find proper posters where flux only has frames." "Those two Heat files are one film." "Name my files the way Plex expects." "Make me a Ghibli collection." "Make me a lens that looks like an airport departure board."
3. **Review.** Everything the agent changed is in Agents & changes, with its summary and sources; undo any changeset there (or `flux changes undo ID`).

## What an agent can do

| Ask your agent to… | It uses |
| --- | --- |
| Fill in what's missing: titles, years, descriptions, directors, cast, episode names | `library.gaps` and `items.find` (what's missing), `item.get` (the file's own evidence), `items.edit` (with the links it used) |
| Identify files flux couldn't: messy names, other languages, no metadata source at all | `item.get` (path, container title, duration, languages), `items.edit` (title, year, ids) |
| Find posters and backdrops | `items.find` with gap `poster`, `art.view` (look at what's there), `art.add` (credited), `art.pin` |
| Make a new way to browse | the lens kit schema, `lens.check`, `lens.add` (on all your devices, live) |
| Fix what flux read wrong: two items that are one film, one that is two, an episode numbered wrong, a trailer taken for a film | `items.merge`, `files.fix` |
| Tidy file and folder names (Plex, Jellyfin, Kodi style) | `files.plan`: a plan you review and apply; undo puts everything back |
| Gather titles: "Ghibli", "Rainy Sunday", "Kids' films" | `collection.put` (picked, or by a rule); they're spaces in the app |
| Contribute without reaching flux, or keep facts with the files | a `.flux.json` beside the video, read on every scan |
| Undo what an agent did | `changes.list`, `changes.undo`, or the Changes screen |

## Principles

1. **One contract, three ways in.** Each operation is defined once in the server: a name, a JSON Schema for its input and its output, who may call it, and what it changes. The same operations are served over HTTP (`POST /api/v1/ops/{name}`), the `flux` command line and MCP. The web client uses them too, so nothing is agent-only and nothing drifts.
2. **Every value says where it came from:** the file (its name, folder or container tags), a provider (TMDB), you, or an agent, with the links it used. What's missing or uncertain is a query (`library.gaps`), not something an agent has to guess.
3. **An agent is a metadata source.** What it digs out is stored as facts with their sources, and flux's automatic matching never overwrites it. An agent never overwrites what you typed, unless you asked it to (`force`).
4. **Every change is a changeset you can undo.** It records the agent's name, a summary in its words, and its sources. You see it in the app, and one click (or `flux changes undo`) reverts it field by field, except fields that changed again since.
5. **Agents propose or apply.** An Owner's or Admin's agent applies its changes (and they're journaled). Anyone else's agent proposes them, for an Owner or Admin to apply or reject. Any agent may choose to propose.
6. **TMDB content never reaches an agent.** TMDB's API terms forbid using its content with AI ([research/05](../research/05-metadata-and-ai.md)), and flux's rule since the start is that AI never sees it. What flux shows an agent leaves out TMDB's text and images and says so (`withheld`). Instead of getting TMDB's description, the agent writes its own from the open web. IDs stay visible: they name a work, and they are how an agent crosses to open data (Wikidata lists TMDB and IMDb ids).
7. **Labels stay honest.** Art an agent found is credited to where it came from. Art made with AI is labelled AI-generated, naming the model, like flux's own.
8. **Your files are yours.** Nothing deletes media. Files are renamed or moved only by a plan a person applies ([Organising files](#organising-files)), never replacing a file, never leaving their library folder, and undo puts them back.

## How it fits together

```
  your agent ─┬─ flux CLI ─────────────┐
              ├─ MCP over stdio (flux mcp) ┼─▶  HTTP  POST /api/v1/ops/{op}   POST /mcp
              └─ MCP over HTTP ─────────┘          │
                                                    ▼
          flux-server  agent: the operations ── contracts (flux_core::agent, JSON Schema)
                         who may (roles) · what's withheld (provider terms) · changesets
                                                    │
                                                    ▼
          flux-catalog  item_field (where each value came from; locks) · changeset + change (journal)
                        item.details (directors, cast, links…) · art 'custom' · lens
                        file_fix (what a file is) · meta_file (.flux.json, .nfo) · collection
```

## Contracts

- **JSON Schema (2020-12), generated from the Rust types** (`flux_core::agent`, with `schemars`), so a contract can't drift from the code. They're checked in under [`schemas/agent/v1/`](../schemas/agent/v1) and served at `/api/v1/schemas/{name}`; a test fails when the checked-in copy differs.
- **Versioned.** The contract is `flux.agent/1`. Fields and operations are added compatibly; a breaking change would be `flux.agent/2`, served alongside.
- **Self-describing.** `GET /api/v1/agent` is the manifest: the contract, every operation with its schemas and hints, the skills, the MCP endpoint and the policy (which providers are withheld). `flux ops` prints it.
- **Errors are data.** `/api/v1/ops` answers errors as `{"error": {"code", "message"}}`, with codes `invalid`, `not_found`, `forbidden`, `conflict`, `unavailable`, `too_large` and `internal`. (The rest of the API answers errors in plain text, for people.)
- **The lens kit is a contract too** (`lens`).

## Operations

| Operation | What it does | Who |
| --- | --- | --- |
| `status` | The server, the contract, the metadata source, the policy, the libraries, who you are and what you may do | anyone |
| `library.gaps` | Per library, how many items lack each thing: identity, year, description, poster, backdrop, episode titles; how many are offline | anyone |
| `items.find` | Items by library, kind, gap, match state, what they're below (a show, a season), or text in their title or path; paged | anyone |
| `item.get` | One item as an agent sees it: its fields and where each came from, what's withheld, its files' own evidence (path, size, duration, tracks and languages, container title, what flux read from the name), its art and its children | anyone |
| `items.edit` | Set or clear fields of up to 100 items, with the sources used: one changeset | Owner/Admin apply; Viewer proposes |
| `art.view` | An image, to look at (over MCP it arrives as an image) | anyone |
| `art.add` | A poster, backdrop, thumb or logo from a URL or from bytes, credited, chosen for the item | Owner/Admin |
| `art.pin` | Choose which image shows (or go back to flux's choice) | Owner/Admin |
| `changes.list`, `changes.get` | Changesets, newest first, and each one's changes | anyone |
| `changes.undo` | Revert an applied changeset | Owner/Admin |
| `changes.apply`, `changes.reject` | Decide a proposed one | Owner/Admin |
| `lenses.list`, `lens.add`, `lens.remove` | Lenses kept on the server: yours, or (Owner/Admin) everyone's | anyone signed in |
| `lens.check` | Check a lens without keeping it | anyone |
| `files.fix` | Say what up to 200 files are (which film or show, title and year, season and episode, extra, ignore): one changeset | Owner/Admin apply; Viewer proposes |
| `items.merge` | Films (or shows) that are one work, as fixes; undo gives them back | Owner/Admin apply; Viewer proposes |
| `files.plan` | Plan renaming and moving files (a naming scheme, or moves you give): a proposed changeset of steps | anyone who may propose; applying it is Owner/Admin |
| `collections.list`, `collection.get` | Collections you may see, and one's titles in order | anyone |
| `collection.put`, `collection.remove` | Keep a collection: yours, or (Owner/Admin) everyone's | anyone signed in |
| `library.scan` | Scan a library now | Owner/Admin |

### What an agent can set

`items.edit` takes, for each item, the fields to set and the ones to clear (a cleared field goes back to what flux finds by itself):

- **Identity:** `title`, `original_title`, `year`, and `ids` (`tmdb`, `imdb`, `tvdb`, and any other: `wikidata`, `anidb`, `musicbrainz`, …).
- **About:** `overview` (in the agent's own words), `tagline`, `genres`, `tags`.
- **Details:** `release_date`, `languages`, `countries`, `directors`, `writers`, `cast` (name and role), `studios`, `content_rating`, `links` (official site, encyclopaedia pages, archives).
- **Episodes:** the same, plus `air_date`.

Each field it sets records where it came from: the agent's name, the person it acted for, the changeset, and the links that support it.

## Where values come from

Each field of an item has one source: **you** (the web app, or the CLI with no agent named), **an agent**, **a metadata file** beside the video (a `.flux.json` or an `.nfo`), **a provider** (TMDB), or **the file** (its name, its folder, its container tags; a field with no recorded source came from the scan).

- **Locks.** The scanner (a re-parse) and the matcher write only fields that are theirs: a field you, an agent or a metadata file set stays until it's cleared. What counts more replaces what counts less: you, then an agent, then a metadata file, then a provider. An agent may replace what an agent or a file set; replacing what you set takes `force`, for when you asked it to.
- **Identity.** When an agent sets ids and a provider is configured, the matcher matches by those ids instead of searching, and fills in what nobody set: your agent identifies the film, TMDB supplies its poster. With no provider (flux's default) the agent's facts are all there is.
- **Withheld.** The agent view lists what it leaves out: `"withheld": {"overview": "tmdb", "genres": "tmdb"}`; a TMDB image is listed without its address (`art[].withheld`), and `art.view` won't show it. A title from TMDB is replaced by the one the file gives (`"sources": {"title": "file"}`), so the agent still knows what it's looking at. Match evidence names TMDB's candidates, so it's left out (the confidence stays). Text search in `items.find` looks at the titles an agent may see and at file paths, never at TMDB's text. What flux read from an `.nfo` is withheld the same way (`"nfo"`): scrapers write NFOs, usually from TMDB, and nothing in one says where its text came from. A collection's rule never counts a withheld value either, so which titles are in it can't tell an agent what TMDB says.

## Changesets

```
changeset  id · actor (who it acted for) · agent · summary · state: proposed | applied | reverted | rejected · times
change     changeset · seq · item · field · op: set | clear · before · after · sources
           · state: pending | applied | skipped | reverted | kept
```

- **Apply** records each field's value before. A proposal also remembers what each field held when it was proposed; applying it later skips a field that has changed since (and says so).
- **Undo** restores a field only while it still holds what the changeset set; otherwise the field is skipped and reported.
- Changesets are small and kept. The Changes screen shows the latest; `changes.list` pages through all of them.
- Besides fields (`title`, `ids.tmdb`, …) and images (`art.poster`), a change can be what a file is (`file.fix`) or a step of a file plan (`folder.new`, `folder.path`, `file.path`, `folder.gone`). `moves_files` says a changeset is a file plan.

## Structure fixes

flux places each file by what its name says ([`flux-catalog` grouping](ARCHITECTURE.md)). When it read one wrong, a **fix** (`files.fix`) says what the file is, and flux places the file by its name corrected by the fix, on every scan, until the fix is cleared: the fix belongs to the file's row, which follows the file through renames.

```
FileFix  kind: movie | episode | extra | ignore · of (the film or show it belongs to)
         · title, year (what to group it under) · season, episodes · extra (its kind) · edition · part
```

- **Two items that are one film**: `items.merge` fixes every file of the others to belong to the one kept (`of`); episodes keep their numbers. A work left with no files is carried into the one its files went to, as when a rename moves them: what people and agents set on it, everyone's watched state and its collections come along. Each change keeps what the merged work held, so undo gives it all back.
- **One item that is two films**: give each film's files their own `title` and `year`.
- **An episode numbered wrong**: `season` and `episodes`. **A trailer taken for a film**: `kind: extra`, `extra: trailer`, `of` the film. **A sample or a stray clip**: `kind: ignore`.
- A fix says only what's wrong; the rest still comes from the name. A fix pinned to a work follows that work when it's merged into another.
- `dry_run` works the fix out (where each file would go) and rolls it back.

## Organising files

`files.plan` names items' files by a scheme, or moves files where an agent says, and keeps the result as a **proposed changeset**: nothing moves until an Owner or Admin applies it (`changes.apply`, or in the app), and `changes.undo` puts everything back.

The `standard` scheme is what Plex, Jellyfin, Emby and Kodi all read:

```
Title (Year)/Title (Year).mkv                       versions " - Director's Cut", " - 1080p"; parts " - part2"
Show (Year)/Season 01/Show (Year) - S01E02 - Episode Title.mkv     specials in Specials/
Title (Year)/Trailers/…, Featurettes/, Behind The Scenes/, Deleted Scenes/, Interviews/, Shorts/, Other/
```

- **Steps.** A plan is a list of steps, run in order and undone in reverse: make a folder (`folder.new`), rename a folder with everything in it (`folder.path`: a film's or show's own folder, holding no other title's files), move a video or a file named like it (`file.path`: subtitles, `.nfo`, `.flux.json`, `-poster.jpg`), and remove a folder the plan empties (`folder.gone`). The plan also lists what it leaves alone and why.
- **Safety.** Every step stays inside its library folder and is checked again right before it runs: a video must still be where the catalog has it, with the size it had; nothing may be at the target. Moves use the system's no-replace rename (`renameat2(RENAME_NOREPLACE)` on Linux, `renamex_np(RENAME_EXCL)` on macOS, `MoveFileExW` without replace on Windows), so a file that appeared since is never overwritten; a move across drives fails instead of copying; nothing goes through a link. A step that can't run is skipped and reported; the others go on. One plan runs at a time.
- **The catalog follows.** Each moved video keeps its file id, asset and everyone's state; its guess is read again from its new name and its work answers to the key that name gives, so the work keeps its id (and a later rescan finds nothing to do).
- Names are written so every system accepts them (no `:` or `?`, no trailing dots, no reserved names) and an agent's own moves are checked the same way.
- A videos library keeps the user's own names: only moves an agent gives apply there. An agent can't plan a title whose name it may not see (a TMDB title).

## Sidecars

flux reads metadata files beside the videos on every scan: its own `.flux.json` ([`flux.sidecar/1`](../schemas/agent/v1/sidecar.json)) and Kodi `.nfo` files.

```json
{
  "flux": "flux.sidecar/1",
  "fields": { "title": "Spirited Away", "original_title": "千と千尋の神隠し", "year": 2001 },
  "fix": { "year": 2001 },
  "episodes": [{ "season": 1, "episode": 1, "fields": { "title": "…" } }],
  "sources": ["https://en.wikipedia.org/wiki/Spirited_Away"],
  "by": "claude-code"
}
```

- `<video name>.flux.json` and `<video name>.nfo` describe that video; `flux.json` and `movie.nfo` in a film's own folder describe the film; `flux.json` and `tvshow.nfo` in a show's folder describe the show (a show's `flux.json` can name its episodes). A `.flux.json` beats an `.nfo`, a video's own file beats its folder's.
- What a file says counts more than a provider's match and less than what a person or an agent set in flux. A fact a file no longer gives is handed back to flux (its value stays). A `.flux.json` may say what its video is (`fix`); a fix set in flux wins.
- The scan reads each new or changed file once (at most 1 MiB); an unchanged rescan reads and writes nothing. `item.get` lists the metadata files beside each video (`files[].sidecars`).
- So an agent that can write files but can't reach flux can still contribute, and the facts travel with the files to another server.

## Collections

A collection is titles picked by hand (in order, each with a note) and/or a rule that takes the titles matching it as the library changes: genres, tags, people (directors, writers, cast), studios, countries, languages, years, text in the title. Kept per person or (Owner/Admin) for everyone. The app shows them as spaces beside the libraries, browsable in every lens; `GET /api/v1/collections/{id}/items` pages one like a library. A merge keeps the title that stays in its collections.

## The `flux` command line

`flux-server` stays the server. `flux` (`crates/flux-cli`) is a small client that talks to a running server over HTTP, on this machine or across the tailnet:

```sh
flux status
flux gaps                                   # what's missing, per library
flux find --gap poster --kind movie
flux get 42
flux edit 42 title="Spirited Away" original_title="千と千尋の神隠し" year=2001 \
  directors="Hayao Miyazaki" --source https://en.wikipedia.org/wiki/Spirited_Away \
  --summary "Identified from the Japanese file name"
flux art add 42 poster https://example.org/poster.jpg --credit "Studio Ghibli" --from https://example.org/press
flux art view 1234 -o poster.jpg
flux merge 40 41 --summary "Two copies of Heat"
flux fix 812 season=2 episodes=5             # what a file is
flux plan 40 41 --summary "Name the Ghibli films' files"   # a proposal; flux changes apply ID runs it
flux collections put Ghibli --pick 40 --pick "41:Where it began"
flux changes                                # newest first
flux changes undo 17
flux lens check board.json && flux lens add board.json
flux call items.find '{"gap": "year"}'      # any operation, with JSON
flux ops                                    # every operation and its schemas
flux mcp                                    # MCP over stdio, for agents that start servers
flux skills install                         # flux's skills where your agent looks for them
```

- It prints JSON (indented on a terminal). Errors go to stderr as JSON, with exit codes: 1 error, 2 usage, 3 not found, 4 conflict, 5 forbidden, 6 server unreachable.
- It finds the server from `--server`, `FLUX_SERVER`, the `server.json` a running server keeps in its data directory, then `http://127.0.0.1:8096`.
- It names the agent from `--agent`, `FLUX_AGENT`, or where it runs (`CLAUDECODE=1` is Claude Code). With none, or with `--human`, a change is yours and nothing is withheld.
- On the server's own machine you are its Owner; across the tailnet your Tailscale identity decides your role. There are no tokens.

## MCP

- `POST /mcp` on every listener (Streamable HTTP, JSON responses). Across the tailnet your identity is the authentication: `claude mcp add --transport http flux https://flux.tail1234.ts.net:8096/mcp`.
- `flux mcp` carries MCP over stdio to the same endpoint, for clients that start a process.
- **Tools** are the operations (`items_find`, `item_get`, …), with input and output schemas and read-only and destructive hints. Results carry `structuredContent` and a JSON text copy; `art_view` returns the image itself.
- **Resources** are the skills (`flux://skills/…`) and the schemas (`flux://schemas/…`). **Prompts** start the common jobs: `fill_gaps`, `find_art`, `organize_files`, `make_collection`, `make_lens`, `review_library`.
- The server's `instructions` carry the house rules: cite sources, label AI art, TMDB stays out, everything can be undone.

## Skills

[`skills/`](../skills) holds [Agent Skills](https://agentskills.io) (a folder with a `SKILL.md`):

| Skill | For |
| --- | --- |
| `flux` | How to reach flux, how its library works, the rules |
| `flux-metadata` | Identifying files and filling in what's missing from the open web, with sources |
| `flux-artwork` | Finding, checking and crediting posters and backdrops; labelling AI art |
| `flux-organize` | Collections, fixing what files are, planning file renames, writing sidecars |
| `flux-lens` | Designing a lens, checking it and adding it |

`flux skills install` copies them where your agent looks (`~/.claude/skills`, or `--dir`). Over MCP the same text is served as resources and prompts.

## Lenses on the server

`lens.add` keeps a lens on the server, for you or (Owner/Admin) for everyone: it shows on all your devices at once, without a reload. `lens.check` validates a lens against the schema and lists what the browser would remove (images, links, `url()`, inline styles), so an agent can fix its lens before you see it. The browser still sanitises and sandboxes every lens ([LENSES.md](LENSES.md#the-lens-kit)): the server's check is feedback, not the safety boundary.

## In the app

- **Changes** (in the corner menu): every changeset, newest first, with who made it (the agent and the person), when, and its summary. Open one for its changes field by field, with their sources, then Undo it, or Apply or Reject a proposal. A file plan reads as its steps; applying one opens it first, and its button says it moves the files. The island says when an agent changes something.
- **Item pages** say where facts came from (for example "claude-code, from en.wikipedia.org", or an `.nfo` file) and show the new details: directors, cast, languages, links. Art an agent added is credited. **Edit details** edits them with the same `items.edit`, as you (a Viewer's edit is a proposal).
- **Collections** are spaces in the island, beside the libraries.
- **Connect an agent** (in Changes) gives the two commands to paste: the MCP address for this server, and `flux skills install`. On an item page, **Fix with your agent…** copies a brief about that item for your agent, as "Make a lens" does for lenses.

## Security

- Roles as everywhere else: callers on the server's own machine are its Owner (unless `--localhost-role` says otherwise), tailnet callers have their Tailscale role, LAN callers are anonymous and may only read.
- `/api/v1/ops` follows the API's CSRF rule (`X-Flux-Request: 1`). `/mcp` instead takes JSON-RPC posts with `Content-Type: application/json` and no foreign `Origin`: a page on another site can't send that without a CORS preflight, which flux never grants. The Host allowlist (DNS rebinding) covers both.
- `art.add` fetches only `http` and `https`, follows 3 redirects at most, stops at 20 MB or 20 s, refuses this machine's loopback and link-local addresses, and keeps the bytes only if they decode as an image.
- File plans: only Owners and Admins apply them; see [Organising files](#organising-files) for what a step may and may not do.
- Metadata files are read at most 1 MiB each, as data (a forgiving XML reader for NFOs, no entities beyond XML's own).
- Everything is bounded: 100 items per edit, 200 files per fix, 200 per page, 16,000 characters per text field, 1,000 changes per changeset (and steps per plan), 24 lenses and 100 collections per person, 2,000 titles picked per collection.

## Plan

**Phase 1 is built** (branch `design/lenses`):

- **The contract** in `flux_core::agent`, its JSON Schemas generated into [`schemas/agent/v1/`](../schemas/agent/v1) (a test fails when they drift), the manifest at `GET /api/v1/agent`.
- **The catalog** (schema v8): `item_field` (where each value came from, and the locks), `changeset` and `change` (the journal), `item.details`, `custom` art with `found_at`, and `lens`. The matcher and the scanner's re-parse leave what people and agents set alone; merges and renamed files carry it, history included. Earlier online TMDB matches are recorded as TMDB's.
- **The operations** above, over HTTP (`agent` in flux-server) and MCP (`/mcp`, with tools, resources and prompts), with roles, the TMDB policy, JSON errors, and `art.add`'s guarded fetch.
- **`flux`** (`crates/flux-cli`), shipped in every release package: every operation, `flux mcp` (stdio, and a new session by itself when the server restarts), `flux skills install`.
- **Four skills** in [`skills/`](../skills).
- **The web client**: Agents & changes (proposals, history, undo, Connect an agent), item pages with the new details and where each came from, "Fix with your agent…", lenses kept on the server in the lens gallery, and the island saying when an agent changed something.
- **Checks**: catalog tests for locks, undo, proposals, merges, gaps and what's withheld; server tests over HTTP and MCP (roles, errors, art, lenses, the server file); CLI tests; the web client's golden test covers the new types.

**Phase 2 is built** too:

- **Structure fixes** (`files.fix`, `items.merge`): catalog `file_fix`; the scan places files by their names corrected by their fixes; undo of a merge gives the work back with what it held.
- **Organising files** (`files.plan`, then `changes.apply` and `changes.undo`): plans of steps, run with no-replace renames; the catalog follows the files and items keep their ids.
- **Sidecars**: `.flux.json` (`flux.sidecar/1`) and Kodi `.nfo` read on scan (catalog `meta_file`), ranked between agents and providers; NFO text withheld from agents.
- **Collections** (`collection.put` and the rest): picked and rule-based, per person or everyone; spaces in the app.
- **Editing in the app**: Edit details on item pages.
- Catalog schema v9 (`file_fix`, `meta_file`, `collection`, `collection_item`, and the `sidecar` source); the `flux-organize` skill; `flux fix`, `flux merge`, `flux plan`, `flux collections`; tests for each (fixes, sidecars and collections in the catalog, a real scan reading metadata files, file plans moving real files and undoing them, collections over HTTP).

**Phase 3:** agents that run on a schedule and leave proposals; tokens for agents outside the tailnet; an OpenAPI document for the whole HTTP API; MCP sampling, so flux can ask your agent for a match hypothesis from local evidence only.
