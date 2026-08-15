import { DEFAULT_TERMINAL_COLORS, TERMINAL_COLOR_PRESETS } from '@/lib/constants';
import type { TerminalColors } from '@/lib/types';

/** Convert a #rgb / #rrggbb color to an rgba() string. */
export function hexToRgba(hex: string, alpha: number): string {
  const h = (hex || '').replace('#', '');
  const full = h.length === 3 ? h.split('').map((c) => c + c).join('') : h;
  const num = parseInt(full, 16);
  if (Number.isNaN(num) || full.length !== 6) return `rgba(0,0,0,${alpha})`;
  return `rgba(${(num >> 16) & 255},${(num >> 8) & 255},${num & 255},${alpha})`;
}

/**
 * 终端配色跟随应用主题（已移除独立终端颜色主题）：
 * 浅色主题 -> “亮色”终端配色，深色主题 -> “暗色”终端配色。
 */
export function resolveTerminalThemeColors(theme: 'light' | 'dark'): TerminalColors {
  const preset =
    theme === 'light'
      ? TERMINAL_COLOR_PRESETS.find((p) => p.name === '亮色')
      : TERMINAL_COLOR_PRESETS[0];
  return preset?.colors ?? DEFAULT_TERMINAL_COLORS;
}

/**
 * 亚克力开启时，让 xterm 背景半透明，透出下方的毛玻璃/桌面。
 */
export function resolveTerminalBackground(
  colors: TerminalColors,
  acrylicOn: boolean,
): TerminalColors {
  if (!acrylicOn) return colors;
  return { ...colors, background: hexToRgba(colors.background, 0.62) };
}
