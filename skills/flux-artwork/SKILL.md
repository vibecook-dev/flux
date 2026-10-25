---
name: flux-artwork
description: Find, check and add posters, backdrops, episode thumbnails and logos for items in a flux library, credited to where they came from; label AI-made art honestly. Use when the user wants better or missing artwork in flux (frame posters, blank backdrops, wrong images). Needs the flux skill.
---

# Artwork for a flux library

flux makes its own art when it has nothing better: a **frame poster** (a frame of the video with the title typeset on it, labelled "Frame" in the app) and frame backdrops. Your job is to find the real thing, add it with credit, and leave the user able to undo it. Read the `flux` skill first.

## Find what needs art

```sh
flux gaps                          # poster / backdrop counts per library
flux find --gap poster --limit 50  # films and shows showing only flux's own art, or none
flux get 42                        # `art`: each image's id, source, credit, which one is shown
flux art view 1234 -o current.jpg  # look at what it shows now
```

## Where to look

Prefer images whose source is clear:

1. The studio's, distributor's or broadcaster's press or official pages (press kits often have posters and stills).
2. Wikipedia infobox posters and Wikimedia Commons (note the licence on the file page).
3. Poster archives (IMP Awards, the Internet Archive for old and public-domain works), the film's festival page.
4. Fan art sites only if the user is happy with that; credit the artist.

Don't take images from TMDB, and never from a page that forbids reuse. Each image is for the user's own library; still, credit it.

## What makes a good image

- **Poster**: portrait 2:3, the film's own key art with its title, at least 600 × 900 (1000 × 1500 or more is better), not a DVD cover with a price sticker, not a fan mock-up. For a non-English film, the original poster or the release poster for the user's language, as they prefer.
- **Backdrop**: landscape 16:9, a clean still or key art without text, at least 1280 × 720.
- **Thumb** (an episode): a 16:9 still from that episode.
- **Logo**: the title treatment on a transparent background (PNG).

Look at a candidate before adding it (open it, or add it and `flux art view` it); if you can't see images, say so and ask the user to check in the app.

## Add it

```sh
flux art add 42 poster https://upload.wikimedia.org/…/Spirited_Away_poster.png \
  --credit "Studio Ghibli" --from https://en.wikipedia.org/wiki/Spirited_Away
flux art add 42 backdrop --file still.jpg --credit "Toho" --from https://example.org/press-kit
```

- `--credit` (who made it) and `--from` (the page you found it on) are required for found images.
- flux fetches the URL itself (http or https), re-encodes the image and shows it at once (`--no-pin` to only add it).
- Each addition is a changeset: `flux changes undo ID` removes the image and brings back the previous one.

To show a different image that's already there: `flux art pin ART_ID`; to let flux choose again: `flux art pin --item 42 --kind poster --auto`.

## Images you make

- **Made without AI** (a crop, a collage of frames from the video, a cleaned scan): `--made`, credit yourself or the source.
- **Made with AI**: only when the user explicitly asks. Pass `--generated MODEL` (for example `--generated gpt-image-2`). flux labels it "AI art" everywhere it shows, naming the model. Never present AI art as an official poster, and never imitate a real poster's design or a real person's likeness without the user's say-so.
