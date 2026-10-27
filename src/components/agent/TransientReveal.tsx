import type { ReactNode } from 'react';
import Reveal from '@/components/ui/Reveal';
import { useShowAfter } from '@/hooks/useShowAfter';

/**
 * 瞬态内容出现前的观望时间（毫秒）：持续存在不到这么久的内容不显示。
 * 取值依据：低于约 400ms 的操作用户感觉是「瞬间完成」，此时再弹一块出来只会抽动；
 * 超过它，用户已经在等了，实时反馈才有价值。
 */
export const TRANSIENT_REVEAL_DELAY_MS = 400;

/**
 * 工具卡里的瞬态块（执行中的实时输出、「模型审批中」）的出入场壳。
 *
 * 过去这类块是条件渲染的：第一行输出一到就整块（最高 120px）瞬间顶出来，命令一
 * 结束又瞬间消失，卡片下面的内容跟着上下跳两次；命令快的时候两次跳动挨在一起，
 * 就是「弹起来又突然没了」。现在：
 *  - 出现：先观望 {@link TRANSIENT_REVEAL_DELAY_MS}（快命令根本不出现），再用高度
 *    从 0 长出来的动画进场；
 *  - 消失：`show` 变假后高度收回 0 再卸载，而不是当场拔掉。
 *
 * 出入场动画本身是通用的 `ui/Reveal`；这里只加「观望」这一层。
 * 子内容在收起动画期间仍由调用方照常渲染，调用方需要让它在此期间保持不变
 * （例如实时输出定格在最后一帧），否则收起的过程中内容会变。
 */
export default function TransientReveal({
  show,
  delayMs = TRANSIENT_REVEAL_DELAY_MS,
  children,
}: {
  show: boolean;
  delayMs?: number;
  children: ReactNode;
}) {
  const visible = useShowAfter(show, delayMs);
  return <Reveal show={visible}>{children}</Reveal>;
}
