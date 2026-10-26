/**
 * An agent at work, on one timeline: the terminal types the request and runs
 * the real `flux` commands (CLI syntax as `flux --help` gives it), the
 * proposal arrives in the app's Agents & changes, a pointer applies it, and
 * the file's frame grab becomes a titled film. It loops while on screen.
 */
import { onVisibility, reducedMotion } from '../lib/motion';

const PROMPT = 'Identify the films flux couldn’t, from their files and the web. Cite your sources.';

type Line = { html: string; wait: number; busy?: number };

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;');
const call = (cmd: string) => `<span class="t-call"><span class="t-dot">●</span> ${cmd}</span>`;
const out = (text: string) => `<span class="t-out">  └ ${text}</span>`;
const arg = (s: string) => `<span class="t-arg">${esc(s)}</span>`;

const SCRIPT: Line[] = [
  { html: call('flux gaps'), wait: 500, busy: 700 },
  { html: out('Movies: 14 titles without an identity'), wait: 350 },
  { html: call('flux get 5'), wait: 600, busy: 600 },
  { html: out(`${esc('天空の城ラピュタ.mkv')} · 2 h 4 min · Japanese audio`), wait: 350 },
  { html: call('Fetch en.wikipedia.org/wiki/Castle_in_the_Sky'), wait: 650, busy: 900 },
  { html: out('Hayao Miyazaki · released 2 August 1986 · 124 minutes'), wait: 350 },
  {
    html: call(
      `flux edit 5 title=${arg('"Castle in the Sky"')} year=${arg('1986')} \\\n    directors=${arg('"Hayao Miyazaki"')} … --propose \\\n    --source ${arg('https://en.wikipedia.org/wiki/Castle_in_the_Sky')}`,
    ),
    wait: 700,
    busy: 900,
  },
  { html: out('proposed changeset 14 · 9 changes · 2 sources'), wait: 300 },
];

export function startSession(): void {
  const root = document.querySelector<HTMLElement>('[data-session]');
  const term = root?.querySelector<HTMLElement>('[data-term]');
  const review = root?.querySelector<HTMLElement>('.review');
  const label = root?.querySelector<HTMLElement>('[data-review-label]');
  const apply = root?.querySelector<HTMLElement>('[data-apply]');
  const state = root?.querySelector<HTMLElement>('[data-state]');
  const cursor = root?.querySelector<HTMLElement>('[data-cursor]');
  if (!root || !term || !review || !label || !apply || !state || !cursor) return;

  let run = 0;
  let visible = false;
  let playing = false;

  const sleep = (ms: number, id: number) =>
    new Promise<void>((resolve, reject) => setTimeout(() => (id === run ? resolve() : reject(new Error('stale'))), ms));

  const line = (html: string, cls = '') => {
    const el = document.createElement('span');
    el.className = `ln ${cls}`.trim();
    el.innerHTML = html;
    term.append(el);
    return el;
  };

  const reset = () => {
    term.replaceChildren();
    root.removeAttribute('data-step-proposed');
    root.removeAttribute('data-step-applied');
    label.textContent = 'Waiting for you · 1';
    state.textContent = 'Waiting for you';
    cursor.removeAttribute('data-on');
    cursor.style.setProperty('--cx', '88%');
    cursor.style.setProperty('--cy', '96%');
  };

  /** The final picture, for reduced motion. */
  const still = () => {
    reset();
    line(`<span class="t-you"><span class="t-p">›</span> ${esc(PROMPT)}</span>`);
    line('', 'gap');
    for (const l of SCRIPT) line(l.html);
    line('', 'gap');
    line('<span class="t-ok">✓</span> Applied by you. Undo it any time: <span class="t-call">flux changes undo 14</span>');
    root.setAttribute('data-step-proposed', '');
    root.setAttribute('data-step-applied', '');
    label.textContent = 'History';
    state.textContent = 'Applied';
  };

  const play = async () => {
    const id = ++run;
    playing = true;
    try {
      reset();
      await sleep(500, id);
      // The request, typed.
      const you = line(`<span class="t-you"><span class="t-p">›</span> <span data-typed></span><span class="caret"></span></span>`);
      const typed = you.querySelector<HTMLElement>('[data-typed]')!;
      for (let i = 1; i <= PROMPT.length; i++) {
        typed.textContent = PROMPT.slice(0, i);
        await sleep(PROMPT[i - 1] === ' ' ? 36 : 24, id);
      }
      you.querySelector('.caret')?.remove();
      line('', 'gap');
      await sleep(450, id);
      // The agent's calls: amber while running, green when answered.
      for (const l of SCRIPT) {
        await sleep(l.wait, id);
        const el = line(l.html);
        const dot = el.querySelector('.t-dot');
        if (dot && l.busy) {
          dot.classList.add('busy');
          await sleep(l.busy, id);
          dot.classList.remove('busy');
        }
      }
      // The proposal reaches the app.
      await sleep(400, id);
      root.setAttribute('data-step-proposed', '');
      await sleep(1500, id);
      // A person applies it.
      const r = review.getBoundingClientRect();
      const b = apply.getBoundingClientRect();
      cursor.setAttribute('data-on', '');
      await sleep(80, id);
      cursor.style.setProperty('--cx', `${(b.left - r.left + b.width * 0.42).toFixed(0)}px`);
      cursor.style.setProperty('--cy', `${(b.top - r.top + b.height * 0.5).toFixed(0)}px`);
      await sleep(1150, id);
      apply.setAttribute('data-pressed', '');
      await sleep(160, id);
      apply.removeAttribute('data-pressed');
      root.setAttribute('data-step-applied', '');
      label.textContent = 'History';
      state.textContent = 'Applied';
      await sleep(500, id);
      cursor.removeAttribute('data-on');
      line('', 'gap');
      line('<span class="t-ok">✓</span> Applied by you. Undo it any time: <span class="t-call">flux changes undo 14</span>');
      await sleep(6500, id);
      playing = false;
      if (visible) void play();
    } catch {
      // A newer run took over.
    }
  };

  if (reducedMotion()) {
    still();
    return;
  }
  reset();
  onVisibility(
    root,
    (v) => {
      visible = v;
      if (v && !playing) void play();
      if (!v && playing) {
        // Stop off screen; start again from the top next time.
        run++;
        playing = false;
      }
    },
    '-10% 0px',
  );
}
