/**
 * A glass segmented control, like the app's lens bar: a thumb slides under
 * the selected button with a spring. `select` moves it from code.
 */
export interface Segmented {
  select(value: string | null, notify?: boolean): void;
}

export function segmented(root: HTMLElement, attr: string, onSelect: (value: string) => void): Segmented {
  const thumb = root.querySelector<HTMLElement>('.segmented-thumb');
  const buttons = [...root.querySelectorAll<HTMLButtonElement>(`button[${attr}]`)];
  let value: string | null = buttons.find((b) => b.getAttribute('aria-selected') === 'true')?.getAttribute(attr) ?? null;

  const place = (animate = true) => {
    const b = buttons.find((x) => x.getAttribute(attr) === value);
    if (!thumb) return;
    if (!b) {
      thumb.style.opacity = '0';
      return;
    }
    if (!animate) thumb.style.transition = 'none';
    thumb.style.opacity = '1';
    thumb.style.setProperty('--x', `${b.offsetLeft}px`);
    thumb.style.setProperty('--w', `${b.offsetWidth}px`);
    if (!animate) {
      thumb.getBoundingClientRect();
      thumb.style.transition = '';
    }
  };

  const select = (v: string | null, notify = false) => {
    value = v;
    for (const b of buttons) b.setAttribute('aria-selected', String(b.getAttribute(attr) === v));
    place();
    if (notify && v !== null) onSelect(v);
  };

  for (const b of buttons) {
    b.addEventListener('click', () => select(b.getAttribute(attr), true));
  }
  // Arrow keys move between the tabs, as a tablist should.
  root.addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    const i = buttons.findIndex((b) => b === document.activeElement);
    if (i < 0) return;
    const next = buttons[(i + (e.key === 'ArrowRight' ? 1 : buttons.length - 1)) % buttons.length];
    next.focus();
    next.click();
    e.preventDefault();
  });
  new ResizeObserver(() => place(false)).observe(root);
  document.fonts?.ready.then(() => place(false));
  place(false);
  return { select };
}
