/**
 * All lenses: the library steps back and the lenses line up as live previews of
 * your own titles. Choose one (it grows back into the window), pin it to the
 * lens bar, or make your own: describe it, hand the prompt to your AI agent,
 * paste back what it writes, and flux previews it before you keep it.
 */
import { type ChangeEvent, type KeyboardEvent as ReactKeyboardEvent, useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { ItemSummary } from '../api/types';
import { Icon } from '../components/Icon';
import { clamp, reducedMotion, SOFT } from '../lens/dom';
import { makePrompt, parseLensReply } from '../lens/kit/prompt';
import { kitFailure, kitModule, onKitFailure, validateSpec, type KitSpec } from '../lens/kit/runtime';
import { op } from '../api/agent';
import { keys } from '../api/queries';
import { invalidateQueries } from '../api/query';
import { keepLens, removeLens } from '../lens/kit/store';
import { mountMini, type MiniLens } from '../lens/mini';
import { LENS_ICONS } from '../lens/registry';
import type { Insets, LensDef } from '../lens/types';
import styles from './LensGallery.module.css';

export interface LensGalleryProps {
  lenses: readonly LensDef[];
  current: string;
  pins: readonly string[];
  /** The space's results, for live previews. */
  items: readonly ItemSummary[];
  total: number;
  theme: 'dark' | 'light';
  phone: boolean;
  /** Where the shell's glass sits: previews keep clear of it, as the lenses do. */
  safe: Insets;
  sectionOf: (item: ItemSummary) => string;
  onChoose: (id: string) => void;
  onPin: (id: string, on: boolean) => void;
  /** A lens was added or removed on this device (the list should be read again). */
  onLensesChanged: () => void;
  onClose: () => void;
  /** Closing (closed, or a lens chosen): it animates away, then says it has gone. */
  closing: boolean;
  onGone: () => void;
}

const AUTHOR: Record<LensDef['author'], string> = { flux: 'flux', kit: 'Lens kit', you: 'Yours', server: 'On the server' };
const IDEAS = ['A 1990s video shop window', 'Airport departure board', 'Polaroids on a fridge', 'Vinyl crate to flip through', 'Constellations in a night sky', 'Festival laurels wall'];

interface Slot {
  mini: MiniLens | null;
  dead: boolean;
}

interface PreviewOptions {
  items: readonly ItemSummary[];
  total: number;
  theme: 'dark' | 'light';
  phone: boolean;
  safe: Insets;
  sectionOf: (item: ItemSummary) => string;
}

export function LensGallery(props: LensGalleryProps) {
  const { lenses, current, pins, items, total, theme, phone, safe, sectionOf, onChoose, onPin, onLensesChanged, onClose, closing, onGone } = props;
  const wrap = useRef<HTMLDivElement>(null);
  const chosen = useRef(false);
  // Gone (a new gallery opened while this one was closing): its animations ending must not close that one.
  const gone = useRef(false);
  useEffect(
    () => () => {
      gone.current = true;
    },
    [],
  );
  const left = useCallback(() => {
    if (!gone.current) onGone();
  }, [onGone]);
  const track = useRef<HTMLDivElement>(null);
  const cards = useRef<(HTMLElement | null)[]>([]);
  const minis = useRef(new Map<string, Slot>());
  const opts = useRef<PreviewOptions>({ items, total, theme, phone, safe, sectionOf });
  useEffect(() => {
    opts.current = { items, total, theme, phone, safe, sectionOf };
  });
  const startIndex = Math.max(0, lenses.findIndex((l) => l.id === current));
  const [centred, setCentred] = useState(startIndex);
  const [leaving, setLeaving] = useState(false);
  const count = lenses.length + 1;

  // ── geometry: previews render at full window size, scaled into their card ──
  const fit = useCallback(() => {
    track.current?.parentElement?.style.setProperty('--ar', String(window.innerWidth / window.innerHeight));
    for (const c of cards.current) {
      const view = c?.querySelector<HTMLElement>('[data-view]');
      const screen = c?.querySelector<HTMLElement>('[data-screen]');
      if (view && screen) scaleScreen(screen, view);
    }
  }, []);

  const center = useCallback(
    (i: number, instant = false) => {
      const t = track.current;
      const c = cards.current[clamp(i, 0, count - 1)];
      if (!t || !c) return;
      t.scrollTo({ left: c.offsetLeft - (t.clientWidth - c.offsetWidth) / 2, behavior: instant || reducedMotion() ? 'auto' : 'smooth' });
    },
    [count],
  );

  /** Shade cards by their distance from the middle; returns the centred one. */
  const paintDistances = useCallback(() => {
    const mid = window.innerWidth / 2;
    let best = 0;
    let bd = Infinity;
    cards.current.forEach((c, i) => {
      if (!c) return;
      const r = c.getBoundingClientRect();
      const d = Math.abs(r.left + r.width / 2 - mid);
      c.style.setProperty('--d', clamp(d / Math.max(1, r.width), 0, 1.5).toFixed(3));
      if (d < bd) {
        bd = d;
        best = i;
      }
    });
    return best;
  }, []);
  const measure = useCallback(() => setCentred(paintDistances()), [paintDistances]);

  useLayoutEffect(() => {
    fit();
    center(startIndex, true);
    paintDistances();
    // Keyboards and screen readers start on the lens in use (Enter keeps it, ← → browse).
    cards.current[startIndex]?.querySelector<HTMLElement>('[data-view]')?.focus({ preventScroll: true });
    if (!reducedMotion()) {
      cards.current.forEach((c, i) => {
        const d = Math.abs(i - startIndex);
        c?.animate([{ opacity: 0, transform: `translateY(${30 + d * 10}px) scale(.96)` }, { opacity: 1, transform: 'none' }], { duration: 560, delay: 60 + d * 40, easing: SOFT, fill: 'backwards' });
      });
    }
    const onResize = () => {
      fit();
      measure();
    };
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
    // Once, when the gallery opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // New or removed cards (a lens you kept) need their screens sized.
  useLayoutEffect(() => {
    fit();
  }, [fit, lenses]);

  useEffect(() => {
    const t = track.current;
    if (!t) return;
    let raf = 0;
    const on = () => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(measure);
    };
    t.addEventListener('scroll', on, { passive: true });
    return () => {
      t.removeEventListener('scroll', on);
      cancelAnimationFrame(raf);
    };
  }, [measure]);

  // ── live previews for the centred card and its neighbours ──
  useEffect(() => {
    const want = new Set(lenses.filter((_, i) => Math.abs(i - centred) <= 1).map((l) => l.id));
    for (const [id, slot] of minis.current) {
      if (!want.has(id)) {
        slot.dead = true;
        slot.mini?.destroy();
        minis.current.delete(id);
      }
    }
    lenses.forEach((def, i) => {
      if (!want.has(def.id) || minis.current.has(def.id) || def.id === 'folders') return;
      const screen = cards.current[i]?.querySelector<HTMLElement>('[data-screen]');
      if (!screen) return;
      const slot: Slot = { mini: null, dead: false };
      minis.current.set(def.id, slot);
      mountMini(def, screen, opts.current).then(
        (m) => {
          if (slot.dead) m.destroy();
          else slot.mini = m;
        },
        () => {
          slot.dead = true;
        },
      );
    });
  }, [centred, lenses]);

  useEffect(() => {
    for (const slot of minis.current.values()) slot.mini?.view.update(items, total);
  }, [items, total]);

  useEffect(() => {
    const all = minis.current;
    return () => {
      for (const slot of all.values()) {
        slot.dead = true;
        slot.mini?.destroy();
      }
      all.clear();
    };
  }, []);

  // ── choosing: the preview grows back into the window ──
  // The library behind switches to the lens at once and steps forward as the preview
  // grows (both full size together); then the preview fades into the lens itself.
  const choose = useCallback(
    (id: string) => {
      if (chosen.current) return;
      const i = lenses.findIndex((l) => l.id === id);
      const view = cards.current[i]?.querySelector<HTMLElement>('[data-view]');
      chosen.current = true;
      if (!view || reducedMotion()) {
        onChoose(id);
        left();
        return;
      }
      setLeaving(true);
      const r = view.getBoundingClientRect();
      view.style.transformOrigin = '0 0';
      const grow = view.animate(
        [
          { transform: 'none', borderRadius: '26px' },
          { transform: `translate(${-r.left}px, ${-r.top}px) scale(${window.innerWidth / r.width}, ${window.innerHeight / r.height})`, borderRadius: '0px' },
        ],
        { duration: 560, easing: SOFT, fill: 'forwards' },
      );
      cards.current.forEach((c, k) => {
        if (k !== i) c?.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 240, fill: 'forwards' });
      });
      onChoose(id);
      // It hands over to the lens while it still moves (a preview is drawn small: the two
      // differ a little), and is gone as it reaches the window's size.
      wrap.current?.animate([{ opacity: 1 }, { opacity: 1, offset: 0.42 }, { opacity: 0 }], { duration: 560, easing: 'linear', fill: 'forwards' });
      void grow.finished.then(left, left);
    },
    [lenses, onChoose, left],
  );

  // Closed without choosing (Esc, ×, a click beside the cards): the cards drop away as
  // the library steps forward again.
  useEffect(() => {
    if (!closing || chosen.current) return;
    const w = wrap.current;
    if (!w || reducedMotion()) return left();
    setLeaving(true);
    w.animate(
      [
        { opacity: 1, transform: 'none' },
        { opacity: 0, transform: 'translateY(28px) scale(.98)' },
      ],
      { duration: 280, easing: 'cubic-bezier(.4,0,.7,.2)', fill: 'forwards' },
    ).finished.then(left, () => undefined);
  }, [closing, left]);

  // ── keys: ← → browse, Enter chooses (Escape belongs to the shell) ──
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Closing (or a lens chosen): the keys are the library's again.
      if (closing || chosen.current) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
      if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
        e.preventDefault();
        const n = clamp(centred + (e.key === 'ArrowRight' ? 1 : -1), 0, count - 1);
        center(n);
        // Focus follows, so Enter acts on the card in the middle.
        cards.current[n]?.querySelector<HTMLElement>('[data-view]')?.focus({ preventScroll: true });
      } else if (e.key === 'Enter' && centred < lenses.length && !t?.closest('button')) {
        e.preventDefault();
        choose(lenses[centred].id);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [centred, count, lenses, center, choose, closing]);

  return (
    <div ref={wrap} className={styles.wrap} role="dialog" aria-modal="true" aria-label="All lenses" data-leaving={leaving || undefined} inert={closing}>
      <div className={styles.back} onClick={onClose} aria-hidden="true" />
      <header className={styles.head}>
        <h2 className="display-l">Lenses</h2>
        <p>The same library, seen another way. Pinned lenses sit in the bar.</p>
      </header>
      <button type="button" className={`${styles.close} glass glass-btn`} onClick={onClose} aria-label="Close lenses" title="Close (esc)">
        <Icon name="close" />
      </button>
      <div ref={track} className={styles.track} role="list" aria-label="Lenses">
        {lenses.map((l, i) => (
          <article
            key={l.id}
            ref={(n) => {
              cards.current[i] = n;
            }}
            className={styles.card}
            data-on={i === centred || undefined}
            role="listitem"
            aria-label={l.name}
          >
            <button type="button" className={styles.view} data-view onClick={() => (i === centred ? choose(l.id) : center(i))} aria-label={i === centred ? `Use ${l.name}` : `Show ${l.name}`}>
              <div className={styles.screen} data-screen aria-hidden="true" />
              {l.id === 'folders' && <span className={styles.still} aria-hidden="true" dangerouslySetInnerHTML={{ __html: l.icon }} />}
              {l.id === current && <span className={`${styles.badge} mono`}>Now</span>}
            </button>
            <Meta
              def={l}
              pinned={pins.includes(l.id)}
              onPin={onPin}
              onRemove={() => {
                onPin(l.id, false);
                if (l.author === 'server') {
                  // Kept on the server: gone from every device.
                  void op('lens.remove', { id: l.id, everyone: l.everyone ?? false })
                    .then(() => invalidateQueries(keys.serverLenses))
                    .catch(() => undefined);
                  return;
                }
                removeLens(l.id);
                onLensesChanged();
              }}
            />
          </article>
        ))}
        <article
          ref={(n) => {
            cards.current[lenses.length] = n;
          }}
          className={`${styles.card} ${styles.makeCard}`}
          data-on={centred === lenses.length || undefined}
          role="listitem"
          aria-label="Make a lens"
        >
          <MakeLens
            options={opts}
            onFocus={() => center(lenses.length)}
            onKeep={(spec, use) => {
              keepLens(spec);
              onPin(spec.id, true);
              onLensesChanged();
              if (use) onChoose(spec.id);
            }}
          />
          <div className={styles.meta}>
            <div className={styles.name}>
              <span className={styles.icon} dangerouslySetInnerHTML={{ __html: LENS_ICONS.sparkle }} />
              <b>Your own</b>
              <span className={`${styles.tag} mono`}>Your agent</span>
            </div>
            <p>A lens is a small JSON file: layout code, a little CSS and an item template. Lenses you keep stay on this device and pin like any other.</p>
          </div>
        </article>
      </div>
      <p className={`${styles.foot} mono`}>← → browse · ↵ choose · esc close</p>
    </div>
  );
}

