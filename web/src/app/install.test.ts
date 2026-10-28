import { describe, expect, it } from 'vitest';
import { safariWay } from './install';

const UA = {
  iphoneSafari: 'Mozilla/5.0 (iPhone; CPU iPhone OS 26_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Mobile/15E148 Safari/604.1',
  iphoneChrome: 'Mozilla/5.0 (iPhone; CPU iPhone OS 26_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/142.0.7444.46 Mobile/15E148 Safari/604.1',
  // iPadOS asks for the desktop site by default: it looks like a Mac, with touch.
  ipad: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Safari/605.1.15',
  macSafari26: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Safari/605.1.15',
  macSafari16: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Safari/605.1.15',
  macChrome: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36',
  macEdge: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36 Edg/142.0.0.0',
  macFirefox: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 15.7; rv:145.0) Gecko/20100101 Firefox/145.0',
  android: 'Mozilla/5.0 (Linux; Android 16; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Mobile Safari/537.36',
};

describe('safariWay', () => {
  it('adds to the Home Screen from any browser on iPhone and iPad', () => {
    expect(safariWay(UA.iphoneSafari, 5)).toBe('ios');
    expect(safariWay(UA.iphoneChrome, 5)).toBe('ios');
    expect(safariWay(UA.ipad, 5)).toBe('ios');
  });

  it('adds to the Dock from Safari 17 and later on a Mac', () => {
    expect(safariWay(UA.macSafari26, 0)).toBe('mac-safari');
    expect(safariWay(UA.macSafari16, 0)).toBeNull();
  });

  it('leaves the others to the browser’s own offer, or to nothing', () => {
    expect(safariWay(UA.macChrome, 0)).toBeNull();
    expect(safariWay(UA.macEdge, 0)).toBeNull();
    expect(safariWay(UA.macFirefox, 0)).toBeNull();
    expect(safariWay(UA.android, 5)).toBeNull();
  });
});
