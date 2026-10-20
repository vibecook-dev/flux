/**
 * A window-scrolled, virtualised poster grid that stays smooth at 10k+ items:
 *
 * - Only the visible rows (± overscan) are in the DOM (~50–90 cards).
 * - Scroll work happens once per animation frame and only re-renders when the
 *   visible row range changes.
 * - While flinging (fast scroll), cards show their blurhash only and skip image
 *   requests; images load as soon as scrolling slows down.
 * - Keyboard/remote: roving tabindex with Arrow keys, Page Up/Down, Home/End,
 *   following the WAI-ARIA grid pattern (aria-rowcount/rowindex on a virtual grid).
 */
import {
  type KeyboardEvent,
  type ReactNode,
  memo,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type Ref,
} from 'react';
import styles from './VirtualGrid.module.css';

export interface VirtualGridHandle {
  scrollToIndex(index: number): void;
}

export interface CellArgs {
  index: number;
  width: number;
  tabIndex: number;
  deferImage: boolean;
  onKeyDown: (e: KeyboardEvent<HTMLAnchorElement>) => void;
}

export interface VirtualGridProps {
  count: number;
  /** Render a cell; `index` is in [0, count). */
  renderCell: (args: CellArgs) => ReactNode;
  /** Minimum card width in px (columns are derived from the container width). */
  minCellWidth?: number;
  /** Image height / width for the card art (2:3 posters = 1.5). */
  aspect?: number;
  /** Height of the caption under the art. */
  captionHeight?: number;
  /** Vertical space taken by sticky headers, for keyboard scrolling. */
  stickyOffset?: number;
  label: string;
  ref?: Ref<VirtualGridHandle>;
}

const OVERSCAN_ROWS = 2;
const FAST_PX_PER_MS = 4;

function layout(width: number, minCellWidth: number) {
  const gap = width < 560 ? 12 : width < 1100 ? 16 : 22;
  // Phones get three columns: a big library should never be a two-poster scroll.
  const minCell = width < 560 ? Math.min(minCellWidth, 100) : minCellWidth;
  const cols = Math.max(2, Math.floor((width + gap) / (minCell + gap)));
  const cell = (width - gap * (cols - 1)) / cols;
  return { gap, cols, cell };
}

