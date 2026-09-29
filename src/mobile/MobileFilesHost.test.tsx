// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { SftpFileEntry } from '@/lib/types';
import type { StoredTransferItem } from '@/stores/transferStore';

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

// jsdom 不实现 ResizeObserver，MobileSheet 用它测滚动区域 —— 补个空壳即可
(globalThis as Record<string, unknown>).ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

const mocks = vi.hoisted(() => ({
  sftpListDir: vi.fn(),
  sftpRename: vi.fn(async () => {}),
  writeText: vi.fn(async () => {}),
}));

vi.mock('@/lib/tauri', () => ({
  sftpListDir: mocks.sftpListDir,
  sftpRename: mocks.sftpRename,
  saveSettings: vi.fn(async () => {}),
  mobileSetAppForeground: vi.fn(async () => {}),
  sftpExtractArchive: vi.fn(async () => {}),
  sftpLocalFileName: vi.fn(async () => 'x'),
  sftpMkdir: vi.fn(async () => {}),
  sftpRemove: vi.fn(async () => {}),
  sftpRemoveViaShell: vi.fn(async () => {}),
  sftpWriteFile: vi.fn(async () => {}),
  isContentUri: (p: string) => p.startsWith('content://'),
}));

// 只保留真实的 MobileFilesHost（被测层）与 App 的返回键兜底；其余宿主/浮层与本
// 用例无关，抹平成空组件，避免把整套 store 依赖拖进来。
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn(async () => null) }));
vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({
  writeText: mocks.writeText,
}));
vi.mock('@/hooks/useSftpUpload', () => ({
  useSftpUpload: () => ({ uploadFile: vi.fn() }),
}));
vi.mock('@/hooks/useSftpDownload', () => ({
  useSftpDownload: () => ({ startDownload: vi.fn() }),
}));
vi.mock('@/stores/transferScheduler', () => ({ cancelTransfer: vi.fn() }));
vi.mock('./mobileBridge', () => ({
  withForegroundKeepAlive: (_enabled: boolean, run: () => unknown) => run(),
}));
// 压缩面板换成一行可见的标记，用来断言「拿到的是哪个目录」
vi.mock('./MobileCompressSheet', () => ({
  default: ({ remoteDir }: { remoteDir: string }) => (
    <div data-compress-dir={remoteDir} />
  ),
}));
vi.mock('./MobileFileEditor', () => ({ default: () => null }));
vi.mock('./MobileImageViewer', () => ({ default: () => null }));
vi.mock('./MobileTerminalHost', () => ({ default: () => null }));
vi.mock('./MobileAgentHost', () => ({ default: () => null }));
vi.mock('./MobileSettings', () => ({ default: () => null }));
vi.mock('./MobileOnboarding', () => ({ default: () => null }));
vi.mock('./MobileUpdateToast', () => ({ default: () => null }));
vi.mock('./MobileUpdateProgress', () => ({ default: () => null }));
vi.mock('./MobileStarPromptSheet', () => ({ default: () => null }));
vi.mock('./MobileGlobalInteractionOverlay', () => ({ default: () => null }));
vi.mock('./bootstrap', () => ({
  bootstrapMobileApp: vi.fn(async () => {}),
}));
vi.mock('@/stores/interactionStore', () => ({
  initInteractionListener: vi.fn(() => () => {}),
}));
vi.mock('@/stores/jobWake', () => ({ initJobWake: vi.fn(() => () => {}) }));
vi.mock('@/stores/jobStore', () => ({
  useJobStore: {
    getState: () => ({
      initEventListener: () => () => {},
      fetchJobs: async () => {},
    }),
  },
}));
vi.mock('@/stores/updateStore', () => ({
  useUpdateStore: { getState: () => ({ init: async () => {} }) },
}));
vi.mock('@/stores/sftpTransferManager', () => ({
  attachTransferListeners: vi.fn(),
  detachTransferListeners: vi.fn(),
}));

import MobileApp from './App';
import { useSessionStore } from '@/stores/sessionStore';
import { useSettingsStore } from '@/stores/settingsStore';
import { useTransferStore } from '@/stores/transferStore';
import { resetBackHandlers } from './backHandler';

const SESSION = {
  id: 's1',
  connectionId: 'user@host:22',
  configId: 'cfg-1',
  status: 'connected' as const,
  createdAt: '2026-01-01T00:00:00.000Z',
};

const DIR: SftpFileEntry = {
  name: 'docs',
  is_dir: true,
  is_file: false,
  is_symlink: false,
  size: 0,
  mode: 0o755,
};
const FILE: SftpFileEntry = {
  name: 'a.txt',
  is_dir: false,
  is_file: true,
  is_symlink: false,
  size: 12,
  mode: 0o644,
};

