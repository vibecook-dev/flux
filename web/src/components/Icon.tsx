/** Inline icon set (1.6px strokes, 24px grid). Decorative unless given a `label`. */
import type { SVGProps } from 'react';

const PATHS = {
  home: 'M4 10.5 12 4l8 6.5V20a1 1 0 0 1-1 1h-4.5v-6h-5v6H5a1 1 0 0 1-1-1z',
  film: 'M4 4h16v16H4zM8 4v16M16 4v16M4 8h4M4 12h4M4 16h4M16 8h4M16 12h4M16 16h4',
  tv: 'M3.5 6.5h17v11h-17zM8 21h8M9 2.5l3 4 3-4',
  archive: 'M3.5 5h17v4h-17zM5 9v10.5h14V9M10 13h4',
  search: 'M10.5 17a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM15.5 15.5 20 20',
  settings: 'M4 7h10M18 7h2M4 17h2M10 17h10M16 4.5v5M8 14.5v5',
  play: 'M7 4.5v15l12.5-7.5z',
  pause: 'M7 5h3.5v14H7zM13.5 5H17v14h-3.5z',
  back: 'M14.5 5 7.5 12l7 7',
  forward: 'M9.5 5l7 7-7 7',
  close: 'M6 6l12 12M18 6 6 18',
  info: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 11v6M12 7.5v.5',
  check: 'm5 12.5 4.5 4.5L19 7.5',
  plus: 'M12 5v14M5 12h14',
  link: 'M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1',
  lock: 'M6 10.5h12V20H6zM8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5',
  copy: 'M9 9h10v11H9zM5 15V4h10',
  restart: 'M4.5 12a7.5 7.5 0 1 0 2.2-5.3M4.5 4v4h4',
  volume: 'M4 9.5h3.5L12 5.5v13l-4.5-4H4zM15.5 9a4 4 0 0 1 0 6M18 6.5a7.5 7.5 0 0 1 0 11',
  mute: 'M4 9.5h3.5L12 5.5v13l-4.5-4H4zM16 9.5l5 5M21 9.5l-5 5',
  fullscreen: 'M4 9V4h5M15 4h5v5M20 15v5h-5M9 20H4v-5',
  exitFullscreen: 'M9 4v5H4M20 9h-5V4M15 20v-5h5M4 15h5v5',
  subtitles: 'M3.5 5.5h17v13h-17zM7 11h4M13 11h4M7 14.5h7M16 14.5h1',
  audio: 'M9 18V6l10-2v12M9 18a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0zM19 16a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0z',
  skipBack: 'M11 7 6 12l5 5M17 7l-5 5 5 5',
  skipForward: 'M13 7l5 5-5 5M7 7l5 5-5 5',
  stats: 'M4 20V10M10 20V4M16 20v-7M22 20H2',
  tailscale: 'M6 6h.01M12 6h.01M18 6h.01M6 12h.01M12 12h.01M18 12h.01M12 18h.01M6 18h.01M18 18h.01',
  folder: 'M3.5 6.5h6l2 2h9v10h-17z',
  refresh: 'M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4v4h-4',
  user: 'M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM4.5 20.5a7.5 7.5 0 0 1 15 0',
  sun: 'M12 16a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M5.3 18.7l1.4-1.4M17.3 6.7l1.4-1.4',
  moon: 'M19.5 14.5A8 8 0 0 1 9.5 4.5a8 8 0 1 0 10 10z',
  monitor: 'M3.5 5h17v11h-17zM9 20h6M12 16v4',
  keyboard: 'M3 6.5h18v11H3zM7 10h.01M11 10h.01M15 10h.01M7 14h10',
  warning: 'M12 4 21 20H3zM12 10v4M12 17v.5',
  sparkle: 'M12 3.5l1.8 5.2 5.2 1.8-5.2 1.8L12 17.5l-1.8-5.2-5.2-1.8 5.2-1.8z',
  agent: 'M4 5h16v11h-9l-5 4v-4H4zM9 10.5h.01M15 10.5h.01',
} as const;

export type IconName = keyof typeof PATHS;

const FILLED: ReadonlySet<IconName> = new Set<IconName>(['play', 'pause']);

export function Icon({ name, label, ...rest }: { name: IconName; label?: string } & SVGProps<SVGSVGElement>) {
  const filled = FILLED.has(name);
  return (
    <svg
      viewBox="0 0 24 24"
      width="24"
      height="24"
      fill={filled ? 'currentColor' : 'none'}
      stroke={filled ? 'none' : 'currentColor'}
      strokeWidth={1.6}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden={label ? undefined : true}
      role={label ? 'img' : undefined}
      aria-label={label}
      {...rest}
    >
      <path d={PATHS[name]} />
    </svg>
  );
}
