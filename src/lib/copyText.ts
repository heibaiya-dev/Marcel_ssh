import { writeText } from '@tauri-apps/plugin-clipboard-manager';

/**
 * 写系统剪贴板：先走 Tauri 插件（手机 WebView 里 navigator.clipboard 常被拒），
 * 不行再退回浏览器 API。两条都失败时抛出，由调用方决定怎么提示「复制失败」。
 */
export async function copyText(text: string): Promise<void> {
  try {
    await writeText(text);
  } catch {
    await navigator.clipboard.writeText(text);
  }
}
