/**
 * Whether this screen is on its way out: the player fading away over the page you go
 * back to, or the shell under a player fading in (App.tsx). A screen on its way out
 * takes no keys (an Escape meant for the page coming in would go back twice).
 */
import { createContext, useContext } from 'react';

export const ScreenLeavingContext = createContext(false);

export function useScreenLeaving(): boolean {
  return useContext(ScreenLeavingContext);
}