/** Draw the preview at window size, scaled to the card's width; a shorter card shows the middle of it. */
function scaleScreen(screen: HTMLElement, view: HTMLElement): void {
  const k = view.clientWidth / window.innerWidth;
  const dy = Math.min(0, (view.clientHeight - window.innerHeight * k) / 2);
  screen.style.width = `${window.innerWidth}px`;
  screen.style.height = `${window.innerHeight}px`;
  screen.style.transform = `translateY(${dy.toFixed(1)}px) scale(${k})`;
}

function Meta({ def, pinned, onPin, onRemove }: { def: LensDef; pinned: boolean; onPin: (id: string, on: boolean) => void; onRemove: () => void }) {
  const [confirm, setConfirm] = useState(false);
  return (
    <div className={styles.meta}>
      <div className={styles.name}>
        <span className={styles.icon} dangerouslySetInnerHTML={{ __html: def.icon }} />
        <b>{def.name}</b>
        <span className={`${styles.tag} mono`} title={def.author === 'server' ? 'Kept on the server: on all your devices' : undefined}>
          {def.author === 'server' && def.by ? `By ${def.by}` : AUTHOR[def.author]}
        </span>
      </div>
      <p>{def.blurb}</p>
      <div className={styles.actions}>
        <button type="button" className={styles.pin} aria-pressed={pinned} onClick={() => onPin(def.id, !pinned)}>
          <Icon name={pinned ? 'check' : 'plus'} />
          {pinned ? 'In the bar' : 'Add to the bar'}
        </button>
        {(def.author === 'you' || def.author === 'server') &&
          (confirm ? (
            <span className={styles.confirm}>
              Remove this lens?
              <button type="button" className={styles.danger} onClick={onRemove}>
                Remove
              </button>
              <button type="button" className={styles.textBtn} onClick={() => setConfirm(false)}>
                Keep it
              </button>
            </span>
          ) : (
            <button type="button" className={styles.textBtn} onClick={() => setConfirm(true)}>
              Remove
            </button>
          ))}
      </div>
    </div>
  );
}

