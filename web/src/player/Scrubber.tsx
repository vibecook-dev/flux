/**
 * The scrub bar. It draws itself, every frame, outside React: the played part,
 * the buffered part and the knob move by transform (on the compositor), and
 * the clock beside it changes only when its second does. What it shows comes
 * from player/playhead.ts: a drag, a seek on its way (held until the video
 * lands, so the playhead never jumps back to where it was), or the video's
 * clock, smoothed.
 *
 * Pointer: press anywhere on the bar and the playhead is there at once; drag
 * and it follows the pointer (captured, so you can drag past the ends); let go
 * and the video seeks. A bubble above the pointer says the time there (and
 * the chapter). Keyboard: it is a slider; Home/End and Page Up/Down move it,
 * the arrows are the player's ±10 s.
 *
 * The loop runs only while the controls are on screen and something moves.
 */
import { type RefObject, useEffect, useLayoutEffect, useRef } from 'react';
import { formatClock } from '../api/format';
import { bufferedEnd, type Clock, fraction, pendingTarget, shownTime } from './playhead';
import styles from './Player.module.css';

export interface Chapter {
  /** Seconds. */
  start: number;
  title: string | null;
}

interface Props {
  video: RefObject<HTMLVideoElement | null>;
  /** The player's playhead clock (one for the player's life). */
  clock: RefObject<Clock>;
  /** Seconds. */
  duration: number;
  chapters: Chapter[];
  /** The controls are on screen (no drawing while they're not). */
  shown: boolean;
  onSeek: (t: number) => void;
  /** Any interaction: keeps the controls up. */
  onActivity: () => void;
  /** Where the playhead is, for the island when you leave. */
  position: { current: number };
}

/** Playing and moving: not paused, seeking, ended or waiting for data. */
function advancing(v: HTMLVideoElement): boolean {
  return !v.paused && !v.seeking && !v.ended && v.readyState >= 3;
}

