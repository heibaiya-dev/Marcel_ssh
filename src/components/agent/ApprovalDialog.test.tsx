// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import ApprovalDialog from './ApprovalDialog';
import type { ToolCallInfo } from '@/lib/types';

// 让 react act() 在 jsdom 下正常工作
(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const TOOL_CALL: ToolCallInfo = {
  id: 'call-1',
  name: 'bash',
  arguments: { command: 'systemctl restart nginx' },
  disposition: 'ForceApproval',
};

let container: HTMLDivElement;
let root: Root;
let onApprove: ReturnType<typeof vi.fn>;
let onReject: ReturnType<typeof vi.fn>;
let onClose: ReturnType<typeof vi.fn>;
let onMinimize: ReturnType<typeof vi.fn>;
let onRejectAndStop: ReturnType<typeof vi.fn>;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
  onApprove = vi.fn();
  onReject = vi.fn();
  onClose = vi.fn();
  onMinimize = vi.fn();
  onRejectAndStop = vi.fn();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function render(
  withMinimize = true,
  withRejectAndStop = true,
  toolCall: ToolCallInfo = TOOL_CALL,
) {
  act(() => {
    root.render(
      <ApprovalDialog
        toolCall={toolCall}
        onApprove={onApprove}
        onReject={onReject}
        open={true}
        onClose={onClose}
        onMinimize={withMinimize ? onMinimize : undefined}
        onRejectAndStop={withRejectAndStop ? onRejectAndStop : undefined}
      />,
    );
  });
}

function backdrop(): HTMLElement {
  const el = container.querySelector<HTMLElement>('.modal-backdrop-enter');
  if (!el) throw new Error('找不到背景遮罩');
  return el;
}

function button(label: string): HTMLButtonElement {
  const el = Array.from(container.querySelectorAll('button')).find(
    (b) => b.textContent?.trim() === label,
  );
  if (!el) throw new Error(`找不到按钮：${label}`);
  return el;
}

/**
 * 派发一次键盘事件，并把时钟推过 300ms 的"队首切换冷却"。
 *
 * 不推时钟的话，全局 Enter 处理会在冷却里直接 return —— 于是"输入框里按 Enter
 * 不该批准"这条测试会因为**冷却**而通过，而不是因为输入框保护生效，
 * 去掉保护也照样绿（第一版正是这么写的假护栏）。
 */
function pressKey(key: string, target: EventTarget = document) {
  const realNow = Date.now;
  Date.now = () => realNow() + 1000;
  try {
    act(() => {
      target.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }));
    });
  } finally {
    Date.now = realNow;
  }
}

function pressEscape() {
  act(() => {
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
  });
}

/** 往拒绝理由输入框里打字（React 受控 input：走原生 setter 再派发 input 事件）。 */
function typeReason(text: string) {
  const input = container.querySelector<HTMLInputElement>('input[aria-label="拒绝原因"]');
  if (!input) throw new Error('找不到拒绝原因输入框');
  act(() => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    setter.call(input, text);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
  return input;
}

/**
 * 这一组盯的是一件事：**误触不能替用户做决定。**
 *
 * 审批的「拒绝」是不可逆的 —— 模型收到「用户拒绝」就换方案走了，用户甚至没意识到
 * 自己做了一个决定。所以点背景、按 Esc 这类"顺手"的动作只能把弹窗收起来（右下角
 * 浮动药丸，随时点得回来），答案必须来自显式按钮。
 *
 * 这三条曾经全接到 `onClose`，而调用方把它映射成了 `reject`。
 */
describe('ApprovalDialog 的误触防护', () => {
  it('点背景只收起，不拒绝', () => {
    render();
    act(() => backdrop().click());

    expect(onMinimize).toHaveBeenCalledTimes(1);
    expect(onReject).not.toHaveBeenCalled();
    expect(onApprove).not.toHaveBeenCalled();
  });

  it('按 Esc 只收起，不拒绝', () => {
    render();
    pressEscape();

    expect(onMinimize).toHaveBeenCalledTimes(1);
    expect(onReject).not.toHaveBeenCalled();
    expect(onApprove).not.toHaveBeenCalled();
  });

  it('没有收起出口时退回 onClose，也不拒绝', () => {
    render(false);
    act(() => backdrop().click());

    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onReject).not.toHaveBeenCalled();
  });

  it('标题栏不再有含义不明的 ✕ —— 只有明确的两个答案 + 收起', () => {
    render();
    const labels = Array.from(container.querySelectorAll('button')).map((b) =>
      b.textContent?.trim(),
    );
    expect(labels).toContain('拒绝');
    expect(labels).toContain('批准');
    expect(labels).not.toContain('×');
  });
});

