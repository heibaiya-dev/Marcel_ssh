import { Card, SettingItem } from './helpers';
import { useSettingsActions } from './SettingsActionsContext';
import { FontSizeInput } from './FontSizeInput';

export function TerminalAppearanceSection() {
  const { settings, update, setPreview } = useSettingsActions();

  return (
    <Card id="settings-appearance" title="终端外观" description="终端配色跟随界面主题（浅色/深色），此处调整字体表现">
      <SettingItem
        id="font-size"
        label="字号"
        description="终端字体大小"
        sectionId="settings-appearance"
        keywords={['font', 'size', '字体大小', '界面', '终端外观']}
      >
        <FontSizeInput
          value={settings.fontSize}
          onChange={(fontSize) => {
            setPreview({ fontSize });
            update({ fontSize });
          }}
        />
      </SettingItem>
      <SettingItem
        id="font-family"
        label="字体"
        description="终端字体族"
        sectionId="settings-appearance"
        keywords={['font', 'family', 'typeface', '字体', '界面', '终端外观']}
      >
        <input
          type="text"
          value={settings.fontFamily}
          onChange={(e) => {
            const fontFamily = e.target.value;
            setPreview({ fontFamily });
            update({ fontFamily });
          }}
          className="flex-1 rounded-lg bg-zinc-800 border border-zinc-700 px-3 py-1.5 text-sm text-zinc-100 focus:outline-none focus:border-indigo-500"
        />
      </SettingItem>
    </Card>
  );
}
