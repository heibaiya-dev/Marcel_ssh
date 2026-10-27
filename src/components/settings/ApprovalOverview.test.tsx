// @vitest-environment jsdom
// 「什么时候问你」总览：格子的值全来自后端，开关与确认方式嵌在表里，拨动时回传对应的字段。
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { AgentModeSettings, ApprovalOverview as Overview } from '@/lib/types';
import { ApprovalOverview } from './ApprovalOverview';

const overview = vi.fn(async (_draft?: AgentModeSettings): Promise<Overview> => ({
  rows: [
    {
      level: 'Deny',
      agent: { verdict: 'deny' },
      plan: { verdict: 'deny' },
      auto: { verdict: 'deny' },
    },
    {
      level: 'ForceApproval',
      agent: { verdict: 'ask' },
      plan: { verdict: 'run' },
      auto: { verdict: 'run' },
    },
    {
      level: 'Approval',
      agent: { verdict: 'ask' },
      plan: { verdict: 'run' },
      auto: { verdict: 'run' },
    },
    {
      level: 'Allow',
      agent: { verdict: 'ask', listed: 'run' },
      plan: { verdict: 'run' },
      auto: { verdict: 'run' },
    },
  ],
}));
vi.mock('@/lib/tauri', () => ({
  agentApprovalOverview: (d?: AgentModeSettings) => overview(d),
}));

(
  globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function render(node: React.ReactNode) {
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => {
    root!.render(node);
  });
}

afterEach(async () => {
  await act(async () => root?.unmount());
  host?.remove();
  root = null;
  host = null;
  overview.mockClear();
});

const baseAgent = {
  listMode: 'allowlist',
  allowCommands: ['ls'],
  confirmEachCommand: false,
  planModeRequiresApproval: false,
  autoModeForceApproval: false,
} as unknown as AgentModeSettings;

describe('ApprovalOverview', () => {
  it('把后端算出的每个格子画出来，白名单下「没命中」一格分名单内外', async () => {
    await render(<ApprovalOverview agent={baseAgent} onChange={() => {}} />);
    const text = host!.textContent ?? '';
    for (const name of ['直接拒绝', '强制审批', '请求审批', '没命中任何规则']) {
      expect(text).toContain(name);
    }
    expect(text).toContain('名单外');
    expect(text).toContain('名单内');
    expect(overview).toHaveBeenCalledTimes(1);
  });

  it('请求审批与强制审批两行一样时明说，区别只在 Auto 的开关', async () => {
    await render(<ApprovalOverview agent={baseAgent} onChange={() => {}} />);
    expect(host!.textContent).toContain('目前表现完全一样');
  });

  it('Plan / Auto 列头的开关回传对应字段', async () => {
    const onChange = vi.fn();
    await render(<ApprovalOverview agent={baseAgent} onChange={onChange} />);
    const switches = host!.querySelectorAll('[role="switch"]');
    expect(switches).toHaveLength(2);
    await act(async () => (switches[0] as HTMLElement).click());
    expect(onChange).toHaveBeenLastCalledWith({ planModeRequiresApproval: true });
    await act(async () => (switches[1] as HTMLElement).click());
    expect(onChange).toHaveBeenLastCalledWith({ autoModeForceApproval: true });
  });

  it('命令确认方式：选「每条都确认」只改确认开关，选回「按规则」同时带上名单模式', async () => {
    const onChange = vi.fn();
    await render(<ApprovalOverview agent={baseAgent} onChange={onChange} />);
    const radio = (label: string) =>
      Array.from(host!.querySelectorAll('[role="radio"]')).find((b) =>
        b.textContent?.includes(label),
      ) as HTMLElement;
    await act(async () => radio('每条都确认').click());
    expect(onChange).toHaveBeenLastCalledWith({ confirmEachCommand: true });
    await act(async () => radio('按规则').click());
    expect(onChange).toHaveBeenLastCalledWith({
      confirmEachCommand: false,
      listMode: 'denylist',
    });
  });

  it('后端没取回来就不画表，不拿猜的值顶上', async () => {
    overview.mockRejectedValueOnce(new Error('boom'));
    const err = vi.spyOn(console, 'error').mockImplementation(() => {});
    await render(<ApprovalOverview agent={baseAgent} onChange={() => {}} />);
    expect(host!.textContent).toContain('正在读取');
    expect(host!.textContent).not.toContain('直接执行');
    err.mockRestore();
  });

  it('白名单下可以加命令，已在名单里的不重复加', async () => {
    const onChange = vi.fn();
    await render(<ApprovalOverview agent={baseAgent} onChange={onChange} />);
    const input = host!.querySelector(
      'input[aria-label="添加允许的命令"]',
    ) as HTMLInputElement;
    const set = (v: string) => {
      const setter = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        'value',
      )!.set!;
      setter.call(input, v);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    };
    const add = Array.from(host!.querySelectorAll('button')).find(
      (b) => b.textContent === '添加',
    ) as HTMLButtonElement;
    await act(async () => set('cat'));
    await act(async () => add.click());
    expect(onChange).toHaveBeenLastCalledWith({ allowCommands: ['ls', 'cat'] });
    onChange.mockClear();
    await act(async () => set('ls'));
    await act(async () => add.click());
    expect(onChange).not.toHaveBeenCalled();
  });
});
