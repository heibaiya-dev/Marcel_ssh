// @vitest-environment jsdom
// 「执行前模型审批」配置区：引擎卡切换、Jev 的 Key 状态、折叠区默认收起 / 有改动自动展开。
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { AgentModeSettings } from '@/lib/types';
import { useSettingsStore } from '@/stores/settingsStore';
import { ModelApprovalPanel } from './ModelApprovalPanel';
import { SearchRegistryProvider } from './helpers';

const saveJevApiKey = vi.fn(async (_k: string) => undefined);
vi.mock('@/lib/tauri', () => ({
  saveJevApiKey: (k: string) => saveJevApiKey(k),
  deleteJevApiKey: async () => undefined,
  agentDefaultApprovalPrompt: async () => '内置提示词',
  agentDefaultJevPrompt: async () => '内置判据',
}));

(
  globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

function agentWith(patch: Partial<AgentModeSettings>): AgentModeSettings {
  return { commandApprovalEngine: 'jev', ...patch } as AgentModeSettings;
}

async function render(agent: AgentModeSettings, onChange = vi.fn()) {
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => {
    root!.render(
      <SearchRegistryProvider>
        <ModelApprovalPanel
          agent={agent}
          onChange={onChange}
          modelPicker={<i>picker</i>}
        />
      </SearchRegistryProvider>,
    );
  });
  return onChange;
}

afterEach(async () => {
  await act(async () => root?.unmount());
  host?.remove();
  root = null;
  host = null;
  useSettingsStore.setState({ hasJevApiKey: false });
  saveJevApiKey.mockClear();
});

describe('ModelApprovalPanel', () => {
  it('点引擎卡切换引擎', async () => {
    const onChange = await render(
      agentWith({ commandApprovalEngine: 'model' }),
    );
    const radios = host!.querySelectorAll<HTMLButtonElement>('[role="radio"]'); // 桌面：分段控件
    expect(radios[0].getAttribute('aria-checked')).toBe('true');
    await act(async () => radios[1].click());
    expect(onChange).toHaveBeenCalledWith({ commandApprovalEngine: 'jev' });
  });

  it('Jev 没配 Key 时明说会拦下每条 bash；配了就没有这条提示', async () => {
    useSettingsStore.setState({ hasJevApiKey: false });
    await render(agentWith({}));
    expect(host!.textContent).toContain('还没配 Key');

    await act(async () => root?.unmount());
    host!.remove();
    useSettingsStore.setState({ hasJevApiKey: true });
    await render(agentWith({}));
    expect(host!.textContent).not.toContain('还没配 Key');
    expect(host!.textContent).toContain('已配置');
  });

  it('高级项默认收起，模型 ID / 地址不抢眼', async () => {
    await render(agentWith({}));
    expect(host!.querySelector('[aria-label="Jev 模型 ID"]')).toBeNull();
    expect(host!.querySelector('[aria-label="Jev API 地址"]')).toBeNull();
  });

  it('改过高级项也照样收起，只在右侧说改过什么；地址缺协议头时右侧直接警告', async () => {
    await render(agentWith({ jevModelId: 'typesafe/jev' }));
    expect(host!.querySelector('[aria-label="Jev 模型 ID"]')).toBeNull();
    expect(host!.textContent).toContain('自定义模型');

    await act(async () => root?.unmount());
    host!.remove();
    await render(agentWith({ jevBaseUrl: 'gw.example.com' }));
    expect(host!.querySelector('[aria-label="Jev API 地址"]')).toBeNull();
    expect(host!.textContent).toContain('地址缺少 http(s)://');
  });

  it('点「模型 ID、API 地址与判据」展开，里面才有输入框', async () => {
    await render(agentWith({ jevBaseUrl: 'gw.example.com' }));
    const toggle = host!.querySelector<HTMLButtonElement>(
      'button[aria-expanded]',
    )!;
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    await act(async () => toggle.click());
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(host!.querySelector('[aria-label="Jev API 地址"]')).not.toBeNull();
    expect(host!.textContent).toContain('请补上开头');
  });

  it('数据去向提示带上实际地址', async () => {
    await render(agentWith({ jevBaseUrl: 'https://gw.example.com/' }));
    expect(host!.textContent).toContain('https://gw.example.com');
    expect(host!.textContent).toContain('第三方中转');
  });

  it('回车保存 Key', async () => {
    await render(agentWith({}));
    const input = host!.querySelector<HTMLInputElement>(
      '[aria-label="Jev API Key"]',
    )!;
    const setter = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      'value',
    )!.set!;
    await act(async () => {
      setter.call(input, ' sk-test ');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      input.dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }),
      );
    });
    expect(saveJevApiKey).toHaveBeenCalledWith('sk-test');
  });

  it('会话模型引擎显示审批模型选择器，审批提示词默认收起、点开才有文本框', async () => {
    await render(agentWith({ commandApprovalEngine: 'model' }));
    expect(host!.textContent).toContain('picker');
    expect(host!.querySelector('textarea')).toBeNull();
    const toggle = host!.querySelector<HTMLButtonElement>(
      'button[aria-expanded]',
    )!;
    await act(async () => toggle.click());
    expect(host!.querySelector('textarea')).not.toBeNull();
  });

  it('Jev 判据预填内置判据；改回原文存空串，改动则存新文本', async () => {
    const onChange = await render(agentWith({}));
    await act(async () =>
      host!.querySelector<HTMLButtonElement>('button[aria-expanded]')!.click(),
    );
    const area = host!.querySelector<HTMLTextAreaElement>(
      'textarea[aria-label="Jev 判据"]',
    )!;
    expect(area.value).toBe('内置判据');
    // 没自定义时不显示「恢复默认」
    expect(host!.textContent).not.toContain('恢复默认');

    const setter = Object.getOwnPropertyDescriptor(
      HTMLTextAreaElement.prototype,
      'value',
    )!.set!;
    const type = async (v: string) =>
      act(async () => {
        setter.call(area, v);
        area.dispatchEvent(new Event('input', { bubbles: true }));
      });
    await type('内置判据 + 我的补充');
    expect(onChange).toHaveBeenLastCalledWith({
      jevApprovalPrompt: '内置判据 + 我的补充',
    });
  });

  it('已自定义的 Jev 判据显示自己的文本，并带「恢复默认」', async () => {
    const onChange = await render(agentWith({ jevApprovalPrompt: '我的判据' }));
    await act(async () =>
      host!.querySelector<HTMLButtonElement>('button[aria-expanded]')!.click(),
    );
    expect(
      host!.querySelector<HTMLTextAreaElement>(
        'textarea[aria-label="Jev 判据"]',
      )!.value,
    ).toBe('我的判据');
    const reset = [...host!.querySelectorAll('button')].find(
      (b) => b.textContent === '恢复默认',
    )!;
    await act(async () => reset.click());
    expect(onChange).toHaveBeenLastCalledWith({ jevApprovalPrompt: '' });
  });
});
