// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import SweepTransition from './SweepTransition';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const view = (value: string) => (
  <SweepTransition value={value} render={(v) => <span>{v}</span>} />
);

describe('SweepTransition', () => {
  let root: Root | null = null;
  const host = document.createElement('div');
  afterEach(() => {
    act(() => root?.unmount());
    root = null;
    vi.useRealTimers();
  });

  const mount = (value: string) => {
    root = createRoot(host);
    act(() => root!.render(view(value)));
    return (next: string) => act(() => root!.render(view(next)));
  };

  it('换值时旧层与新层同时挂上，连续换值时两层一起重挂（进度同步，不留空白带）', () => {
    const update = mount('a');
    expect(host.querySelector('.sweep-hide')).toBeNull();

    update('b');
    const firstHide = host.querySelector('.sweep-hide');
    const firstReveal = host.querySelector('.sweep-reveal');
    expect(firstHide?.textContent).toBe('a');
    expect(firstReveal?.textContent).toBe('b');

    update('c');
    const secondHide = host.querySelector('.sweep-hide');
    const secondReveal = host.querySelector('.sweep-reveal');
    expect(secondHide?.textContent).toBe('b');
    expect(secondReveal?.textContent).toBe('c');
    // 旧层必须是新元素：复用上一轮的元素，它的蒙版进度会领先新层
    expect(secondHide).not.toBe(firstHide);
    expect(secondReveal).not.toBe(firstReveal);
  });

  it('动画结束（animationend 丢了也有超时兜底）后只剩新内容，没有蒙版类残留', () => {
    vi.useFakeTimers();
    const update = mount('a');
    update('b');
    expect(host.querySelector('.sweep-reveal')).not.toBeNull();
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(host.querySelector('.sweep-hide')).toBeNull();
    expect(host.querySelector('.sweep-reveal')).toBeNull();
    expect(host.textContent).toBe('b');
  });
});
