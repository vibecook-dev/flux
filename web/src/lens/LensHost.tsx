/**
 * Mounts the current lens into the stage and keeps it fed with results. Switching
 * lens cross-fades the two and flies the focused poster to its place in the new
 * one, so you never lose your place: the old lens steps back from where the
 * poster was, the new one settles in around where it lands, and the poster is
 * never on screen twice (flight.ts). Lens code loads on demand.
 */
import { type RefObject, useEffect, useRef } from 'react';
import type { Id, ItemSummary } from '../api/types';
import { activeLens, lensFocus, setActiveLens } from './bridge';
import { reducedMotion, SOFT } from './dom';
import { fly, hideIn, quadCentre, quadSize, spotIn, srcFor, type Flight } from './flight';
import styles from './LensHost.module.css';
import type { Insets, LensContext, LensDef, LensEnv, LensView, Spot } from './types';

export interface LensHostProps {
  def: LensDef;
  items: readonly ItemSummary[];
  total: number;
  /** Covered by detail or the player: stop animation loops. */
  paused: boolean;
  safe: Insets;
  phone: boolean;
  theme: 'dark' | 'light';
  /** Set by the shell when the next switch should just happen (the lens gallery grew it already). */
  quietNext?: RefObject<boolean>;
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

/** A switch under way: the lens coming in, the poster flying to it (from where, in which lens), and the cross-fade (started once). */
interface Switch {
  into: Animation | null;
  flight: Flight | null;
  id: Id | null;
  from: Spot | null;
  fromView: LensView | null;
  begin(): void;
}

const OUT_MS = 380;
const IN_MS = 560;

export function LensHost(props: LensHostProps) {
  const stage = useRef<HTMLDivElement>(null);
  const live = useRef(props);
  // The context reads the latest props through this ref; refresh it after every render.
  useEffect(() => {
    live.current = props;
  });
  const mounted = useRef<Mounted | null>(null);
  const mounting = useRef<HTMLDivElement | null>(null);
  const switching = useRef<Switch | null>(null);
  /** Lenses on their way out (destroyed when they have faded). */
  const leaving = useRef(new Set<Mounted>());

  // Mount (and switch) lenses.
  useEffect(() => {
    let cancelled = false;
    const def = props.def;
    def
      .load()
      .then((mod) => {
        const host = stage.current;
        if (cancelled || !host) return;
        const prev = mounted.current;
        const quiet = live.current.quietNext?.current ?? false;
        if (live.current.quietNext) live.current.quietNext.current = false;
        const motion = Boolean(prev) && !quiet && !reducedMotion();
        const root = document.createElement('div');
        root.className = styles.lensRoot;
        root.dataset.lens = def.id;
        // It arrives unseen; the switch below fades it in as the poster takes off.
        if (motion) root.style.opacity = '0';
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
        // The title you were on, and where its poster is: still in the air from the last
        // switch, or in the lens on screen.
        const id = lensFocus();
        const item = id !== null ? live.current.items.find((i) => i.id === id) : undefined;
        const last = switching.current;
        const airborne = motion && last?.flight?.airborne ? last.flight : null;
        // The last switch's poster hadn't left yet (its image was still coming): it is still
        // where that switch found it, in the lens before, not in the one between (unseen).
        const unflown = motion && item && last?.flight && !last.flight.started && last.id === item.id && last.from ? last : null;
        const from = motion && item && prev && !airborne ? (unflown?.from ?? spotIn(prev.view, item.id)) : null;
        const fromView = unflown?.fromView ?? prev?.view ?? null;
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
        // A switch still under way gives way to this one: its cross-fade runs now if its
        // poster hadn't taken off yet, and a poster still in the air flies on to this lens.
        switching.current = null;
        last?.begin();
        if (last?.flight && last.flight !== airborne) last.flight.cancel();
        if (live.current.paused) view.pause?.(true);
        if (!prev) return;
        // Keyboard focus comes along: from the old lens (about to go) to the same title in the new one.
        const a = document.activeElement;
        if (a === document.body || (a && prev.root.contains(a))) {
          const fid = lensFocus();
          const target = fid !== null ? view.artOf?.(fid)?.closest<HTMLElement>('a[href], button, [tabindex]') : null;
          if (target && root.contains(target)) target.focus({ preventScroll: true });
        }
        prev.root.classList.add(styles.leaving);
        const gone = () => {
          leaving.current.delete(prev);
          prev.view.destroy();
          prev.root.remove();
        };
        if (!motion) {
          root.style.opacity = '';
          last?.into?.cancel();
          gone();
          return;
        }
        leaving.current.add(prev);

        // Where it lands (on the Shelf, a tape's spine: the poster turns into it on the way),
        // hidden in the new lens until the poster gets there.
        const target = item && (from || airborne) ? item : null;
        const to = target ? spotIn(view, target.id) : null;
        const undoTo = to && target ? hideIn(view, target.id) : () => undefined;

        let begun = false;
        const begin = () => {
          if (begun) return;
          begun = true;
          // The old lens steps back from where the poster was; the new one settles in
          // around where it lands (so the poster's spot holds still as it does).
          const fromC = from ? quadCentre(from.quad) : null;
          const toC = to ? quadCentre(to.quad) : null;
          const was = getComputedStyle(prev.root);
          const opacity = Number(was.opacity);
          last?.into?.cancel();
          prev.root.style.zIndex = '2';
          if (fromC) prev.root.style.transformOrigin = `${fromC.x}px ${fromC.y}px`;
          prev.root
            .animate(
              [
                { opacity, transform: 'none', filter: 'none' },
                { opacity: 0, transform: 'scale(.95)', filter: 'blur(10px)' },
              ],
              { duration: OUT_MS, easing: 'cubic-bezier(.4,0,.2,1)', fill: 'forwards' },
            )
            .finished.then(gone, () => undefined);
          if (toC) root.style.transformOrigin = `${toC.x}px ${toC.y}px`;
          sw.into = root.animate(
            [
              { opacity: 0, transform: 'scale(1.045)', filter: 'blur(8px)' },
              { opacity: 1, transform: 'none', filter: 'none' },
            ],
            { duration: IN_MS, easing: SOFT },
          );
          root.style.opacity = '';
        };
        const sw: Switch = { into: null, flight: null, id: target?.id ?? null, from, fromView, begin };
        switching.current = sw;

        if (airborne && to && airborne.retarget(to, { duration: IN_MS, land: undoTo })) {
          // Still flying from the last switch: it changes course for the new lens.
          begin();
          sw.flight = airborne;
        } else if (from && to && target) {
          sw.flight = fly({
            item: target,
            from,
            to,
            hi: srcFor(target.poster, quadSize(to.quad).w),
            duration: IN_MS,
            start: () => {
              // The lens it leaves is going: its copy can stay hidden until then.
              hideIn(fromView, target.id);
              begin();
            },
            land: () => {
              begin();
              undoTo();
            },
          });
        } else {
          // Nothing flies: the title shows in the new lens straight away.
          airborne?.cancel();
          undoTo();
          begin();
        }
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
  useEffect(() => {
    const out = leaving.current;
    return () => {
      switching.current?.flight?.cancel();
      switching.current = null;
      for (const m of out) {
        m.view.destroy();
        m.root.remove();
      }
      out.clear();
      const m = mounted.current;
      if (m) {
        m.view.destroy();
        m.root.remove();
        mounted.current = null;
        if (activeLens()?.view === m.view) setActiveLens(null);
      }
    };
  }, []);

  return <div ref={stage} className={styles.stage} />;
}