let container: HTMLDivElement;
let root: Root;

function setRememberedPath(path: string | undefined) {
  const settings = useSettingsStore.getState().settings;
  useSettingsStore.setState({
    loaded: true,
    settings: {
      ...settings,
      hasCompletedOnboarding: true,
      fileManagerPath: '/',
      fileManagerPaths: path ? { 'cfg-1': path } : {},
      mobileBackgroundSettings: {
        ...settings.mobileBackgroundSettings,
        keepAliveEnabled: false,
      },
    },
  });
}

function findButton(label: string): HTMLButtonElement {
  const button = [...container.querySelectorAll('button')].find((b) =>
    b.textContent?.includes(label),
  );
  expect(button, `未找到「${label}」按钮`).toBeTruthy();
  return button as HTMLButtonElement;
}

function findBodyButton(label: string): HTMLButtonElement {
  const button = [...document.body.querySelectorAll('button')].find((b) =>
    b.textContent?.includes(label),
  );
  expect(button, `未找到「${label}」按钮`).toBeTruthy();
  return button as HTMLButtonElement;
}

async function click(button: HTMLElement) {
  await act(async () => {
    button.dispatchEvent(new MouseEvent('click', { bubbles: true }));
  });
  await act(async () => {});
}

function activeTabLabel(): string {
  const nav = container.querySelector('nav[aria-label="主导航"]');
  const active = nav?.querySelector('button[aria-current="page"]');
  return active?.textContent ?? '';
}

async function switchTab(label: string) {
  const nav = container.querySelector('nav[aria-label="主导航"]');
  const button = [...(nav?.querySelectorAll('button') ?? [])].find((b) =>
    b.textContent?.includes(label),
  );
  expect(button, `未找到「${label}」标签`).toBeTruthy();
  await click(button as HTMLButtonElement);
}

async function mountFilesTab() {
  await act(async () => {
    root.render(<MobileApp />);
  });
  await switchTab('文件');
  await act(async () => {});
}

/** 入队 + 直接推进到目标状态（跳过调度器，模拟传输事件后的 store） */
async function putTransfer(
  id: string,
  patch: Partial<StoredTransferItem> = {},
) {
  await act(async () => {
    useTransferStore.getState().addItem({
      id,
      kind: 'download',
      sessionId: 's1',
      fileName: 'a.txt',
      localPath: '/local/a.txt',
      remotePath: '/srv/a.txt',
      written: 12,
      total: 12,
      statusText: '传输中',
      createdAt: 1,
      ...patch,
    });
  });
  await act(async () => {
    useTransferStore.getState().updateItem(id, patch);
  });
}

beforeEach(() => {
  mocks.sftpListDir.mockReset();
  mocks.sftpListDir.mockResolvedValue([DIR, FILE]);
  mocks.sftpRename.mockClear();
  mocks.writeText.mockClear();
  resetBackHandlers();
  useSessionStore.setState({
    sessions: { s1: SESSION },
    activeSessionId: 's1',
  });
  useTransferStore.setState({ items: {}, order: [], open: false });
  setRememberedPath('/home/user');
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  resetBackHandlers();
});

/**
 * 文件页 vs 标签页兜底：返回键的优先级。
 *
 * backHandler 是 LIFO 栈，而 React 同一 commit 里子组件的 useEffect 先于父组件的
 * useEffect 执行。若 App 的「切回终端页」也在 useEffect 里注册，切回文件页那一次
 * 提交就会把它压在文件页「回上一级」之上 —— 第一次返回跳终端页（而不是回上级）。
 */
describe('返回键：文件页优先于标签页兜底', () => {
  it('从终端页切回文件页后，第一次返回是回上一级', async () => {
    await mountFilesTab();
    expect(mocks.sftpListDir).toHaveBeenLastCalledWith('s1', '/home/user');
    expect(activeTabLabel()).toContain('文件');

    let consumed = false;
    await act(async () => {
      consumed = window.__marcelHandleBack?.() ?? false;
    });
    await act(async () => {});

    expect(consumed).toBe(true);
    expect(mocks.sftpListDir).toHaveBeenLastCalledWith('s1', '/home');
    expect(activeTabLabel()).toContain('文件');
  });

  it('在文件页内导航过一次（handler 重新注册）后仍回上一级', async () => {
    await mountFilesTab();

    const pathButton = [...container.querySelectorAll('button')].find(
      (b) => b.textContent === 'home',
    );
    expect(pathButton).toBeTruthy();
    await click(pathButton as HTMLButtonElement);
    expect(mocks.sftpListDir).toHaveBeenLastCalledWith('s1', '/home');

    await act(async () => {
      window.__marcelHandleBack?.();
    });
    await act(async () => {});

    expect(activeTabLabel()).toContain('文件');
    expect(mocks.sftpListDir).toHaveBeenLastCalledWith('s1', '/');
  });

  it('在根目录（没有上一级）时返回键仍然切回终端页', async () => {
    setRememberedPath(undefined);
    await mountFilesTab();
    expect(mocks.sftpListDir).toHaveBeenLastCalledWith('s1', '/');

    await act(async () => {
      window.__marcelHandleBack?.();
    });
    await act(async () => {});

    expect(activeTabLabel()).toContain('终端');
  });
});

