/**
 * 流式文字的「水墨晕染」：只给**最新到达的那一小段**字包一个 `.ink-a` / `.ink-b`
 * 内联 span，让它从「略糊 + 半透明」收敛到清晰（样式见 globals.css）。
 *
 * 为什么是「只包最新一段、一次一段」：
 * - 早先按「距结尾第 N 个字」开窗，每来一个 delta 窗口整体前移，span 全换字符、
 *   换延迟，React 按位置复用会把动画反复重启 —— 屏幕上就是一卡一卡地闪。
 * - 只包最新一段时，结构变化只有「上一段 span 退回文本、新一段变成 span」两处，
 *   上一段的动画早已播完（130ms），退回去看不出任何变化。
 * - 两个动画名交替使用（`.ink-a` / `.ink-b`）：React 复用同一个 span 元素时，
 *   class 变化会让新的一段重新起播，不会出现「新文字不播动画」。
 *
 * 不拆 `pre` / `code` / `a` / KaTeX 等子树：拆开会破坏高亮结构、公式排版与
 * 链接点击区，复制出来的文本还会多出标签边界；这些子树里的字只推进计数。
 */

export interface InkNode {
  type?: string;
  value?: string;
  tagName?: string;
  properties?: Record<string, unknown>;
  children?: InkNode[];
}

/** 这些子树里的字只推进计数、不包 span。 */
const OPAQUE_TAGS = new Set(['pre', 'code', 'a', 'kbd', 'samp', 'textarea', 'script', 'style']);

/** 自带排版、拆开会坏掉的根节点（KaTeX 公式、高亮代码）。 */
function isOpaqueElement(node: InkNode): boolean {
  if (OPAQUE_TAGS.has(node.tagName ?? '')) return true;
  const className = node.properties?.className;
  const names = Array.isArray(className)
    ? className
    : typeof className === 'string'
      ? className.split(/\s+/)
      : [];
  return names.some((name) => typeof name === 'string' && (name.startsWith('katex') || name === 'hljs'));
}

/** 一段字符区间（渲染文本的序号，左闭右开）。 */
export interface InkRange {
  start: number;
  end: number;
}

/** 渲染文本的字符数（按码点，与 span 切分口径一致）。 */
export function inkTextLength(node: InkNode | undefined): number {
  if (!node) return 0;
  if (node.type === 'text') {
    let count = 0;
    for (const _ of node.value ?? '') count += 1;
    return count;
  }
  const children = node.children;
  if (!children) return 0;
  let total = 0;
  for (const child of children) total += inkTextLength(child);
  return total;
}

interface Cursor {
  index: number;
}

function wrapNode(node: InkNode, range: InkRange, inkClass: string, cursor: Cursor, opaque: boolean): InkNode {
  if (node.type === 'text') {
    const chars = Array.from(node.value ?? '');
    if (chars.length === 0) return node;
    const start = cursor.index;
    const end = start + chars.length;
    cursor.index = end;
    if (opaque) return node;
    const from = Math.max(range.start, start);
    const to = Math.min(range.end, end);
    if (to <= from) return node;

    const parts: InkNode[] = [];
    const head = chars.slice(0, from - start).join('');
    if (head) parts.push({ type: 'text', value: head });
    parts.push({
      type: 'element',
      tagName: 'span',
      properties: { className: [inkClass] },
      children: [{ type: 'text', value: chars.slice(from - start, to - start).join('') }],
    });
    const tail = chars.slice(to - start).join('');
    if (tail) parts.push({ type: 'text', value: tail });
    // 文本节点不能带 children：包一层无样式的内联 span 当替身（不影响断行与复制）。
    return { type: 'element', tagName: 'span', properties: { className: ['ink-run'] }, children: parts };
  }

  const children = node.children;
  if (!children || children.length === 0) return node;
  const childOpaque = opaque || isOpaqueElement(node);
  let changed = false;
  const next = children.map((child) => {
    const replaced = wrapNode(child, range, inkClass, cursor, childOpaque);
    if (replaced !== child) changed = true;
    return replaced;
  });
  return changed ? { ...node, children: next } : node;
}

export interface InkResult<T> {
  tree: T;
  /** 整棵树的渲染字符数（顺带算出来，省一次全量遍历）。 */
  length: number;
}

/**
 * 给 `[range.start, range.end)` 这段字挂一个晕染 span；区间外保持纯文本。
 * `range = null` 表示只量长度、不包 span。
 * `even` 决定用 `.ink-a` 还是 `.ink-b`（交替让复用元素时动画重新起播）。
 * 返回的树与入参结构共享，未改动的节点保持同一引用。
 */
export function wrapInkRange<T extends InkNode>(
  tree: T,
  range: InkRange | null,
  even = true,
): InkResult<T> {
  if (!tree) return { tree, length: 0 };
  const usable = range && range.end > range.start ? range : null;
  const cursor: Cursor = { index: 0 };
  const next = wrapNode(tree, usable ?? { start: 0, end: 0 }, even ? 'ink-a' : 'ink-b', cursor, false) as T;
  return { tree: usable ? next : tree, length: cursor.index };
}

/**
 * 解析结果落后于显示节拍时，把尚未解析的尾巴接到最后一个文本节点。
 * 只克隆尾路径：已排版的前缀保持原引用，绝不整块退回裸文本。
 */
export function appendStreamingTail<T extends InkNode>(tree: T, tail: string): T {
  if (!tail) return tree;
  let appended = false;
  const visit = (node: InkNode): InkNode => {
    if (appended) return node;
    if (node.type === 'text') {
      appended = true;
      return { ...node, value: (node.value ?? '') + tail };
    }
    const children = node.children;
    if (!children?.length) return node;
    for (let index = children.length - 1; index >= 0; index--) {
      const next = visit(children[index]);
      if (next !== children[index]) {
        const nextChildren = [...children];
        nextChildren[index] = next;
        return { ...node, children: nextChildren };
      }
    }
    return node;
  };
  const next = visit(tree);
  if (next !== tree) return next as T;
  return {
    ...tree,
    children: [
      ...(tree.children ?? []),
      { type: 'element', tagName: 'p', children: [{ type: 'text', value: tail }] },
    ],
  };
}
