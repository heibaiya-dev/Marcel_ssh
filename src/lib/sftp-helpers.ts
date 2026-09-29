import { parseAppError } from './errors';
import { IMAGE_EXTENSIONS } from './constants';

/**
 * 字节数 → 展示文案。0 显示 `0 B`：列表里「没有大小」由调用方显式给 `-`
 * （目录行），而进度语境复用本函数（`sftpTransferManager.progressText`、
 * `sftpUploadStatus`），`- / 3.2 MB` 会被读成「总量未知」，所以 0 必须是 0 B。
 */
export function formatSize(bytes: number): string {
  if (bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
}

export function getFileExtension(fileName: string): string {
  const idx = fileName.lastIndexOf('.');
  return idx >= 0 ? fileName.slice(idx).toLowerCase() : '';
}

/** 判断文件是否为浏览器可预览的图片格式（与后端 50MB 限制配合使用）。 */
export function isPreviewableImage(fileName: string): boolean {
  return IMAGE_EXTENSIONS.has(getFileExtension(fileName));
}

export function modeToString(mode: number): string {
  const isDir = (mode & 0o170000) === 0o040000;
  const isLink = (mode & 0o170000) === 0o120000;
  const chars = isDir ? 'd' : isLink ? 'l' : '-';
  const perms = [
    mode & 0o400 ? 'r' : '-',
    mode & 0o200 ? 'w' : '-',
    mode & 0o100 ? 'x' : '-',
    mode & 0o040 ? 'r' : '-',
    mode & 0o020 ? 'w' : '-',
    mode & 0o010 ? 'x' : '-',
    mode & 0o004 ? 'r' : '-',
    mode & 0o002 ? 'w' : '-',
    mode & 0o001 ? 'x' : '-',
  ].join('');
  return chars + perms;
}

/**
 * Android 上 dialog 插件在用户取消文件选择/保存对话框时 reject
 * （桌面是返回 null），用于把"用户取消"与真实错误区分开。
 */
export function isDialogCancelled(err: unknown): boolean {
  const msg =
    typeof err === 'string'
      ? err
      : err instanceof Error
        ? err.message
        : String(err ?? '');
  return msg.includes('File picker cancelled');
}

/** SFTP 错误码 → 中文友好提示（对应 Rust `AppError::Sftp { code }`）。 */
const SFTP_CODE_HINTS: Record<number, string> = {
  2: '文件或目录不存在',
  3: '权限不足',
  4: '操作失败',
  5: '错误的文件句柄',
};

/**
 * 在基础文案上补一句 SFTP 错误码的含义。
 *
 * 码在 **`data.code`** 里：Rust 序列化形态是 `{ kind: 'Sftp', message, data: { code } }`
 * （`src-tauri/src/error.rs`）。此前读顶层 `obj.code`，与线上形态不符，是永远走不到的
 * 死分支——用户只看得到 `SFTP error (code 2): ...` 这种机器话。
 */
export function getErrorMessage(err: unknown): string {
  const { message, data } = parseAppError(err);
  const code = data?.code;
  if (typeof code === 'number') {
    const hint = SFTP_CODE_HINTS[code];
    if (hint) return `${message}（${hint}）`;
  }
  return message;
}