type MakeStage = { at: 'form' } | { at: 'preview'; spec: KitSpec };

function MakeLens({ options, onFocus, onKeep }: { options: { current: PreviewOptions }; onFocus: () => void; onKeep: (spec: KitSpec, use: boolean) => void }) {
  const [desc, setDesc] = useState('');
  const [reply, setReply] = useState('');
  const [copied, setCopied] = useState<'idle' | 'done' | 'manual'>('idle');
  const [manualPrompt, setManualPrompt] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [stage, setStage] = useState<MakeStage>({ at: 'form' });
  const [runError, setRunError] = useState<string | null>(null);
  const screen = useRef<HTMLDivElement>(null);
  const view = useRef<HTMLDivElement>(null);

  const copy = () => {
    const text = makePrompt(desc, options.current.items);
    const manual = () => {
      setManualPrompt(text);
      setCopied('manual');
    };
    if (!navigator.clipboard) {
      manual();
      return;
    }
    navigator.clipboard.writeText(text).then(() => setCopied('done'), manual);
  };

  const preview = () => {
    setError(null);
    let raw: unknown;
    try {
      raw = parseLensReply(reply);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return;
    }
    const r = validateSpec(raw);
    if (!r.ok) {
      setError(r.error);
      return;
    }
    setRunError(null);
    setStage({ at: 'preview', spec: r.spec });
  };

  // Mount the pasted lens as a live preview, and hear if it stops.
  useEffect(() => {
    if (stage.at !== 'preview' || !screen.current || !view.current) return;
    const spec = stage.spec;
    const host = screen.current;
    scaleScreen(host, view.current);
    const def: LensDef = { id: spec.id, name: spec.name, icon: LENS_ICONS.sparkle, blurb: spec.blurb, author: 'you', load: () => Promise.resolve(kitModule(spec)) };
    let mini: MiniLens | null = null;
    let dead = false;
    let off: (() => void) | null = null;
    void mountMini(def, host, options.current).then((m) => {
      if (dead) {
        m.destroy();
        return;
      }
      mini = m;
      const why = kitFailure(m.view);
      if (why) setRunError(why);
      off = onKitFailure(m.view, (msg) => setRunError(msg));
    });
    return () => {
      dead = true;
      off?.();
      mini?.destroy();
    };
  }, [stage, options]);

  const onFile = (e: ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    if (!f) return;
    if (f.size > 200_000) {
      setError('That file is too big to be a lens.');
      return;
    }
    void f.text().then((t) => {
      setReply(t);
      setError(null);
    });
  };

  if (stage.at === 'preview') {
    return (
      <div ref={view} className={`${styles.view} ${styles.makeView}`} data-view>
        <div ref={screen} className={styles.screen} data-screen />
        <div className={`${styles.previewBar} glass`}>
          <span className={styles.previewName}>
            <b>{stage.spec.name}</b>
            <small>{runError ? `Stopped: ${runError}` : stage.spec.blurb}</small>
          </span>
          <span className={styles.previewActions}>
            <button type="button" className={styles.primary} disabled={Boolean(runError)} onClick={() => onKeep(stage.spec, true)}>
              Keep and use
            </button>
            <button type="button" className={styles.textBtn} disabled={Boolean(runError)} onClick={() => onKeep(stage.spec, false)}>
              Keep
            </button>
            <button type="button" className={styles.textBtn} onClick={() => setStage({ at: 'form' })}>
              Back
            </button>
          </span>
        </div>
      </div>
    );
  }

  // Escape in a field you wrote in leaves the field rather than closing the gallery and losing the text (the shell skips handled Escapes).
  const keepText = (e: ReactKeyboardEvent<HTMLElement>) => {
    const t = e.target;
    if (e.key === 'Escape' && t instanceof HTMLTextAreaElement && !t.readOnly && t.value.trim()) {
      e.preventDefault();
      t.blur();
    }
  };

  return (
    <div className={`${styles.view} ${styles.makeView} ${styles.makeForm}`} data-view onFocusCapture={onFocus}>
      <div className={styles.form} onKeyDown={keepText}>
        <div className={styles.makeTitle}>
          <span className={styles.icon} dangerouslySetInnerHTML={{ __html: LENS_ICONS.sparkle }} />
          <b>Make a lens</b>
        </div>
        <ol className={styles.steps}>
          <li>
            <label htmlFor="lens-desc">Describe how you want to see your library.</label>
            <textarea id="lens-desc" rows={2} maxLength={400} value={desc} placeholder="Posters in the window of a 1990s video shop, with hand-written staff picks" onChange={(e) => setDesc(e.target.value)} />
            <div className={styles.ideas}>
              {IDEAS.map((idea) => (
                <button key={idea} type="button" className={styles.idea} onClick={() => setDesc(idea)}>
                  {idea}
                </button>
              ))}
            </div>
            <div className={styles.row}>
              <button type="button" className={styles.primary} onClick={copy}>
                <Icon name="copy" />
                Copy prompt
              </button>
              <span className={styles.status} aria-live="polite">
                {copied === 'done' && 'Copied. Paste it into Claude or another agent, then paste its answer below.'}
                {copied === 'manual' && 'Copy this prompt into your agent:'}
              </span>
            </div>
            {copied === 'manual' && <textarea className={styles.mono} readOnly rows={4} value={manualPrompt} onFocus={(e) => e.currentTarget.select()} aria-label="Prompt for your agent" />}
          </li>
          <li>
            <label htmlFor="lens-json">Paste the lens your agent wrote, or choose its file.</label>
            <textarea
              id="lens-json"
              className={styles.mono}
              rows={3}
              value={reply}
              spellCheck={false}
              placeholder='{ "name": "…", "layout": "function layout(items, view) { … }", … }'
              onChange={(e) => {
                setReply(e.target.value);
                setError(null);
              }}
            />
            <div className={styles.row}>
              <button type="button" className={styles.primary} onClick={preview} disabled={!reply.trim()}>
                Preview
              </button>
              <label className={styles.textBtn}>
                Choose a file…
                <input type="file" accept=".json,application/json,text/plain" className="visually-hidden" onChange={onFile} />
              </label>
            </div>
            {error && (
              <p className={styles.error} role="alert">
                {error}
              </p>
            )}
          </li>
        </ol>
      </div>
    </div>
  );
}
