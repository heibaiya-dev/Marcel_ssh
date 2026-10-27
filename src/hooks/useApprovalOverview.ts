import { useEffect, useRef, useState } from 'react';
import * as tauri from '@/lib/tauri';
import type { AgentModeSettings, ApprovalOverview } from '@/lib/types';

/**
 * 「什么时候问你」总览表的数据（桌面与手机共用）。
 *
 * 表里每个格子都是后端用真实执行时的判定算出来的（`agent_approval_overview`），前端不自己
 * 推：规则、开关怎么改，表都不会和实际行为对不上。用的是设置页里的**草稿**，开关一拨表就
 * 跟着变，不用先保存。只有跟这张表有关的几个字段变了才重新取（别的字段如提示词、超时不触发）。
 *
 * 取回来之前、或后端失败时返回 `null`，调用方不画表（不拿猜的值顶上）。
 */
export function useApprovalOverview(
  agent: AgentModeSettings,
): ApprovalOverview | null {
  const [overview, setOverview] = useState<ApprovalOverview | null>(null);
  const seq = useRef(0);

  const relevant = {
    listMode: agent.listMode,
    allowCommands: agent.allowCommands,
    confirmEachCommand: agent.confirmEachCommand,
    planModeRequiresApproval: agent.planModeRequiresApproval,
    autoModeForceApproval: agent.autoModeForceApproval,
  };
  const key = JSON.stringify(relevant);

  useEffect(() => {
    const mine = ++seq.current;
    tauri
      .agentApprovalOverview(agent)
      .then((o) => {
        if (seq.current === mine) setOverview(o);
      })
      .catch((e) => {
        // 总览只是帮助理解的呈现，取不到不该挡住设置页：不画表即可。
        console.error('取审批总览失败:', e);
        if (seq.current === mine) setOverview(null);
      });
    // agent 整个对象每次渲染都新，用 key 代表「与总览有关的内容变了」。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  return overview;
}