/** 显式动作照旧生效 —— 上面的豁免不能把正常路径一起关掉。 */
describe('ApprovalDialog 的显式回答', () => {
  it('点「拒绝」才拒绝', () => {
    render();
    act(() => button('拒绝').click());

    expect(onReject).toHaveBeenCalledTimes(1);
    expect(onMinimize).not.toHaveBeenCalled();
  });

  it('点「批准」才批准', () => {
    render();
    act(() => button('批准').click());

    expect(onApprove).toHaveBeenCalledTimes(1);
    expect(onMinimize).not.toHaveBeenCalled();
  });

  it('点「收起」按钮只收起', () => {
    render();
    act(() => {
      container.querySelector<HTMLButtonElement>('button[aria-label="收起"]')!.click();
    });

    expect(onMinimize).toHaveBeenCalledTimes(1);
    expect(onReject).not.toHaveBeenCalled();
  });
});

/** 拒绝理由：模型唯一能看到的「为什么不让我做」。 */
describe('拒绝理由', () => {
  it('填了理由后点「拒绝」，理由跟着一起送出去', () => {
    render();
    typeReason('这台机器上不许动 nginx 配置');
    act(() => button('拒绝').click());

    expect(onReject).toHaveBeenCalledWith('这台机器上不许动 nginx 配置');
  });

  it('没填理由时拒绝照常，只是不带理由', () => {
    render();
    act(() => button('拒绝').click());

    expect(onReject).toHaveBeenCalledWith(undefined);
  });

  /// **最容易出事的一条**：用户打完理由顺手回车。
  /// 如果不把输入框里的键盘事件排除掉，全局的 Enter = 批准会先生效 ——
  /// 用户想拒绝，结果批准了。
  it('在理由输入框里按 Enter 是拒绝，绝不是批准', () => {
    render();
    const input = typeReason('别动生产库');
    pressKey('Enter', input);

    expect(onApprove).not.toHaveBeenCalled();
    expect(onReject).toHaveBeenCalledWith('别动生产库');
  });

  /// 反面：焦点不在输入框时，Enter 仍然是「批准」——上面的保护不能把正常路径关掉。
  it('焦点不在输入框时，Enter 还是批准', () => {
    render();
    pressKey('Enter');

    expect(onApprove).toHaveBeenCalledTimes(1);
    expect(onReject).not.toHaveBeenCalled();
  });

  it('只有空白的理由等于没填', () => {
    render();
    typeReason('   ');
    act(() => button('拒绝').click());

    expect(onReject).toHaveBeenCalledWith(undefined);
  });
});

/**
 * 键盘护栏只管**弹窗自己的**理由输入框。
 *
 * 监听器挂在 `document` 上：以前放行条件是「任意 INPUT/TEXTAREA 有焦点」，于是
 * 用户刚在面板输入框打完字、弹窗一到，Enter 和 Esc 就一起失效 —— 而弹窗还在
 * 提示那两个键能用。
 */
describe('键盘护栏只管自己的输入框', () => {
  it('焦点在弹窗以外的输入框时，Enter 仍是批准', () => {
    render();
    const foreign = document.createElement('input');
    document.body.appendChild(foreign);
    try {
      foreign.focus();
      pressKey('Enter', foreign);
      expect(onApprove).toHaveBeenCalledTimes(1);
      expect(onReject).not.toHaveBeenCalled();
    } finally {
      foreign.remove();
    }
  });

  it('焦点在弹窗以外的输入框时，Esc 仍是收起', () => {
    render();
    const foreign = document.createElement('input');
    document.body.appendChild(foreign);
    try {
      foreign.focus();
      pressKey('Escape', foreign);
      expect(onMinimize).toHaveBeenCalledTimes(1);
    } finally {
      foreign.remove();
    }
  });

  it('理由输入框里的 Esc 也是收起（与弹窗其他位置一致）', () => {
    render();
    const input = typeReason('x');
    pressKey('Escape', input);
    expect(onMinimize).toHaveBeenCalledTimes(1);
  });
});

/**
 * 队首前进时，上一条的理由不能带进下一条 —— 它是给「那条命令」的。
 *
 * 以前 `reason` 没有像 `mountedAtRef` 那样跟着 `toolCall.id` 重置，于是给 A 写的
 * 理由会原样发给 B（队列前进时是同一个组件实例）。
 */
describe('队首切换', () => {
  it('队首换成下一条后，理由输入框被清空', () => {
    render();
    typeReason('只针对这条');
    act(() => {
      root.render(
        <ApprovalDialog
          toolCall={{ ...TOOL_CALL, id: 'call-2' }}
          onApprove={onApprove}
          onReject={onReject}
          open={true}
          onClose={onClose}
          onMinimize={onMinimize}
          onRejectAndStop={onRejectAndStop}
        />,
      );
    });

    const input = container.querySelector<HTMLInputElement>('input[aria-label="拒绝原因"]')!;
    expect(input.value).toBe('');
    act(() => button('拒绝').click());
    expect(onReject).toHaveBeenCalledWith(undefined);
  });
});

