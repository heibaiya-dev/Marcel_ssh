/**
 * 流式文字的显示节拍：收到多少 ≠ 立刻显示多少。
 *
 * delta 的到达节奏完全由模型决定，是一股一股的；逐帧照单全收，屏幕上就是
 * 「憋一下、蹦一串、再憋一下」。按固定节拍推进之后：字是一小块一小块连续出现的，
 * 节奏恒定，像有人在敲键盘——而且**比模型实际吐字慢**。
 *
 * 规则（见 `paceShown`）：
 * 1. 每拍 100ms 放 `STREAM_CHARS_PER_TICK` 个字（30 字/秒）：这是「看起来比实际慢」的来源，
 *    也是唯一需要调的旋钮；
 * 2. 欠账超过 `STREAM_BACKLOG_HIGH` 才线性提速（每多 `STREAM_RAMP_STEP` 字，每拍多 1 个字），
 *    封顶 `STREAM_MAX_CHUNK_CHARS`：短回答始终慢放，长回答也不会被拖住——慢是质感，
 *    越拖越远是故障；
 * 3. 流结束后用稍快的节奏把欠账排完（`paceChunk` 的 `live=false` 分支）：结尾不能「啪」地
 *    整块补上，但也不能让人干等。
 */

/** 节拍间隔（毫秒）：每拍检查一次攒了多少。 */
export const STREAM_TICK_MS = 100;
/** 每拍字数：30 字/秒。调小更慢，调大更快。 */
export const STREAM_CHARS_PER_TICK = 3;
/** 欠账超过这么多字（约 2 秒）才开始提速：正常速度的模型永远不会越拖越远。 */
export const STREAM_BACKLOG_HIGH = 60;
/** 提速斜率：每多欠这么多字，每拍多放 1 个字。 */
export const STREAM_RAMP_STEP = 20;
/** 单拍上限：约 240 字/秒，只用来给异常长的欠账兜底。 */
export const STREAM_MAX_CHUNK_CHARS = 24;
/** 收尾排欠账的单拍下限：结尾也不能一个字一个字抠。 */
export const STREAM_DRAIN_MIN_CHUNK = 8;
/** 收尾排欠账的单拍上限。 */
export const STREAM_DRAIN_MAX_CHUNK = 90;
/** 最多压这么久（毫秒）：不够一拍也要放出来，免得模型吐字慢时看起来卡住。 */
export const STREAM_MAX_HOLD_MS = 200;

/** 按 UTF-16 下标切字符串时别把 emoji 的代理对劈开（劈开就是一个乱码方块）。 */
export function safeCut(text: string, cut: number): number {
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
 * - 攒够一拍 / 压太久了：只放这一拍该放的字数。
 * - 否则：什么都不做，等下一拍（调用方据此跳过渲染）。
 *
 * `live=false` 表示流已经结束、只是还没排完：这时换成收尾节奏。
 */
export function paceShown(
  current: string,
  next: string,
  state: PaceState,
  now: number,
  live = true,
): PaceResult {
  if (next === current) {
    return { state: { shownLength: next.length, pendingSince: null }, changed: false };
  }
  if (!next.startsWith(current)) {
    return { state: { shownLength: next.length, pendingSince: null }, changed: true };
  }
  const backlog = next.length - current.length;
  const since = state.pendingSince ?? now;
  const ready = backlog >= STREAM_CHARS_PER_TICK || now - since >= STREAM_MAX_HOLD_MS;
  if (!ready) {
    return { state: { shownLength: current.length, pendingSince: since }, changed: false };
  }
  const chunk = paceChunk(backlog, live);
  const nextLength = backlog <= chunk ? next.length : safeCut(next, current.length + chunk);
  return {
    // 没排完就保留 `since`：之后每拍按同一个节拍继续滴，不会一拍快一拍慢。
    state: { shownLength: nextLength, pendingSince: nextLength >= next.length ? null : since },
    changed: true,
  };
}

/** 这一拍该放几个字。 */
function paceChunk(backlog: number, live: boolean): number {
  if (!live) {
    // 收尾：欠账约分十拍排完（夹在 8–90 之间），既不留尾巴也不整段闪现。
    const drain = Math.ceil(backlog / 10);
    return Math.min(STREAM_DRAIN_MAX_CHUNK, Math.max(STREAM_DRAIN_MIN_CHUNK, drain));
  }
  if (backlog <= STREAM_BACKLOG_HIGH) return STREAM_CHARS_PER_TICK;
  const extra = Math.ceil((backlog - STREAM_BACKLOG_HIGH) / STREAM_RAMP_STEP);
  return Math.min(STREAM_MAX_CHUNK_CHARS, STREAM_CHARS_PER_TICK + extra);
}
