# Lenses

The web client has no home page of rows and no sidebar. Your library fills the window, and a **lens** decides how it looks and moves: a zoomable wall, a Cover Flow, a shelf of tapes, a gallery you walk through. The controls float over it as glass:

- **The island** (top centre) is where you are, what you are looking for, and what is live. Click it or start typing and it grows into search (the library filters as you type, and server results list below), the space switcher, filters, sort and Continue watching. Server events appear in it and settle back: a scan in progress (a ring), new titles, a drive that stops answering, remote access changing. A film you leave halfway stays in it as a ring you can tap to resume.
- **The lens bar** (bottom centre) holds your pinned lenses and one control the current lens lends it (zoom for Wall, a scrubber for Flow, Tour for Gallery). It shrinks to one button while you scroll down.
- **The corner button** is who you are (your Tailscale identity), the theme, and the server's own screens: Libraries & scanning, Remote access.

Item pages, Libraries & scanning and Remote access open over the library, which stays mounted underneath (and inert, so Tab stays in what is open); the poster you picked flies into the item page and back, and keyboard focus goes with it. The player is full screen.

## Spaces, filters and sort

A **space** is what the lens shows: `All` (`/`, every film and show library merged in the server's order) or one library (`/library/:id`). A videos library opens by folder. The space's filter (`show=unwatched|progress|new|4k|hdr`), sort (`sort=title|-year|year|-added|colour`) and search text (`q=`) live in the query string, so Back, Forward and shared links keep them. The shell loads, searches, filters and sorts once (`src/lens/sections.ts`, `src/api/space-list.ts`); every lens receives the result.

## The contract

A lens is a plain DOM module (`src/lens/types.ts`):

```ts
const myLens: LensModule = {
  mount(root: HTMLElement, ctx: LensContext): LensView { … },
};
```

`ctx` gives the lens `items` (the API's `ItemSummary`, already filtered and sorted), `total` (more than `items.length` while a library is still loading), the shared `focus`, the `safe` insets where the glass sits, `env` (`mini`, `reducedMotion`, `theme`, `phone`), and the calls it can make: `setFocus`, `open(id, art)`, `play`, `setAccessory`, `scrolled`, `sectionOf(item)` and `intent(item)`.

The `LensView` it returns answers `update(items, total)` (search typing calls it on every keystroke; pages of a big library call it as they land), `move(dir)` (arrows and TV remotes), `key(e)` (Enter opens; lens-specific keys), `rectOf(id)` and `artOf(id)` (for the flight into the item page and back), `hide(id)` (a lens that draws on a canvas hides the title whose poster is flying), `reveal(id)`, `pause(on)` and `destroy()`.

Every lens follows the same rules:

1. Draw content only: no menus, buttons or fields of its own, and one accessory at most.
2. Keep the focused title visible and report focus with `ctx.setFocus` (the room light follows it).
3. Move with arrows and open with Enter; Escape belongs to the shell.
4. Keep key content clear of `ctx.safe`; art may run under the glass.
5. Honour `env.reducedMotion` and `env.mini` (a small live preview in the lens gallery).
6. Build artwork with `src/lens/art.ts`, so progress, watched, new and offline marks look the same everywhere and frame or AI art is always labelled.
7. Stay fast at 10,000 titles: render only what is on screen or near the focus.

## First-party lenses

| Lens | What it is |
| --- | --- |
| Wall (`src/lens/wall`) | Everything at once, like a photo library: four densities from a mosaic of decades to posters with captions, anchored where you point. Drawn with WebGPU (below): a pinch follows your fingers live and settles on the nearest density, and a zoom morphs every tile on screen. Held arrow keys scroll with the focus. |
| Flow (`src/lens/flow`) | Cover Flow: one poster faces you, the rest wait in two stacks. Drawn with WebGPU. |
| Shelf (`src/lens/shelf`) | VHS tapes on shelves, spines painted from each poster's colours; point at one to pull it out. Drawn with WebGPU. |
| Gallery (`src/lens/gallery`) | An exhibition you walk through: framed posters, wall labels, a room per section. |

To add one: write the module, then add an entry (id, name, icon, blurb, `load`) to `FIRST_PARTY` in `src/lens/registry.ts`. The module loads on demand, in its own chunk.

## Drawing on the GPU

With thousands of posters, the DOM is the bottleneck: every tile is a little tree of elements to create, style, lay out, paint and composite, and a zoom or a flick animates hundreds of them. So the Wall, Flow and the Shelf draw with WebGPU, on a small engine in `src/lens/gpu`:

- **One draw call a frame.** Every poster, shadow, mark, label and caption is an instance of one rounded-rectangle shader (signed-distance edges, antialiased to the device pixel). The Wall's quads are flat and live in content coordinates, so scrolling changes one uniform (`quads.ts`).
- **Perspective without the DOM.** Flow's covers and the Shelf's tapes are the same quads placed in 3D (`scene3d.ts`) with CSS's own transform maths (translate, then rotate about an origin, seen through the CSS perspective camera), so each GPU lens lines up with its DOM version to the pixel. Reflections are mirrored copies, posters are sampled anisotropically so a face seen at a steep angle stays sharp, and edges antialias at the rate the face is seen. There is no depth buffer: quads are drawn back to front.
- **Native scrolling.** The Wall keeps a transparent scroller over the canvas: momentum, scroll chaining and keyboard scrolling are the browser's own, and the canvas redraws at the scroll position each frame. Flow and the Shelf move with their own springs and momentum, as their DOM versions do.
- **Posters as textures** (`posters.ts`, one cache shared by every GPU lens). Four size classes (128 to 640 pixels wide, mipmapped texture arrays) keep each size sharp without wasting memory; a class grows with demand and is freed after 20 seconds unused. Images are fetched nearest-first and decoded off the main thread (`createImageBitmap`), and the GPU scales each one to its slot: in Chrome, resizing on the CPU would run on the main thread. Uploads take at most 3 ms a frame. Until a poster arrives its average colour shows, and it fades in once.
- **Painted sprites** (`sprites.ts`). Artwork a lens paints itself, like the Shelf's spines (six treatments, condensed titles, stickers, the progress stripe), is drawn once with Canvas 2D into the layers of a texture array, a few new ones a frame, and kept while it is on screen.
- **Text** (section labels, captions, the frame/AI labels and other marks) is drawn once per string with Canvas 2D in the page's own fonts, kept in a glyph atlas, and drawn on whole device pixels.
- **Motion on the GPU.** A zoom morphs every tile from where it was to where it goes (with the app's `SOFT` curve); hover lifts use the CSS spring; a search reflow slides titles and shrinks away the ones that left.
- **Still a page.** What is on screen is mirrored as invisible elements, synced when motion rests rather than every frame: the Wall's titles as links (and its sections as row headers), Flow's covers as a listbox, the Shelf's tapes as buttons. Screen readers browse them, middle-click and "open in new tab" work on the Wall, and the e2e tests find titles as before. One more element carries keyboard focus to the focused title.

Each GPU lens keeps its DOM version (`wall.ts`, `flow.ts`, `shelf.ts`; the lens's `load.ts` picks one). It is used in browsers without WebGPU (and TV browsers), if the pipeline doesn't compile, and if the GPU is lost mid-session (the view carries on as the DOM version). Set `flux.render` to `"dom"` in localStorage to use it anyway.

Browsers offer WebGPU only to secure pages: HTTPS, or `localhost` on the machine itself. Opened over plain HTTP from another device (a tailnet or LAN address), flux uses the DOM lenses. With HTTPS on the tailnet switched on ([README, HTTPS](../README.md#https)), tailnet devices get the GPU lenses, and a page opened over plain HTTP there moves to the HTTPS address by itself.

Measured in headless Chrome on an M1 Max at 2× (`vite preview` of the mock build, the 10,000-title library), same build, DOM vs GPU:

| | DOM | GPU |
| --- | --- | --- |
| Wall: zoom in and out (6 steps) | 23 dropped frames, worst 117 ms, 8.8 ms of main thread a frame | 0 dropped, worst 16.8 ms, 3.6 ms a frame |
| Wall: overview (the densest), slow scroll | 5 dropped, worst 83 ms, 7.5 ms a frame | 0 dropped, 5.8 ms a frame (mostly decoding the mock's SVG posters; real art decodes off the main thread) |
| Wall: grid, fast scroll | 3 dropped, worst 50 ms, 4.4 ms a frame | 0 dropped, 1.8 ms a frame |
| Wall: posters, scroll | 0 dropped, 1.4 ms a frame | 0 dropped, 0.7 ms a frame |
| Wall: `pnpm bench:grid` | worst frame 33.3 ms, 467 DOM nodes | worst frame 16.8 ms, 0 janky frames, 193 DOM nodes |
| Flow: wheel through the stacks | 0 dropped, 2.8–4.6 ms a frame | 0 dropped, 1.8–2.6 ms a frame |
| Flow: a steady flick, compositor | 10 ms a frame (two dozen 3D layers with reflections) | 1.1 ms a frame |
| Shelf: scroll | 0 dropped, 3.1–3.7 ms a frame, 2,420 DOM nodes | 0 dropped, 0.9–1.2 ms a frame, 218 DOM nodes |
| Shelf: search reflow | 2–3 dropped, worst 83 ms, 6.6–7.2 ms a frame | 0 dropped, 2.9 ms a frame |
| Shelf: open a tape | 0–2 dropped, 1.8–2.1 ms a frame | 0 dropped, 1.1–1.4 ms a frame |

Headless Chrome runs at 60 Hz; on a 120 Hz display the budget is 8.3 ms a frame, which every GPU lens stays inside. Flow shows why the main thread isn't the whole story: a flick costs it about 2 ms either way, but the DOM Flow also gives the compositor 10 ms of work a frame, more than a 120 Hz frame has.

## The lens kit

A kit lens is data, a JSON object any agent can write (`src/lens/kit`, `KitSpec` in `runtime.ts`):

| Field | What it is |
| --- | --- |
| `name`, `blurb` | What the lens gallery shows. |
| `input`, `step` | Which scroll axis moves through the titles (`x`, `y` or `none`) and how many pixels make one title; arrow keys always step. |
| `window` | Titles kept either side of the position (default 40). Only these get DOM and reach the layout code, so a kit lens stays fast at 10,000 titles. |
| `item` | HTML for one title, with fields like `{{title}}` and `{{year}}`, and `<div class="art"></div>` where the poster goes. |
| `scene` | Static HTML behind the titles. |
| `css` | The look. Every selector starts with `.lens`; it is scoped to the lens and its keyframes are renamed. |
| `layout` | Source of `function layout(items, view)`, which returns a position, rotation, scale, opacity and class for each title. `view` has the size, time, position, focus, safe insets, theme and the `mini` and `reducedMotion` flags. |
| `animated` | Ask for a frame every tick instead of only when something moved. |

Five ship as examples: Tonight, Helix, Timeline, Film strip and Corkboard. You can add your own from the lens gallery (the last button in the lens bar, or `\`). Describe the lens, copy the prompt for your agent, then paste back the JSON it writes, or open a lens file. Preview it, then keep it. Lenses you add are kept on this device, 24 at most.

A lens can come from anyone, so its code gets no page, no network and no account:

- The layout runs in a Web Worker whose script first removes `fetch`, `XMLHttpRequest`, `WebSocket`, `EventSource`, `importScripts`, nested workers, storage, `navigator` and `location`. Lens code that uses `import` is refused. A blob: worker shares the page's origin, so without this it could call flux's API as you.
- flux's Content-Security-Policy applies inside the worker too: no other origins, and no `eval`.
- The page takes from the worker only the frame it asked for, sized to the window, with every number checked, so a lens can't hang or break it.
- The HTML is sanitised: no scripts, links, images, forms, event handlers or inline styles; only markup, classes and simple SVG shapes.
- The CSS loses `@import`, `@font-face` and `url()`, and can't reach outside the lens.

The dev server sends no CSP, so there the worker lockdown is the only guard.

## Keyboard

Type anywhere to search · `/` or `⌘K` opens the island · arrows move in the lens · Enter opens · `[` `]` switch lenses · `\` shows all lenses · `+` `−` zoom the Wall · Esc steps back (closes the item page, the island, then clears the search).
