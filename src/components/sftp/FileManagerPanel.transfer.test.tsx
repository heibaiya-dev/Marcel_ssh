// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useTransferStore } from '@/stores/transferStore';
import type { SftpFileEntry } from '@/lib/types';

type DropHandler = (paths: string[], position?: { x: number; y: number }) => void;

const mocks = vi.hoisted(() => ({
  sftpListDir: vi.fn(),
  sftpPrepareDragUpload: vi.fn(),
  sftpCleanupTempDir: vi.fn(async () => {}),
  /** 永不 resolve：转存入队后停在 active，便于断言条目状态 */
  sftpUploadFolderStream: vi.fn(() => new Promise<void>(() => {})),
  uploadFile: vi.fn(),
  startDownload: vi.fn(),
  dropHandler: null as DropHandler | null,
  treeStub: {
    cache: {},
    expanded: new Set<string>(),
    selectedPath: null as string | null,
    toggleExpand: vi.fn(),
    loadNode: vi.fn(async () => {}),
    seedFromListing: vi.fn(),
  },
}));

vi.mock('@/lib/tauri', () => ({
  sftpListDir: mocks.sftpListDir,
  sftpMkdir: vi.fn(),
  sftpRemove: vi.fn(),
  sftpRemoveViaShell: vi.fn(),
  sftpRename: vi.fn(),
  sftpWriteFile: vi.fn(),
  sftpExtractArchive: vi.fn(),
  sftpOpenWithSystem: vi.fn(),
  sftpPrepareDragUpload: mocks.sftpPrepareDragUpload,
  sftpCleanupTempDir: mocks.sftpCleanupTempDir,
  // transferScheduler 会被动态 import，用到的命令都要在 mock 里备齐
  sftpUploadFolderStream: mocks.sftpUploadFolderStream,
  sftpUploadStream: vi.fn(() => new Promise<void>(() => {})),
  sftpDownloadStream: vi.fn(() => new Promise<void>(() => {})),
  sftpCancelUpload: vi.fn(async () => {}),
  sftpCancelDownload: vi.fn(async () => {}),
  sftpCancelSysopen: vi.fn(async () => {}),
}));

vi.mock('@tauri-apps/plugin-dialog', () => ({
  open: vi.fn(async () => 'C:/tmp/big.txt'),
  save: vi.fn(),
}));

vi.mock('@tauri-apps/plugin-fs', () => ({
  stat: vi.fn(async () => ({ isDirectory: false, isFile: true })),
}));

vi.mock('@/hooks/useContainerWidth', () => ({ useContainerWidth: () => 900 }));
vi.mock('@/hooks/useFileTree', () => ({ useFileTree: () => mocks.treeStub }));
vi.mock('@/hooks/useFileDrop', () => ({
  useFileDrop: (handler: DropHandler) => {
    mocks.dropHandler = handler;
    return { isDragging: false };
  },
}));
vi.mock('@/hooks/useSftpUpload', () => ({
  useSftpUpload: () => ({ uploadFile: mocks.uploadFile, pickFolder: vi.fn(), uploadFolder: vi.fn() }),
}));
vi.mock('@/hooks/useSftpDownload', () => ({
  useSftpDownload: () => ({ startDownload: mocks.startDownload }),
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
    update: vi.fn(async () => {}),
  };
  const useSettingsStore = (selector: (s: typeof stub) => unknown) => selector(stub);
  (useSettingsStore as unknown as { getState: () => typeof stub }).getState = () => stub;
  return { useSettingsStore };
});

const FileManagerPanel = (await import('@/components/sftp/FileManagerPanel')).default;

function dir(name: string): SftpFileEntry {
  return { name, is_dir: true, is_file: false, is_symlink: false, size: 0, mode: 0o040755 };
}
function file(name: string, size = 10): SftpFileEntry {
  return { name, is_dir: false, is_file: true, is_symlink: false, size, mode: 0o100644 };
}

let host: HTMLDivElement;
let root: Root;

const flush = () => act(async () => {});

/** 找到可见文本包含 text 的按钮 */
function findButton(text: string): HTMLButtonElement | undefined {
  return [...document.querySelectorAll('button')].find((b) => b.textContent?.includes(text));
}

function findRow(name: string): HTMLTableRowElement | undefined {
  return [...host.querySelectorAll('tr')].find((tr) => tr.textContent?.includes(name));
}

