// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import * as tauri from '@/lib/tauri';
import { useSettingsStore } from '@/stores/settingsStore';
import type { AppSettings } from '@/lib/types';
import Settings from './Settings';

vi.mock('@/lib/tauri', () => ({
  getSettings: vi.fn(),
  saveSettings: vi.fn(),
}));

vi.mock('@/stores/pluginStore', () => ({
  usePluginStore: { getState: () => ({ fetchPlugins: () => Promise.resolve() }) },
}));

const probe = vi.hoisted(() => ({
  actions: null as null | {
    settings: AppSettings;
    update: (patch: Partial<AppSettings>) => void;
  },
  dirty: false,
  save: () => {},
}));

// 设置页的两块子界面各换成最小替身：一个把草稿的 settings / update 暴露出来，
// 一个暴露「保存」按钮与 dirty 标记。要测的是 Settings.tsx 本身的草稿 ↔ store 对齐逻辑。
vi.mock('./SettingsContent', async () => {
  const { useSettingsActions } = await import('./SettingsActionsContext');
  return {
    SettingsContent: () => {
      probe.actions = useSettingsActions();
      return null;
    },
  };
});
vi.mock('./SettingsSidebar', () => ({
  SettingsSidebar: (props: { dirty: boolean; onSave: () => void }) => {
    probe.dirty = props.dirty;
    probe.save = props.onSave;
    return null;
  },
}));

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
// jsdom 没有 ResizeObserver（设置页用它量外框宽度做响应式布局）
(globalThis as Record<string, unknown>).ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

const saveSettings = vi.mocked(tauri.saveSettings);
const getSettings = vi.mocked(tauri.getSettings);
let disk: AppSettings;

/**
 * 桌面设置页的「草稿 + 保存」流与跨设备同步的交互：
 * 打开草稿 → 远端（别的设备）改了字段 A → 用户改字段 B 保存 → 结果 A 是远端值、B 是用户值。
 */
describe('Settings 草稿与同步落地的改动合并', () => {
  let container: HTMLDivElement;
  let root: Root | null = null;

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
    container = document.createElement('div');
    document.body.appendChild(container);
    const created = createRoot(container);
    root = created;
    act(() => {
      created.render(<Settings />);
    });
  });

  afterEach(() => {
    const current = root;
    if (current) act(() => current.unmount());
    root = null;
    container.remove();
    vi.restoreAllMocks();
  });

  async function remoteApplies(patch: Partial<AppSettings>) {
    disk = { ...disk, ...patch };
    await act(async () => {
      await useSettingsStore.getState().refreshFromBackend();
    });
  }

  async function clickSave() {
    await act(async () => {
      probe.save();
    });
  }

  it('远端改了 A、用户改了 B：草稿先跟进 A，保存后两边都在', async () => {
    act(() => probe.actions?.update({ fontSize: 22 }));
    expect(probe.dirty).toBe(true);

    await remoteApplies({ fontFamily: 'Remote Mono' });
    // 草稿里没动过的 A 跟进成远端值，动过的 B 保留
    expect(probe.actions?.settings.fontFamily).toBe('Remote Mono');
    expect(probe.actions?.settings.fontSize).toBe(22);

    await clickSave();
    const saved = saveSettings.mock.calls[saveSettings.mock.calls.length - 1][0] as AppSettings;
    expect(saved.fontFamily).toBe('Remote Mono');
    expect(saved.fontSize).toBe(22);
    expect(disk.fontFamily).toBe('Remote Mono');
    // 保存完草稿与 store 对齐，不残留「未保存的更改」
    expect(probe.dirty).toBe(false);
  });

  it('改的是同一个对象里不同子字段：两个都保留（旧版按顶层字段合并会丢掉远端那个）', async () => {
    const am = useSettingsStore.getState().settings.agentModeSettings;
    act(() => probe.actions?.update({ agentModeSettings: { ...am, maxToolRounds: 321 } }));

    await remoteApplies({ agentModeSettings: { ...am, listMode: 'allowlist' } });
    expect(probe.actions?.settings.agentModeSettings.listMode).toBe('allowlist');
    expect(probe.actions?.settings.agentModeSettings.maxToolRounds).toBe(321);

    await clickSave();
    expect(disk.agentModeSettings.listMode).toBe('allowlist');
    expect(disk.agentModeSettings.maxToolRounds).toBe(321);
  });

  it('同一个字段两边都改了：用户刚改的赢，保存后不再被同步改回去', async () => {
    act(() => probe.actions?.update({ fontSize: 22 }));
    await remoteApplies({ fontSize: 18 });
    expect(probe.actions?.settings.fontSize).toBe(22);
    await clickSave();
    expect(disk.fontSize).toBe(22);
  });

  it('没有未保存改动时，远端改动直接跟进，不出现「未保存的更改」', async () => {
    await remoteApplies({ fontFamily: 'Remote Mono' });
    expect(probe.actions?.settings.fontFamily).toBe('Remote Mono');
    expect(probe.dirty).toBe(false);
  });
});