/**
 * 传输成功后必须就地可见：顶部活动条只认 active/cancelling，条目一到 done
 * 就整条消失，用户既不知道成没成，也拿不到文件落点。
 */
describe('传输完成提示', () => {
  it('下载完成后展示「已保存到」本地路径，并能复制该路径', async () => {
    await mountFilesTab();
    await putTransfer('d1', {
      status: 'done',
      statusText: 'a.txt 下载完成',
      finishedAt: Date.now(),
    });

    expect(container.textContent).toContain('已保存到 /local/a.txt');
    await click(findButton('复制路径'));
    expect(mocks.writeText).toHaveBeenCalledWith('/local/a.txt');
  });

  it('上传完成后展示「已上传到」远端路径', async () => {
    await mountFilesTab();
    await putTransfer('u1', {
      kind: 'upload',
      status: 'done',
      statusText: 'a.txt 上传完成',
      remotePath: '/home/user/a.txt',
      finishedAt: Date.now(),
    });

    expect(container.textContent).toContain('已上传到 /home/user/a.txt');
  });

  it('不补播历史完成记录（陈旧条目不提示）', async () => {
    await mountFilesTab();
    await putTransfer('d-old', {
      status: 'done',
      statusText: 'a.txt 下载完成',
      finishedAt: Date.now() - 10 * 60 * 1000,
    });

    expect(container.textContent).not.toContain('已保存到');
  });

  it('失败仍然走错误条，不误报成功', async () => {
    await mountFilesTab();
    await putTransfer('d2', {
      status: 'error',
      statusText: '下载失败：网络中断',
      finishedAt: Date.now(),
    });

    expect(container.textContent).toContain('下载失败：网络中断');
    expect(container.textContent).not.toContain('已保存到');
  });
});

/**
 * 断连态的错误提示：失败常常正是断线那一刻产生的，早退分支
 * （「连接已断开」空状态）不能把错误横幅一起挡掉。
 */
describe('断连时错误可见', () => {
  it('连接断开后仍能看到传输失败原因', async () => {
    await mountFilesTab();

    await act(async () => {
      useSessionStore.setState({
        sessions: { s1: { ...SESSION, status: 'disconnected' } },
      });
    });
    expect(container.textContent).toContain('连接已断开');

    // 「会话已断开，任务已跳过」这类失败只在断连后产生 —— 用 ready 才有的
    // sessionId 去扫 store 是扫不到的，必须按当前会话真实 id 扫
    await putTransfer('d3', {
      status: 'error',
      statusText: '会话已断开，任务已跳过',
      finishedAt: Date.now(),
    });

    expect(container.textContent).toContain('会话已断开，任务已跳过');
  });

  it('失败先到、掉线后到，提示也不会被清掉', async () => {
    await mountFilesTab();
    await putTransfer('d4', {
      status: 'error',
      statusText: '下载失败：网络中断',
      finishedAt: Date.now(),
    });
    expect(container.textContent).toContain('下载失败：网络中断');

    await act(async () => {
      useSessionStore.setState({
        sessions: { s1: { ...SESSION, status: 'disconnected' } },
      });
    });

    expect(container.textContent).toContain('连接已断开');
    expect(container.textContent).toContain('下载失败：网络中断');
  });
});

/**
 * 目录在移动端「点按 = 进入」，永远不会成为 selectedEntry，于是单选操作条里的
 * 压缩 / 重命名对目录是死代码。长按进入的选择模式（单选）必须补上这两个动作。
 */
