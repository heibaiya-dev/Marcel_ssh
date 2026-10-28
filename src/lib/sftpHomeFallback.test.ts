import { beforeEach, describe, expect, it, vi } from 'vitest';

const sftpHomeDir = vi.hoisted(() => vi.fn());
vi.mock('@/lib/tauri', () => ({ sftpHomeDir }));

import { resolveHomeFallback } from './sftpHomeFallback';

describe('resolveHomeFallback', () => {
  beforeEach(() => {
    sftpHomeDir.mockReset();
  });

  it('起始目录读不了时返回家目录（serv00：/ 无权限 → /usr/home/u）', async () => {
    sftpHomeDir.mockResolvedValue('/usr/home/u');
    await expect(resolveHomeFallback('s1', '/')).resolves.toBe('/usr/home/u');
    expect(sftpHomeDir).toHaveBeenCalledWith('s1');
  });

  it('家目录就是刚失败的那个目录：没有别处可去，返回 null 让原错误照常显示', async () => {
    sftpHomeDir.mockResolvedValue('/home/u');
    await expect(resolveHomeFallback('s1', '/home/u')).resolves.toBeNull();
  });

  it('取家目录本身失败（连接已断）：返回 null，不再抛出遮住原错误', async () => {
    sftpHomeDir.mockImplementation(async () => {
      throw { kind: 'Ssh', message: '会话不存在' };
    });
    await expect(resolveHomeFallback('s1', '/')).resolves.toBeNull();
  });

  it('服务器返回空串：当作没有', async () => {
    sftpHomeDir.mockResolvedValue('');
    await expect(resolveHomeFallback('s1', '/')).resolves.toBeNull();
  });
});
