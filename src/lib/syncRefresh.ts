import { useEffect, useRef } from 'react';
import { useSettingsStore } from '@/stores/settingsStore';
import { useConnectionStore } from '@/stores/connectionStore';
import { useQuickCommandStore } from '@/stores/quickCommandStore';
import { useSkillStore } from '@/stores/skillStore';
import { useMcpStore } from '@/stores/mcpStore';
import { getErrorMessage } from '@/lib/errors';

/**
 * 跨设备同步：后端把别的设备的改动落到本机某个同步源后会发 `sync-data-applied { source }`，
 * 前端必须把对应的内存 store 重新读一遍——否则界面还在显示旧数据，而下一次用户在界面上
 * 动一下就会把旧数据整份存回去，等于把刚同步来的改动又改回去（还会被当成本机改动同步出去）。
 *
 * 这里是「同步源 → 刷新函数」的唯一一张表。后端新增同步源时：
 * 1. 在 `SYNC_SOURCES` 里加上它的 key（与后端 `DataSourceId::sync_key` 一致，跨版本稳定）；
 * 2. `Record<SyncSource, …>` 的类型会逼着在 `SYNC_REFRESHERS` 里补一个刷新函数；
 * 3. `syncRefresh.test.ts` 里的后端 source 清单快照也要同步（它钉住「后端每个 source 都有人刷新」）。
 */
export const SYNC_SOURCES = [
  'settings',
  'connections',
  'quickCommands',
  'skills',
  'mcpServers',
  'knownHosts',
] as const;

export type SyncSource = (typeof SYNC_SOURCES)[number];

type Refresher = () => Promise<void> | void;

export const SYNC_REFRESHERS: Record<SyncSource, Refresher> = {
  // 设置走自己的串行队列（与保存互斥），见 settingsStore.refreshFromBackend。
  settings: () => useSettingsStore.getState().refreshFromBackend(),
  // 以下都用静默刷新：同步是后台发生的事，列表不该因此闪一下「加载中」。
  connections: () => useConnectionStore.getState().fetchConnections({ silent: true }),
  quickCommands: () => {
    const { lastSessionKey, load } = useQuickCommandStore.getState();
    return load(lastSessionKey, { silent: true });
  },
  skills: () => useSkillStore.getState().fetchSkills({ silent: true }),
  mcpServers: () => useMcpStore.getState().fetchServers({ silent: true }),
  // 已知主机指纹只在后端连接时校验，前端没有缓存，无需刷新。
  knownHosts: () => undefined,
};

function isSyncSource(source: string): source is SyncSource {
  return (SYNC_SOURCES as readonly string[]).includes(source);
}

type AppliedListener = (source: SyncSource) => void;
const appliedListeners = new Set<AppliedListener>();

/**
 * 订阅「某个同步源被别的设备的改动更新了」。给**自己持有本地状态**（没走上面那些 store）的
 * 界面用，例如手机端快捷命令管理页。刷新函数已经跑完（store 里是新数据）之后才通知。
 */
export function onSyncDataApplied(listener: AppliedListener): () => void {
  appliedListeners.add(listener);
  return () => {
    appliedListeners.delete(listener);
  };
}

/** React 版：卸载自动退订；回调不必 memo。`sources` 为空 = 任何源都通知。 */
export function useSyncDataApplied(
  sources: readonly SyncSource[],
  callback: (source: SyncSource) => void,
): void {
  const cbRef = useRef(callback);
  cbRef.current = callback;
  const key = sources.join(',');
  useEffect(() => {
    const wanted = key ? key.split(',') : [];
    return onSyncDataApplied((source) => {
      if (wanted.length === 0 || wanted.includes(source)) cbRef.current(source);
    });
  }, [key]);
}

/**
 * `sync-data-applied` 的处理入口：刷新对应 store，再通知本地状态型界面。
 * 未知 source（后端比前端新）只记一条日志，不当成错误——旧前端不认识的数据源本来就没有界面。
 */
export async function handleSyncDataApplied(source: string): Promise<void> {
  if (!isSyncSource(source)) {
    console.warn(`[sync] 收到未知的同步源 ${source}，前端没有对应的刷新函数`);
    return;
  }
  try {
    await SYNC_REFRESHERS[source]();
  } catch (err) {
    console.error(`[sync] 刷新 ${source} 失败:`, getErrorMessage(err));
  }
  for (const listener of [...appliedListeners]) {
    try {
      listener(source);
    } catch (err) {
      console.error('[sync] 同步落地监听器抛错:', getErrorMessage(err));
    }
  }
}
