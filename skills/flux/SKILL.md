---
name: flux
description: Work with a flux media server (a self-hosted film, TV and home-video library). Use when the user asks about their flux library, wants it organised, fixed, filled in, searched or browsed differently, or mentions flux. Covers connecting (the flux CLI or MCP), how flux's library and changesets work, and the rules every edit follows. The flux-metadata, flux-artwork, flux-organize and flux-lens skills build on this one.
---

# flux

flux is a self-hosted media server. It scans folders of films, shows and home videos, names them from their files, and serves a web app. It has no AI of its own: **you** are how a library gets identified, described, illustrated and reshaped. Everything you change is a *changeset* the user can see in the app (Changes) and undo.

## Connecting

Use whichever is available:

- **The `flux` command line** (`flux --help`). It finds the server on this machine by itself; elsewhere pass `--server https://host:port` or set `FLUX_SERVER`. Output is JSON. Run `flux status` first.
- **MCP**: the server's `/mcp` endpoint (tools `status`, `items_find`, `item_get`, `items_edit`, …), or `flux mcp` over stdio.
- **HTTP**: `POST /api/v1/ops/{name}` with a JSON body and the headers `X-Flux-Request: 1` and `X-Flux-Agent: <your name>`. `GET /api/v1/agent` lists every operation with its JSON Schemas.

Your role comes from where you connect: on the server's own machine you act as its Owner; over the tailnet, as the user's Tailscale identity. `flux status` says what you may do (`you.may`).

## How the library works

- **Items**: films (`movie`), shows → seasons → episodes, extras, and videos (a `videos` library: one item per file, home videos and clips, never matched). Ids are integers.
- **Every field says where its value came from** (`item.get` → `sources`): the file (its name, folder, container tags), a provider (TMDB), a person, or an agent (with its links). What a person or an agent set is locked: flux's scanner and matcher leave it alone.
- **Gaps** are what an item lacks: `identity` (nobody confirmed what it is), `year`, `overview`, `poster`, `backdrop`, `episode_titles`, `offline`. `flux gaps` counts them; `flux find --gap poster` lists them.
- **Withheld**: TMDB's terms forbid using its content with AI, so flux never shows you TMDB's text or images. `item.get` lists such fields under `withheld` (`{"overview": "tmdb"}`): they have a value you can't see. Don't try to get it another way; write your own from the open web if the user wants it replaced. Text flux read from `.nfo` files is withheld the same way (`"nfo"`): scrapers usually copy it from TMDB.
- **Files**: `item.get` lists an item's files, each with its `id`, its `path` inside its library folder (`root`), what flux read from the name (`parsed`), the metadata files beside it (`sidecars`) and its `fix` if it has one. A fix (`flux fix`) says what a file is when its name is wrong; a plan (`flux plan`) renames and moves files once the user applies it (flux-organize).
- **Collections**: titles picked by hand or taken by a rule, shown in the app beside the libraries (flux-organize).
- **Changesets**: each `flux edit` (one or many items) is one changeset with your summary. The user reviews them in the app; `flux changes undo ID` reverts one field by field, keeping fields changed since.

## Commands

```sh
flux status                       # the server, its libraries, your role, the rules
flux gaps                         # what's missing, per library
flux find --gap identity --limit 20
flux find -q "ghibli"             # text in titles and file paths
flux find --parent 42 --kind episode --gap episode_titles
flux get 42                       # one item: fields, sources, files' evidence, art, children
flux edit 42 title="Spirited Away" year=2001 directors="Hayao Miyazaki" \
  --source https://en.wikipedia.org/wiki/Spirited_Away --summary "Identified from the Japanese file name"
flux edit 42 --clear tags          # hand a field back to flux
flux edit --file edits.json       # many items at once (an items.edit input)
flux art view 1234 -o poster.jpg  # look at an image
flux art add 42 poster https://example.org/p.jpg --credit "Studio Ghibli" --from https://example.org/press
flux changes                      # newest first; flux changes show 17; flux changes undo 17
flux lens check board.json && flux lens add board.json
flux collections put Ghibli --pick 40 --pick 41        # see flux-organize
flux fix 812 season=2 episodes=5                       # what a file is
flux plan 40 --summary "Name its files the standard way"  # a proposal; nothing moves until applied
flux ops                          # every operation and its schemas
flux call items.find '{"gap": "year", "limit": 5}'
```

Values: text as is; lists comma-separated (`genres="Animation, Fantasy"`); JSON with `:=` (`cast:='[{"name": "Rumi Hiiragi", "role": "Chihiro"}]'`); ids as `ids.tmdb=129`, `ids.wikidata=Q155653`.

## Rules

1. **Cite.** Every value you set needs the pages that support it (`--source`). Write descriptions in your own words; never paste a provider's text.
2. **Start from the files.** `flux get` shows each file's path, size, modified time, duration, tracks and their languages, the title stored inside the file, chapter names, subtitle files and what flux read from the name. They usually say what a file is; a duration within a few minutes of a film's runtime is strong evidence.
3. **Don't overrule people.** A field a person set (`sources.<field>.source = "user"`) is refused unless you pass `--force`, which you do only when they asked for that change.
4. **Small, clear changesets.** One topic per changeset (for example "Years for 12 films"), a summary a person understands, at most 100 items. Use `--dry-run` to see what would change.
5. **Propose when unsure** (`--propose`): the user applies or rejects it in the app.
6. **Honest images.** Credit what you add; make an image with AI only when the user asks, and name the model (see flux-artwork).
7. **Files move only by plan.** Never rename, move or delete media yourself; `flux plan` proposes renames and moves, the user approves, flux carries them out (and can put them back). flux never deletes media.
8. **Private libraries stay private.** Home videos are personal: describe them only as much as the user asks, and never send their details to a web service without asking.

## When something fails

Errors are JSON with a `code`: `invalid` (fix the input, the message says how), `not_found`, `forbidden` (your role: see `flux status`), `conflict`, `unavailable` (try later), `too_large`. Exit codes: 1 error, 2 usage, 3 not found, 4 conflict, 5 forbidden, 6 server unreachable.
