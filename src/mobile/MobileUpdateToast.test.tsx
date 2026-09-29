// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { UpdateCapabilities, UpdateState } from '@/lib/types';

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

// jsdom 不实现 ResizeObserver，而 MobileSheet 用它测滚动区域 —— 补个空壳即可，
// 这里断言的是错误文案，不涉及尺寸。
(globalThis as Record<string, unknown>).ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

const mocks = vi.hoisted(() => ({
  state: { status: 'idle' } as UpdateState,
  capabilities: null as UpdateCapabilities | null,
  installNow: vi.fn(async () => {}),
  download: vi.fn(async () => {}),
  dismiss: vi.fn(),
  dismissFailure: vi.fn(),
}));

vi.mock('@/stores/updateStore', () => ({
  // 可见性判定本身在 updateStore.test.ts 里测；这里只关心错误文案。
  isUpdateVisible: () => true,
  useUpdateStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      state: mocks.state,
      capabilities: mocks.capabilities,
      dismissedVersion: null,
      failureDismissed: false,
      installNow: mocks.installNow,
      download: mocks.download,
      dismiss: mocks.dismiss,
      dismissFailure: mocks.dismissFailure,
    }),
}));

vi.mock('@/lib/externalLinks', () => ({ openExternalLink: vi.fn() }));

import MobileUpdateToast from './MobileUpdateToast';

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.clearAllMocks();
  mocks.state = { status: 'ready', version: '1.4.1' };
  mocks.capabilities = null;
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function render() {
  await act(async () => {
    root.render(<MobileUpdateToast />);
  });
}

async function clickButton(label: string) {
  // MobileSheet 用 createPortal 挂在 document.body 上，不在 container 里
  const button = [...document.body.querySelectorAll('button')].find((b) =>
    b.textContent?.includes(label),
  );
  expect(button, `未找到「${label}」按钮`).toBeTruthy();
  await act(async () => {
    button!.dispatchEvent(new MouseEvent('click', { bubbles: true }));
  });
  await act(async () => {});
}

/**
 * Tauri 命令失败抛的是结构化 `{ kind, message }`，不是 Error。
 * 直接 String(e) 会渲染成 "[object Object]"（项目铁律禁止），必须走
 * `getErrorMessage`。这里锁住安装/重试下载失败时的展示文案。
 */
describe('MobileUpdateToast 失败文案', () => {
  it('安装失败时展示结构化错误里的 message（不是 [object Object]）', async () => {
    mocks.installNow.mockRejectedValue({
      kind: 'Update',
      message: '安装包校验失败，请重试',
    });
    await render();
    await clickButton('立即安装');

    expect(document.body.textContent).toContain('安装包校验失败，请重试');
    expect(document.body.textContent).not.toContain('[object Object]');
  });

  it('重试下载失败时同样展示 message', async () => {
    mocks.state = { status: 'failed', message: '上次自动更新失败' };
    mocks.capabilities = {
      silentDownload: true,
      installKind: 'apk',
    } as UpdateCapabilities;
    mocks.download.mockRejectedValue({
      kind: 'Update',
      message: '网络不可用，请检查连接',
    });
    await render();
    expect(document.body.textContent).toContain('上次自动更新失败');

    await clickButton('重试下载');

    expect(document.body.textContent).toContain('网络不可用，请检查连接');
    expect(document.body.textContent).not.toContain('[object Object]');
  });
});
