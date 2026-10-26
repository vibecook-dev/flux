# The lens kit format

The same format the flux web app's "Make a lens" prompt describes (`web/src/lens/kit/prompt.ts`) and the server checks (`flux lens check`; schema: `flux schema lens`).

```json
{
  "name": "Short name, one or two words (at most 28 characters)",
  "blurb": "One plain sentence: what it looks like and how you move through it.",
  "input": "x",
  "step": 160,
  "window": 40,
  "animated": false,
  "css": "...",
  "scene": "...",
  "item": "...",
  "layout": "function layout(items, view) { ... }"
}
```

- `input`: which scroll axis moves through the titles, `x`, `y` or `none`; arrow keys always step.
- `step`: pixels of drag or scroll per title (30 to 600).
- `window`: titles either side of the position that get drawn (8 to 150).
- `animated`: `true` only if `layout` uses `view.t` to move things by itself.

## layout(items, view)

Runs in a Web Worker whenever something moved. No DOM, no network, no storage, no `import`: `fetch`, `XMLHttpRequest` and the like are not there. Available: `Math`, `clamp(v, a, b)`, `lerp(a, b, t)`, `rand(i, salt)` (a stable pseudo-random number in [0, 1)).

- Libraries hold up to tens of thousands of titles, so `items` is only the window of titles near the position: `[{ i, id, title, year, decade, kind ('movie', 'show', …), seasons, badges, progress (0 to 1), watched, isNew, available, hue (0 to 360), sat (0 to 1), lum (0 to 1) }]`. `i` is the title's index in the whole library, in the order the user chose; `year` can be null.
- `view`: `{ w, h, pos, n, first, focus, t, safe: { top, bottom }, phone, theme ('dark' or 'light'), reducedMotion }`. `pos` is a float index into the whole library (0 = first title); the title with `i === Math.round(pos)` is focused. `n` is the number of titles; `first` is `items[0].i`. Scrolling and arrow keys move `pos`.
- Return one object per item, same order: `{ x, y, z, rx, ry, rz, s, o, zIndex, cls }`. `x`, `y`, `z` are pixels from the centre of the screen (y down, z towards the viewer); `rx`, `ry`, `rz` are degrees; `s` is scale; `o` is opacity (0 hides it); `cls` adds classes. All optional.
- Place titles by their distance from the position (`it.i - view.pos`), so the focused title sits near the centre, fully visible, between `view.safe.top` and `view.h - view.safe.bottom`. Give titles that shouldn't show `o: 0`. It must work for 1 title and for 20,000.

## item template

- Plain HTML only: no scripts, no event handlers, no images, no links, no style attributes (put all styling in `css`). Placeholders: `{{title}}` `{{year}}` `{{meta}}` (year, seasons and quality, joined) `{{kind}}` `{{decade}}` `{{seasons}}` `{{badges}}` `{{index}}` `{{number}}` `{{show}}`.
- Put `<div class="art"></div>` where the poster goes (2:3 artwork; size it in `css`). Leave it out only for a purely typographic lens.
- flux adds the class `is-focus` to the focused title. Each item has CSS variables `--c1`, `--c2`, `--c3` (colours sampled from its poster), `--i` (index), `--n` (count), `--p` (watch progress 0 to 1) and `--hue`.

## css

- Every selector starts with `.lens` (the lens root). Useful: `.lens .item`, `.lens .item.is-focus`, `.lens .kit-scene` (the backdrop layer), `.lens .kit-stage` (has `perspective: 1200px`).
- Fonts: `var(--font-body)` (Jost), `var(--font-mono)` (DM Mono), `var(--font-cond)` (Archivo; use `font-stretch` 62% to 75% for condensed).
- Theme colours: `var(--bg)`, `var(--text)`, `var(--text-2)`, `var(--text-3)`, `var(--line)`. It must work in dark and light themes: use these for text unless the text sits on a surface you painted yourself.
- No `url()`, `@import` or `@font-face`. Gradients, shadows, transforms and `@keyframes` are fine.
- The posters are the hero. Keep decoration quiet and specific to the idea; no buttons or menus.

## A complete lens

```json
{
  "name": "Tonight",
  "blurb": "One title at a time, the rest waiting behind it. For nights you can't decide.",
  "input": "x",
  "step": 240,
  "window": 40,
  "animated": false,
  "css": ".lens .item { width: min(36vh, 290px); }\n.lens .item .art { width: 100%; border-radius: 16px; box-shadow: 0 40px 80px -30px rgb(0 0 0 / .75), 0 0 0 .5px rgb(255 255 255 / .08); }\n.lens .cap { position: absolute; left: -40%; right: -40%; top: calc(100% + 22px); text-align: center; opacity: 0; transition: opacity .35s; }\n.lens .item.is-focus .cap { opacity: 1; }\n.lens .cap b { display: block; font-weight: 400; font-size: 1.375rem; letter-spacing: -.01em; color: var(--text); }\n.lens .cap span { display: block; margin-top: 4px; font-family: var(--font-mono); font-size: .75rem; color: var(--text-3); }",
  "scene": "",
  "item": "<div class=\"art\"></div><div class=\"cap\"><b>{{title}}</b><span>{{meta}}</span></div>",
  "layout": "function layout(items, view) {\n  const lift = -view.h * 0.05;\n  return items.map((it) => {\n    const d = it.i - view.pos;\n    if (d < -1.3 || d > 6) return { o: 0 };\n    if (d < 0) return { x: d * view.w * 0.55, y: lift - d * 30, z: 20, rz: d * 12, o: 1 + d * 0.85 };\n    return { x: d * 24, y: lift - d * 14, z: -d * 70, s: 1 - d * 0.035, rz: d * 1.5, o: 1 - d * 0.13 };\n  });\n}"
}
```

More examples ship with flux (Helix, Timeline, Film strip, Corkboard): open the lens gallery in the app and save one as a file to read it.
