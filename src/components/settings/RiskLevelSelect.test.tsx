// @vitest-environment jsdom
// 档位选择（手机）：不能是原生 select——Android 会弹白底系统对话框，深色主题下刺眼、放不下说明。
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { LevelSelect } from './RiskLevelSelect';

(
  globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
).IS_REACT_ACT_ENVIRONMENT = true;

// jsdom 不实现 ResizeObserver，而 MobileSheet 用它盯滚动区高度 —— 补个空壳即可。
(globalThis as Record<string, unknown>).ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function renderCompact(onChange = vi.fn()) {
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => {
    root!.render(
      <LevelSelect
        compact
        ariaLabel="写入受保护路径：处置档位"
        value="ForceApproval"
        options={['Allow', 'Approval', 'ForceApproval', 'Deny']}
        defaultLevel="ForceApproval"
        onChange={onChange}
      />,
    );
  });
  return onChange;
}

const trigger = () =>
  document.querySelector<HTMLButtonElement>('button[aria-haspopup="dialog"]')!;
const radios = () => [
  ...document.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
];

async function click(el: Element) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent('click', { bubbles: true }));
  });
}

afterEach(async () => {
  await act(async () => root?.unmount());
  host?.remove();
  root = null;
  host = null;
});

describe('LevelSelect（手机）', () => {
  it('是一个按钮而不是原生 select', async () => {
    await renderCompact();
    expect(document.querySelector('select')).toBeNull();
    expect(trigger().textContent).toBe('强制审批');
    expect(trigger().getAttribute('aria-label')).toBe(
      '写入受保护路径：处置档位（当前强制审批）',
    );
  });

  it('点开是底部面板：四档都在，各带说明，默认档标「默认」，当前档被选中', async () => {
    await renderCompact();
    expect(radios()).toHaveLength(0);
    await click(trigger());

    expect(radios().map((r) => r.getAttribute('aria-checked'))).toEqual([
      'false',
      'false',
      'true',
      'false',
    ]);
    const text = document.body.textContent ?? '';
    expect(text).toContain('不拦');
    expect(text).toContain('不执行，让模型换写法');
    // 说明文字里也有「默认」二字，所以数的是整段文字恰为「默认」的标签
    const badges = [...document.querySelectorAll('span')].filter(
      (s) => s.textContent === '默认',
    );
    expect(badges).toHaveLength(1);
    expect(radios()[2].contains(badges[0])).toBe(true);
  });

  it('选另一档会回调；点当前档不回调', async () => {
    const onChange = await renderCompact();
    await click(trigger());
    await click(radios()[3]);
    expect(onChange).toHaveBeenCalledWith('Deny');

    await click(trigger());
    await click(radios()[2]);
    expect(onChange).toHaveBeenCalledTimes(1);
  });
});
