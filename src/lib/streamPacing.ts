/**
 * 流式文字的显示节拍：收到多少 ≠ 立刻显示多少。
 *
 * delta 的到达节奏完全由模型决定，是一股一股的；逐帧照单全收，屏幕上就是
 * 「憋一下、蹦一串、再憋一下」，而且每一帧都在重排整段 Markdown（长回答越写越卡）。
 * 攒一小会儿、按固定节拍整块推进之后：字是成段出现的、节奏恒定，像人在打字。
 *
 * 两条规则（见 `paceShown`）：
 * 1. 攒够 `STREAM_MIN_CHUNK_CHARS` 才放，一次放一大段，不会一个字一个字蹦；
 * 2. 但最多压 `STREAM_MAX_HOLD_MS`：模型吐得慢的时候也要往外放，不能干等。
 */

/** 节拍间隔（毫秒）：每拍检查一次攒了多少。 */
export const STREAM_TICK_MS = 100;
/** 攒够这么多字才成段出现。 */
export const STREAM_MIN_CHUNK_CHARS = 10;
/** 最多压这么久（毫秒）：再少也要放出来，免得模型吐字慢时看起来卡住。 */
export const STREAM_MAX_HOLD_MS = 240;

/** 按 UTF-16 下标切字符串时别把 emoji 的代理对劈开（劈开就是一个乱码方块）。 */
function safeCut(text: string, cut: number): number {
  const clamped = Math.max(0, Math.min(cut, text.length));
  if (clamped <= 0 || clamped >= text.length) return clamped;
  const code = text.charCodeAt(clamped - 1);
  return code >= 0xd800 && code <= 0xdbff ? clamped - 1 : clamped;
}

export interface PaceState {
  /** 屏幕上已有多少个字符。 */
  shownLength: number;
  /** 这批还没放出去的字是从哪一拍开始攒的（毫秒时间戳；没攒东西就是 null）。 */
  pendingSince: number | null;
}

export interface PaceResult {
  state: PaceState;
  /** 这一拍该不该改屏幕。 */
  changed: boolean;
}

/**
 * 推进一拍：
 * - `next` 不是 `current` 的续写（撤回、重试、换了另一条答案）：直接对齐，没有中间态。
 * - 攒够了 / 压太久了：一次放一大段（落后多时按 1/3 追，最多几拍追上，不会越拖越远）。
 * - 否则：什么都不做，等下一拍（调用方据此跳过渲染）。
 */
export function paceShown(
  current: string,
  next: string,
  state: PaceState,
  now: number,
): PaceResult {
  if (next === current) {
    return { state: { shownLength: next.length, pendingSince: null }, changed: false };
  }
  if (!next.startsWith(current)) {
    return { state: { shownLength: next.length, pendingSince: null }, changed: true };
  }
  const backlog = next.length - current.length;
  const since = state.pendingSince ?? now;
  const ready = backlog >= STREAM_MIN_CHUNK_CHARS || now - since >= STREAM_MAX_HOLD_MS;
  if (!ready) {
    return { state: { shownLength: current.length, pendingSince: since }, changed: false };
  }
  const chunk = Math.max(STREAM_MIN_CHUNK_CHARS, Math.ceil(backlog / 3));
  const nextLength = backlog <= chunk ? next.length : safeCut(next, current.length + chunk);
  return {
    state: { shownLength: nextLength, pendingSince: nextLength >= next.length ? null : now },
    changed: true,
  };
}
