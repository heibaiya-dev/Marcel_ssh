import { useMemo } from 'react';
import { buildQrMatrix } from './qrMatrix';

/**
 * 二维码。**永远是白底黑块**，不跟深浅色主题走：扫码器认的是"深色块 + 浅色底"的对比，
 * 暗色主题下把它画成"白块黑底"很多相机直接扫不出来。外围的白边就是规范里的静区。
 */
export function QrCode({ text, size = 176, label }: { text: string; size?: number; label: string }) {
  const matrix = useMemo(() => buildQrMatrix(text), [text]);
  if (!matrix) return null;
  return (
    <svg
      role="img"
      aria-label={label}
      viewBox={`0 0 ${matrix.size} ${matrix.size}`}
      width={size}
      height={size}
      shapeRendering="crispEdges"
      className="rounded-lg"
    >
      <rect width={matrix.size} height={matrix.size} fill="#ffffff" />
      <path d={matrix.path} fill="#000000" />
    </svg>
  );
}
