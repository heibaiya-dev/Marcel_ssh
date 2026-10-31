# 001 — 消除流式文本闪现与滚动抢位置

- **Status**: TODO
- **Commit**: f1b56f1
- **Severity**: HIGH
- **Category**: Performance / Interruptibility
- **Estimated scope**: 4–6 files，约 160 行

## Problem

`src/components/agent/MarkdownBody.tsx:128-131` 当前在解析结果落后于节拍文本时，把整棵 Markdown 树替换成裸文本：

```tsx
if (rendered && shown.startsWith(rendered.source)) {
  return <CompiledMarkdown tree={tree ?? rendered.tree} />;
}
return <div className="whitespace-pre-wrap">{shown}</div>;
```

每次异步解析尚未交付时，排版树与 `whitespace-pre-wrap` 裸文本交替上屏，产生闪现。`src/components/agent/AgentTranscript.tsx:89-95` 的 ResizeObserver 又不检查 `userScrolling` 就同步贴底，而 `scrollToBottom()` 会重置滚动意图：

```tsx
const observer = new ResizeObserver(() => {
  if (nearBottomRef.current) scrollToBottom();
  else if (containerRef.current) { /* … */ }
});
```

最后，`src/components/agent/AgentTranscript.tsx:71-81` 以 `lastMessage` 对象为依赖，流式每个 delta 都会触发一次贴底，与 ResizeObserver 重复。

## Target

1. 已存在解析树时永不整块退回裸文本；保留树，把 `shown.slice(rendered.source.length)` 的未解析尾巴追加到树中最后一个文本节点，只有尾路径需要克隆。
2. 只有完全没有可用树时，才允许首帧裸文本；`fallback || sync` 仍走 `SynchronousMarkdown`。
3. ResizeObserver 在 `userScrolling.current === true` 时不得写 `scrollTop`，也不得重置滚动意图。
4. 消息内容增长只由 ResizeObserver 跟随；`lastMessage` effect 只在新消息 id、会话切换或用户刚发送时贴底。
5. 墨水晕染仍是单层、只作用于最新范围；不得改成逐字动画。

## Repo conventions to follow

- 树增量处理沿用 `src/lib/streamInk.ts` 的不可变克隆方式；`pre/code/a` 等 opaque 节点处理见 `OPAQUE_TAGS`。
- 流式测试使用 `src/components/agent/MarkdownBody.test.tsx` 与 `src/lib/streamInk.test.ts` 的 fake-timer/worker mock 风格。
- 滚动行为测试沿用 `src/components/agent/scrollFollow.test.tsx`、`AgentTranscript.foldScroll.test.tsx`。

## Steps

1. 在 `src/lib/streamInk.ts` 增加 `appendStreamingTail(tree, tail)`：`tail === ''` 原样返回；否则沿最后一个 child 找到最后的 text 节点，只克隆该路径，把 tail 拼到该 text.value 末尾。没有文本节点时返回带尾部 paragraph 的新 root。导出纯函数，不访问 DOM。
2. 为该函数补测试：普通段落、代码块、空树、tail 为空、原树不被修改。
3. `MarkdownBody.tsx` 增加 memo：当 `rendered && shown.startsWith(rendered.source)` 时，以 `tree ?? rendered.tree` 为基树，调用 `appendStreamingTail(base, shown.slice(rendered.source.length))`；始终渲染 `CompiledMarkdown`。无 `rendered` 时才保留原文首帧。
4. 更新 `MarkdownBody.test.tsx`： paced 文本超前解析结果时，屏幕必须同时包含已排版前缀与未解析尾巴；DOM 不得切换成整段 `whitespace-pre-wrap` 裸文本；结束直播后最后一帧不闪回。
5. `AgentTranscript.tsx` 的 ResizeObserver 首行增加 `if (userScrolling.current) return;`，依赖数组补 `userScrolling`。
6. 将 `lastMessage` effect 的内容增长跟随改为只跟踪 `lastMessage?.id`；会话切换、首条消息、`userJustSentRef` 行为不变。不得删除 `visible`/`canInteract` 守卫。
7. 补一条滚动测试：用户向上滚动且 `userScrolling=true` 时，内容 ResizeObserver 触发后 scrollTop 不被改写。

## Boundaries

- **禁止修改** `compose-split`、`tool-card-row`、`transient-reveal`、`ToolCallCard`、`ToolFoldRow`、`toolCardChrome`、`toolGroupReveal` 及其 CSS/测试。
- 不改 `STREAM_TICK_MS`、`STREAM_MIN_CHUNK_CHARS`、`STREAM_MAX_HOLD_MS`，不重做 pacing。
- 不改 virtua 配置、消息数据结构、worker 协议。
- 不新增依赖。

## Verification

- **Mechanical**: `pnpm exec tsc --noEmit` 通过；`pnpm vitest run src/lib/streamInk.test.ts src/components/agent/MarkdownBody.test.tsx src/components/agent/scrollFollow.test.tsx src/components/agent/AgentTranscript.foldScroll.test.tsx` 通过。
- **Feel check**: 流式回答超过一屏时，文字只向前增长，不出现排版/裸文本交替；向上滚动阅读时不会被拽回底部；松开并回到底部后恢复跟随。
- **Done when**: 所有上述测试通过，且 diff 不含任何工具调用动画路径。
