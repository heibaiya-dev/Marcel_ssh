import { collectPlatformHints, isMobilePlatform } from '@/platform';
import { useSettingsNavStore } from '@/stores/settingsNavStore';
import { useViewStore } from '@/stores/viewStore';

/** 设置里「同步」分类的 id（桌面 `settingsNavigation`、手机 `mobileSettingsModel` 同名）。 */
export const SYNC_SETTINGS_CATEGORY = 'sync';

/**
 * 打开设置页的「同步」分类（桌面顶栏状态图标 / 手机入口共用）。
 *
 * 平台差异只在这里：桌面切到设置视图再跳分类，手机经 `mobile:open-settings`
 * 事件切到设置 Tab（与记忆、后台保活卡片同一条路）。
 */
export function openSyncSettings(): void {
  if (isMobilePlatform(collectPlatformHints())) {
    window.dispatchEvent(
      new CustomEvent('mobile:open-settings', { detail: { category: SYNC_SETTINGS_CATEGORY } }),
    );
    return;
  }
  useViewStore.getState().setActiveId('builtin.settings');
  useSettingsNavStore.getState().requestNavigate(SYNC_SETTINGS_CATEGORY);
}