export const VirtualGrid = memo(function VirtualGrid({
  count,
  renderCell,
  minCellWidth = 160,
  aspect = 1.5,
  captionHeight = 50,
  stickyOffset = 0,
  label,
  ref,
}: VirtualGridProps) {
  const rootRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  const [range, setRange] = useState<[number, number]>([0, 6]);
  const [fast, setFast] = useState(false);
  const [active, setActive] = useState(0);
  const focusPending = useRef(false);

  const { gap, cols, cell } = layout(width || 1200, minCellWidth);
  const rowHeight = Math.round(cell * aspect + captionHeight + gap);
  const rows = Math.ceil(count / cols);

  // Container width.
  useLayoutEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    setWidth(el.clientWidth);
    const ro = new ResizeObserver(([entry]) => setWidth(entry.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Visible rows, recomputed at most once per frame.
  const measure = useRef({ lastY: 0, lastT: 0 });
  const update = useCallback(() => {
    const el = rootRef.current;
    if (!el) return;
    const top = el.getBoundingClientRect().top + window.scrollY;
    const y = window.scrollY - top;
    const first = Math.max(0, Math.floor(y / rowHeight) - OVERSCAN_ROWS);
    const last = Math.min(rows - 1, Math.ceil((y + window.innerHeight) / rowHeight) + OVERSCAN_ROWS);
    setRange((prev) => (prev[0] === first && prev[1] === last ? prev : [first, last]));
    const now = performance.now();
    const m = measure.current;
    const speed = Math.abs(window.scrollY - m.lastY) / Math.max(1, now - m.lastT);
    m.lastY = window.scrollY;
    m.lastT = now;
    if (speed > FAST_PX_PER_MS) setFast(true);
  }, [rowHeight, rows]);

  useEffect(() => {
    let raf = 0;
    let idle: ReturnType<typeof setTimeout> | null = null;
    const onScroll = () => {
      if (!raf)
        raf = requestAnimationFrame(() => {
          raf = 0;
          update();
        });
      if (idle) clearTimeout(idle);
      idle = setTimeout(() => setFast(false), 140);
    };
    update();
    window.addEventListener('scroll', onScroll, { passive: true });
    window.addEventListener('resize', onScroll);
    return () => {
      cancelAnimationFrame(raf);
      if (idle) clearTimeout(idle);
      window.removeEventListener('scroll', onScroll);
      window.removeEventListener('resize', onScroll);
    };
  }, [update]);

  const scrollRowIntoView = useCallback(
    (row: number, align: 'nearest' | 'start' = 'nearest') => {
      const el = rootRef.current;
      if (!el) return;
      const top = el.getBoundingClientRect().top + window.scrollY + row * rowHeight;
      const viewTop = window.scrollY + stickyOffset;
      const viewBottom = window.scrollY + window.innerHeight;
      if (align === 'start' || top < viewTop) window.scrollTo({ top: top - stickyOffset - 8 });
      else if (top + rowHeight > viewBottom) window.scrollTo({ top: top + rowHeight - window.innerHeight + 8 });
    },
    [rowHeight, stickyOffset],
  );

  useImperativeHandle(
    ref,
    () => ({
      scrollToIndex(index: number) {
        const i = Math.max(0, Math.min(count - 1, index));
        setActive(i);
        scrollRowIntoView(Math.floor(i / cols), 'start');
      },
    }),
    [cols, count, scrollRowIntoView],
  );

  const onKeyDown = useCallback(
    (e: KeyboardEvent<HTMLAnchorElement>) => {
      const pageRows = Math.max(1, Math.floor((window.innerHeight - stickyOffset) / rowHeight) - 1);
      let next: number;
      switch (e.key) {
        case 'ArrowRight':
          next = active + 1;
          break;
        case 'ArrowLeft':
          next = active - 1;
          break;
        case 'ArrowDown':
          next = active + cols;
          break;
        case 'ArrowUp':
          next = active - cols;
          break;
        case 'PageDown':
          next = active + cols * pageRows;
          break;
        case 'PageUp':
          next = active - cols * pageRows;
          break;
        case 'Home':
          next = e.ctrlKey ? 0 : active - (active % cols);
          break;
        case 'End':
          next = e.ctrlKey ? count - 1 : Math.min(count - 1, active - (active % cols) + cols - 1);
          break;
        default:
          return;
      }
      e.preventDefault();
      next = Math.max(0, Math.min(count - 1, next));
      if (next === active) return;
      focusPending.current = true;
      setActive(next);
      scrollRowIntoView(Math.floor(next / cols));
    },
    [active, cols, count, rowHeight, scrollRowIntoView, stickyOffset],
  );

  // Move DOM focus once the target cell is rendered.
  useLayoutEffect(() => {
    if (!focusPending.current) return;
    const target = rootRef.current?.querySelector<HTMLElement>(`[data-index="${active}"]`);
    if (target) {
      focusPending.current = false;
      target.focus({ preventScroll: true });
    }
  });

  const [first, last] = range;
  const activeRow = Math.floor(active / cols);
  // Keep one cell tabbable even when the active one has scrolled out of the DOM.
  const tabbable = activeRow >= first && activeRow <= last ? active : first * cols;

  const rowEls: ReactNode[] = [];
  for (let r = first; r <= last && r < rows; r++) {
    const cells: ReactNode[] = [];
    for (let c = 0; c < cols; c++) {
      const index = r * cols + c;
      if (index >= count) break;
      cells.push(
        <div
          role="gridcell"
          key={index}
          className={styles.cell}
          onFocus={() => {
            if (index !== active) setActive(index);
          }}
        >
          {renderCell({ index, width: cell, tabIndex: index === tabbable ? 0 : -1, deferImage: fast, onKeyDown })}
        </div>,
      );
    }
    rowEls.push(
      <div
        role="row"
        aria-rowindex={r + 1}
        key={r}
        className={styles.row}
        style={{ transform: `translateY(${r * rowHeight}px)`, gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`, columnGap: gap }}
      >
        {cells}
      </div>,
    );
  }

  return (
    <div
      ref={rootRef}
      role="grid"
      aria-label={label}
      aria-rowcount={rows}
      aria-colcount={cols}
      className={styles.grid}
      style={{ height: rows * rowHeight }}
    >
      {width > 0 && rowEls}
    </div>
  );
});