/** 「拒绝并停止」：用户压根不想让 Agent 继续试。 */
describe('拒绝并停止任务', () => {
  it('走 onRejectAndStop 而不是 onReject', () => {
    render();
    act(() => button('拒绝并停止任务').click());

    expect(onRejectAndStop).toHaveBeenCalledTimes(1);
    expect(onReject).not.toHaveBeenCalled();
    expect(onApprove).not.toHaveBeenCalled();
  });

  it('理由同样带上', () => {
    render();
    const input = container.querySelector<HTMLInputElement>('input[aria-label="拒绝原因"]')!;
    act(() => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      setter.call(input, '整条路都不对');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => button('拒绝并停止任务').click());

    expect(onRejectAndStop).toHaveBeenCalledWith('整条路都不对');
  });

  it('调用方没提供该能力时，按钮不出现', () => {
    render(true, false);
    const labels = Array.from(container.querySelectorAll('button')).map((b) =>
      b.textContent?.trim(),
    );
    expect(labels).not.toContain('拒绝并停止任务');
  });
});

/**
 * 命令说明（bash 的必填 description）。
 *
 * 这一行是用户判断"要不要批准"的主要依据之一，所以两件事都要钉住：
 * 有说明时必须显示、且标注它是 agent 自述（不是系统判定）；没说明时不留空行
 * （旧会话、其他命令类工具都没有这个字段）。
 */
describe('Agent 说明', () => {
  const withDescription = (description: unknown): ToolCallInfo => ({
    ...TOOL_CALL,
    arguments: { command: 'systemctl restart nginx', description },
  });

  it('有说明时显示在命令上方，并标注来源', () => {
    render(true, true, withDescription('重启 nginx 以加载新配置'));

    const text = container.textContent ?? '';
    expect(text).toContain('Agent 说明');
    expect(text).toContain('重启 nginx 以加载新配置');

    // 位置：说明要排在命令前面（用户先读它再读命令）
    const labelAt = text.indexOf('Agent 说明');
    const cmdAt = text.indexOf('systemctl restart nginx');
    expect(labelAt).toBeGreaterThan(-1);
    expect(cmdAt).toBeGreaterThan(labelAt);
  });

  it('说明不再重复出现在参数 JSON 里', () => {
    render(true, true, withDescription('重启 nginx 以加载新配置'));

    const text = container.textContent ?? '';
    const occurrences = text.split('重启 nginx 以加载新配置').length - 1;
    expect(occurrences).toBe(1);
  });

  it('没有说明时不显示这一行，也不留空白', () => {
    render();
    expect(container.textContent ?? '').not.toContain('Agent 说明');
  });

  it('空白说明等同于没有', () => {
    render(true, true, withDescription('   '));
    expect(container.textContent ?? '').not.toContain('Agent 说明');
  });
});

/**
 * 文件改动的审批必须看到 diff，而不是一坨参数 JSON。
 *
 * `local_edit_file`（本机编辑）在后端与远端 `edit_file` 共用同一份展示 metadata
 * （`build_edit_display_metadata`），所以输入照远端那一行的形状造。曾经有两处会
 * 让它掉回 JSON：catalog 把它标成普通呈现、弹窗把交给 `FileChangeView` 的
 * toolName 写死成 `"edit_file"`。这里同时钉住「本机编辑走 diff」与「远端编辑没
 * 被这次取名字的改动带坏」。
 */
