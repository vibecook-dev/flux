---
name: flux-lens
description: Design a new "lens" for the flux web app (a way of browsing the library: a shelf, a wall, a carousel, a timeline, a departure board…), check it and add it to the user's flux so it appears on all their devices. Use when the user wants flux to look or browse differently, or asks for a custom view. Needs the flux skill for connecting.
---

# Designing a flux lens

A lens is one way of seeing the library: the same list of films and shows, presented differently. flux's own controls float above it as glass, so a lens draws only the titles (and an optional backdrop). A lens is **data**: one JSON object with HTML, CSS and a `layout` function. flux sanitises it and runs `layout` in a locked-down worker, so it can't reach the network or the page.

The complete format, the fields `layout` receives, and an example are in [reference.md](reference.md). The JSON Schema is `flux schema lens`.

## Workflow

1. **Understand the idea.** Ask what it should feel like and how one moves through it (scroll down, scroll across, arrow keys only). Keep the posters the hero.
2. **Write the lens** as a JSON file, following reference.md. Keep `layout` fast: it gets only the titles near the position (`window`), and must work for 1 title and for 20,000.
3. **Check it:**
   ```sh
   flux lens check board.json
   ```
   Fix every error. Read the warnings: they list what flux will remove (images, links, inline styles, `url()`, `@import`, rules for `body`) and placeholders flux doesn't fill.
4. **Add it:**
   ```sh
   flux lens add board.json              # for the user; --everyone (Owner/Admin) for every person
   ```
   It appears in the lens gallery (the last button in the lens bar, or `\`) on every device at once. Adding a lens with the same name replaces it, so iterate by editing the file and adding it again.
5. **Look at it**, if you can drive a browser: open the flux web app, press `\`, pick the lens. Otherwise ask the user how it looks and what to change.
6. `flux lens list` shows what's kept; `flux lens remove ID` takes one away.

## Taste

- Posters first; decoration quiet and specific to the idea. No buttons, menus or fields: flux's glass controls do that.
- It must work in dark and light themes: use `var(--text)`, `var(--text-2)`, `var(--bg)` for text unless it sits on a surface you painted.
- Fonts: `var(--font-body)` (Jost), `var(--font-mono)` (DM Mono), `var(--font-cond)` (Archivo; `font-stretch: 62%` to `75%` for condensed).
- Respect the focused title (`is-focus` class): keep it near the centre, fully visible, clear of `view.safe.top` and `view.safe.bottom`.
- Honour `view.reducedMotion` (no idle motion) and keep `animated` false unless the layout really moves by itself.
