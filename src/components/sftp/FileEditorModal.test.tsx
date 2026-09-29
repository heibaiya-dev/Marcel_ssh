// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const cm = vi.hoisted(() => ({ constructed: 0 }));
const lang = vi.hoisted(() => ({
  deferred: null as null | { promise: Promise<unknown>; resolve: () => void },
}));
const mocks = vi.hoisted(() => ({
  sftpReadFile: vi.fn(),
  sftpWriteFile: vi.fn(async () => {}),
  sftpGetMtime: vi.fn(async () => 1),
}));

/** 计数用子类：语言包 resolve 之后是否还会 new 出 EditorView（泄漏与否的判据） */
vi.mock('@codemirror/view', async () => {
  const actual = (await vi.importActual('@codemirror/view')) as unknown as {
    EditorView: new (config: unknown) => object;
  } & Record<string, unknown>;
  const Base = actual.EditorView;
  class EditorViewCounted extends Base {
    constructor(config: unknown) {
      super(config);
      cm.constructed += 1;
    }
  }
  return { ...actual, EditorView: EditorViewCounted };
});

/** 语言包加载可控：测试在 resolve 之前关闭弹窗，复现卸载竞态 */
vi.mock('@codemirror/lang-json', () => ({
  json: () => {
    if (!lang.deferred) throw new Error('lang.deferred 未初始化');
    return lang.deferred.promise;
  },
}));

vi.mock('@/lib/tauri', () => ({
  sftpReadFile: mocks.sftpReadFile,
  sftpWriteFile: mocks.sftpWriteFile,
  sftpGetMtime: mocks.sftpGetMtime,
}));

const FileEditorModal = (await import('@/components/sftp/FileEditorModal')).default;

function deferred() {
  let resolve: () => void = () => {};
  const promise = new Promise<unknown>((r) => {
    resolve = () => r([]);
  });
  return { promise, resolve };
}

let host: HTMLDivElement;
let root: Root;

const flush = () => act(async () => {});

const baseProps = {
  sessionId: 'sess-1',
  filePath: '/a.json',
  fileName: 'a.json',
  fileSize: 2,
  onClose: () => {},
  onSaved: () => {},
};

async function renderModal(open: boolean) {
  await act(async () => {
    root.render(<FileEditorModal {...baseProps} open={open} />);
  });
  await flush();
}

async function clickSave() {
  const button = [...document.querySelectorAll('button')].find((b) =>
    b.textContent?.includes('保存 ('),
  );
  expect(button, '保存按钮').toBeDefined();
  await act(async () => {
    button!.click();
  });
  await flush();
}

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  // jsdom 没实现 Range 的测量 API，CodeMirror 布局时会打一堆报错（与断言无关）
  Range.prototype.getClientRects = () =>
    ({ length: 0, item: () => null }) as unknown as DOMRectList;
  Range.prototype.getBoundingClientRect = () => new DOMRect(0, 0, 0, 0);
  cm.constructed = 0;
  lang.deferred = null;
  mocks.sftpReadFile.mockReset();
  mocks.sftpWriteFile.mockClear();
  mocks.sftpGetMtime.mockClear();
  mocks.sftpGetMtime.mockResolvedValue(1);
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

describe('FileEditorModal', () => {
  it('语言包 resolve 前被关闭：不再创建 EditorView（无泄漏）', async () => {
    lang.deferred = deferred();
    mocks.sftpReadFile.mockResolvedValue({ content: '{}', mtime: 1, hasBom: false });

    await renderModal(true);
    // 文件已读出，正卡在 await loader()（动态语言包）
    expect(cm.constructed).toBe(0);

    await renderModal(false);
    // 卸载/关闭的 cleanup 已跑过 destroyEditor()，此后不能再建 view
    await act(async () => {
      lang.deferred!.resolve();
    });
    await flush();

    expect(cm.constructed).toBe(0);
  });

  it.each([true, false])('读到的 hasBom=%s 原样回传给写回命令', async (hasBom) => {
    lang.deferred = deferred();
    mocks.sftpReadFile.mockResolvedValue({ content: '{}', mtime: 1, hasBom });

    await renderModal(true);
    await act(async () => {
      lang.deferred!.resolve();
    });
    await flush();
    expect(cm.constructed).toBe(1);

    await clickSave();
    expect(mocks.sftpWriteFile).toHaveBeenCalledWith('sess-1', '/a.json', '{}', hasBom);
  });
});