async function mountPanel() {
  await act(async () => {
    root.render(<FileManagerPanel sessionId="sess-1" connectionKey="conn-1" />);
  });
  await flush();
}

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  mocks.sftpListDir.mockReset();
  mocks.sftpPrepareDragUpload.mockReset();
  mocks.sftpCleanupTempDir.mockClear();
  mocks.uploadFile.mockReset();
  mocks.startDownload.mockReset();
  mocks.dropHandler = null;
  mocks.sftpListDir.mockImplementation(async (_sessionId: string, path: string) =>
    path === '/' ? [dir('sub'), file('a.txt')] : [file('inner.txt')],
  );
  useTransferStore.setState({ items: {}, order: [] });
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  host.remove();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('FileManagerPanel 传输后的列表刷新', () => {
  it('上传完成回调不会用旧目录覆盖用户已导航到的目录', async () => {
    await mountPanel();
    expect(host.textContent).toContain('a.txt');

    // 点「上传文件」→ 捕获上传完成回调（真实场景是传大文件夹，期间用户切走）
    await act(async () => {
      host.querySelector<HTMLButtonElement>('button[title="上传文件"]')!.click();
    });
    await flush();
    expect(mocks.uploadFile).toHaveBeenCalledTimes(1);
    const onFinished = mocks.uploadFile.mock.calls[0][3] as () => void;

    // 导航到 /sub
    await act(async () => {
      findRow('sub')!.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    });
    await flush();
    expect(host.textContent).toContain('inner.txt');
    expect(host.textContent).not.toContain('a.txt');

    // 上传这时才完成
    await act(async () => {
      onFinished();
    });
    await flush();

    // 列表仍是 /sub 的内容，没有再次加载旧目录
    expect(host.textContent).toContain('inner.txt');
    expect(host.textContent).not.toContain('a.txt');
    expect(mocks.sftpListDir.mock.calls.map(([, path]) => path)).toEqual(['/', '/sub']);
  });
});

describe('FileManagerPanel 拖拽打包上传', () => {
  async function dropAndZipFailure(failures: string[]) {
    await mountPanel();
    mocks.sftpPrepareDragUpload.mockResolvedValue({
      tempDir: '/tmp/marcel-drag-1',
      failures,
    });
    await act(async () => {
      mocks.dropHandler!(['C:/a', 'C:/b', 'C:/c']);
    });
    await act(async () => {
      findButton('打包上传')!.click();
    });
    await flush();
  }

  it('全部条目复制失败时中止并报错，不留空任务', async () => {
    await dropAndZipFailure([
      'C:/a: 文件不存在',
      'C:/b: 文件不存在',
      'C:/c: 文件不存在',
    ]);

    expect(host.textContent).toContain('全部复制失败');
    expect(host.textContent).toContain('C:/a: 文件不存在');
    expect(mocks.sftpCleanupTempDir).toHaveBeenCalledWith('/tmp/marcel-drag-1');
    expect(useTransferStore.getState().order).toHaveLength(0);
  });

  it('部分失败仍继续上传，但显眼提示缺了哪些条目', async () => {
    await dropAndZipFailure(['C:/b: 文件不存在']);

    // 提示必须撑过上传完成后的列表刷新（loadDirectory 会清 error，但不该清它）
    expect(host.textContent).toContain('以下 1/3 项复制失败');
    expect(host.textContent).toContain('C:/b: 文件不存在');
    const order = useTransferStore.getState().order;
    expect(order).toHaveLength(1);
    expect(useTransferStore.getState().items[order[0]].fileName).toBe('拖拽上传 (2 项)');
  });
});

describe('FileManagerPanel 右键下载', () => {
  async function openDownloadMenuItem() {
    await mountPanel();
    await act(async () => {
      findRow('a.txt')!.dispatchEvent(
        new MouseEvent('contextmenu', { bubbles: true, clientX: 10, clientY: 10 }),
      );
    });
    await act(async () => {
      findButton('下载')!.click();
    });
    await flush();
  }

  it('save 对话框失败时给出可见错误（不再是未捕获 rejection）', async () => {
    mocks.startDownload.mockRejectedValue({ kind: 'Ssh', message: '磁盘已满' });
    await openDownloadMenuItem();
    expect(host.textContent).toContain('下载失败：磁盘已满');
  });

  it('Android 取消保存对话框不算错误', async () => {
    mocks.startDownload.mockRejectedValue(new Error('File picker cancelled'));
    await openDownloadMenuItem();
    expect(host.textContent).not.toContain('下载失败');
  });
});
