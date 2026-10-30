import { describe, expect, it } from 'vitest';
import { deepEqual, threeWayMerge } from './settingsMerge';

describe('deepEqual', () => {
  it('对象 key 顺序无关，undefined 等同缺失', () => {
    expect(deepEqual({ a: 1, b: { c: 2 } }, { b: { c: 2 }, a: 1 })).toBe(true);
    expect(deepEqual({ a: 1, x: undefined }, { a: 1 })).toBe(true);
    expect(deepEqual({ a: 1 }, { a: 2 })).toBe(false);
  });

  it('数组按顺序逐项比较', () => {
    expect(deepEqual([1, 2, 3], [1, 2, 3])).toBe(true);
    expect(deepEqual([1, 2, 3], [3, 2, 1])).toBe(false);
    expect(deepEqual([1], [1, 2])).toBe(false);
    expect(deepEqual([], {})).toBe(false);
  });
});

describe('threeWayMerge', () => {
  const base = {
    fontSize: 14,
    theme: 'dark',
    agent: { listMode: 'denylist', maxRounds: 500, tools: { edit: 'Approval' } },
    paths: ['/a'],
  };

  it('我没改任何东西：原样返回最新值（引用不变）', () => {
    const latest = { ...base, fontSize: 18 };
    expect(threeWayMerge(base, base, latest)).toBe(latest);
  });

  it('别人没改任何东西：原样返回我的版本', () => {
    const mine = { ...base, theme: 'light' };
    expect(threeWayMerge(base, mine, base)).toBe(mine);
  });

  it('改的是不同字段：两边的改动都保留', () => {
    const mine = { ...base, theme: 'light' };
    const latest = { ...base, fontSize: 18 };
    expect(threeWayMerge(base, mine, latest)).toEqual({ ...base, theme: 'light', fontSize: 18 });
  });

  it('同一个对象里改了不同子字段：逐层合并，不是整个对象二选一', () => {
    const mine = { ...base, agent: { ...base.agent, maxRounds: 100 } };
    const latest = { ...base, agent: { ...base.agent, listMode: 'allowlist' } };
    const merged = threeWayMerge(base, mine, latest);
    expect(merged.agent).toEqual({ listMode: 'allowlist', maxRounds: 100, tools: { edit: 'Approval' } });
  });

  it('两边改了同一个叶子：以我的为准', () => {
    const mine = { ...base, fontSize: 20 };
    const latest = { ...base, fontSize: 18 };
    expect(threeWayMerge(base, mine, latest).fontSize).toBe(20);
  });

  it('数组整体算一个字段：两边都改了就取我的，只有对方改了就取对方的', () => {
    expect(threeWayMerge(base, { ...base, paths: ['/b'] }, { ...base, paths: ['/c'] }).paths).toEqual(['/b']);
    expect(threeWayMerge(base, base, { ...base, paths: ['/c'] }).paths).toEqual(['/c']);
  });

  it('我删掉了一个 key、对方改了别的：删除保留，对方的改动保留', () => {
    const mine: Record<string, unknown> = { ...base };
    delete mine.theme;
    const merged = threeWayMerge(base, mine, { ...base, fontSize: 18 }) as Record<string, unknown>;
    expect('theme' in merged).toBe(false);
    expect(merged.fontSize).toBe(18);
  });

  it('对方新增了我没有的 key：带过来；我新增了对方没有的 key：保留', () => {
    const mine = { ...base, mine: 1 } as Record<string, unknown>;
    const latest = { ...base, theirs: 2 } as Record<string, unknown>;
    expect(threeWayMerge(base, mine, latest)).toMatchObject({ mine: 1, theirs: 2 });
  });

  it('key 顺序以最新值为准（与 store 里那份的 JSON 一致）', () => {
    const latest = { b: 5, a: 1 };
    const mine = { a: 1, b: 2, c: 3 };
    const merged = threeWayMerge({ a: 1, b: 2 }, mine, latest);
    expect(Object.keys(merged)).toEqual(['b', 'a', 'c']);
    expect(merged).toEqual({ a: 1, b: 5, c: 3 });
  });
});

describe('threeWayMerge · 带 id 的列表按条目合并', () => {
  const ch = (id: string, name: string, extra: Record<string, unknown> = {}) => ({ id, name, ...extra });
  const base = { channels: [ch('a', 'A'), ch('b', 'B')] };

  it('对方改了 A、我改了 B：两处改动都保留（不再整份盖回去）', () => {
    const mine = { channels: [ch('a', 'A'), ch('b', 'B2')] };
    const latest = { channels: [ch('a', 'A2'), ch('b', 'B')] };
    expect(threeWayMerge(base, mine, latest)).toEqual({ channels: [ch('a', 'A2'), ch('b', 'B2')] });
  });

  it('同一条目的不同字段：逐字段合并；同一字段：我的赢', () => {
    const b = { channels: [ch('a', 'A', { url: 'u', retry: 1 })] };
    const mine = { channels: [ch('a', 'Mine', { url: 'u', retry: 1 })] };
    const latest = { channels: [ch('a', 'Theirs', { url: 'u2', retry: 1 })] };
    expect(threeWayMerge(b, mine, latest)).toEqual({ channels: [ch('a', 'Mine', { url: 'u2', retry: 1 })] });
  });

  it('删除：一边删、另一边没动 → 删；一边删、另一边改了 → 留改动', () => {
    const iDeleteA = { channels: [ch('b', 'B')] };
    expect(threeWayMerge(base, iDeleteA, base)).toEqual({ channels: [ch('b', 'B')] });
    const theyEditA = { channels: [ch('a', 'A2'), ch('b', 'B')] };
    expect(threeWayMerge(base, iDeleteA, theyEditA)).toEqual({ channels: [ch('a', 'A2'), ch('b', 'B')] });
    const theyDeleteB = { channels: [ch('a', 'A')] };
    const iEditB = { channels: [ch('a', 'A'), ch('b', 'B2')] };
    expect(threeWayMerge(base, iEditB, theyDeleteB)).toEqual({ channels: [ch('a', 'A'), ch('b', 'B2')] });
  });

  it('两边各自新增：都保留；我调了顺序则按我的顺序，对方新增的接在后面', () => {
    const mine = { channels: [ch('b', 'B'), ch('a', 'A'), ch('m', 'M')] };
    const latest = { channels: [ch('a', 'A'), ch('b', 'B'), ch('t', 'T')] };
    expect(threeWayMerge(base, mine, latest).channels.map((c) => c.id)).toEqual(['b', 'a', 'm', 't']);
    const mineNoReorder = { channels: [ch('a', 'A'), ch('b', 'B'), ch('m', 'M')] };
    expect(threeWayMerge(base, mineNoReorder, latest).channels.map((c) => c.id)).toEqual(['a', 'b', 't', 'm']);
  });
});