export function Scrubber({ video, clock, duration, chapters, shown, onSeek, onActivity, position }: Props) {
  const rootRef = useRef<HTMLDivElement>(null);
  const fillRef = useRef<HTMLSpanElement>(null);
  const bufRef = useRef<HTMLSpanElement>(null);
  const hoverRef = useRef<HTMLSpanElement>(null);
  const knobRef = useRef<HTMLSpanElement>(null);
  const bubbleRef = useRef<HTMLSpanElement>(null);
  const bubbleTimeRef = useRef<HTMLSpanElement>(null);
  const bubbleChapterRef = useRef<HTMLSpanElement>(null);
  const elapsedRef = useRef<HTMLSpanElement>(null);
  const remainingRef = useRef<HTMLSpanElement>(null);
  // Everything the frame loop reads, kept current without re-binding it.
  const live = useRef({ duration, chapters, shown, onSeek, onActivity });
  useLayoutEffect(() => {
    live.current = { duration, chapters, shown, onSeek, onActivity };
  });
  const kickRef = useRef<() => void>(() => undefined);

  useEffect(() => {
    const root = rootRef.current;
    const v = video.current;
    if (!root || !v) return;
    const st = {
      raf: 0,
      width: root.clientWidth,
      left: 0,
      dragging: false,
      dragTime: 0,
      pointer: -1,
      hovering: false,
      hoverX: 0,
      bubbleW: 0,
      bubbleText: '',
      lastF: -1,
      lastW: -1,
      lastBufF: -1,
      lastBufAt: 0,
      lastSec: -1,
      lastDur: -1,
    };

    const timeAt = (clientX: number) => {
      const f = Math.min(1, Math.max(0, (clientX - st.left) / Math.max(1, st.width)));
      return f * live.current.duration;
    };

    const chapterAt = (t: number): string | null => {
      let name: string | null = null;
      for (const c of live.current.chapters) if (c.start <= t) name = c.title;
      return name;
    };

    const draw = () => {
      const now = performance.now();
      const dur = live.current.duration;
      const t = st.dragging ? st.dragTime : shownTime(clock.current, { time: v.currentTime, advancing: advancing(v), rate: v.playbackRate || 1 }, now);
      position.current = st.dragging ? (pendingTarget(clock.current) ?? v.currentTime) : t;
      const w = st.width;
      const resized = w !== st.lastW;
      st.lastW = w;
      const f = fraction(t, dur);
      if (f !== st.lastF || resized) {
        st.lastF = f;
        fillRef.current!.style.transform = `translate3d(${((f - 1) * w).toFixed(2)}px,0,0)`;
        knobRef.current!.style.transform = `translate3d(${(f * w).toFixed(2)}px,0,0)`;
      }
      // What's buffered changes slowly: four looks a second.
      if (now - st.lastBufAt > 250 || resized) {
        st.lastBufAt = now;
        const bf = fraction(bufferedEnd(v.buffered, v.currentTime), dur);
        if (bf !== st.lastBufF || resized) {
          st.lastBufF = bf;
          bufRef.current!.style.transform = `translate3d(${((bf - 1) * w).toFixed(2)}px,0,0)`;
        }
      }
      const sec = Math.floor(t);
      if (sec !== st.lastSec || dur !== st.lastDur) {
        st.lastSec = sec;
        st.lastDur = dur;
        elapsedRef.current!.textContent = formatClock(sec * 1000);
        remainingRef.current!.textContent = `−${formatClock(Math.max(0, dur - sec) * 1000)}`;
        root.setAttribute('aria-valuenow', String(sec));
        root.setAttribute('aria-valuetext', `${formatClock(sec * 1000)} of ${formatClock(dur * 1000)}`);
      }
      // The bubble: over the pointer while hovering, over the knob while dragging.
      if (st.hovering || st.dragging) {
        const bx = st.dragging ? f * w : st.hoverX;
        const bt = st.dragging ? st.dragTime : (st.hoverX / Math.max(1, w)) * dur;
        const chapter = chapterAt(bt);
        const text = formatClock(bt * 1000) + (chapter ?? '');
        if (text !== st.bubbleText) {
          st.bubbleText = text;
          bubbleTimeRef.current!.textContent = formatClock(bt * 1000);
          bubbleChapterRef.current!.textContent = chapter ?? '';
          st.bubbleW = bubbleRef.current!.offsetWidth;
        }
        const half = st.bubbleW / 2;
        const x = Math.min(w - half, Math.max(half, bx));
        bubbleRef.current!.style.transform = `translate3d(${(x - half).toFixed(1)}px,0,0)`;
        hoverRef.current!.style.transform = `translate3d(${((Math.min(1, Math.max(0, st.hoverX / Math.max(1, w))) - 1) * w).toFixed(1)}px,0,0)`;
      }
    };

    const running = () => live.current.shown && (st.dragging || st.hovering || pendingTarget(clock.current) !== null || advancing(v));
    const loop = () => {
      st.raf = 0;
      draw();
      if (running()) st.raf = requestAnimationFrame(loop);
    };
    const kick = () => {
      if (!st.raf) st.raf = requestAnimationFrame(loop);
    };
    kickRef.current = kick;

    const measure = () => {
      const r = root.getBoundingClientRect();
      st.left = r.left;
      st.width = r.width;
      kick();
    };
    const ro = new ResizeObserver(measure);
    ro.observe(root);

    // ── pointer ──
    const down = (e: PointerEvent) => {
      if (e.button !== 0 || !e.isPrimary || !(live.current.duration > 0)) return;
      // No focus from a pointer (no focus ring mid-drag); keyboard users reach it with Tab.
      e.preventDefault();
      measure();
      root.setPointerCapture(e.pointerId);
      st.pointer = e.pointerId;
      st.dragging = true;
      st.dragTime = timeAt(e.clientX);
      st.hoverX = e.clientX - st.left;
      root.setAttribute('data-dragging', '');
      live.current.onActivity();
      draw();
      kick();
    };
    const move = (e: PointerEvent) => {
      st.hoverX = Math.min(st.width, Math.max(0, e.clientX - st.left));
      if (st.dragging && e.pointerId === st.pointer) {
        st.dragTime = timeAt(e.clientX);
        live.current.onActivity();
      }
      kick();
    };
    const endDrag = (commit: boolean) => {
      if (!st.dragging) return;
      st.dragging = false;
      root.removeAttribute('data-dragging');
      if (commit) live.current.onSeek(st.dragTime);
      kick();
    };
    const up = (e: PointerEvent) => {
      if (e.pointerId !== st.pointer) return;
      endDrag(true);
      if (root.hasPointerCapture(e.pointerId)) root.releasePointerCapture(e.pointerId);
    };
    const cancel = () => endDrag(false);
    const enter = (e: PointerEvent) => {
      if (e.pointerType !== 'mouse') return;
      measure();
      st.hovering = true;
      st.hoverX = Math.min(st.width, Math.max(0, e.clientX - st.left));
      root.setAttribute('data-hover', '');
      kick();
    };
    const leave = (e: PointerEvent) => {
      if (e.pointerType !== 'mouse') return;
      st.hovering = false;
      root.removeAttribute('data-hover');
      kick();
    };
    root.addEventListener('pointerdown', down);
    root.addEventListener('pointermove', move);
    root.addEventListener('pointerup', up);
    root.addEventListener('pointercancel', cancel);
    root.addEventListener('lostpointercapture', cancel);
    root.addEventListener('pointerenter', enter);
    root.addEventListener('pointerleave', leave);

    // ── the video: any change of state draws (and starts the loop if it should run) ──
    const events = ['play', 'playing', 'pause', 'seeking', 'seeked', 'timeupdate', 'durationchange', 'ratechange', 'waiting', 'progress', 'loadedmetadata', 'ended'];
    for (const ev of events) v.addEventListener(ev, kick);
    measure();

    return () => {
      if (st.raf) cancelAnimationFrame(st.raf);
      kickRef.current = () => undefined;
      ro.disconnect();
      root.removeEventListener('pointerdown', down);
      root.removeEventListener('pointermove', move);
      root.removeEventListener('pointerup', up);
      root.removeEventListener('pointercancel', cancel);
      root.removeEventListener('lostpointercapture', cancel);
      root.removeEventListener('pointerenter', enter);
      root.removeEventListener('pointerleave', leave);
      for (const ev of events) v.removeEventListener(ev, kick);
    };
  }, [video, clock, position]);

  // Coming back on screen (or a new duration): draw the current state at once.
  useEffect(() => {
    kickRef.current();
  }, [shown, duration, chapters]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    const v = video.current;
    if (!v || !(duration > 0)) return;
    const base = pendingTarget(clock.current) ?? v.currentTime;
    let to: number;
    switch (e.key) {
      case 'Home':
        to = 0;
        break;
      case 'End':
        to = duration - 1;
        break;
      case 'PageUp':
        to = base + 60;
        break;
      case 'PageDown':
        to = base - 60;
        break;
      default:
        return; // the arrows are the player's ±10 s
    }
    e.preventDefault();
    e.stopPropagation();
    onActivity();
    onSeek(to);
  };

  return (
    <div className={styles.scrub}>
      <span ref={elapsedRef} className={`${styles.time} mono`}>
        0:00
      </span>
      <div
        ref={rootRef}
        className={styles.scrubber}
        role="slider"
        tabIndex={0}
        aria-label="Seek"
        aria-valuemin={0}
        aria-valuemax={Math.round(duration)}
        data-scrub=""
        onKeyDown={onKeyDown}
      >
        <span className={styles.rail}>
          <span ref={bufRef} className={styles.railBuf} />
          <span ref={hoverRef} className={styles.railHover} />
          <span ref={fillRef} className={styles.railFill} />
          {duration > 0 &&
            chapters.map((c) => <span key={c.start} className={styles.tick} style={{ left: `${(c.start / duration) * 100}%` }} />)}
        </span>
        <span ref={knobRef} className={styles.knob} data-scrub-knob="" />
        <span ref={bubbleRef} className={`${styles.bubble} glass`} aria-hidden="true">
          <span ref={bubbleTimeRef} className="mono" />
          <span ref={bubbleChapterRef} className={styles.bubbleChapter} />
        </span>
      </div>
      <span ref={remainingRef} className={`${styles.time} mono`}>
        −0:00
      </span>
    </div>
  );
}
