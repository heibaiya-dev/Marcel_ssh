import { create as createQr } from 'qrcode';

/** 二维码四周的空白（以模块为单位）。扫码器靠它找到边界，规范要求至少 4 个。 */
const QUIET_ZONE = 4;

export interface QrMatrix {
  size: number;
  /** 深色模块逐行合并成的 SVG path。 */
  path: string;
}

/**
 * 把文本编码成二维码矩阵并拼成一条 SVG path（纯前端、同步、不经 innerHTML）。
 * 编码失败（内容超过二维码容量）返回 null，由调用方退回"只显示文字"。
 */
export function buildQrMatrix(text: string): QrMatrix | null {
  try {
    const qr = createQr(text, { errorCorrectionLevel: 'M' });
    const size = qr.modules.size;
    const parts: string[] = [];
    for (let row = 0; row < size; row += 1) {
      let col = 0;
      while (col < size) {
        if (!qr.modules.get(row, col)) {
          col += 1;
          continue;
        }
        const start = col;
        while (col < size && qr.modules.get(row, col)) col += 1;
        parts.push(`M${start + QUIET_ZONE} ${row + QUIET_ZONE}h${col - start}v1h-${col - start}z`);
      }
    }
    return { size: size + QUIET_ZONE * 2, path: parts.join('') };
  } catch {
    return null;
  }
}
