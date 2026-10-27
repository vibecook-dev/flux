/**
 * Volume: the mute button, and over it a glass slider that opens while the
 * pointer is on the button (or the slider has keyboard focus). Like the scrub
 * bar it draws itself: the level fills by transform from the element's own
 * `volumechange`, so dragging re-renders nothing. Phones have volume buttons;
 * there the slider stays closed and the button only mutes.
 */
import { type RefObject, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Icon } from '../components/Icon';
import styles from './Player.module.css';

interface Props {
  video: RefObject<HTMLVideoElement | null>;
  muted: boolean;
  silent: boolean;
  /** Set the volume (0..1); the player unmutes when it rises from silence. */
  onLevel: (level: number) => void;
  onToggleMute: () => void;
}

export function Volume({ video, muted, silent, onLevel, onToggleMute }: Props) {
  const [open, setOpen] = useState(false);
  const closeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const trackRef = useRef<HTMLDivElement>(null);
  const fillRef = useRef<HTMLSpanElement>(null);
  const knobRef = useRef<HTMLSpanElement>(null);
  const setLevel = useRef(onLevel);
  useLayoutEffect(() => {
    setLevel.current = onLevel;
  });

  const keepOpen = () => {
    if (closeTimer.current) clearTimeout(closeTimer.current);
    closeTimer.current = null;
    setOpen(true);
  };
  const closeSoon = () => {
    if (closeTimer.current) clearTimeout(closeTimer.current);
    closeTimer.current = setTimeout(() => setOpen(false), 260);
  };
  useEffect(() => () => {
    if (closeTimer.current) clearTimeout(closeTimer.current);
  }, []);

  useEffect(() => {
    const v = video.current;
    const track = trackRef.current;
    if (!v || !track) return;
    let dragging = -1;
    let rect = track.getBoundingClientRect();

    const draw = () => {
      const f = v.muted ? 0 : v.volume;
      const h = track.clientHeight;
      fillRef.current!.style.transform = `translate3d(0,${((1 - f) * h).toFixed(1)}px,0)`;
      knobRef.current!.style.transform = `translate3d(0,${((1 - f) * h).toFixed(1)}px,0)`;
      track.setAttribute('aria-valuenow', String(Math.round(f * 100)));
      track.setAttribute('aria-valuetext', v.muted ? 'Muted' : `${Math.round(f * 100)}%`);
    };
    const set = (f: number) => setLevel.current(Math.min(1, Math.max(0, f)));
    const at = (clientY: number) => 1 - (clientY - rect.top) / Math.max(1, rect.height);

    const down = (e: PointerEvent) => {
      if (e.button !== 0) return;
      e.preventDefault();
      rect = track.getBoundingClientRect();
      dragging = e.pointerId;
      track.setPointerCapture(e.pointerId);
      track.setAttribute('data-dragging', '');
      set(at(e.clientY));
    };
    const move = (e: PointerEvent) => {
      if (e.pointerId === dragging) set(at(e.clientY));
    };
    const end = (e: PointerEvent) => {
      if (e.pointerId !== dragging) return;
      dragging = -1;
      track.removeAttribute('data-dragging');
      if (track.hasPointerCapture(e.pointerId)) track.releasePointerCapture(e.pointerId);
    };
    track.addEventListener('pointerdown', down);
    track.addEventListener('pointermove', move);
    track.addEventListener('pointerup', end);
    track.addEventListener('pointercancel', end);
    v.addEventListener('volumechange', draw);
    draw();
    return () => {
      track.removeEventListener('pointerdown', down);
      track.removeEventListener('pointermove', move);
      track.removeEventListener('pointerup', end);
      track.removeEventListener('pointercancel', end);
      v.removeEventListener('volumechange', draw);
    };
  }, [video]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    const v = video.current;
    if (!v) return;
    const step = e.key === 'ArrowUp' || e.key === 'ArrowRight' ? 0.05 : e.key === 'ArrowDown' || e.key === 'ArrowLeft' ? -0.05 : 0;
    const to = e.key === 'Home' ? 0 : e.key === 'End' ? 1 : step ? (v.muted ? 0 : v.volume) + step : null;
    if (to === null) return;
    e.preventDefault();
    e.stopPropagation();
    onLevel(to);
  };

  return (
    <div
      className={styles.volume}
      data-open={open || undefined}
      onPointerEnter={(e) => e.pointerType === 'mouse' && keepOpen()}
      onPointerLeave={(e) => e.pointerType === 'mouse' && closeSoon()}
      onFocus={keepOpen}
      onBlur={(e) => !e.currentTarget.contains(e.relatedTarget) && closeSoon()}
    >
      <div className={`${styles.volumePop} glass`}>
        <div
          ref={trackRef}
          className={styles.volTrack}
          role="slider"
          tabIndex={0}
          aria-label="Volume"
          aria-orientation="vertical"
          aria-valuemin={0}
          aria-valuemax={100}
          onKeyDown={onKeyDown}
        >
          <span className={styles.volRail}>
            <span ref={fillRef} className={styles.volFill} />
          </span>
          <span ref={knobRef} className={styles.volKnob} />
        </div>
      </div>
      <button type="button" className={styles.btn} onClick={onToggleMute} aria-label={muted ? 'Unmute' : 'Mute'}
        title={muted ? 'Unmute (m)' : 'Mute (m)'}
      >
        <Icon name={muted || silent ? 'mute' : 'volume'} />
      </button>
    </div>
  );
}
