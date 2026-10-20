/**
 * Which player engine plays a given PlayResponse (research/03 §11):
 *
 * - progressive file → the `<video>` element directly (HTTP Range).
 * - HLS on Apple WebKit (Safari, every iOS browser) → native HLS. iOS MSE
 *   (ManagedMediaSource) has AirPlay/remote-playback strings attached.
 * - HLS elsewhere → hls.js over MSE, even where native HLS exists: Chrome 142+
 *   ships native HLS but early builds had playback-error bugs (Mux incident),
 *   so flux forces MSE there.
 */
import type { DeliveryFormat } from '../api/types';
import { isAppleWebKit } from './detect';

export type PlayerEngine = 'progressive' | 'hls.js' | 'native-hls';

export interface EngineEnv {
  userAgent: string;
  /** `Hls.isSupported()` (MSE with the needed codecs). */
  hlsJsSupported: boolean;
  /** `<video>.canPlayType('application/vnd.apple.mpegurl') !== ''`. */
  nativeHls: boolean;
}

export function choosePlayerEngine(delivery: DeliveryFormat, env: EngineEnv): PlayerEngine | null {
  if (delivery === 'progressive_file') return 'progressive';
  if (isAppleWebKit(env.userAgent) && env.nativeHls) return 'native-hls';
  if (env.hlsJsSupported) return 'hls.js';
  if (env.nativeHls) return 'native-hls';
  return null;
}
