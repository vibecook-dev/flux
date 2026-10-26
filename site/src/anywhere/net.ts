/**
 * The remote-access map. Nodes are laid out by CSS (--x/--y per breakpoint);
 * this draws the lines between them: "the usual way" threads every stream
 * through a router, the open internet and a proxy, "with flux" each device
 * reaches the server across the tailnet. It plays the change once when the
 * map comes into view; the toggle switches it after that.
 */
import { segmented } from '../lib/segmented';
import { reducedMotion, whenVisible } from '../lib/motion';

const NS = 'http://www.w3.org/2000/svg';
const DEVICES = ['phone', 'laptop', 'tv', 'tablet'];

const CAPTIONS = {
  usual: 'Six things to set up, keep up to date and keep secure, and your server still sits on the open internet.',
  flux: 'One private network. Every device reaches flux over WireGuard, encrypted end to end, and nothing is open to the internet.',
};

type Mode = keyof typeof CAPTIONS;

export function startNet(): void {
  const root = document.querySelector<HTMLElement>('[data-net]');
  const canvas = root?.querySelector<HTMLElement>('[data-net-canvas]');
  const svg = root?.querySelector<SVGSVGElement>('[data-net-lines]');
  const toggle = root?.querySelector<HTMLElement>('.net-toggle');
  const caption = root?.querySelector<HTMLElement>('[data-net-caption]');
  if (!root || !canvas || !svg || !toggle || !caption) return;

  let touched = false;
  const setMode = (mode: Mode) => {
    if (root.dataset.mode === mode) return;
    root.dataset.mode = mode;
    caption.setAttribute('data-swap', '');
    setTimeout(() => {
      caption.textContent = CAPTIONS[mode];
      caption.removeAttribute('data-swap');
    }, 200);
  };
  const tabs = segmented(toggle, 'data-net-mode', (m) => {
    touched = true;
    setMode(m as Mode);
  });

  /**
   * Where a node's lines meet it, in the map's own pixels, from layout alone: a node is
   * centred on its --x/--y by translate(-50%, -50%), and transforms (a hurdle shrinking
   * away) don't move it. Across the map, lines meet the middle of the icon; down a phone's
   * map, they leave from below the server's caption and land on top of each device.
   */
  const anchor = (name: string, edge: 'middle' | 'below' | 'top' = 'middle') => {
    const node = canvas.querySelector<HTMLElement>(`[data-node="${name}"]`);
    const icon = node?.querySelector<HTMLElement>('.node-icon, .hurdle-icon');
    if (!node || !icon) return { x: 0, y: 0 };
    const top = node.offsetTop - node.offsetHeight / 2;
    const y = edge === 'below' ? top + node.offsetHeight + 6 : edge === 'top' ? top + icon.offsetTop - 2 : top + icon.offsetTop + icon.offsetHeight / 2;
    return { x: node.offsetLeft - node.offsetWidth / 2 + icon.offsetLeft + icon.offsetWidth / 2, y };
  };

  const draw = () => {
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    svg.setAttribute('viewBox', `0 0 ${w} ${h}`);
    const across = w > 760;
    const curve = (a: { x: number; y: number }, b: { x: number; y: number }) => {
      if (across) {
        const dx = (b.x - a.x) * 0.5;
        return `M${a.x},${a.y} C${a.x + dx},${a.y} ${b.x - dx},${b.y} ${b.x},${b.y}`;
      }
      const dy = (b.y - a.y) * 0.5;
      return `M${a.x},${a.y} C${a.x},${a.y + dy} ${b.x},${b.y - dy} ${b.x},${b.y}`;
    };
    const path = (d: string, cls: string, extra: Record<string, string> = {}) => {
      const p = document.createElementNS(NS, 'path');
      p.setAttribute('d', d);
      p.setAttribute('class', cls);
      for (const [k, v] of Object.entries(extra)) p.setAttribute(k, v);
      return p;
    };

    const edge = (n: string) => (across ? 'middle' : n === 'server' ? 'below' : DEVICES.includes(n) ? 'top' : 'middle');
    const at = Object.fromEntries(['server', 'router', 'internet', 'ddns', 'proxy', 'cert', 'password', ...DEVICES].map((n) => [n, anchor(n, edge(n))]));
    const parts: SVGPathElement[] = [];
    // The usual way: one long road, and the chores beside it.
    parts.push(path(curve(at.server, at.router), 'usual'));
    parts.push(path(curve(at.router, at.internet), 'usual'));
    parts.push(path(curve(at.internet, at.proxy), 'usual'));
    for (const d of DEVICES) parts.push(path(curve(at.proxy, at[d]), 'usual'));
    parts.push(path(curve(at.internet, at.ddns), 'usual side'));
    parts.push(path(curve(at.proxy, at.cert), 'usual side'));
    parts.push(path(curve(at.proxy, at.password), 'usual side'));
    // With flux: straight across the tailnet, with traffic on it.
    DEVICES.forEach((d, i) => {
      const line = curve(at.server, at[d]);
      parts.push(path(line, 'flux', { pathLength: '1000', style: `--d:${(0.15 + i * 0.09).toFixed(2)}s` }));
      parts.push(path(line, 'packet', { pathLength: '1000', style: `--pd:${(i * 0.55).toFixed(2)}s;--dur:${(2.1 + i * 0.35).toFixed(2)}s` }));
      // A second packet coming back: a stream is a conversation.
      parts.push(path(line, 'packet back', { pathLength: '1000', style: `--pd:${(1.2 + i * 0.4).toFixed(2)}s;--dur:${(2.6 + i * 0.3).toFixed(2)}s` }));
    });
    svg.replaceChildren(...parts);
  };

  new ResizeObserver(draw).observe(canvas);
  document.fonts?.ready.then(draw);

  // The story, once: the usual way for a moment, then the way it should be.
  whenVisible(
    root,
    () => {
      if (reducedMotion()) {
        tabs.select('flux');
        setMode('flux');
        return;
      }
      setTimeout(() => {
        if (touched) return;
        tabs.select('flux');
        setMode('flux');
      }, 1900);
    },
    '0px 0px -30% 0px',
  );
}
