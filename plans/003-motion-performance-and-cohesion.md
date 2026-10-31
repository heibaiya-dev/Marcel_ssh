# 003 — 收敛高频动效的性能与属性范围

- **Status**: TODO
- **Commit**: f1b56f1
- **Severity**: MEDIUM
- **Category**: Performance / Physicality & origin / Accessibility
- **Estimated scope**: 12–18 files，约 180 行

## Problem

已确认的中低优先级问题：24 处 `transition-all` 让非目标布局属性参与补间；6 个进度条动画 width；popover 固定从 `bottom left` 长出；ink 的 inline blur 与状态光晕/FAB backdrop blur 增加重绘；设置页过渡布局属性；两处折叠无 reduced-motion；手写 switch 与共享 Toggle 不一致。

## Target

1. 只对 `transform/opacity` 做位移动效；进度条统一 scaleX；颜色只 transition 颜色属性。
2. popover 支持 `transform-origin: var(--transform-origin, bottom left)`，每个触发器按锚点设置。
3. 流式 ink 保持单层设计但只动 opacity，70ms，给 100ms tick 留 30ms。
4. 所有 collapse/位移在 reduce 下只留 opacity 或立即完成。
5. 不为了“全修”而制造大范围视觉回归；只改列出的属性与时长。

## Repo conventions to follow

- 复用 #002 新增的 `--ease-out`、`--ease-in-out`、`--ease-drawer`。
- `PlanList.tsx:163` 是进度条入口；`TransferQueue.tsx:126` 等结构相同。
- popover 已有 `useAnimatedPresence`；只加 origin CSS 变量，不改 presence 状态机。
- `ConnectionList.tsx:1000-1013` 与 `MobileConnectionList.tsx:409-422` 的 grid 0fr→1fr 技术保留，只收窄属性。

## Steps

1. 收窄 `transition-all`：`ui/Button.tsx:44`、`TurnActions.tsx:164,176,191,201`、`AgentMessage.tsx:347`、`ConnectionList.tsx:1000,1007`、`MobileConnectionList.tsx:409,416`、`MultiHostPicker.tsx:168`、`MobileAgentHost.tsx:526`、`MobileActiveAgentsSheet.tsx:265`、`MobileMultiHostPicker.tsx:98`、`AgentTasksDrawer.tsx:175,245,292`、`QuestionPanel.tsx:245`。按钮用 `transition-[background-color,border-color,color,transform]`，图标用 `transition-[opacity,transform]`，折叠用 `transition-[grid-template-rows]` / `transition-[transform,opacity]`。
2. 给上述两个 connection collapse 加 `motion-reduce:transition-none`，reduce 下不 translate。
3. 进度条：`MobileFilesHost.tsx:1593`、`MobileImageViewer.tsx:522`、`InstallOverlay.tsx:125`、`ImagePreviewModal.tsx:512`、`TransferQueue.tsx:126`、`PlanList.tsx:163` 改定宽条 `transform: scaleX(pct / 100)`、`transform-origin: left`、`transition: transform 200ms linear`，并统一 `motion-reduce:transition-none`。不引入全局 CSS 变量驱动子元素。
4. `globals.css` 的 `.mobile-popover-enter/.mobile-popover-exit` 改 `transform-origin: var(--transform-origin, bottom left)`，入场 150ms、退场 120ms `var(--ease-out)`。在 `ModeSelector.tsx`、`ModelPicker.tsx`、`MultiHostPicker.tsx`、`MobileAgentHost.tsx` 按实际上下左右锚点传 inline `--transform-origin`；默认值留给 `ReasoningEffortPicker`。
5. `globals.css:549-582` ink 两个 keyframes 删除 `filter`，从 `opacity: .45` 到 `1`，class 时长 70ms、曲线改 `var(--ease-out)`；仍 `.ink-a/.ink-b` 交替，`display:inline` 不动。
6. `AgentStatusIndicator.tsx:54,94,103` 去 blur，改透明径向渐变或仅保留透明底色；不动 spinner。`AgentTranscript.tsx:149,163` 去 `backdrop-blur-sm`，保留不透明底色与阴影。
7. `globals.css:760-788` 设置页布局 transition 收敛：不在 `settings-item/settings-label-column` 上持续过渡 width/padding/gap；仅保留必要的容器宽度/间距过渡，统一 200ms `var(--ease-drawer)`。若无法用 transform 表达宽度，接受该容器级布局过渡，不新增逐帧 JS。
8. `MobileConnectionForm.tsx:627,634` 复用 `src/components/ui/Toggle.tsx`；若表单布局不允许，至少让 knob 使用 160ms `var(--ease-out)`，与共享组件一致。
9. 添加低频入场机会（不阻塞交互）：用户消息行仅首次挂载 `opacity 0→1, translateY(4px)→0` 180ms `--ease-out`；scroll-to-bottom FAB 用 presence/transition 做 `scale(.9)+opacity` 入场 140ms、退场 100ms。不要给虚拟列表每次回收重挂载加动画。composer 自动长高若已有同步测量，保持即时高度（输入响应优先），不强行加 height 动画。

## Boundaries

- 禁止修改工具调用动画路径（同 #001）。
- 不改 popover 定位算法、手势、工具状态逻辑。
- 不把 24 种时长一次性重写为 5 档；只对本计划触及的新/改规则使用 token。
- 不添加依赖，不改测试期望来掩盖行为回退。

## Verification

- **Mechanical**: `pnpm exec tsc --noEmit` 通过；相关 UI/agent/mobile 测试通过；grep 确认本计划列出的 `transition-all` 与 width 进度条已消除。
- **Feel check**: 流式滚动时没有光晕/FAB 模糊层闪烁；进度条平滑且 reduce 下立即更新；popover 从各自触发器方向展开；connection 折叠在 reduce 下不位移；按钮只改变声明过的属性。
- **Done when**: 类型检查/测试通过，所有 diff 均在计划范围，工具调用动画完全未改。
