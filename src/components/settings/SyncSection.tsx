import { Card } from './helpers';
import { SyncPanel } from './sync/SyncPanel';
import { SYNC_SECTION_ID } from './sync/syncUi';

/**
 * 跨设备同步（桌面设置页）。
 *
 * 同步的开启 / 加入 / 立即同步等都是**即时动作**（直接调后端命令），不走设置页的
 * 「草稿 + 保存」流程，所以这里不读写 SettingsActionsContext。内容与手机端共用 `SyncPanel`。
 */
export function SyncSection() {
  return (
    <Card
      id={SYNC_SECTION_ID}
      title="跨设备同步"
      description="端到端加密，在你自己的设备之间同步连接、设置和技能"
    >
      <SyncPanel variant="desktop" />
    </Card>
  );
}
