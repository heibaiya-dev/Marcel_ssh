// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useResizablePanel } from './useResizablePanel';

let host: HTMLDivElement;
let root: Root;

function Harness({
  initialWidth = 200,
  onChange,
}: {
  initialWidth?: number;
  onChange: (width: number) => void;
}) {
  const { width, isResizing, startResize } = useResizablePanel({
    initialWidth,
    minWidth: 100,
    maxWidth: 400,
    edge: 'left',
    onChange,
  });
  return (
    <div>
      <button type="button" onMouseDown={startResize}>
        handle
      </button>
      <output>{`${width}|${isResizing ? 'resizing' : 'idle'}`}</output>
    </div>
  );
}

function mouse(type: string, clientX: number) {
  document.dispatchEvent(new MouseEvent(type, { bubbles: true, clientX }));
}

function buttonMouseDown(clientX: number) {
  const button = host.querySelector('button');
  expect(button).not.toBeNull();
  button!.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, clientX }));
}

function visibleWidth() {
  return host.querySelector('output')!.textContent;
}

beforeEach(() => {
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  host.remove();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('useResizablePanel', () => {
  it('拖动期间只改本地宽度，不提交 onChange', async () => {
    const onChange = vi.fn();
    await act(async () => {
      root.render(<Harness onChange={onChange} />);
    });

    await act(async () => {
      buttonMouseDown(200);
    });
    expect(visibleWidth()).toBe('200|resizing');

    await act(async () => {
      mouse('mousemove', 260);
      mouse('mousemove', 300);
      mouse('mousemove', 320);
    });
    // 关键：每个 mousemove 都 onChange 会触发「整份设置序列化 + IPC + 写盘」
    expect(onChange).not.toHaveBeenCalled();
    expect(visibleWidth()).toBe('320|resizing');
  });

  it('松手后按最终宽度提交一次（含边界 clamp）', async () => {
    const onChange = vi.fn();
    await act(async () => {
      root.render(<Harness onChange={onChange} />);
    });

    await act(async () => {
      buttonMouseDown(200);
    });
    await act(async () => {
      mouse('mousemove', 260);
      mouse('mousemove', 1000);
    });
    await act(async () => {
      mouse('mouseup', 1000);
    });

    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith(400);
    expect(visibleWidth()).toBe('400|idle');
  });

  it('宽度没变时不提交（单击手柄不该写盘）', async () => {
    const onChange = vi.fn();
    await act(async () => {
      root.render(<Harness onChange={onChange} />);
    });

    await act(async () => {
      buttonMouseDown(200);
    });
    await act(async () => {
      mouse('mouseup', 200);
    });

    expect(onChange).not.toHaveBeenCalled();
  });
});
