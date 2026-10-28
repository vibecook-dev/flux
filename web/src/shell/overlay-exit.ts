/**
 * How a page open over the library (detail, a sheet) leaves: when its address
 * goes (Esc, its back button, the browser's back), the shell keeps it on screen,
 * marked `leaving`, until it says it is `done` (its poster has flown back, its
 * sheet has slid away). One path for every way of closing.
 */
import { createContext, useContext } from 'react';

export interface OverlayExit {
  /** Its address has gone: animate out, then call `done`. */
  leaving: boolean;
  /** Leaving back to the library (true), or under another page opening over it (false: nothing to fly to). */
  toLibrary: boolean;
  /** It was there when the shell came up (a link, back from the player): it shows at once, without its entrance. */
  restored: boolean;
  /** Its leaving is over: the shell takes it away. */
  done: () => void;
}

export const OverlayExitContext = createContext<OverlayExit>({ leaving: false, toLibrary: true, restored: false, done: () => undefined });

export function useOverlayExit(): OverlayExit {
  return useContext(OverlayExitContext);
}
