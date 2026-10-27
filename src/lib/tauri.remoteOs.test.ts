import { beforeEach, describe, expect, it, vi } from 'vitest';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

import { WINDOWS_PROBE_MARKER, WINDOWS_TERMINATED_MARKER } from '@/lib/remoteSystem';
import { detectRemoteOs, forgetRemoteOs, sshTerminateProcess } from '@/lib/tauri';

/** 按命令内容回话：uname 与 PowerShell 探针各自一份预设输出。 */
function remote(replies: { uname?: string; probe?: string; other?: string }) {
  invoke.mockImplementation(async (_cmd: string, args: { command: string }) => {
    if (args.command.startsWith('uname')) return replies.uname ?? '';
    if (args.command.startsWith('powershell.exe') && args.command.length < 200) {
      return replies.probe ?? '';
    }
    return replies.other ?? '';
  });
}

const commandsRun = () => invoke.mock.calls.map((call) => call[1].command as string);

describe('detectRemoteOs', () => {
  beforeEach(() => {
    invoke.mockReset();
    forgetRemoteOs('s1');
  });

  it('POSIX 主机只跑一条 uname，不碰 PowerShell', async () => {
    remote({ uname: 'Linux\n' });
    await expect(detectRemoteOs('s1')).resolves.toBe('linux');
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it('uname 认不出（cmd.exe / PowerShell）才用 PowerShell 探针确认 Windows', async () => {
    remote({ uname: 'unknown', probe: `${WINDOWS_PROBE_MARKER}\n` });
    await expect(detectRemoteOs('s1')).resolves.toBe('windows');
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it('结果缓存；同一时刻的并发询问共用一次探测', async () => {
    remote({ uname: 'Darwin\n' });
    const [a, b] = await Promise.all([detectRemoteOs('s1'), detectRemoteOs('s1')]);
    expect([a, b]).toEqual(['macos', 'macos']);
    await detectRemoteOs('s1');
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it('认不出 = null 且不缓存（受限 shell 之后还能重试）', async () => {
    remote({ uname: 'unknown', probe: 'nope' });
    await expect(detectRemoteOs('s1')).resolves.toBeNull();
    remote({ uname: 'Linux\n' });
    await expect(detectRemoteOs('s1')).resolves.toBe('linux');
  });

  it('命令出错：向上抛，且不缓存失败', async () => {
    invoke.mockRejectedValueOnce(new Error('断了'));
    await expect(detectRemoteOs('s1')).rejects.toThrow('断了');
    remote({ uname: 'Linux\n' });
    await expect(detectRemoteOs('s1')).resolves.toBe('linux');
  });

  it('forgetRemoteOs 之后重新探测', async () => {
    remote({ uname: 'Linux\n' });
    await detectRemoteOs('s1');
    forgetRemoteOs('s1');
    await detectRemoteOs('s1');
    expect(invoke).toHaveBeenCalledTimes(2);
  });
});

describe('sshTerminateProcess', () => {
  beforeEach(() => {
    invoke.mockReset();
    forgetRemoteOs('s1');
  });

  it('POSIX：非数字 PID 在到达远端前就被拒绝', async () => {
    remote({ uname: 'Linux\n' });
    await expect(sshTerminateProcess('s1', '1; rm -rf /')).rejects.toThrow('无效的进程 PID');
    expect(commandsRun().some((c) => c.startsWith('kill'))).toBe(false);
  });

  it('POSIX：发 kill -TERM', async () => {
    remote({ uname: 'Linux\n' });
    await sshTerminateProcess('s1', 42);
    expect(commandsRun()).toContain('kill -TERM 42');
  });

  it('Windows：收到终止标记算成功', async () => {
    remote({ uname: '', probe: WINDOWS_PROBE_MARKER, other: WINDOWS_TERMINATED_MARKER });
    await expect(sshTerminateProcess('s1', '42')).resolves.toBeUndefined();
  });

  it('Windows：没有标记就报错，不回显原始输出', async () => {
    remote({ uname: '', probe: WINDOWS_PROBE_MARKER, other: '#< CLIXML <Objs/>' });
    await expect(sshTerminateProcess('s1', '42')).rejects.toThrow('未收到远端确认');
  });
});
