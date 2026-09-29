import { useState, useCallback, useEffect, useRef } from 'react';

interface UseResizablePanelOptions {
  initialWidth: number;
  minWidth: number;
  maxWidth: number;
  /** 宽度提交回调：**拖动结束（松手）后**调用一次，拖动过程中不调用。 */
  onChange?: (width: number) => void;
  /**
   * Which side the panel sits on relative to the drag handle.
   * - `right` (default): handle on panel's left edge; drag left → wider (agent/sidebar style)
   * - `left`: handle on panel's right edge; drag right → wider (file tree)
   */
  edge?: 'left' | 'right';
}

export function useResizablePanel({
  initialWidth,
  minWidth,
  maxWidth,
  onChange,
  edge = 'right',
}: UseResizablePanelOptions) {
  const [width, setWidth] = useState(initialWidth);
  const [isResizing, setIsResizing] = useState(false);
  const resizeStartRef = useRef<{ x: number; width: number } | null>(null);
  /** 拖动期间的最新宽度：松手时才会交给 onChange（见 handleMouseUp）。 */
  const latestWidthRef = useRef(initialWidth);
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const edgeRef = useRef(edge);
  edgeRef.current = edge;

  const startResize = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    resizeStartRef.current = { x: e.clientX, width };
    latestWidthRef.current = width;
    setIsResizing(true);
  }, [width]);

  useEffect(() => {
    if (!isResizing) return;

    const handleMouseMove = (e: MouseEvent) => {
      if (!resizeStartRef.current) return;
      const raw =
        edgeRef.current === 'left'
          ? e.clientX - resizeStartRef.current.x
          : resizeStartRef.current.x - e.clientX;
      const newWidth = Math.min(maxWidth, Math.max(minWidth, resizeStartRef.current.width + raw));
      setWidth(newWidth);
      // 只改本地状态。此前每个 mousemove 都调 onChange ⇒ 每次拖动 60~120 次
      // 「整份设置序列化 + IPC + 写盘」（调用方 FileManagerPanel.persistTreeWidth）。
      latestWidthRef.current = newWidth;
    };

    const handleMouseUp = () => {
      const start = resizeStartRef.current;
      resizeStartRef.current = null;
      setIsResizing(false);
      // 松手才提交一次，且只有宽度真的变了才提交（单击手柄不该产生写盘）。
      if (start && start.width !== latestWidthRef.current) {
        onChangeRef.current?.(latestWidthRef.current);
      }
    };

    document.addEventListener('mousemove', handleMouseMove);
    document.addEventListener('mouseup', handleMouseUp);
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';

    return () => {
      document.removeEventListener('mousemove', handleMouseMove);
      document.removeEventListener('mouseup', handleMouseUp);
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    };
  }, [isResizing, minWidth, maxWidth]);

  return { width, isResizing, startResize, setWidth };
}
