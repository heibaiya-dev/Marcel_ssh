// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import * as tauri from '@/lib/tauri';
import { useLiveSettingsPatch } from './useLiveSettingsPatch';
import { useSettingsStore } from '@/stores/settingsStore';
import type { AppSettings } from '@/lib/types';

vi.mock('@/lib/tauri', () => ({
  getSettings: vi.fn(),
  saveSettings: vi.fn(),
}));

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

/**
 * 应用主题分区的「免保存」通道：
 * 改动 → preview 立即生效（终端/主题的实时预览认 preview）→ 停稳后一次性落库；
 * 卸载 flush 兜底，离开设置页不丢最后一次改动。桌面草稿 + 保存按钮不参与。
 */
describe('useLiveSettingsPatch', () => {
  let container: HTMLDivElement;
  let root: Root | null = null;
  let liveUpdate: (patch: Partial<AppSettings>) => void = () => {};
  const saveSettings = vi.mocked(tauri.saveSettings);

  beforeEach(() => {
    vi.useFakeTimers();
    saveSettings.mockClear().mockResolvedValue(undefined);
    container = document.createElement('div');
    document.body.appendChild(container);

    useSettingsStore.setState({
      settings: useSettingsStore.getInitialState().settings,
      preview: null,
      loaded: true,
    });

    function Harness() {
      const hook = useLiveSettingsPatch();
      liveUpdate = hook.liveUpdate;
      return null;
    }
    const created = createRoot(container);
    root = created;
    act(() => {
      created.render(<Harness />);
    });
  });

  afterEach(() => {
    const current = root;
    if (current) {
      act(() => current.unmount());
      root = null;
    }
    container.remove();
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it('改动先进 preview（实时预览），停稳后一次性落库并清 preview', async () => {
    act(() => liveUpdate({ fontSize: 18 }));
    expect(useSettingsStore.getState().preview?.fontSize).toBe(18);
    expect(saveSettings).not.toHaveBeenCalled();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(400);
    });
    expect(saveSettings).toHaveBeenCalledTimes(1);
    expect((saveSettings.mock.calls[0][0] as AppSettings).fontSize).toBe(18);
    expect(useSettingsStore.getState().preview).toBeNull();
  });

  it('连续改动合并成一次落库（拖取色器不刷盘）', async () => {
    act(() => liveUpdate({ fontSize: 16 }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100);
    });
    act(() => liveUpdate({ fontFamily: 'Consolas' }));

    await act(async () => {
      await vi.advanceTimersByTimeAsync(400);
    });
    expect(saveSettings).toHaveBeenCalledTimes(1);
    const persisted = saveSettings.mock.calls[0][0] as AppSettings;
    expect(persisted.fontSize).toBe(16);
    expect(persisted.fontFamily).toBe('Consolas');
    expect(useSettingsStore.getState().preview).toBeNull();
  });

  it('防抖窗口里别的设备的改动同步落地：落库合并到最新设置，远端字段不被改回去', async () => {
    const base = useSettingsStore.getState().settings;
    act(() => liveUpdate({ fontSize: 18 }));

    // 防抖等待期间收到 sync-data-applied：store 从后端重读，带上了远端改的 fontFamily
    vi.mocked(tauri.getSettings).mockResolvedValue({
      settings: { ...base, fontFamily: 'Remote Mono' },
      hasApiKey: false,
    });
    await act(async () => {
      await useSettingsStore.getState().refreshFromBackend();
    });
    expect(useSettingsStore.getState().preview?.fontSize).toBe(18);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(400);
    });
    expect(saveSettings).toHaveBeenCalledTimes(1);
    const persisted = saveSettings.mock.calls[0][0] as AppSettings;
    expect(persisted.fontSize).toBe(18);
    expect(persisted.fontFamily).toBe('Remote Mono');
  });

  it('卸载时 flush 在途改动，不等防抖', async () => {
    act(() => liveUpdate({ fontSize: 21 }));
    expect(saveSettings).not.toHaveBeenCalled();

    act(() => {
      root?.unmount();
    });
    root = null;

    // 落库异步完成后再清 preview，让微任务跑完
    await act(async () => {
      await Promise.resolve();
    });
    expect(saveSettings).toHaveBeenCalledTimes(1);
    expect((saveSettings.mock.calls[0][0] as AppSettings).fontSize).toBe(21);
    expect(useSettingsStore.getState().preview).toBeNull();
  });
});
