/**
 * "Make a lens": flux has no AI of its own, so it writes the brief for yours.
 * You describe a lens; flux copies a prompt with the kit's rules, a complete
 * example and a sample of your library; your agent answers with a lens as JSON;
 * you paste it back (or choose the file) and flux previews and keeps it.
 */
import type { ItemSummary } from '../../api/types';
import { TONIGHT } from './examples';

function exampleJSON(): string {
  const { name, blurb, input, step, css, item, layout } = TONIGHT;
  return JSON.stringify({ name, blurb, input, step, window: 40, animated: false, css: css.trim(), scene: '', item, layout }, null, 2);
}

function kindWord(it: ItemSummary): string {
  return it.kind === 'show' ? 'show' : it.kind === 'movie' ? 'film' : it.kind;
}

export function makePrompt(description: string, items: readonly ItemSummary[]): string {
  const sample = items
    .slice(0, 14)
    .map((i) => `${i.title}${i.year ? ` (${i.year}, ${kindWord(i)})` : ` (${kindWord(i)})`}`)
    .join('; ');
  const wish = description.trim().replace(/"/g, '”') || 'Surprise me: a lens that suits a personal film library.';
  return `You design "lenses" for flux, a self-hosted home media server. A lens is one way of seeing the user's library: the same list of films and shows, presented differently (a shelf, a wall, a carousel). flux's own controls float above the lens as glass, so a lens draws only the titles and an optional backdrop.

Write a new lens from the user's description, as one JSON object in the lens kit format below. Reply with only the JSON.

## Lens kit format
{
  "name": "Short name, one or two words",
  "blurb": "One plain sentence: what it looks like and how you move through it.",
  "input": "x" or "y" or "none",   (which scroll axis moves through the titles; arrow keys always step)
  "step": 160,                      (pixels of drag or scroll per title)
  "window": 40,                     (titles either side of the position that get drawn; 8 to 150)
  "animated": false,                (true only if layout uses view.t to move on its own)
  "css": "...",                     (the look; every selector starts with .lens)
  "scene": "...",                   (optional static HTML behind the titles, decoration only)
  "item": "...",                    (HTML template for one title)
  "layout": "function layout(items, view) { ... }"
}

### layout(items, view)
Runs in a Web Worker whenever something moved. No DOM, no network, no storage, no import: fetch, XMLHttpRequest and the like are not there. Available: Math, clamp(v, a, b), lerp(a, b, t), rand(i, salt) (a stable pseudo-random number in [0, 1)).
- Libraries hold up to tens of thousands of titles, so items is only the window of titles near the position: [{ i, id, title, year, decade, kind ('movie', 'show', …), seasons, badges, progress (0 to 1), watched, isNew, available, hue (0 to 360), sat (0 to 1), lum (0 to 1) }]. i is the title's index in the whole library, in the order the user chose; year can be null.
- view: { w, h, pos, n, first, focus, t, safe: { top, bottom }, phone, theme ('dark' or 'light'), reducedMotion }. pos is a float index into the whole library (0 = first title); the title with i === Math.round(pos) is focused. n is the number of titles; first is items[0].i. Scrolling and arrow keys move pos.
- Return one object per item, same order: { x, y, z, rx, ry, rz, s, o, zIndex, cls }. x, y, z are pixels from the centre of the screen (y down, z towards the viewer); rx, ry, rz are degrees; s is scale; o is opacity (0 hides it); cls adds classes. All optional.
- Place titles by their distance from the position (it.i - view.pos), so the focused title sits near the centre, fully visible, between view.safe.top and view.h - view.safe.bottom. Give titles that shouldn't show o: 0. It must work for 1 title and for 20,000.

### item template
- Plain HTML only: no scripts, no event handlers, no images, no links, no style attributes (put all styling in css). Placeholders: {{title}} {{year}} {{meta}} (year, seasons and quality, joined) {{kind}} {{decade}} {{seasons}} {{badges}} {{index}} {{number}} {{show}}.
- Put <div class="art"></div> where the poster goes (2:3 artwork; size it in css). Leave it out only for a purely typographic lens.
- flux adds the class is-focus to the focused title. Each item has CSS variables --c1, --c2, --c3 (colours sampled from its poster), --i (index), --n (count), --p (watch progress 0 to 1) and --hue.

### css
- Every selector starts with .lens (the lens root). Useful: .lens .item, .lens .item.is-focus, .lens .kit-scene (the backdrop layer), .lens .kit-stage (has perspective: 1200px).
- Fonts: var(--font-body) (Jost), var(--font-mono) (DM Mono), var(--font-cond) (Archivo; use font-stretch 62% to 75% for condensed).
- Theme colours: var(--bg), var(--text), var(--text-2), var(--text-3), var(--line). It must work in dark and light themes: use these for text unless the text sits on a surface you painted yourself.
- No url(), @import or @font-face. Gradients, shadows, transforms and @keyframes are fine.
- The posters are the hero. Keep decoration quiet and specific to the idea; no buttons or menus.

## Example of a complete lens
${exampleJSON()}

## The library
${items.length.toLocaleString('en')} titles, for example: ${sample || 'none loaded yet'}.

## The user's description
"${wish}"`;
}

/**
 * Read a lens out of an agent's answer: the whole text as JSON, else a fenced
 * code block, else the text from the first "{" to the last "}".
 */
export function parseLensReply(text: string): unknown {
  const t = text.trim();
  if (!t) throw new Error('Paste the lens your agent wrote (a JSON object), or choose its file.');
  const attempts: string[] = [t];
  const fence = /```(?:json)?\s*([\s\S]*?)```/i.exec(t);
  if (fence) attempts.push(fence[1]);
  const a = t.indexOf('{');
  const b = t.lastIndexOf('}');
  if (a >= 0 && b > a) attempts.push(t.slice(a, b + 1));
  for (const s of attempts) {
    try {
      return JSON.parse(s) as unknown;
    } catch {
      // try the next reading
    }
  }
  throw new Error('That isn’t a lens: no JSON object could be read from it. Ask your agent for only the JSON.');
}
