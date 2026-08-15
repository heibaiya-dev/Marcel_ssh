import Toggle from '@/components/ui/Toggle';
import { Card, SettingItem } from './helpers';
import { useSettingsActions } from './SettingsActionsContext';

export function AppearanceSection() {
  const { settings, update, setPreview } = useSettingsActions();
  const appearance = settings.appearance ?? { theme: 'dark' as const, acrylic: true };

  const changeAppearance = (patch: Partial<typeof appearance>) => {
    const next = { ...appearance, ...patch };
    setPreview({ appearance: next });
    update({ appearance: next });
  };

  return (
    <Card
      id="settings-appearance-ui"
      title="外观"
      description="深色主题 + 亚克力背景（浅色主题已移除）"
    >
      <SettingItem
        id="ui-acrylic"
        label="亚克力效果"
        description="让窗口透出桌面背景（Windows 原生 Acrylic），可在设置中随时关闭"
        sectionId="settings-appearance-ui"
        keywords={['acrylic', 'mica', 'transparent', '亚克力', '透明', '背景', '磨砂', '外观']}
      >
        <Toggle
          checked={appearance.acrylic}
          onChange={(checked) => changeAppearance({ acrylic: checked })}
          label={appearance.acrylic ? '已开启' : '已关闭'}
        />
      </SettingItem>
    </Card>
  );
}
