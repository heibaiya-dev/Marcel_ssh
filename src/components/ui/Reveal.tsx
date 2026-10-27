import { useEffect, useState, type ReactNode } from 'react';
import { useAnimatedPresence } from '@/hooks/useAnimatedPresence';

/**
 * 通用的「高度长出来 / 收回去」出入场壳：条件显示的一整块内容（折叠区、开关打开后才出现的子设置）
 * 用它包住，而不是 `{show && <div/>}` 当场顶出来 —— 后者会让下面所有内容瞬间跳一下。
 *
 * 动画是 `globals.css` 里 `.transient-reveal*` 那一套（grid-template-rows 0fr↔1fr，内容高度不用量，
 * spring-smooth 曲线，「减少动态效果」下压成 1ms 但仍靠 animationend 卸载）。
 * 收起期间子内容仍由调用方照常渲染，需要让它在此期间保持不变。
 *
 * 初始就是显示的（比如进设置页时开关本来就开着）不播进场动画：用户没做任何事，不该有东西在动。
 * 之后任何一次「先隐藏再显示」才播。
 */
export default function Reveal({ show, children }: { show: boolean; children: ReactNode }) {
  const presence = useAnimatedPresence(show);
  const [skipEnter, setSkipEnter] = useState(show);
  useEffect(() => {
    if (!show) setSkipEnter(false);
  }, [show]);

  if (!presence.mounted) return null;
  const exiting = presence.phase === 'exit';
  const phaseClass = exiting
    ? 'transient-reveal-exit'
    : skipEnter
      ? ''
      : 'transient-reveal-enter';
  return (
    <div
      className={`transient-reveal ${phaseClass}`}
      aria-hidden={exiting || undefined}
      onAnimationEnd={presence.onAnimationEnd}
    >
      <div>{children}</div>
    </div>
  );
}