describe('审批面板的 diff 视图', () => {
  /** `edit_file` / `local_edit_file` 同形的参数与 metadata（后端同形，见上）。 */
  const EDIT_ARGS = {
    path: 'D:\\work\\app\\.env',
    old_content: 'PORT=80\nDEBUG=false',
    new_content: 'PORT=8080\nDEBUG=false',
    replace_all: false,
  };
  const EDIT_META = {
    path: 'D:\\work\\app\\.env',
    occurrences: 1,
    old_bytes: 20,
    new_bytes: 22,
    line_position: 1,
    line_count: 2,
    match_line_positions: [1],
    before: 'PORT=80\nDEBUG=false\n',
    after: 'PORT=8080\nDEBUG=false\n',
    file_content: 'PORT=8080\nDEBUG=false\n',
  };

  const localEditCall: ToolCallInfo = {
    id: 'call-local-edit',
    name: 'local_edit_file',
    arguments: { ...EDIT_ARGS },
    disposition: 'Approval',
    metadata: EDIT_META,
  };

  it('本机编辑（local_edit_file）渲染改动行，而不是参数 JSON', () => {
    render(true, true, localEditCall);

    const text = container.textContent ?? '';
    // 改动两侧都在：旧行（红）与新行（绿）
    expect(text).toContain('DEBUG=false');
    expect(text).toContain('PORT=8080');
    // 参数 JSON 分支才会出现参数字段名 —— 出现它说明又退回 JSON 了
    expect(text).not.toContain('old_content');
    expect(text).not.toContain('new_content');
    // 确认走的是 FileChangeView 的全文件对照 diff（data-match 是它的改动锚点）
    expect(container.querySelector('[data-match]')).not.toBeNull();
  });

  it('远端 edit_file 照旧走 diff（取名字改成从调用解析没有回退它）', () => {
    render(true, true, { ...localEditCall, id: 'call-remote-edit', name: 'edit_file' });

    const text = container.textContent ?? '';
    expect(text).toContain('PORT=8080');
    expect(text).not.toContain('old_content');
  });
});

/**
 * 超长正文不许把「批准 / 拒绝」挤出视口。
 *
 * 背景：外层是 `fixed inset-0 flex items-center justify-center`（不可滚），面板
 * 一旦高于视口，多出来的部分没有任何滚动条能到达 —— 按钮被顶到屏幕外，而
 * **Enter 恰好 = 批准**（Esc 只是收起）：按钮点不到的时候，键盘上那个「批准」
 * 还活着，用户可能在没看清内容的情况下批下去。
 *
 * 断开的是模型给的长文本（`description` / `reasons` / 参数 JSON 都无上限）。
 * jsdom 不做排版，所以这里钉的是「结构护栏」：面板限高 + 自己滚，且承载两个
 * 答案的底栏是 sticky —— 结构在，任何长度的正文都推不走它。
 */
describe('ApprovalDialog 的超长正文', () => {
  const LONG_TEXT = '这是一段很长的说明。'.repeat(400);

  /** 命令类工具（local_bash）：`description` 是模型自述，走「Agent 说明」那一支。 */
  const longCommandCall: ToolCallInfo = {
    id: 'call-long-cmd',
    name: 'local_bash',
    arguments: { command: 'Get-ChildItem -Recurse C:\\', description: LONG_TEXT },
    disposition: 'ForceApproval',
  };

  /** 派发本机子 agent：`description` + `prompt` 都是长正文。 */
  const longSubagentCall: ToolCallInfo = {
    id: 'call-long-subagent',
    name: 'local_subagent',
    arguments: { description: LONG_TEXT, prompt: LONG_TEXT, mode: 'plan' },
    disposition: 'ForceApproval',
  };

  function panel(): HTMLElement {
    const el = container.querySelector<HTMLElement>('.modal-panel-enter');
    if (!el) throw new Error('找不到审批面板');
    return el;
  }

  /** 往上找到承载答案的 sticky 底栏。 */
  function stickyFooterOf(el: Element): HTMLElement | null {
    let cur: Element | null = el;
    while (cur && cur !== document.body) {
      if (
        cur instanceof HTMLElement &&
        cur.className.includes('sticky') &&
        cur.className.includes('bottom-0')
      ) {
        return cur;
      }
      cur = cur.parentElement;
    }
    return null;
  }

  it.each([
    ['命令说明', longCommandCall],
    ['本机子 agent 的指令正文', longSubagentCall],
  ])('%s 再长也整段可见（检查点不许截断）', (_label, toolCall) => {
    render(true, true, toolCall);
    // 正文完整渲染：审批面板是用户唯一的检查点，不能为了好看截断
    expect(container.textContent ?? '').toContain(LONG_TEXT);
  });

  it.each([
    ['命令说明', longCommandCall],
    ['本机子 agent 的指令正文', longSubagentCall],
  ])('%s 超长时：面板限高自己滚，批准 / 拒绝钉在底栏', (_label, toolCall) => {
    render(true, true, toolCall);

    const p = panel();
    // 限高 + 滚动都必须在面板这一层：外层容器不可滚
    expect(p.className).toContain('max-h-[85vh]');
    expect(p.className).toContain('overflow-y-auto');

    const approveBtn = button('批准');
    const footer = stickyFooterOf(approveBtn);
    expect(footer, '批准按钮必须在一个 sticky 底栏里').not.toBeNull();
    expect(p.contains(footer as HTMLElement)).toBe(true);
    // 拒绝也在同一条底栏里 —— 两个答案要在一起，不能只钉住一个
    expect((footer as HTMLElement).contains(button('拒绝'))).toBe(true);
  });
});
