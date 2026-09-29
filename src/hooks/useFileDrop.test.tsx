// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * 只 mock 事件总线（@tauri-apps/api/event），`subscribeTauriEvent` 用真实实现 ——
 * 本用例要验证的正是「hook 是否经该原语订阅、注册失败后是否可重试」。
 */
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }));

import { listen } from '@tauri-apps/api/event';
import { resetEventChannelsForTest, subscriberCount } from '@/lib/tauriEvent';
import { useFileDrop, type DropPosition } from './useFileDrop';

/**
 * jsdom 环境下 `globalThis` 是 window，`process` 仍是 Node 的模块级全局
 * （没有 @types/node，这里只补声明，运行时取真实进程对象）。
 */
declare const process: {
  on: (event: string, listener: (...args: unknown[]) => void) => void;
  off: (event: string, listener: (...args: unknown[]) => void) => void;
};

const listenMock = listen as unknown as ReturnType<typeof vi.fn>;

/** 已注册的事件回调：事件名 → handler（读的就是 listen 收到的那个） */
let registered: Map<string, (event: { payload: unknown }) => void>;
let unlistenCalls: number;
/** 每次 listen 调用的行为：resolve / reject */
let listenResult: 'resolve' | 'reject';

let host: HTMLDivElement;
let root: Root;

function Harness({
  onDrop,
  enabled = true,
}: {
  onDrop: (paths: string[], position?: DropPosition) => void;
  enabled?: boolean;
}) {
  const { isDragging } = useFileDrop(onDrop, enabled);
  return <output>{isDragging ? 'dragging' : 'idle'}</output>;
}

function emit(eventName: string, payload: unknown) {
  const handler = registered.get(eventName);
  expect(handler, `subscription for ${eventName}`).toBeDefined();
  handler!({ payload });
}

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  listenResult = 'resolve';
  registered = new Map();
  unlistenCalls = 0;
  listenMock.mockReset();
  listenMock.mockImplementation(async (eventName: string, handler: (e: { payload: unknown }) => void) => {
    if (listenResult === 'reject') throw new Error('listen failed');
    registered.set(eventName, handler);
    return () => {
      unlistenCalls++;
    };
  });
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  resetEventChannelsForTest();
  host.remove();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('useFileDrop', () => {
  it('拖入/离开切换 isDragging，drop 把路径与 CSS 坐标交给 handler', async () => {
    const onDrop = vi.fn();
    await act(async () => {
      root.render(<Harness onDrop={onDrop} />);
    });

    await act(async () => {
      emit('tauri://drag-enter', {});
    });
    expect(host.textContent).toBe('dragging');

    await act(async () => {
      emit('tauri://drag-drop', {
        paths: ['C:/tmp/a.txt'],
        position: { x: 40, y: 60 },
      });
    });
    expect(onDrop).toHaveBeenCalledWith(['C:/tmp/a.txt'], { x: 40, y: 60 });
    expect(host.textContent).toBe('idle');

    await act(async () => {
      emit('tauri://drag-enter', {});
      emit('tauri://drag-leave', {});
    });
    expect(host.textContent).toBe('idle');
  });

  it('只有栈顶消费者处理 drop，卸载后退订', async () => {
    const first = vi.fn();
    const second = vi.fn();
    await act(async () => {
      root.render(
        <>
          <Harness onDrop={first} />
          <Harness onDrop={second} />
        </>,
      );
    });
    expect(subscriberCount('tauri://drag-drop')).toBe(2);

    await act(async () => {
      emit('tauri://drag-drop', { paths: ['/tmp/x'] });
    });
    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledWith(['/tmp/x'], undefined);

    await act(async () => {
      root.render(<Harness onDrop={first} />);
    });
    expect(subscriberCount('tauri://drag-drop')).toBe(1);

    await act(async () => {
      root.unmount();
    });
    expect(subscriberCount('tauri://drag-drop')).toBe(0);
    expect(unlistenCalls).toBeGreaterThan(0);
  });

  it('注册失败只记日志、不产生未捕获 rejection，重新挂载会重试', async () => {
    const onDrop = vi.fn();
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
    // 旧的裸 listen + 先置位守卫：这一失败会让本进程内的拖拽上传永久失效
    listenResult = 'reject';
    const unhandled = vi.fn();
    process.on('unhandledRejection', unhandled);

    await act(async () => {
      root.render(<Harness onDrop={onDrop} />);
    });
    // 等注册失败的 Promise 链与 Node 的 unhandledRejection 派发都跑完
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });

    expect(consoleError).toHaveBeenCalled();
    expect(subscriberCount('tauri://drag-drop')).toBe(0);
    expect(unhandled).not.toHaveBeenCalled();

    // 失败的空壳通道已摘掉：再挂载一次就能重新注册（可重试）
    listenResult = 'resolve';
    await act(async () => {
      root.render(<Harness onDrop={onDrop} enabled={false} />);
    });
    await act(async () => {
      root.render(<Harness onDrop={onDrop} />);
    });
    const dropSubscriptions = listenMock.mock.calls.filter(
      ([name]) => name === 'tauri://drag-drop',
    );
    expect(dropSubscriptions).toHaveLength(2);

    await act(async () => {
      emit('tauri://drag-drop', { paths: ['/tmp/retry'] });
    });
    expect(onDrop).toHaveBeenCalledWith(['/tmp/retry'], undefined);

    process.off('unhandledRejection', unhandled);
  });
});
