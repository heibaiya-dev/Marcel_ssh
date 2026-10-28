import { memo, useContext, useEffect, useMemo, useRef, useState } from 'react';
import Markdown, { type Components } from 'react-markdown';
import type { PluggableList } from 'unified';
import { markdownRemarkPlugins, markdownRehypePlugins } from '@/lib/markdownPlugins';
import { normalizeMathDelimiters } from '@/lib/mathDelimiters';
import { markdownRenderManager } from '@/lib/markdownRenderManager';
import type { MarkdownTree } from '@/lib/markdownProcessor';
import { openExternalLink } from '@/lib/externalLinks';
import { MarkdownVisibility } from './markdownVisibility';
import CodeBlock from './CodeBlock';

const components: Components = {
  pre: CodeBlock,
  a: ({ href, children, node: _node, ...props }) => (
    <a {...props} href={href} onClick={(event) => {
      event.preventDefault();
      if (href) openExternalLink(href);
    }}>{children}</a>
  ),
};

/** Keep react-markdown's HTML escaping and URL filtering for worker-produced trees too. */
const CompiledMarkdown = memo(function CompiledMarkdown({ tree }: { tree: MarkdownTree }) {
  const plugins = useMemo<PluggableList>(() => [() => () => tree], [tree]);
  return <Markdown rehypePlugins={plugins} components={components} />;
});

const SynchronousMarkdown = memo(function SynchronousMarkdown({ content }: { content: string }) {
  return <Markdown remarkPlugins={markdownRemarkPlugins} rehypePlugins={markdownRehypePlugins} components={components}>
    {normalizeMathDelimiters(content)}
  </Markdown>;
});

/**
 * `sync`：不经 worker、当帧直接渲染。短文本、会反复重挂载的场景（设置页里的整理助手）用它，
 * 免得每次挂载先露一帧未渲染的原文、再换成排版后的样子（一闪）；长对话列表保持默认的异步渲染。
 */
export default memo(function MarkdownBody({ content, sync = false }: { content: string; sync?: boolean }) {
  const visible = useContext(MarkdownVisibility);
  const owner = useRef(Symbol('markdown'));
  const [rendered, setRendered] = useState<{ tree: MarkdownTree; source: string } | null>(null);
  const [fallback, setFallback] = useState(() => typeof Worker === 'undefined');
  useEffect(() => {
    if (fallback || sync || !visible) return;
    return markdownRenderManager.request(owner.current, content, (tree, source) => {
      if (tree) setRendered({ tree, source });
      else setFallback(true);
    });
  }, [content, fallback, sync, visible]);

  if (fallback || sync) {
    return <SynchronousMarkdown content={content} />;
  }
  return rendered && content.startsWith(rendered.source)
    ? <CompiledMarkdown tree={rendered.tree} />
    : <div className="whitespace-pre-wrap">{content}</div>;
});
