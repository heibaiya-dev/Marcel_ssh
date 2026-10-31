import { describe, expect, it } from 'vitest';
import {
  STREAM_BACKLOG_HIGH,
  STREAM_CHARS_PER_TICK,
  STREAM_MAX_CHUNK_CHARS,
  STREAM_MAX_HOLD_MS,
  STREAM_TICK_MS,
  paceShown,
  type PaceState,
} from './streamPacing';

const empty: PaceState = { shownLength: 0, pendingSince: null };

/** 按节拍一路推到追平，返回每拍放出的字数。 */
function drain(next: string, live = true, ticks = 20000): number[] {
  let current = '';
  let state = empty;
  const added: number[] = [];
  for (let tick = 0; current.length < next.length && tick < ticks; tick += 1) {
    const result = paceShown(current, next, state, 1000 + tick * STREAM_TICK_MS, live);
    state = result.state;
    if (result.changed) {
      added.push(state.shownLength - current.length);
      current = next.slice(0, state.shownLength);
    }
  }
  expect(current).toBe(next);
  return added;
}

describe('paceShown', () => {
  it('每拍只放固定字数：显示被刻意压慢', () => {
    const result = paceShown('', 'a'.repeat(STREAM_CHARS_PER_TICK * 4), empty, 1000);
    expect(result.changed).toBe(true);
    expect(result.state.shownLength).toBe(STREAM_CHARS_PER_TICK);
  });

  it('不够一拍就先攒着（下一拍一起给）', () => {
    const short = 'a'.repeat(STREAM_CHARS_PER_TICK - 1);
    const result = paceShown('', short, empty, 1000);
    expect(result.changed).toBe(false);
    expect(result.state.shownLength).toBe(0);
    expect(result.state.pendingSince).toBe(1000);
  });

  it('压太久之后即使不够也放出来（模型吐字慢时不能干等）', () => {
    const holding: PaceState = { shownLength: 0, pendingSince: 1000 };
    expect(paceShown('', 'ab', holding, 1000 + STREAM_MAX_HOLD_MS - 1).changed).toBe(false);
    const released = paceShown('', 'ab', holding, 1000 + STREAM_MAX_HOLD_MS);
    expect(released.changed).toBe(true);
    expect(released.state.shownLength).toBe(2);
  });

  it('短回答全程匀速：每拍一模一样，不会时快时慢', () => {
    const added = drain('a'.repeat(STREAM_BACKLOG_HIGH));
    // 最后一拍是余数，其余每拍必须一致。
    expect(new Set(added.slice(0, -1))).toEqual(new Set([STREAM_CHARS_PER_TICK]));
  });

  it('100 字至少摊到 2.5 秒，不会整块蹦出来', () => {
    const added = drain('a'.repeat(100));
    expect(added.length).toBeGreaterThanOrEqual(25);
    // 单拍最多也就几个字：绝没有「一大段突然出现」。
    expect(Math.max(...added)).toBeLessThanOrEqual(10);
  });

  it('欠账太深时会提速，但每拍不超过上限（长回答不被拖住）', () => {
    const added = drain('a'.repeat(10000));
    expect(Math.max(...added)).toBeLessThanOrEqual(STREAM_MAX_CHUNK_CHARS);
    expect(Math.max(...added)).toBeGreaterThan(STREAM_CHARS_PER_TICK);
    // 10000 字不该拖成十几分钟。
    expect(added.length).toBeLessThan(2000);
  });

  it('内容被替换（不是续写）时直接对齐，不逐个回退', () => {
    const result = paceShown('old answer', 'new', { shownLength: 10, pendingSince: 500 }, 1000);
    expect(result.changed).toBe(true);
    expect(result.state.shownLength).toBe(3);
  });

  it('没有新字时完全不触发渲染', () => {
    const result = paceShown('same', 'same', { shownLength: 4, pendingSince: null }, 1000);
    expect(result.changed).toBe(false);
  });

  it('流结束后两秒内把欠账排完，不整段跳出来', () => {
    const added = drain('a'.repeat(300), false);
    expect(added.length).toBeLessThanOrEqual(22);
    expect(Math.max(...added)).toBeLessThan(300);
  });

  it('不把 emoji 的代理对劈开', () => {
    const next = 'a'.repeat(STREAM_CHARS_PER_TICK * 3) + '🙂🙂';
    let current = '';
    let state = empty;
    for (let tick = 0; current.length < next.length && tick < 50; tick += 1) {
      const result = paceShown(current, next, state, 1000 + tick * STREAM_TICK_MS);
      state = result.state;
      if (result.changed) current = next.slice(0, state.shownLength);
    }
    expect([...current].join('')).toBe(current);
    expect(current).toBe(next);
  });
});
