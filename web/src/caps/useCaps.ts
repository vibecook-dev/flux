import { useEffect, useState } from 'react';
import type { ClientCaps } from '../api/types';
import { getClientCaps } from './detect';

/** This browser's detected ClientCaps (null until detection finishes, usually at boot). */
export function useCaps(): ClientCaps | null {
  const [caps, setCaps] = useState<ClientCaps | null>(null);
  useEffect(() => {
    let live = true;
    void getClientCaps().then((c) => live && setCaps(c));
    return () => {
      live = false;
    };
  }, []);
  return caps;
}
