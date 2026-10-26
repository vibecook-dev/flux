# flux website

flux's product site: one static page with the real web client embedded as a
live demo. It is plain TypeScript and CSS on Vite, with no framework, so it
stays small and smooth: the page's own script is about 17 kB compressed,
poster wall included.

| Section | What it shows | Where |
| --- | --- | --- |
| Hero | A wall of posters drifting in the dark (WebGL2, one draw call), lit by the pointer | `src/hero/` |
| Lenses | The web client (`../web`) itself, in mock mode: its lenses take turns until you touch it, and the tabs below drive it | `src/lenses/lenses.ts` |
| Anywhere | Remote access the usual way vs. with Tailscale, as a map | `src/anywhere/net.ts` |
| Speed | The cold-scan race and the benchmark medians against Jellyfin | `src/speed/race.ts` |
| Agents | An agent's session in the terminal and its proposal in Agents & changes | `src/agents/session.ts` |
| Download | The newest release, the visitor's build first, and install commands | `src/download/release.ts` |

## Develop

```sh
cd site
pnpm install
pnpm demo        # build the web client as the demo, into .demo/ (needs ../web's dependencies)
pnpm dev         # http://localhost:5173, with the demo at /demo/
```

`pnpm demo` builds `../web` with `VITE_FLUX_API=mock` for `<base>demo/`: the
app the Lenses section shows (the page never calls it a demo). Run it again
after changing the web client. To work on the site against a running
web dev server instead, set `VITE_DEMO_URL` (for example
`VITE_DEMO_URL=http://localhost:5173/?mock_tailnet=running pnpm dev`; the lens
buttons then need the web client's bridge, which only mock builds have).

## Build and deploy

```sh
pnpm build       # typecheck, the site into dist/, then the app (web/, mock mode) into dist/demo/
pnpm preview     # serve dist/ as a host would
```

**GitHub Pages.** The site lives at <https://vibecook-dev.github.io/flux/>,
served from the `gh-pages` branch, which holds only the built site.
`.github/workflows/pages.yml` builds and publishes it on every push to `main`
that touches `site/` or `web/` (or by hand, from the Actions tab). To publish
from your machine instead:

```sh
SITE_BASE=/flux/ SITE_URL=https://vibecook-dev.github.io/flux/ pnpm build
node scripts/publish-pages.mjs
```

Elsewhere, `dist/` is a static site: put it on any host that serves files.
Two settings, as environment variables at build time:

- `SITE_BASE`: the path the site lives under, with slashes at both ends
  (default `/`). For a GitHub Pages project site: `SITE_BASE=/flux/`.
- `SITE_URL`: the site's public address, e.g. `https://flux.example.com/`. Link
  previews need it for their image (`og.jpg`); without it they get a relative
  address, which most previews ignore.

The demo is a single-page app with routes of its own (`/demo/item/42`). A
reload on one of those needs the host to answer with `demo/index.html`:
`public/_redirects` does that on Netlify and Cloudflare Pages, and on GitHub
Pages `404.html` sends the visitor back to the demo's start.

The demo's service worker and its GPU lenses need a secure page: HTTPS, or
`localhost` while developing.

## Content

- **Benchmarks** are the medians from
  [docs/prototype/benchmarks.md](../docs/prototype/benchmarks.md) (flux vs.
  Jellyfin 12.2, 2026-10-20) and the client's frame times from
  [docs/LENSES.md](../docs/LENSES.md). The page links the full report and
  repeats its main caveats. When the benchmarks are re-run, update the numbers
  in `index.html` (the race's are in `src/speed/race.ts`).
- **Release data** comes from `src/data/release.json`, a snapshot taken with
  `pnpm release` (GitHub's API; `GITHUB_TOKEN` is used when set), and the page
  refreshes it from GitHub's API in the browser.
- **Poster art** in the hero is painted on a canvas from invented titles
  (`src/hero/posters.ts`), with the same motifs and palettes as the web
  client's mock art. No real posters are used anywhere.
- **Tailscale's free plan**: "up to 6 users" is from tailscale.com/pricing
  (October 2026). Check it before quoting it again.
- `public/og.jpg` is the link-preview image, a 1200×630 render of the hero.
