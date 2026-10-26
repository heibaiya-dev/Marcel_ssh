import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';

interface Props<T> {
  /** 当前值；变化时播一遍「新内容自左向右扫过旧内容」的过渡。 */
  value: T;
  /** 按值渲染内容。新旧两层都用它渲染，所以必须是纯函数。 */
  render: (value: T) => ReactNode;
  className?: string;
  style?: React.CSSProperties;
  ariaLabel?: string;
}

/** 与 globals.css `.sweep-reveal` 的时长一致；兜底卸载旧层用。 */
const SWEEP_FALLBACK_MS = 900;

function prefersReducedMotion(): boolean {
  return typeof window !== 'undefined' && !!window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
}

/**
 * 值变化时的「扫光揭示」：新内容用渐变蒙版自左向右揭开，盖在旧内容上，蒙版前沿带一道光。
 * 用来把「这次改了什么」演给用户看（字体预览、主题预览之类），样式见 globals.css 的 `.sweep-*`。
 *
 * 新旧两层叠在同一个网格格子里，尺寸由两层中较大的一个撑开，过渡中不会抖动。
 * 「减少动态效果」下直接换内容。
 */
export default function SweepTransition<T>({ value, render, className = '', style, ariaLabel }: Props<T>) {
  const shown = useRef(value);
  // id 只增不减、结束后也保留：顶层 key 不变，扫完不会重挂载
  const [sweep, setSweep] = useState<{ from: T; id: number; active: boolean } | null>(null);

  // 用 layout effect：值变化的那一帧还没绘制就把扫光层挂上，否则会先闪一帧「已经是新内容」再从头扫
  useLayoutEffect(() => {
    if (Object.is(shown.current, value)) return;
    const from = shown.current;
    shown.current = value;
    setSweep((s) => ({ from, id: (s?.id ?? 0) + 1, active: !prefersReducedMotion() }));
  }, [value]);

  // animationend 可能因标签页隐藏等原因不触发，超时兜底清掉旧层。
  useEffect(() => {
    if (!sweep?.active) return;
    const timer = setTimeout(() => setSweep((s) => s && { ...s, active: false }), SWEEP_FALLBACK_MS);
    return () => clearTimeout(timer);
  }, [sweep]);
  const active = !!sweep?.active;

  return (
    <div aria-label={ariaLabel} className={`relative grid overflow-hidden ${className}`} style={style}>
      {active && sweep && (
        // key 与新层同步：连续换值时旧层必须跟新层一起从头起动画。旧层若复用上一轮的元素，
        // 它的蒙版进度会比新层超前，两层之间就出现「谁都没显示」的空白带（快速连点字体时发生）
        <div key={`hide-${sweep.id}`} className="col-start-1 row-start-1 sweep-hide" aria-hidden>
          {render(sweep.from)}
        </div>
      )}
      <div
        key={sweep?.id ?? 0}
        className={`col-start-1 row-start-1 ${active ? 'sweep-reveal' : ''}`}
        onAnimationEnd={(e) => {
          if (e.target === e.currentTarget) setSweep((s) => s && { ...s, active: false });
        }}
      >
        {render(value)}
      </div>
      {active && sweep && <div key={`glint-${sweep.id}`} className="sweep-glint" aria-hidden />}
    </div>
  );
}
