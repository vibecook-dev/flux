import type { KeyboardEvent } from 'react';

/** Move focus between `[data-nav-row]` shelves by geometry, like a TV launcher. */
export function spatialKeyDown(e: KeyboardEvent<HTMLElement>): void {
  const target = e.target as HTMLElement;
  const row = target.closest<HTMLElement>('[data-nav-row]');
  if (!row) return;
  const items = [...row.querySelectorAll<HTMLElement>('[data-nav-item]')];
  const i = items.indexOf(target);
  if (i < 0) return;
  let next: HTMLElement | undefined;
  if (e.key === 'ArrowRight') next = items[i + 1];
  else if (e.key === 'ArrowLeft') next = items[i - 1];
  else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    const rows = [...document.querySelectorAll<HTMLElement>('[data-nav-row]')];
    const r = rows.indexOf(row);
    const other = rows[e.key === 'ArrowDown' ? r + 1 : r - 1];
    if (!other) return;
    const x = target.getBoundingClientRect().left + target.offsetWidth / 2;
    let best = Infinity;
    for (const cand of other.querySelectorAll<HTMLElement>('[data-nav-item]')) {
      const rect = cand.getBoundingClientRect();
      const d = Math.abs(rect.left + rect.width / 2 - x);
      if (d < best) {
        best = d;
        next = cand;
      }
    }
  } else return;
  if (next) {
    e.preventDefault();
    next.focus({ preventScroll: true });
    next.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
  }
}
