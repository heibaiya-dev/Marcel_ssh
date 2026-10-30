import { beforeEach, describe, expect, it, vi } from 'vitest';
import * as tauri from '@/lib/tauri';
import { useSettingsStore } from '@/stores/settingsStore';
import type { AppSettings } from '@/lib/types';

vi.mock('@/lib/tauri', () => ({
  getSettings: vi.fn(),
  saveSettings: vi.fn(),
}));

const saveSettings = vi.mocked(tauri.saveSettings);
const getSettings = vi.mocked(tauri.getSettings);

/** 模拟后端「磁盘上的真值」：saveSettings 写入、getSettings 读出。 */
let disk: AppSettings;

function lastSaved(): AppSettings {
  return saveSettings.mock.calls[saveSettings.mock.calls.length - 1][0] as AppSettings;
}

/** 模拟跨设备同步把别的设备的改动落到本机：磁盘变了，store 经 refreshFromBackend 跟上。 */
async function remoteApplies(patch: Partial<AppSettings>) {
  disk = { ...disk, ...patch };
  await useSettingsStore.getState().refreshFromBackend();
}

describe('settingsStore 与跨设备同步', () => {
  beforeEach(() => {
    const initial = useSettingsStore.getInitialState().settings;
    disk = JSON.parse(JSON.stringify(initial));
    useSettingsStore.setState({ settings: initial, loaded: true, preview: null });
    saveSettings.mockReset().mockImplementation(async (s) => {
      disk = JSON.parse(JSON.stringify(s));
    });
    getSettings.mockReset().mockImplementation(async () => ({
      settings: JSON.parse(JSON.stringify(disk)),
      hasApiKey: false,
    }));
  });

  describe('refreshFromBackend', () => {
    it('把后端的新值读进 store，并保留进行中的 preview', async () => {
      useSettingsStore.getState().setPreview({ fontSize: 30 });
      await remoteApplies({ fontFamily: 'Remote Mono' });
      const state = useSettingsStore.getState();
      expect(state.settings.fontFamily).toBe('Remote Mono');
      expect(state.preview).toEqual({ fontSize: 30 });
    });

    it('内容没变不换引用（下游 effect 不白跑）', async () => {
      const before = useSettingsStore.getState().settings;
      await useSettingsStore.getState().refreshFromBackend();
      expect(useSettingsStore.getState().settings).toBe(before);
    });

    it('读取失败只记日志，保留内存里的设置', async () => {
      const spy = vi.spyOn(console, 'error').mockImplementation(() => {});
      getSettings.mockRejectedValueOnce({ kind: 'Io', message: '读配置失败' });
      const before = useSettingsStore.getState().settings;
      await expect(useSettingsStore.getState().refreshFromBackend()).resolves.toBeUndefined();
      expect(useSettingsStore.getState().settings).toBe(before);
      spy.mockRestore();
    });

    it('还没加载完设置时不动', async () => {
      useSettingsStore.setState({ loaded: false });
      await useSettingsStore.getState().refreshFromBackend();
      expect(getSettings).not.toHaveBeenCalled();
    });
  });

  describe('save(draft, base)：草稿流只落用户真正改过的字段', () => {
    it('打开草稿 → 远端改了 A → 用户改 B 保存 → A 是远端值、B 是用户值', async () => {
      const base = useSettingsStore.getState().settings;
      const draft: AppSettings = { ...base, fontSize: 22 }; // 用户在草稿里改 B
      await remoteApplies({ fontFamily: 'Remote Mono' }); // 别的设备同步来 A（草稿还没来得及跟进）

      await useSettingsStore.getState().save(draft, base);

      expect(lastSaved().fontFamily).toBe('Remote Mono');
      expect(lastSaved().fontSize).toBe(22);
      expect(useSettingsStore.getState().settings.fontFamily).toBe('Remote Mono');
      expect(useSettingsStore.getState().settings.fontSize).toBe(22);
    });

    it('同一个对象里的不同子字段也各取各的（顶层字段粒度会丢掉远端的）', async () => {
      const base = useSettingsStore.getState().settings;
      const draft: AppSettings = {
        ...base,
        agentModeSettings: { ...base.agentModeSettings, maxToolRounds: 123 },
      };
      await remoteApplies({
        agentModeSettings: { ...base.agentModeSettings, listMode: 'allowlist' },
      });

      await useSettingsStore.getState().save(draft, base);

      expect(lastSaved().agentModeSettings.listMode).toBe('allowlist');
      expect(lastSaved().agentModeSettings.maxToolRounds).toBe(123);
    });

    it('不传 base = 整份覆盖（旧行为，仅限确定这份就是最新的场景）', async () => {
      const base = useSettingsStore.getState().settings;
      await remoteApplies({ fontFamily: 'Remote Mono' });
      await useSettingsStore.getState().save({ ...base, fontSize: 22 });
      expect(lastSaved().fontFamily).toBe(base.fontFamily);
    });

    it('保存失败：不改 store', async () => {
      const base = useSettingsStore.getState().settings;
      saveSettings.mockRejectedValueOnce({ kind: 'Io', message: '写盘失败' });
      await expect(
        useSettingsStore.getState().save({ ...base, fontSize: 22 }, base),
      ).rejects.toBeDefined();
      expect(useSettingsStore.getState().settings.fontSize).toBe(base.fontSize);
    });
  });

  describe('update(patch)：合并到执行时的最新设置', () => {
    it('远端先落地、随后本机 update：远端的字段不被改回去', async () => {
      await remoteApplies({ fontFamily: 'Remote Mono' });
      await useSettingsStore.getState().update({ fontSize: 19 });
      expect(lastSaved().fontFamily).toBe('Remote Mono');
      expect(lastSaved().fontSize).toBe(19);
    });

    it('update 在途时远端落地：刷新排在保存之后，谁的改动都不丢', async () => {
      let releaseSave: () => void = () => {};
      saveSettings.mockImplementationOnce(
        (s) =>
          new Promise<void>((resolve) => {
            releaseSave = () => {
              // 后端自己串行：本机这次保存先落盘，紧接着远端的同步改动落盘。
              disk = { ...JSON.parse(JSON.stringify(s)), fontFamily: 'Remote Mono' };
              resolve();
            };
          }),
      );
      const updating = useSettingsStore.getState().update({ fontSize: 19 });
      // 保存还没完成时收到 sync-data-applied：刷新必须等保存结束，否则会读到落盘前的旧值，
      // 或者被保存完成后的 set 盖回去。
      const refreshing = useSettingsStore.getState().refreshFromBackend();
      expect(getSettings).not.toHaveBeenCalled();
      releaseSave();
      await Promise.all([updating, refreshing]);

      const state = useSettingsStore.getState().settings;
      expect(state.fontSize).toBe(19);
      expect(state.fontFamily).toBe('Remote Mono');
    });

    it('两次几乎同时的 update 不互相覆盖（旧实现各拿同一份旧设置，后完成的盖掉先完成的）', async () => {
      await Promise.all([
        useSettingsStore.getState().update({ fontSize: 19 }),
        useSettingsStore.getState().update({ fontFamily: 'Consolas' }),
      ]);
      const state = useSettingsStore.getState().settings;
      expect(state.fontSize).toBe(19);
      expect(state.fontFamily).toBe('Consolas');
      expect(disk.fontSize).toBe(19);
      expect(disk.fontFamily).toBe('Consolas');
    });

    it('排队中的 update 用的是调用时看到的基准：只落自己改的，不把前一个改回去', async () => {
      const first = useSettingsStore.getState().update({
        agentModeSettings: {
          ...useSettingsStore.getState().settings.agentModeSettings,
          maxToolRounds: 7,
        },
      });
      // 第二个 update 基于「第一个还没落地」的旧 agentModeSettings 算出整个子对象
      const stale = useSettingsStore.getState().settings.agentModeSettings;
      const second = useSettingsStore.getState().update({
        agentModeSettings: { ...stale, listMode: 'allowlist' },
      });
      await Promise.all([first, second]);
      const am = useSettingsStore.getState().settings.agentModeSettings;
      expect(am.maxToolRounds).toBe(7);
      expect(am.listMode).toBe('allowlist');
    });

    it('上一个保存失败不会卡死队列', async () => {
      saveSettings.mockRejectedValueOnce({ kind: 'Io', message: '写盘失败' });
      await expect(useSettingsStore.getState().update({ fontSize: 19 })).rejects.toBeDefined();
      await useSettingsStore.getState().update({ fontSize: 21 });
      expect(useSettingsStore.getState().settings.fontSize).toBe(21);
    });
  });
});
