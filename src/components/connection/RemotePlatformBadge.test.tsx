// @vitest-environment jsdom
// 平台徽标：只标少数派（Windows / macOS / Unix），Linux 不画；颜色走主题中性色，不用整块描边色。
import { afterEach, describe, expect, it } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { RemotePlatform } from '@/lib/types';
import RemotePlatformBadge from './RemotePlatformBadge';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLElement | null = null;

function render(platform?: RemotePlatform, compact = false) {
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  act(() => root!.render(<RemotePlatformBadge platform={platform} compact={compact} />));
  return host;
}

afterEach(() => {
  act(() => root?.unmount());
  host?.remove();
  root = null;
  host = null;
});

describe('RemotePlatformBadge', () => {
  it('Linux 不显示（探测结果仍会存库，只是不画）', () => {
    expect(render('linux').innerHTML).toBe('');
  });

  it('未探测到也不显示', () => {
    expect(render(undefined).innerHTML).toBe('');
  });

  it.each([
    ['windows', 'Windows', 'Win'],
    ['macos', 'macOS', 'macOS'],
    ['unix', 'Unix', 'Unix'],
  ] as const)('%s 显示标签；紧凑模式用短名', (platform, label, short) => {
    expect(render(platform).textContent).toBe(label);
    act(() => root!.unmount());
    host!.remove();
    expect(render(platform, true).textContent).toBe(short);
  });

  it('底与字走主题中性色，没有描边色块', () => {
    const cls = render('windows').querySelector('span')!.className;
    expect(cls).toContain('bg-state-soft');
    expect(cls).not.toMatch(/border-|sky-500|amber-500/);
  });
});
