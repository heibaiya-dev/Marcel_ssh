import { SyncPanel } from '@/components/settings/sync/SyncPanel';

/**
 * 跨设备同步（手机设置页）。与桌面共用 `SyncPanel`：功能完全一致，区别只有——
 * 不生成二维码（手机用不着扫自己），改为显示同步码 + 复制按钮；布局是整宽卡片，弹层是底部 Sheet。
 */
export function MobileSyncSection() {
  return (
    <div className="space-y-2">
      <SyncPanel variant="mobile" />
    </div>
  );
}
