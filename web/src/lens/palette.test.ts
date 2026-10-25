import { describe, expect, it } from 'vitest';
import { averageOfHash, deepen, fallbackPalette, luminance, mixHex, paletteFromPixels, paletteOfHash } from './palette';

describe('palette', () => {
  it('finds the salient colour and the shadow', () => {
    const px: [number, number, number][] = [];
    for (let i = 0; i < 40; i++) px.push([200, 40, 40]); // red field
    for (let i = 0; i < 14; i++) px.push([10, 10, 12]); // shadow
    const p = paletteFromPixels(px);
    expect(p.c1).toBe('#c82828');
    expect(p.c2).toBe('#0a0a0c');
    expect(p.hue).toBeCloseTo(0, 0);
    expect(p.sat).toBeGreaterThan(0.5);
  });

  it('decodes a blurhash, and falls back for titles without art', () => {
    const p = paletteOfHash('LEHV6nWB2yk8pyo0adR*.7kCMdnj', 'Arrival');
    expect(p.c1).toMatch(/^#[0-9a-f]{6}$/);
    expect(paletteOfHash(null, 'Arrival')).toEqual(fallbackPalette('Arrival'));
    expect(paletteOfHash('###', 'Broken')).toEqual(fallbackPalette('Broken'));
  });

  it('deepens a colour until white text reads on it', () => {
    expect(luminance(deepen('#ffcc00'))).toBeLessThanOrEqual(0.09);
    expect(mixHex('#000000', '#ffffff', 0.5)).toBe('#808080');
  });
});

describe('averageOfHash', () => {
  it('reads the DC colour without decoding', () => {
    const a = averageOfHash('LEHV6nWB2yk8pyo0adR*.7kCMdnj');
    expect(a).not.toBeNull();
    expect(a?.sat).toBeGreaterThanOrEqual(0);
    expect(averageOfHash(null)).toBeNull();
    expect(averageOfHash('LE')).toBeNull();
  });
});
