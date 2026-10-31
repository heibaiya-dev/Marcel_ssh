# 002 — 修复全局按钮按压与退场缓动

- **Status**: TODO
- **Commit**: f1b56f1
- **Severity**: HIGH
- **Category**: Easing & duration / Cohesion & tokens
- **Estimated scope**: `globals.css` 与 2 个移动端组件，约 100 行

## Problem

`src/styles/globals.css:630-639`、`:818-824`、`:984-992` 对同一批按钮写三条互相覆盖的 transition，且都在 Tailwind layer 外，覆盖组件的 `duration-100`。最终颜色、transform 都跑 200ms `--spring-bounce`。

`globals.css:826-828` 的 `button:active { transform: scale(0.95) }` 与 Tailwind v4 的 `scale` 属性叠加，实际可到 0.9025；同时无 reduced-motion 处理。全站 12 处退场用 `ease-in`，第一帧没有响应。

## Target

在 `:root` spring token 附近加入：

```css
--ease-out: cubic-bezier(0.23, 1, 0.32, 1);
--ease-in-out: cubic-bezier(0.77, 0, 0.175, 1);
--ease-drawer: cubic-bezier(0.32, 0.72, 0, 1);
```

只保留一套全局交互规则，放进 `@layer base`：

```css
button, a, [role='button'], [role='tab'], [role='option'], select {
  transition-property: background-color, border-color, color, fill, stroke, opacity, box-shadow;
  transition-duration: 150ms;
  transition-timing-function: ease;
}
button:active { transform: scale(0.97); }
button { transition-property: background-color, border-color, color, fill, stroke, opacity, box-shadow, transform; }
```

transform 的按压反馈 160ms `cubic-bezier(0.23, 1, 0.32, 1)`，颜色 150ms `ease`；不得让全局规则再覆盖 Tailwind 的 `transition-*` 工具。reduced-motion 下仅去掉按压位移/缩放，保留颜色反馈。

全部退场改 `var(--ease-out)`：`slideDownOut` 180ms、modal scale/fade 160ms、`slideOutRight/fadeOut` 160ms、mobile sheet 180ms、mobile backdrop 160ms、fullscreen/overlay 140ms、popover 120ms、dropdown 100ms。仍保留 `both`/`1ms` 卸载约定。

## Repo conventions to follow

- token 放 `globals.css` 的 `--spring-*` 附近，不创建第二套 token 文件。
- 退场不能 `animation: none`，因为 `useAnimatedPresence` 依赖 `animationend`；沿用 reduced-motion 的 `1ms` 模式。
- 不调整工具调用动画。

## Steps

1. 加 3 个 easing token，删除 630/984 重复规则，将剩余全局交互规则移入 `@layer base`；用独立 longhand 或两个有明确职责的 base 规则，避免 Tailwind utilities 被覆盖。
2. 全局 press 改 0.97，transform 160ms `--ease-out`，补 reduced-motion。
3. 删除 `src/mobile/MobileAuxKeyBar.tsx:80` 与 `MobileQuickCommandBar.tsx:69` 的 `active:scale-95`；保留 `disabled:active:scale-100` 可一并删除，因为不再缩放。
4. 按 Target 替换上述退场 keyframe class 的 easing/duration，不改 keyframes 几何轨迹。
5. `globals.css:1253` `.animate-fadeIn` 改 `160ms var(--ease-out)`，加入 reduced-motion `1ms`。
6. `src/mobile/MobileSettings.tsx:221-227` 去掉永久 `mobile-panel-enter` 与仅为重播动画的 `key={activeCategory ?? 'root'}`；分类切换直接显示，不新增 keyframe。
7. 删除未使用的 `.spring-bounce/.spring-smooth/.spring-snappy` 类、`@keyframes spring-pop`、`.smooth-scroll`；删除第二份 `@keyframes fadeIn`；ink 注释里的 110ms 改 100ms。
8. 把手写的 `cubic-bezier(0.32, 0.72, 0, 1)` 替换为 `var(--ease-drawer)`（TSX inline 与 CSS 均可）。

## Boundaries

- 不改 shared Toggle 的 400ms knob 关键帧（刻意过冲）；不改 View Transitions 主题揭示。
- 不改 `MobileSheet` 手势/橡皮筋逻辑。
- 不改 modal 的 `transform-origin: center`。
- 不批量替换所有 `--spring-smooth`，那是仓库现有入场约定。

## Verification

- **Mechanical**: `pnpm exec tsc --noEmit` 通过；`pnpm vitest run src/components/ui src/components/agent/AgentRunStatus.test.tsx`（存在的测试按实际路径运行）通过。
- **Feel check**: DevTools 检查任意按钮，Tailwind `duration-100` 不再被全局无层规则覆盖；按下立即反馈、松手不回弹；terminal aux key 只变色不缩放。以 10% 播放速度检查 sheet/dropdown 退场，首帧立即移动。reduce 下无缩放/位移但仍有透明度与颜色反馈。
- **Done when**: CSS 中只有一份全局按钮 transition/press 规则，所有列出的退场不再含 `ease-in`，组件层级时长可以生效。
