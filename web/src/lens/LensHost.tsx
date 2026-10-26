/**
 * Mounts the current lens into the stage and keeps it fed with results. Switching
 * lens cross-fades the two and flies the focused poster to its place in the new
 * one, so you never lose your place. Lens code loads on demand.
 */
import { useEffect, useRef } from 'react';
import type { Id, ItemSummary } from '../api/types';
import { activeLens, lensFocus, setActiveLens } from './bridge';
import { reducedMotion, SOFT } from './dom';
import { fly } from './flight';
import styles from './LensHost.module.css';
import type { Insets, LensContext, LensDef, LensEnv, LensView } from './types';

export interface LensHostProps {
  def: LensDef;
  items: readonly ItemSummary[];
  total: number;
  /** Covered by detail or the player: stop animation loops. */
  paused: boolean;
  safe: Insets;
  phone: boolean;
  theme: 'dark' | 'light';
  onFocus(id: Id): void;
  onOpen(id: Id, art: Element | null): void;
  onPlay(id: Id): void;
  onAccessory(node: HTMLElement | null): void;
  onScrolled(dir: 'down' | 'up' | 'top'): void;
  onIntent(item: ItemSummary): void;
  sectionOf(item: ItemSummary): string;
  onError(def: LensDef, err: unknown): void;
}

interface Mounted {
  def: LensDef;
  root: HTMLDivElement;
  view: LensView;
}

function safeRect(view: LensView, id: Id): DOMRect | null {
  try {
    const r = view.rectOf(id);
    if (!r || !r.width || r.bottom < 0 || r.top > window.innerHeight || r.right < 0 || r.left > window.innerWidth) return null;
    return r;
  } catch {
    return null;
  }
}

export function LensHost(props: LensHostProps) {
  const stage = useRef<HTMLDivElement>(null);
  const live = useRef(props);
  // The context reads the latest props through this ref; refresh it after every render.
  useEffect(() => {
    live.current = props;
  });
  const mounted = useRef<Mounted | null>(null);
  const mounting = useRef<HTMLDivElement | null>(null);

  // Mount (and switch) lenses.
  useEffect(() => {
    let cancelled = false;
    const def = props.def;
    def
      .load()
      .then((mod) => {
        const host = stage.current;
        if (cancelled || !host) return;
        const root = document.createElement('div');
        root.className = styles.lensRoot;
        root.dataset.lens = def.id;
        host.append(root);
        const ctx: LensContext = {
          get items() {
            return live.current.items;
          },
          get total() {
            return live.current.total;
          },
          get focus() {
            return lensFocus();
          },
          get safe() {
            return live.current.safe;
          },
          get env(): LensEnv {
            return { mini: false, reducedMotion: reducedMotion(), theme: live.current.theme, phone: live.current.phone };
          },
          setFocus: (id) => live.current.onFocus(id),
          open: (id, art) => live.current.onOpen(id, art ?? null),
          play: (id) => live.current.onPlay(id),
          setAccessory: (node) => {
            // Lenses usually offer it while mounting, before they become current.
            if (mounting.current === root || mounted.current?.root === root) live.current.onAccessory(node);
          },
          scrolled: (dir) => live.current.onScrolled(dir),
          sectionOf: (item) => live.current.sectionOf(item),
          intent: (item) => live.current.onIntent(item),
        };
        const prev = mounted.current;
        const fromRect = prev && lensFocus() !== null ? safeRect(prev.view, lensFocus() as Id) : null;
        live.current.onAccessory(null);
        // A new lens starts at its top as far as the chrome goes: the lens bar shows in full.
        live.current.onScrolled('top');
        let view: LensView;
        mounting.current = root;
        try {
          view = mod.mount(root, ctx);
        } catch (err) {
          root.remove();
          live.current.onError(def, err);
          return;
        } finally {
          mounting.current = null;
        }
        const next: Mounted = { def, root, view };
        mounted.current = next;
        setActiveLens({ def, view });
        if (live.current.paused) view.pause?.(true);
        if (!prev) return;
        // Keyboard focus comes along: from the old lens (about to go) to the same title in the new one.
        const a = document.activeElement;
        if (a === document.body || (a && prev.root.contains(a))) {
          const id = lensFocus();
          const target = id !== null ? view.artOf?.(id)?.closest<HTMLElement>('a[href], button, [tabindex]') : null;
          if (target && root.contains(target)) target.focus({ preventScroll: true });
        }
        prev.root.classList.add(styles.leaving);
        if (reducedMotion()) {
          prev.view.destroy();
          prev.root.remove();
          return;
        }
        prev.root.style.zIndex = '2';
        const out = prev.root.animate(
          [
            { opacity: 1, transform: 'none', filter: 'none' },
            { opacity: 0, transform: 'scale(.95)', filter: 'blur(12px)' },
          ],
          { duration: 420, easing: 'cubic-bezier(.4,0,.2,1)', fill: 'forwards' },
        );
        root.animate(
          [
            { opacity: 0, transform: 'scale(1.045)', filter: 'blur(10px)' },
            { opacity: 1, transform: 'none', filter: 'none' },
          ],
          { duration: 680, delay: 80, easing: SOFT, fill: 'backwards' },
        );
        // Your place carries over: the focused poster flies to its spot in the new lens.
        requestAnimationFrame(() =>
          requestAnimationFrame(() => {
            const id = lensFocus();
            const item = id !== null ? live.current.items.find((i) => i.id === id) : undefined;
            const toRect = item ? safeRect(view, item.id) : null;
            if (item && fromRect && toRect) void fly(item, fromRect, toRect);
          }),
        );
        out.finished.then(
          () => {
            prev.view.destroy();
            prev.root.remove();
          },
          () => undefined,
        );
      })
      .catch((err: unknown) => {
        if (!cancelled) live.current.onError(def, err);
      });
    return () => {
      cancelled = true;
    };
  }, [props.def]);

  // Results.
  useEffect(() => {
    mounted.current?.view.update(props.items, props.total);
  }, [props.items, props.total]);

  useEffect(() => {
    mounted.current?.view.pause?.(props.paused);
  }, [props.paused]);

  // Leaving the shell (the player): tear the lens down.
  useEffect(
    () => () => {
      const m = mounted.current;
      if (m) {
        m.view.destroy();
        m.root.remove();
        mounted.current = null;
        if (activeLens()?.view === m.view) setActiveLens(null);
      }
    },
    [],
  );

  return <div ref={stage} className={styles.stage} />;
}
