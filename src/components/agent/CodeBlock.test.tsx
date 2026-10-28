// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import MarkdownBody from './MarkdownBody';
import { codeToCopy } from '@/lib/hastText';
import { copyText } from '@/lib/copyText';

vi.mock('@/lib/copyText', () => ({ copyText: vi.fn(() => Promise.resolve()) }));
vi.mock('@/lib/externalLinks', () => ({ openExternalLink: vi.fn() }));

let root: Root;
let host: HTMLDivElement;
beforeEach(() => {
  vi.clearAllMocks();
  vi.useFakeTimers();
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

// sync：走同步渲染，免去 worker
async function render(content: string) {
  await act(async () => root.render(<MarkdownBody content={content} sync />));
}
const button = () => host.querySelector<HTMLButtonElement>('.marcel-code-copy');

it('joins highlighted spans back into the plain code and drops the fence newline', () => {
  const node = {
    type: 'element',
    children: [{
      type: 'element',
      children: [
        { type: 'element', children: [{ type: 'text', value: 'const' }] },
        { type: 'text', value: ' a = 1;\n' },
      ],
    }],
  };
  expect(codeToCopy(node)).toBe('const a = 1;');
});

it('copies the whole fenced block, without the surrounding prose', async () => {
  await render('前面的话\n\n```ts\nconst a = 1;\nconsole.log(a);\n```\n\n后面的话');
  expect(host.querySelectorAll('.marcel-code-copy')).toHaveLength(1);
  await act(async () => button()!.click());
  expect(copyText).toHaveBeenCalledWith('const a = 1;\nconsole.log(a);');
  expect(button()!.dataset.state).toBe('copied');
  await act(async () => { vi.advanceTimersByTime(1400); });
  expect(button()!.dataset.state).toBe('idle');
});

it('shows the fence language in the bar, falling back to text', async () => {
  await render('```python\nprint(1)\n```\n\n```\nplain\n```');
  const bars = host.querySelectorAll('.marcel-code-bar');
  expect(bars[0].firstElementChild!.textContent).toBe('python');
  expect(bars[1].firstElementChild!.textContent).toBe('text');
});

it('gives every block its own button and leaves inline code alone', async () => {
  await render('行内 `x` 代码\n\n```\na\n```\n\n```\nb\n```');
  expect(host.querySelectorAll('.marcel-code-copy')).toHaveLength(2);
  await act(async () => host.querySelectorAll<HTMLButtonElement>('.marcel-code-copy')[1].click());
  expect(copyText).toHaveBeenCalledWith('b');
});

it('shows a failure state when the clipboard rejects', async () => {
  vi.mocked(copyText).mockRejectedValueOnce(new Error('denied'));
  await render('```\nx\n```');
  await act(async () => button()!.click());
  expect(button()!.dataset.state).toBe('failed');
  expect(button()!.textContent).toBe('复制失败');
});

it('keeps the button inside the bar, never over the code', async () => {
  await render('```\nx\n```');
  expect(button()!.closest('pre')).toBeNull();
  expect(button()!.closest('.marcel-code-bar')).not.toBeNull();
});
