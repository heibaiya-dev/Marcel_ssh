import { sftpHomeDir } from '@/lib/tauri';

/**
 * 文件管理器**首次加载**失败时的兜底目录（桌面 FileManagerPanel / 移动 MobileFilesHost
 * 共用，唯一来源）。
 *
 * 起始目录默认是 `/`（或上次记住的目录），但共享主机（serv00 等）的普通用户没有根目录
 * 的读权限，记住的目录也可能已被删除——停在一条「Permission denied」上用户无处可去，
 * 看起来就是 SFTP 连不上。OpenSSH 的 `sftp` 登录后落在家目录，这里照做：起始目录读不了，
 * 就改去家目录。
 *
 * 返回要改去的目录；没有可去的地方（连接本身已断、家目录就是刚失败的这个）返回 `null`，
 * 调用方照常展示原始错误。只用于起始目录——用户自己点进去的目录读不了，报错就是答案。
 */
export async function resolveHomeFallback(
  sessionId: string,
  failedPath: string,
): Promise<string | null> {
  try {
    const home = await sftpHomeDir(sessionId);
    return home && home !== failedPath ? home : null;
  } catch {
    return null;
  }
}
