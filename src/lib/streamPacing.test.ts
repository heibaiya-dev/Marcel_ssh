import { describe, expect, it } from 'vitest';
import {
  STREAM_MAX_HOLD_MS,
  STREAM_MIN_CHUNK_CHARS,
  STREAM_TICK_MS,
  paceShown,
  type PaceState,
} from './streamPacing';

const empty: PaceState = { shownLength: 0, pendingSince: null };

describe('paceShown', () => {
  it('攒不够就先不动（不会一个字一个字蹦）', () => {
    const short = 'a'.repeat(STREAM_MIN_CHUNK_CHARS - 1);
    const result = paceShown('', short, empty, 1000);
    expect(result.changed).toBe(false);
    expect(result.state.shownLength).toBe(0);
    expect(result.state.pendingSince).toBe(1000);
  });

  it('攒够了整段出现，并且一次就把欠账放完', () => {
    const enough = 'a'.repeat(STREAM_MIN_CHUNK_CHARS);
    const result = paceShown('', enough, empty, 1000);
    expect(result.changed).toBe(true);
    expect(result.state.shownLength).toBe(STREAM_MIN_CHUNK_CHARS);
    expect(result.state.pendingSince).toBeNull();
  });

  it('压太久之后即使不够也放出来（模型吐字慢时不能干等）', () => {
    const short = 'ab';
    const holding: PaceState = { shownLength: 0, pendingSince: 1000 };
    expect(paceShown('', short, holding, 1000 + STREAM_MAX_HOLD_MS - 1).changed).toBe(false);
    const released = paceShown('', short, holding, 1000 + STREAM_MAX_HOLD_MS);
    expect(released.changed).toBe(true);
    expect(released.state.shownLength).toBe(2);
  });

  it('积压很多时逐拍追赶，不会越拖越远', () => {
    const next = 'a'.repeat(300);
    let current = '';
    let state = empty;
    let ticks = 0;
    while (current.length < next.length && ticks < 30) {
      const result = paceShown(current, next, state, 1000 + ticks * STREAM_TICK_MS);
      state = result.state;
      if (result.changed) current = next.slice(0, state.shownLength);
      ticks += 1;
    }
    expect(current).toBe(next);
    // 300 字应该在 12 拍（约 1.2 秒）内追平，远快于「越拖越远」的线性掉队。
    expect(ticks).toBeLessThanOrEqual(12);
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

  it('不把 emoji 的代理对劈开', () => {
    const next = 'a'.repeat(STREAM_MIN_CHUNK_CHARS) + '🙂🙂';
    const result = paceShown('', next, empty, 1000);
    const shown = next.slice(0, result.state.shownLength);
    expect(shown.length).toBe(result.state.shownLength);
    expect([...shown].join('')).toBe(shown);
  });
});
