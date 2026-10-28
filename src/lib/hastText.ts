export type HastNode = {
  type?: string;
  value?: string;
  properties?: { className?: unknown };
  children?: HastNode[];
};

/** 围栏的语言标注（```ts → "ts"）：react-markdown 把它挂在 <code> 的 language-xxx 类上。 */
export function codeLanguage(pre: HastNode | undefined): string | null {
  const className = pre?.children?.[0]?.properties?.className;
  if (!Array.isArray(className)) return null;
  const tag = className.find((c): c is string => typeof c === 'string' && c.startsWith('language-'));
  return tag ? tag.slice('language-'.length) : null;
}

/** 取代码块的纯文本：语法高亮会把代码拆成一串 span，这里把它们的文字原样接回去。 */
export function hastText(node: HastNode | undefined): string {
  if (!node) return '';
  if (node.type === 'text') return node.value ?? '';
  return (node.children ?? []).map(hastText).join('');
}

/** 围栏代码块末尾那个由 Markdown 补上的换行不属于代码，复制时去掉一个。 */
export function codeToCopy(node: HastNode | undefined): string {
  return hastText(node).replace(/\n$/, '');
}
