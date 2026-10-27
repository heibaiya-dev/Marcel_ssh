// @vitest-environment jsdom
// 移动端共享下拉：不是原生 select，点开是底部面板。
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import MobileSelect, { type MobileSelectOption } from './MobileSelect';

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

const OPTIONS: MobileSelectOption<string>[] = [
  { value: 'a', label: '选项 A', hint: 'A 的说明', badge: '默认' },
  { value: 'b', label: '选项 B' },
  { value: 'c', label: '选项 C', disabled: true },
];

async function renderSelect(
  props: Partial<React.ComponentProps<typeof MobileSelect<string>>> = {},
) {
  const onChange = vi.fn();
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => {
    root!.render(
      <MobileSelect
        ariaLabel="测试下拉"
        value="a"
        options={OPTIONS}
        onChange={onChange}
        {...props}
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

describe('MobileSelect', () => {
  it('是按钮不是原生 select，显示当前项并报给读屏', async () => {
    await renderSelect();
    expect(document.querySelector('select')).toBeNull();
    expect(trigger().textContent).toBe('选项 A');
    expect(trigger().getAttribute('aria-label')).toBe('测试下拉（当前选项 A）');
  });

  it('点开后各项带说明和标签，当前项选中；选另一项回调，点当前项不回调', async () => {
    const onChange = await renderSelect();
    expect(radios()).toHaveLength(0);
    await click(trigger());

    expect(radios().map((r) => r.getAttribute('aria-checked'))).toEqual([
      'true',
      'false',
      'false',
    ]);
    expect(radios()[0].textContent).toContain('A 的说明');
    expect(radios()[0].textContent).toContain('默认');

    await click(radios()[1]);
    expect(onChange).toHaveBeenCalledWith('b');

    // value 是受控的（这里没更新），当前仍是 A；再点开点 A 不应再回调
    await click(trigger());
    await click(radios()[0]);
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it('value 不在选项里时显示占位文字', async () => {
    await renderSelect({ value: '', placeholder: '选择连接…' });
    expect(trigger().textContent).toBe('选择连接…');
    expect(trigger().getAttribute('aria-label')).toBe('测试下拉');
  });

  it('不可选的项禁用；整个下拉可禁用', async () => {
    await renderSelect();
    await click(trigger());
    expect(radios()[2].disabled).toBe(true);

    await act(async () => root?.unmount());
    host?.remove();
    await renderSelect({ disabled: true });
    expect(trigger().disabled).toBe(true);
  });
});
