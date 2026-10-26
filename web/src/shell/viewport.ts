/** Viewport facts the shell and lenses need: phone width, and the safe-area insets (env() is CSS-only). */
import { useEffect, useState } from 'react';

const PHONE = '(max-width: 640px)';

export function usePhone(): boolean {
  const [phone, setPhone] = useState(() => typeof matchMedia !== 'undefined' && matchMedia(PHONE).matches);
  useEffect(() => {
    const mq = matchMedia(PHONE);
    const on = () => setPhone(mq.matches);
    mq.addEventListener('change', on);
    return () => mq.removeEventListener('change', on);
  }, []);
  return phone;
}

let probe: HTMLDivElement | null = null;

function readInsets(): { top: number; bottom: number } {
  if (typeof document === 'undefined') return { top: 0, bottom: 0 };
  if (!probe) {
    probe = document.createElement('div');
    probe.setAttribute('aria-hidden', 'true');
    probe.style.cssText = 'position:fixed;inset:0;visibility:hidden;pointer-events:none;padding-top:env(safe-area-inset-top,0px);padding-bottom:env(safe-area-inset-bottom,0px)';
    document.body.append(probe);
  }
  const cs = getComputedStyle(probe);
  return { top: parseFloat(cs.paddingTop) || 0, bottom: parseFloat(cs.paddingBottom) || 0 };
}

export function useInsets(): { top: number; bottom: number } {
  const [insets, setInsets] = useState(readInsets);
  useEffect(() => {
    const on = () => {
      const next = readInsets();
      setInsets((prev) => (prev.top === next.top && prev.bottom === next.bottom ? prev : next));
    };
    window.addEventListener('resize', on);
    return () => window.removeEventListener('resize', on);
  }, []);
  return insets;
}
