/** The detail page's poster: the width it is drawn at, and the image that shows (warmed while you point at a title). */
import type { ItemSummary } from '../api/types';
import { srcFor } from '../lens/flight';

export const DETAIL_POSTER_W = 360;

export function detailPosterSrc(item: Pick<ItemSummary, 'poster'>): string | null {
  return srcFor(item.poster, DETAIL_POSTER_W);
}
