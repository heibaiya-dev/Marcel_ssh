// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { QrCode } from './QrCode';
import { buildQrMatrix } from './qrMatrix';

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

describe('buildQrMatrix', () => {
  it('矩阵尺寸 = 17 + 4×版本号 + 两侧各 4 个模块的静区', () => {
    const matrix = buildQrMatrix('MSYNC1-hello');
    expect(matrix).not.toBeNull();
    const modules = matrix!.size - 8;
    expect((modules - 17) % 4).toBe(0);
    expect(matrix!.path.startsWith('M')).toBe(true);
  });

  it('同步码这种几百字符的内容能编码；超过二维码容量时返回 null 而不是抛错', () => {
    expect(buildQrMatrix('MSYNC1-' + 'A'.repeat(600))).not.toBeNull();
    expect(buildQrMatrix('x'.repeat(5000))).toBeNull();
  });

  it('相同内容得到相同矩阵（可重复）', () => {
    expect(buildQrMatrix('MSYNC1-abc')).toEqual(buildQrMatrix('MSYNC1-abc'));
  });
});

describe('QrCode', () => {
  let root: Root;
  let container: HTMLDivElement;
  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it('永远白底黑块（不随深浅色主题变），带可读的无障碍名', () => {
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    act(() => root.render(<QrCode text="MSYNC1-hello" label="同步码二维码" />));
    const svg = container.querySelector('svg')!;
    expect(svg.getAttribute('aria-label')).toBe('同步码二维码');
    expect(svg.querySelector('rect')?.getAttribute('fill')).toBe('#ffffff');
    expect(svg.querySelector('path')?.getAttribute('fill')).toBe('#000000');
  });

  it('内容太长编不进去：什么都不渲染', () => {
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    act(() => root.render(<QrCode text={'x'.repeat(5000)} label="同步码二维码" />));
    expect(container.querySelector('svg')).toBeNull();
  });
});
