---
name: flux-organize
description: Organise a flux library. Fix what files are (an episode numbered wrong, two items that are one film, one item that is two, a trailer taken for a film), rename and move files and folders the way media servers read them (as a plan the user applies, with undo), and keep collections ("Ghibli", "Rainy Sunday", "Kids' films"). Use when the user asks to tidy, rename, reorganise, merge, split, group or curate their flux library. Needs the flux skill.
---

# Organising a flux library

Three tools, gentlest first. Read the `flux` skill first for connecting and the rules.

| The user wants | Use | Changes files on disk? |
| --- | --- | --- |
| Titles grouped for browsing ("all the Ghibli films") | a **collection** | no |
| flux to understand a file differently (wrong episode, a trailer, two films merged) | a **fix** | no |
| Tidy folders and file names | a **file plan** | yes, once the user applies it |

## Collections

A collection is titles picked by hand (in order, each with a note) and/or a rule that takes matching titles as the library changes. The app shows collections as spaces beside the libraries, in every lens.

```sh
flux collections                                          # everyone's and yours
flux collections put Ghibli --pick 40 --pick "41:Where it began" \
  --description "Studio Ghibli's features" --source https://en.wikipedia.org/wiki/Studio_Ghibli
flux collections put "Rainy Sunday" --rule '{"genres": ["Comedy", "Romance"], "year_to": 1999}'
flux collections put "Kurosawa" --rule '{"people": ["Akira Kurosawa"]}' --everyone   # Owner/Admin
flux collections show 3
flux collections remove 3
```

- A rule matches every condition given, and any value within a list: `genres`, `tags`, `people` (directors, writers, cast), `studios`, `countries` (ISO codes), `languages` (ISO codes), `year_from`, `year_to`, `text` (in the title). It reads the fields flux has, so fill those in first (flux-metadata) for rules to work.
- `--pick` adds after the titles already picked; `--unpick` takes one out. Over MCP or HTTP, `collection.put` takes `items` (the whole picked list, in order), `add`, `remove` and `rule`.
- Yours unless `--everyone` (Owner or Admin). A collection of yours with the same name is changed, not duplicated.
- Collections hold films and shows (not seasons or episodes).

## Fixing what files are

flux names each file from its path. When it read one wrong, a **fix** says what the file is; flux places the file by it on every scan, until the fix is cleared. Fixes are changesets (undo in the app).

Read first: `flux get ID` lists each file with its `id`, `path`, `parsed` (what flux read) and `fix`.

```sh
flux merge 40 41 --summary "Two copies of Heat: 4K and 1080p"          # 41's files join 40
flux fix 812 season=2 episodes=5 --summary "S01E17 is really S02E05 (broadcast order)"
flux fix 813 kind=extra extra=trailer of=40 --summary "The trailer, not a film"
flux fix 814 year=1982 --summary "The Thing (1982), not the 2011 prequel"   # with 815 year=2011: one item becomes two
flux fix 816 kind=ignore --summary "A sample clip"
flux fix 816 --clear                                                    # its name decides again
flux fix 812 season=2 episodes=5 --dry-run                               # where it would go
```

- A fix says only what's wrong: `kind` (movie, episode, extra, ignore), `of` (the film or show it belongs to), `title` and `year` (what to group it under), `season` and `episodes`, `extra` (trailer, featurette, behind_the_scenes, deleted_scene, interview, short, scene, other), `edition`, `part`.
- **A fix is about a file; `flux edit` is about an item.** To say a film is "Spirited Away (2001)", edit the item. To say a file is the second version of item 40, fix the file.
- `flux merge INTO FROM…` fixes every file of each FROM to belong to INTO (episodes keep their numbers). What people set on the merged-away item, everyone's watched state and its collections come along; undo gives it all back.
- Many files at once: `flux call files.fix '{"summary": "…", "fixes": [{"file": 812, "fix": {"season": 2, "episodes": [5]}}, …]}'` (at most 200).

## Renaming and moving files

flux can rename and move files and folders, as a **plan**: a proposed changeset of steps that changes nothing until a person applies it. Then flux runs the steps, never replacing a file, never moving one to another drive, out of its library folder or through a link; and the catalog follows, so items keep their ids, watched state and everything set on them. `flux changes undo` puts every file back.

```sh
flux plan 40 41 --summary "Name the Ghibli films' files the standard way"
flux plan 52 --ids --summary "…"          # with ids in folder names: Interstellar (2014) [imdbid-tt0816692]
flux plan --move "812=Kids/Spirited Away (2001)/Spirited Away (2001).mkv" --summary "Into Kids"
flux changes show 23                      # every step; show it to the user
flux changes apply 23                     # Owner/Admin, when the user says yes
flux changes undo 23                      # everything back
```

The standard scheme is what Plex, Jellyfin, Emby and Kodi all read:

```
Films:   Title (Year)/Title (Year).mkv            versions: " - Director's Cut", " - 1080p"; parts: " - part2"
Shows:   Show (Year)/Season 01/Show (Year) - S01E02 - Episode Title.mkv     specials in Specials/
Extras:  Title (Year)/Trailers/…, Featurettes/, Behind The Scenes/, Deleted Scenes/, Interviews/, Shorts/, Other/
```

- It names by what flux knows now: **fill in titles and years first** (flux-metadata), then plan. An item without a year gets `Title/Title.mkv`.
- Files named like a video go with it: subtitles (`Film.en.srt`), `Film.nfo`, `Film.flux.json`, `Film-poster.jpg`. A film's or show's own folder (holding no other title's files) is renamed in place, with everything in it.
- The plan lists what it leaves alone and why (`skipped`): a drive that doesn't answer, a name already taken, a title the agent may not see.
- **Always show the plan to the user and wait for their yes before `changes apply`.** Applying it is theirs to decide; a Viewer's agent can plan but not apply.
- Don't rename files yourself with shell commands: flux would see files disappear and new ones appear, and has to find them again. Plans keep everything attached.
- Videos libraries (home videos) keep the user's own names: plan moves there only when asked (`--move`).

## Sidecars: facts that travel with the files

A `.flux.json` beside a video (named like it), or `flux.json` in a film's or show's folder, is read on every scan. Use it when you can write to the library's folders but can't reach the flux server, or so the facts stay with the files if they move to another server:

```json
{
  "flux": "flux.sidecar/1",
  "fields": { "title": "Spirited Away", "original_title": "千と千尋の神隠し", "year": 2001, "directors": ["Hayao Miyazaki"] },
  "fix": { "year": 2001 },
  "sources": ["https://en.wikipedia.org/wiki/Spirited_Away"],
  "by": "claude-code"
}
```

A show's `flux.json` may also list `"episodes": [{"season": 1, "episode": 1, "fields": {"title": "…"}}]`. The schema is `flux schema sidecar`. What a sidecar says counts more than a provider's match and less than what a person or an agent set in flux. Kodi `.nfo` files are read too (people see what they say; agents get only their ids, as their text usually comes from TMDB).
