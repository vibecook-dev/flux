# flux for agents

flux has no AI of its own, and doesn't need one: the agent you already use (Claude Code, Codex, Claude Desktop, …) can read the open web, look at images and write code. For a rare film, a fan edit, a festival short, a concert recording or an old TV special, the web knows far more than any metadata database, and an agent can dig it out. So flux's job is to be the best tool for that agent:

- say exactly what it knows about each file, and how it knows it;
- take changes safely;
- show you what every agent did, and let you undo it.

This document is the design. [Plan](#plan) at the end says what is built.

## Try it

1. **Connect your agent.** For Claude Code (or any MCP client) on the server's own machine: `claude mcp add --transport http flux http://localhost:8096/mcp`; across the tailnet use the server's tailnet address. For agents that work in a terminal: `flux skills install`, then `flux status`. The app's **Agents & changes** screen (the corner menu) shows both commands for your server.
2. **Ask.** "What's missing in my flux library?" "Identify the films flux couldn't, from their files and the web, and fill in what you can, with sources." "Find proper posters where flux only has frames." "Make me a lens that looks like an airport departure board."
3. **Review.** Everything the agent changed is in Agents & changes, with its summary and sources; undo any changeset there (or `flux changes undo ID`).

## What an agent can do

| Ask your agent to… | It uses |
| --- | --- |
| Fill in what's missing: titles, years, descriptions, directors, cast, episode names | `library.gaps` and `items.find` (what's missing), `item.get` (the file's own evidence), `items.edit` (with the links it used) |
| Identify files flux couldn't: messy names, other languages, no metadata source at all | `item.get` (path, container title, duration, languages), `items.edit` (title, year, ids) |
| Find posters and backdrops | `items.find` with gap `poster`, `art.view` (look at what's there), `art.add` (credited), `art.pin` |
| Make a new way to browse | the lens kit schema, `lens.check`, `lens.add` (on all your devices, live) |
| Undo what an agent did | `changes.list`, `changes.undo`, or the Changes screen |

## Principles

1. **One contract, three ways in.** Each operation is defined once in the server: a name, a JSON Schema for its input and its output, who may call it, and what it changes. The same operations are served over HTTP (`POST /api/v1/ops/{name}`), the `flux` command line and MCP. The web client uses them too, so nothing is agent-only and nothing drifts.
2. **Every value says where it came from:** the file (its name, folder or container tags), a provider (TMDB), you, or an agent, with the links it used. What's missing or uncertain is a query (`library.gaps`), not something an agent has to guess.
3. **An agent is a metadata source.** What it digs out is stored as facts with their sources, and flux's automatic matching never overwrites it. An agent never overwrites what you typed, unless you asked it to (`force`).
4. **Every change is a changeset you can undo.** It records the agent's name, a summary in its words, and its sources. You see it in the app, and one click (or `flux changes undo`) reverts it field by field, except fields that changed again since.
5. **Agents propose or apply.** An Owner's or Admin's agent applies its changes (and they're journaled). Anyone else's agent proposes them, for an Owner or Admin to apply or reject. Any agent may choose to propose.
6. **TMDB content never reaches an agent.** TMDB's API terms forbid using its content with AI ([research/05](../research/05-metadata-and-ai.md)), and flux's rule since the start is that AI never sees it. What flux shows an agent leaves out TMDB's text and images and says so (`withheld`). Instead of getting TMDB's description, the agent writes its own from the open web. IDs stay visible: they name a work, and they are how an agent crosses to open data (Wikidata lists TMDB and IMDb ids).
7. **Labels stay honest.** Art an agent found is credited to where it came from. Art made with AI is labelled AI-generated, naming the model, like flux's own.
8. **Your files are yours.** No operation deletes or moves media. Renaming and moving files is in [Phase 2](#plan), as plans with a dry run and an undo.

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
| `library.scan` | Scan a library now | Owner/Admin |

### What an agent can set

`items.edit` takes, for each item, the fields to set and the ones to clear (a cleared field goes back to what flux finds by itself):

- **Identity:** `title`, `original_title`, `year`, and `ids` (`tmdb`, `imdb`, `tvdb`, and any other: `wikidata`, `anidb`, `musicbrainz`, …).
- **About:** `overview` (in the agent's own words), `tagline`, `genres`, `tags`.
- **Details:** `release_date`, `languages`, `countries`, `directors`, `writers`, `cast` (name and role), `studios`, `content_rating`, `links` (official site, encyclopaedia pages, archives).
- **Episodes:** the same, plus `air_date`.

Each field it sets records where it came from: the agent's name, the person it acted for, the changeset, and the links that support it.

## Where values come from

Each field of an item has one source: **you** (the web app, or the CLI with no agent named), **an agent**, **a provider** (TMDB), or **the file** (its name, its folder, its container tags; a field with no recorded source came from the scan).

- **Locks.** The scanner (a re-parse) and the matcher write only fields that are theirs: a field you or an agent set stays until it's cleared. An agent may replace what an agent set; replacing what you set takes `force`, for when you asked it to.
- **Identity.** When an agent sets ids and a provider is configured, the matcher matches by those ids instead of searching, and fills in what nobody set: your agent identifies the film, TMDB supplies its poster. With no provider (flux's default) the agent's facts are all there is.
- **Withheld.** The agent view lists what it leaves out: `"withheld": {"overview": "tmdb", "genres": "tmdb"}`; a TMDB image is listed without its address (`art[].withheld`), and `art.view` won't show it. A title from TMDB is replaced by the one the file gives (`"sources": {"title": "file"}`), so the agent still knows what it's looking at. Match evidence names TMDB's candidates, so it's left out (the confidence stays). Text search in `items.find` looks at the titles an agent may see and at file paths, never at TMDB's text.

## Changesets

```
changeset  id · actor (who it acted for) · agent · summary · state: proposed | applied | reverted | rejected · times
change     changeset · seq · item · field · op: set | clear · before · after · sources
           · state: pending | applied | skipped | reverted | kept
```

- **Apply** records each field's value before. A proposal also remembers what each field held when it was proposed; applying it later skips a field that has changed since (and says so).
- **Undo** restores a field only while it still holds what the changeset set; otherwise the field is skipped and reported.
- Changesets are small and kept. The Changes screen shows the latest; `changes.list` pages through all of them.

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
- **Resources** are the skills (`flux://skills/…`) and the schemas (`flux://schemas/…`). **Prompts** start the common jobs: `fill_gaps`, `find_art`, `make_lens`, `review_library`.
- The server's `instructions` carry the house rules: cite sources, label AI art, TMDB stays out, everything can be undone.

## Skills

[`skills/`](../skills) holds [Agent Skills](https://agentskills.io) (a folder with a `SKILL.md`):

| Skill | For |
| --- | --- |
| `flux` | How to reach flux, how its library works, the rules |
| `flux-metadata` | Identifying files and filling in what's missing from the open web, with sources |
| `flux-artwork` | Finding, checking and crediting posters and backdrops; labelling AI art |
| `flux-lens` | Designing a lens, checking it and adding it |

`flux skills install` copies them where your agent looks (`~/.claude/skills`, or `--dir`). Over MCP the same text is served as resources and prompts.

## Lenses on the server

`lens.add` keeps a lens on the server, for you or (Owner/Admin) for everyone: it shows on all your devices at once, without a reload. `lens.check` validates a lens against the schema and lists what the browser would remove (images, links, `url()`, inline styles), so an agent can fix its lens before you see it. The browser still sanitises and sandboxes every lens ([LENSES.md](LENSES.md#the-lens-kit)): the server's check is feedback, not the safety boundary.

## In the app

- **Changes** (in the corner menu): every changeset, newest first, with who made it (the agent and the person), when, and its summary. Open one for its changes field by field, with their sources, then Undo it, or Apply or Reject a proposal. The island says when an agent changes something.
- **Item pages** say where facts came from (for example "claude-code, from en.wikipedia.org") and show the new details: directors, cast, languages, links. Art an agent added is credited.
- **Connect an agent** (in Changes) gives the two commands to paste: the MCP address for this server, and `flux skills install`. On an item page, **Fix with your agent…** copies a brief about that item for your agent, as "Make a lens" does for lenses.

## Security

- Roles as everywhere else: callers on the server's own machine are its Owner (unless `--localhost-role` says otherwise), tailnet callers have their Tailscale role, LAN callers are anonymous and may only read.
- `/api/v1/ops` follows the API's CSRF rule (`X-Flux-Request: 1`). `/mcp` instead takes JSON-RPC posts with `Content-Type: application/json` and no foreign `Origin`: a page on another site can't send that without a CORS preflight, which flux never grants. The Host allowlist (DNS rebinding) covers both.
- `art.add` fetches only `http` and `https`, follows 3 redirects at most, stops at 20 MB or 20 s, refuses this machine's loopback and link-local addresses, and keeps the bytes only if they decode as an image.
- Everything is bounded: 100 items per edit, 200 per page, 16,000 characters per text field, 1,000 changes per changeset, 24 lenses per person.

## Plan

**Phase 1 is built** (branch `design/lenses`):

- **The contract** in `flux_core::agent`, its JSON Schemas generated into [`schemas/agent/v1/`](../schemas/agent/v1) (a test fails when they drift), the manifest at `GET /api/v1/agent`.
- **The catalog** (schema v8): `item_field` (where each value came from, and the locks), `changeset` and `change` (the journal), `item.details`, `custom` art with `found_at`, and `lens`. The matcher and the scanner's re-parse leave what people and agents set alone; merges and renamed files carry it, history included. Earlier online TMDB matches are recorded as TMDB's.
- **The operations** above, over HTTP (`agent` in flux-server) and MCP (`/mcp`, with tools, resources and prompts), with roles, the TMDB policy, JSON errors, and `art.add`'s guarded fetch.
- **`flux`** (`crates/flux-cli`), shipped in every release package: every operation, `flux mcp` (stdio, and a new session by itself when the server restarts), `flux skills install`.
- **Four skills** in [`skills/`](../skills).
- **The web client**: Agents & changes (proposals, history, undo, Connect an agent), item pages with the new details and where each came from, "Fix with your agent…", lenses kept on the server in the lens gallery, and the island saying when an agent changed something.
- **Checks**: catalog tests for locks, undo, proposals, merges, gaps and what's withheld; server tests over HTTP and MCP (roles, errors, art, lenses, the server file); CLI tests; the web client's golden test covers the new types.

**Phase 2:**

- **Collections.** Hand-picked or by rule, shown as spaces in the island ("Ghibli", "Rainy Sunday", "Kids' films").
- **Organising files.** Rename and move files to a naming scheme, as a plan with a dry run that flux carries out (inside a library's folders, never across drives, never overwriting) and can undo. The scanner re-attaches moved files, so watched state and resume points stay.
- **Sidecars.** A `.flux.json` beside a video, read on scan, so an agent with only file access can contribute and the facts travel with the files. Kodi `.nfo` import.
- **Structure fixes.** Merge or split items, renumber episodes, mark extras.
- **Editing in the app.** The same `items.edit`, from an item page, for people.

**Phase 3:** agents that run on a schedule and leave proposals; tokens for agents outside the tailnet; an OpenAPI document for the whole HTTP API; MCP sampling, so flux can ask your agent for a match hypothesis from local evidence only.
