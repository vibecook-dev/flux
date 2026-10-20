import type { ArtRef } from '../api/types';

/** How to label art flux didn't get from a provider: `frame` (no AI) and `generated` (AI). */
export function sourceLabel(art: ArtRef | null): { short: string; long: string; tone: 'frame' | 'ai' } | null {
  if (!art) return null;
  if (art.source === 'generated') return { short: 'AI art', long: 'AI-generated artwork, made on this server', tone: 'ai' };
  if (art.source === 'frame') return { short: 'Frame', long: 'A frame from the video, typeset by flux (no AI)', tone: 'frame' };
  return null;
}
