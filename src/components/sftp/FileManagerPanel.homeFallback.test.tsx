// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
(globalThis as Record<string, unknown>).ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

const mocks = vi.hoisted(() => ({
  sftpListDir: vi.fn(),
  sftpHomeDir: vi.fn(),
}));

vi.mock('@/components/terminal/TerminalInstanceManager', () => ({
  terminalInstanceManager: {
    showDisconnectBanner: vi.fn(),
    onReconnected: vi.fn(),
    setStdinEnabled: vi.fn(),
    dispose: vi.fn(),
  },
}));
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn(), save: vi.fn() }));
vi.mock('@tauri-apps/plugin-fs', () => ({ stat: vi.fn() }));
vi.mock('@/lib/tauri', () => ({
  sftpListDir: mocks.sftpListDir,
  sftpHomeDir: mocks.sftpHomeDir,
  sftpMkdir: vi.fn(),
  sftpRemove: vi.fn(),
  sftpRemoveViaShell: vi.fn(),
  sftpRename: vi.fn(),
  sftpReadFile: vi.fn(),
  sftpWriteFile: vi.fn(),
  sftpExtractArchive: vi.fn(),
}));
vi.mock('@/hooks/useSftpUpload', () => ({
  useSftpUpload: () => ({ uploadFile: vi.fn(), pickFolder: vi.fn(), uploadFolder: vi.fn() }),
}));
vi.mock('@/hooks/useSftpDownload', () => ({
  useSftpDownload: () => ({ startDownload: vi.fn() }),
}));
vi.mock('@/stores/settingsStore', () => {
  const stub = {
    settings: {
      fileManagerPath: '/',
      fileManagerPaths: {},
      fileManagerShowHidden: false,
      fileManagerTreeWidth: 200,
      fileManagerTreeUserHidden: false,
    },
    loaded: true,
    update: vi.fn(),
  };
  return {
    useSettingsStore: Object.assign(
      (selector: (s: typeof stub) => unknown) => selector(stub),
      { getState: () => stub },
    ),
  };
});

const FileManagerPanel = (await import('@/components/sftp/FileManagerPanel')).default;

const denied = { kind: 'Ssh', message: 'SSH error: 读取目录失败: Permission denied' };

let container: HTMLDivElement;
let root: Root;

async function mount() {
  await act(async () => {
    root.render(<FileManagerPanel sessionId="sess-1" connectionKey="conn-1" />);
  });
  await act(async () => {});
}

beforeEach(() => {
  localStorage.clear();
  mocks.sftpListDir.mockReset();
  mocks.sftpHomeDir.mockReset();
  mocks.sftpHomeDir.mockResolvedValue('/usr/home/u');
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe('桌面文件管理器：起始目录 `/` 读不了时改去家目录', () => {
  it('serv00 场景：/ 被拒 → 进入家目录并正常列出，不留错误', async () => {
    mocks.sftpListDir.mockImplementation(async (_s: string, path: string) => {
      if (path === '/') throw denied;
      return [];
    });
    await mount();

    expect(mocks.sftpHomeDir).toHaveBeenCalledWith('sess-1');
    const paths = mocks.sftpListDir.mock.calls.map((c) => c[1]);
    expect(paths).toContain('/usr/home/u');
    expect(container.textContent).not.toContain('Permission denied');
  });

  it('家目录也读不了：显示原始错误，家目录只尝试一次', async () => {
    mocks.sftpListDir.mockRejectedValue(denied);
    await mount();

    expect(container.textContent).toContain('Permission denied');
    expect(mocks.sftpHomeDir).toHaveBeenCalledTimes(1);
  });
});
