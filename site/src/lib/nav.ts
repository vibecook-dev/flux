/**
 * The island at the top: its thumb slides under the section you're in (like
 * the app's lens bar), and it firms up once the page has scrolled.
 */
import { addScene, pageTop } from './motion';

export function startNav(): void {
  const nav = document.querySelector<HTMLElement>('[data-nav]');
  const links = document.querySelector<HTMLElement>('[data-island-links]');
  const thumb = document.querySelector<HTMLElement>('[data-island-thumb]');
  if (!nav || !links || !thumb) return;
  const anchors = [...links.querySelectorAll<HTMLAnchorElement>('a[data-section]')];
  const sections = anchors.map((a) => document.getElementById(a.dataset.section ?? '')).filter((s): s is HTMLElement => !!s);

  let tops: number[] = [];
  let boxes: { x: number; w: number }[] = [];
  let current = -2;
  let scrolled: boolean | null = null;

  const place = (i: number) => {
    anchors.forEach((a, j) => a.setAttribute('aria-current', String(i === j)));
    const b = boxes[i];
    if (!b || b.w === 0) {
      thumb.style.setProperty('--o', '0');
      return;
    }
    thumb.style.setProperty('--x', `${b.x}px`);
    thumb.style.setProperty('--w', `${b.w}px`);
    thumb.style.setProperty('--o', '1');
  };

  addScene({
    measure() {
      tops = sections.map((s) => pageTop(s));
      boxes = anchors.map((a) => ({ x: a.offsetLeft, w: a.offsetWidth }));
      if (current >= 0) place(current);
    },
    update(y, vh) {
      const isScrolled = y > 24;
      if (isScrolled !== scrolled) {
        scrolled = isScrolled;
        nav.toggleAttribute('data-scrolled', isScrolled);
      }
      // The section whose top has passed 40% of the window, unless the page is at its end.
      const line = y + vh * 0.4;
      let i = -1;
      for (let j = 0; j < tops.length; j++) if (tops[j] <= line) i = j;
      if (window.innerHeight + y >= document.documentElement.scrollHeight - 4) i = tops.length - 1;
      if (i !== current) {
        // The thumb appears where it is, instead of sliding in from the left.
        if (current < 0 && i >= 0) {
          thumb.style.transition = 'opacity 0.4s';
          place(i);
          thumb.getBoundingClientRect();
          thumb.style.transition = '';
        } else {
          place(i);
        }
        current = i;
      }
    },
  });
}
