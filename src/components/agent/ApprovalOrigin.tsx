/**
 * 审批来自哪个会话 / 哪个对话。
 *
 * 只是一行灰色小字，不画框。用户正盯着发起审批的那个会话时就这样；审批来自**别的**
 * 会话时，同一行多出「来自其他会话」和一个文字链接「跳转」—— 那时用户最容易困惑
 * 「这是哪来的」，给出口，但不靠色块抢戏。桌面审批弹窗与手机审批底部面板共用。
 */
export function ApprovalOrigin({
  sessionName,
  conversationTitle,
  isCurrentContext,
  onNavigateToContext,
  compact,
}: {
  sessionName?: string;
  conversationTitle?: string;
  isCurrentContext: boolean;
  onNavigateToContext?: (e?: React.MouseEvent) => void;
  /** 手机：跳转点击区更大。 */
  compact?: boolean;
}) {
  if (!sessionName && !conversationTitle) return null;

  return (
    <div className="flex items-center justify-between gap-3 text-xs text-zinc-500">
      <span className="min-w-0 truncate">
        {!isCurrentContext && <span>来自其他会话 · </span>}
        <span className="text-zinc-300">{sessionName || 'SSH 会话'}</span>
        {conversationTitle && <span> · {conversationTitle}</span>}
      </span>
      {!isCurrentContext && onNavigateToContext && (
        <button
          type="button"
          onClick={onNavigateToContext}
          className={`shrink-0 rounded-md text-zinc-300 transition-colors hover:text-zinc-100 active:text-zinc-100 ${
            compact ? 'px-1 py-1.5' : 'py-0.5'
          }`}
        >
          跳转 ›
        </button>
      )}
    </div>
  );
}
