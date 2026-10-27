/**
 * Draws mock art and encodes it as JPEG, off the page's main thread. Started
 * and fed by art-serve.ts: each message names the art and its pixel width, and
 * the answer carries the JPEG (or an error, and the page falls back to SVG).
 */
import { ART_SIZE, drawArt, type ArtDraw } from './art';

interface Job {
  id: number;
  art: ArtDraw;
  width: number;
}

// The DOM lib types `self` as a window; a worker posts without a target origin.
const post = (msg: unknown) => (self as unknown as { postMessage(m: unknown): void }).postMessage(msg);

self.onmessage = async (e: MessageEvent<Job>) => {
  const { id, art, width } = e.data;
  try {
    const [w0, h0] = ART_SIZE[art.kind];
    const scale = width / w0;
    const canvas = new OffscreenCanvas(width, Math.round(h0 * scale));
    // A CPU canvas: a GPU one would draw and read back on the GPU process's main
    // thread, which the lenses' WebGPU frames also run on (and it is faster at
    // poster sizes). Art is opaque.
    const ctx = canvas.getContext('2d', { willReadFrequently: true, alpha: false });
    if (!ctx) throw new Error('no 2D canvas in a worker');
    ctx.scale(scale, scale);
    drawArt(ctx, art);
    post({ id, blob: await canvas.convertToBlob({ type: 'image/jpeg', quality: 0.9 }) });
  } catch (err) {
    post({ id, error: String(err) });
  }
};
