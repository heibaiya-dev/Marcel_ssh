import { beforeEach, describe, expect, it, vi } from 'vitest';
import * as tauri from '@/lib/tauri';
import { useSettingsStore } from '@/stores/settingsStore';
import { useConnectionStore } from '@/stores/connectionStore';
import { useQuickCommandStore } from '@/stores/quickCommandStore';
import { useSkillStore } from '@/stores/skillStore';
import { useMcpStore } from '@/stores/mcpStore';
import {
  SYNC_REFRESHERS,
  SYNC_SOURCES,
  handleSyncDataApplied,
  onSyncDataApplied,
} from './syncRefresh';

vi.mock('@/lib/tauri', () => ({
  getSettings: vi.fn(),
  saveSettings: vi.fn(),
  getConnections: vi.fn(),
  quickCommandList: vi.fn(),
  skillList: vi.fn(),
  mcpListServers: vi.fn(),
}));

/**
 * 后端同步源清单的快照（`DataSourceId::sync_key`，见 phase1-spec 4.3 / 4.9）。
 * 后端新增同步源时这里必须跟着改，而 `SYNC_REFRESHERS` 的类型会逼着补刷新函数——
 * 漏了的后果是别的设备的改动落到本机后界面还显示旧数据、下一次保存再把它改回去。
 */
const BACKEND_SYNC_SOURCES = [
  'settings',
  'connections',
  'quickCommands',
  'skills',
  'mcpServers',
  'knownHosts',
];

describe('同步源 → 刷新函数表', () => {
  beforeEach(() => {
    vi.mocked(tauri.getSettings).mockReset().mockResolvedValue({
      settings: { ...useSettingsStore.getInitialState().settings, fontSize: 31 },
      hasApiKey: false,
    });
    vi.mocked(tauri.getConnections).mockReset().mockResolvedValue([]);
    vi.mocked(tauri.quickCommandList).mockReset().mockResolvedValue([]);
    vi.mocked(tauri.skillList).mockReset().mockResolvedValue([]);
    vi.mocked(tauri.mcpListServers).mockReset().mockResolvedValue({ servers: [], statuses: [] });
    useSettingsStore.setState({
      settings: useSettingsStore.getInitialState().settings,
      loaded: true,
      preview: null,
    });
  });

  it('后端的每一个同步源都有刷新函数，且前端没有多余的', () => {
    expect([...SYNC_SOURCES].sort()).toEqual([...BACKEND_SYNC_SOURCES].sort());
    for (const source of BACKEND_SYNC_SOURCES) {
      expect(typeof SYNC_REFRESHERS[source as keyof typeof SYNC_REFRESHERS], `${source} 没有刷新函数`).toBe(
        'function',
      );
    }
  });

  it('settings：从后端重读进 settingsStore', async () => {
    await handleSyncDataApplied('settings');
    expect(tauri.getSettings).toHaveBeenCalled();
    expect(useSettingsStore.getState().settings.fontSize).toBe(31);
  });

  it('connections / quickCommands / skills / mcpServers：各自重读，且是静默刷新（不翻 loading）', async () => {
    const loadingSeen: boolean[] = [];
    const unsubs = [
      useConnectionStore.subscribe((s) => loadingSeen.push(s.loading)),
      useSkillStore.subscribe((s) => loadingSeen.push(s.loading)),
      useQuickCommandStore.subscribe((s) => loadingSeen.push(s.loading)),
      useMcpStore.subscribe((s) => loadingSeen.push(s.loading)),
    ];
    await handleSyncDataApplied('connections');
    await handleSyncDataApplied('quickCommands');
    await handleSyncDataApplied('skills');
    await handleSyncDataApplied('mcpServers');
    unsubs.forEach((u) => u());

    expect(tauri.getConnections).toHaveBeenCalledTimes(1);
    expect(tauri.quickCommandList).toHaveBeenCalledTimes(1);
    expect(tauri.skillList).toHaveBeenCalledTimes(1);
    expect(tauri.mcpListServers).toHaveBeenCalledTimes(1);
    expect(loadingSeen.every((v) => v === false)).toBe(true);
  });

  it('quickCommands 沿用最近一次加载用的 sessionKey', async () => {
    useQuickCommandStore.setState({ lastSessionKey: 'conn-1' });
    await handleSyncDataApplied('quickCommands');
    expect(tauri.quickCommandList).toHaveBeenCalledWith('conn-1');
  });

  it('knownHosts：前端没有缓存，什么都不用读', async () => {
    await handleSyncDataApplied('knownHosts');
    expect(tauri.getSettings).not.toHaveBeenCalled();
    expect(tauri.getConnections).not.toHaveBeenCalled();
  });

  it('刷新失败不抛出（记日志），其它监听者照常通知', async () => {
    const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
    vi.mocked(tauri.skillList).mockRejectedValue(new Error('读取失败'));
    const seen: string[] = [];
    const off = onSyncDataApplied((source) => seen.push(source));
    await expect(handleSyncDataApplied('skills')).resolves.toBeUndefined();
    off();
    expect(seen).toEqual(['skills']);
    errorSpy.mockRestore();
  });

  it('未知同步源（后端比前端新）只告警，不当错误', async () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const seen: string[] = [];
    const off = onSyncDataApplied((source) => seen.push(source));
    await handleSyncDataApplied('conversations');
    off();
    expect(warnSpy).toHaveBeenCalled();
    expect(seen).toEqual([]);
    warnSpy.mockRestore();
  });

  it('监听者在 store 刷新完成之后才被通知（读到的一定是新数据）', async () => {
    let fontSizeWhenNotified = 0;
    const off = onSyncDataApplied(() => {
      fontSizeWhenNotified = useSettingsStore.getState().settings.fontSize;
    });
    await handleSyncDataApplied('settings');
    off();
    expect(fontSizeWhenNotified).toBe(31);
  });
});
