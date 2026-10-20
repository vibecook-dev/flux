/** A QR code as one SVG path (crisp at any size, no canvas). Always dark-on-white with a quiet zone, so phones scan it in either theme. */
import qrcode from 'qrcode-generator';
import { useMemo } from 'react';

export function QrCode({ value, size = 220, label }: { value: string; size?: number; label: string }) {
  const { d, n } = useMemo(() => {
    const qr = qrcode(0, 'M');
    qr.addData(value);
    qr.make();
    const count = qr.getModuleCount();
    let path = '';
    for (let r = 0; r < count; r++) {
      for (let c = 0; c < count; c++) {
        if (qr.isDark(r, c)) path += `M${c + 4} ${r + 4}h1v1h-1z`;
      }
    }
    return { d: path, n: count + 8 };
  }, [value]);
  return (
    <svg viewBox={`0 0 ${n} ${n}`} width={size} height={size} role="img" aria-label={label} shapeRendering="crispEdges" style={{ borderRadius: 12 }}>
      <rect width={n} height={n} fill="#ffffff" />
      <path d={d} fill="#0b0b0e" />
    </svg>
  );
}