describe('目录的压缩 / 重命名入口', () => {
  async function longPress(entry: SftpFileEntry) {
    const row = [...container.querySelectorAll('button')].find((b) =>
      b.textContent?.includes(entry.name),
    );
    expect(row, `未找到「${entry.name}」行`).toBeTruthy();
    await act(async () => {
      row!.dispatchEvent(
        new MouseEvent('contextmenu', { bubbles: true, cancelable: true }),
      );
    });
    await act(async () => {});
  }

  it('长按目录 → 单选操作条给出「压缩」，打开的是该目录', async () => {
    await mountFilesTab();
    await longPress(DIR);

    expect(container.textContent).toContain('已选 1 个');
    await click(findButton('压缩'));

    expect(
      container.querySelector('[data-compress-dir]')?.getAttribute(
        'data-compress-dir',
      ),
    ).toBe('/home/user/docs');
    // 压缩面板接管后选择模式收尾，避免操作条和面板叠着
    expect(container.textContent).not.toContain('已选 1 个');
  });

  it('长按目录 → 重命名按目录路径调用 sftpRename', async () => {
    await mountFilesTab();
    await longPress(DIR);

    await click(findButton('重命名'));
    const input = document.body.querySelector(
      'input[type="text"]',
    ) as HTMLInputElement | null;
    expect(input?.value).toBe('docs');

    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      'value',
    )!.set!;
    await act(async () => {
      setter.call(input, 'docs2');
      input!.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await click(findBodyButton('确定'));

    expect(mocks.sftpRename).toHaveBeenCalledWith(
      's1',
      '/home/user/docs',
      '/home/user/docs2',
    );
  });

  it('选中多个条目时不提供单项动作（压缩只对单个目录）', async () => {
    await mountFilesTab();
    await longPress(DIR);
    await click(findButton('全选'));

    expect(container.textContent).toContain('已选 2 个');
    expect(
      [...container.querySelectorAll('button')].some(
        (b) => b.textContent === '压缩',
      ),
    ).toBe(false);
    expect(
      [...container.querySelectorAll('button')].some(
        (b) => b.textContent === '重命名',
      ),
    ).toBe(false);
  });
});

/**
 * 上传 / 下载是调度器里两条独立 lane，桌面两端可并行；
 * 用一个总布尔禁用会把另一条道白白堵死。
 */
describe('上传 / 下载互不阻塞', () => {
  it('上传进行中时下载按钮仍可用（反之亦然）', async () => {
    await mountFilesTab();
    await putTransfer('u-active', {
      kind: 'upload',
      status: 'active',
      statusText: '正在上传 a.txt ...',
    });

    expect(findButton('上传').disabled).toBe(true);

    // 选中一个文件 → 单选操作条出现
    const fileRow = [...container.querySelectorAll('button')].find((b) =>
      b.textContent?.includes('a.txt') && b.textContent?.includes('12 B'),
    );
    expect(fileRow).toBeTruthy();
    await click(fileRow as HTMLButtonElement);

    expect(findButton('下载').disabled).toBe(false);
  });

  it('下载进行中时上传按钮仍可用', async () => {
    await mountFilesTab();
    await putTransfer('d-active', {
      kind: 'download',
      status: 'active',
      statusText: '正在下载 a.txt ...',
    });

    expect(findButton('上传').disabled).toBe(false);
  });
});

/** 软链接的 is_file 在 lstat 语义下是 false —— 不能因此变成死按钮 */
describe('软链接打开', () => {
  const LINK: SftpFileEntry = {
    name: 'link.txt',
    is_dir: false,
    is_file: false,
    is_symlink: true,
    size: 12,
    mode: 0o777,
  };

  it('软链接标注为 LINK，并可进入「打开」流程', async () => {
    mocks.sftpListDir.mockResolvedValue([LINK]);
    await mountFilesTab();

    expect(container.textContent).toContain('LINK');
    expect(container.textContent).toContain('符号链接');

    const row = [...container.querySelectorAll('button')].find((b) =>
      b.textContent?.includes('link.txt'),
    );
    await click(row as HTMLButtonElement);
    await click(row as HTMLButtonElement);

    // 文本类软链接 → 打开编辑器（Mock 为空组件），不应静默无反应
    expect(container.textContent).not.toContain('无法打开');
  });

  it('特殊文件（非 is_file 非 is_symlink）给出明确提示', async () => {
    const FIFO: SftpFileEntry = {
      name: 'pipe',
      is_dir: false,
      is_file: false,
      is_symlink: false,
      size: 0,
      mode: 0o644,
    };
    mocks.sftpListDir.mockResolvedValue([FIFO]);
    await mountFilesTab();

    const row = [...container.querySelectorAll('button')].find((b) =>
      b.textContent?.includes('pipe'),
    );
    await click(row as HTMLButtonElement);
    await click(row as HTMLButtonElement);

    expect(container.textContent).toContain('不是普通文件');
  });
});
