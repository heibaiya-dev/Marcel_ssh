/**
 * 设置的「三方合并」：只把**我真正改过的字段**落到**最新**数据上。
 *
 * 为什么需要它：前端把**整份**设置发给后端保存（`config_save_settings`）。开了跨设备同步之后，
 * 别的设备的改动会在任何时刻落到本机，如果保存时拿的是「打开设置页那一刻」的旧整份，
 * 就会把刚同步来的字段原样改回去——而且这个「改回去」会被当成本机的新改动再同步给所有设备。
 *
 * 三个输入：
 * - `base`   我开始改的时候看到的那一份（草稿打开时的快照）；
 * - `mine`   我现在的版本（base + 我的改动）；
 * - `latest` 此刻最新的真值（可能已经包含别的设备同步来的改动）。
 *
 * 规则（逐层，对象按 key 递归；普通数组 / 标量按整体算一个"字段"）：
 * 1. 我没改这个字段（mine == base）→ 取 latest；
 * 2. 别人没改这个字段（latest == base）→ 取 mine；
 * 3. 两边都改了且都是对象 → 继续往下逐 key 合并（两台设备改了同一个对象里的不同子字段，应当都保留）；
 * 4. 两边都改了且都是「带 id 的条目列表」（模型渠道、模型、自定义规则……）→ 按 id 逐条合并，
 *    见 {@link mergeIdLists}；
 * 5. 两边都改了同一个叶子 → 我的赢（用户刚刚亲手改的，优先于一次后台同步）。
 *
 * 粒度对应同步引擎的「叶子」：对象逐 key，带 id 的列表逐条（后端 `sync::core::flatten` 的 id 映射），
 * 其余数组整体。这样前端合并的结果与后端捕获改动时的口径一致——否则设置页里改了渠道 B，
 * 会把别的设备刚同步来的渠道 A 的改动整份盖回去。
 */

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/**
 * 深比较（JSON 语义）：对象 key 顺序无关；值为 `undefined` 的 key 等同于不存在
 * （序列化给后端时本来就会被丢掉）。
 */
export function deepEqual(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i += 1) {
      if (!deepEqual(a[i], b[i])) return false;
    }
    return true;
  }
  if (isPlainObject(a) && isPlainObject(b)) {
    const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
    for (const key of keys) {
      if (!deepEqual(a[key], b[key])) return false;
    }
    return true;
  }
  return false;
}

/**
 * 三方合并。返回值在「没有任何本地改动」时就是 `latest` 本身（引用不变，下游不会白白重渲染）。
 * key 的顺序以 `latest` 为准、再接上只在 `mine` 里新增的 key——保持与 store 里那份一致，
 * 草稿与 store 的 JSON 比较不会因为 key 顺序不同而误报"有未保存的更改"。
 */
export function threeWayMerge<T>(base: T, mine: T, latest: T): T {
  if (deepEqual(mine, base)) return latest;
  if (deepEqual(latest, base)) return mine;
  if (deepEqual(mine, latest)) return latest;
  if (isPlainObject(mine) && isPlainObject(latest) && isPlainObject(base ?? {})) {
    const baseObj = (isPlainObject(base) ? base : {}) as Record<string, unknown>;
    const out: Record<string, unknown> = {};
    const keys = [...Object.keys(latest), ...Object.keys(mine).filter((k) => !(k in latest))];
    for (const key of keys) {
      const merged = threeWayMerge<unknown>(baseObj[key], mine[key], latest[key]);
      if (merged !== undefined) out[key] = merged;
    }
    // base 里有、latest 和 mine 里都已经没有的 key，自然不会出现在 keys 里，无需处理。
    return out as T;
  }
  if (isIdList(mine) && isIdList(latest) && (base === undefined || isIdList(base))) {
    return mergeIdLists((base ?? []) as IdItem[], mine, latest) as T;
  }
  // 标量 / 数组 / 类型不一致：两边都动过同一个字段，以我的为准。
  return mine;
}

type IdItem = Record<string, unknown> & { id: string };

/** 非空、且每一项都是带字符串 `id` 的对象（与后端 id 映射的判定一致；空数组按普通数组算）。 */
function isIdList(v: unknown): v is IdItem[] {
  return (
    Array.isArray(v) &&
    v.length > 0 &&
    v.every((item) => isPlainObject(item) && typeof item.id === 'string')
  );
}

/**
 * 带 id 的列表按条目三方合并：
 * - 两边都有的条目 → 逐字段 {@link threeWayMerge}；
 * - 我删了、对方没动它 → 删；我删了、对方改了它 → 留对方的（与后端「改动压过删除」一致）；
 * - 对方删了、我没动它 → 删；对方删了、我改了它 → 留我的；
 * - 只在一边新增的 → 留下。
 * 顺序：我动过顺序（相对 base）就以我的顺序为准，否则以最新的为准；另一边独有的条目接在后面。
 */
function mergeIdLists(base: IdItem[], mine: IdItem[], latest: IdItem[]): IdItem[] {
  const byId = (list: IdItem[]) => new Map(list.map((item) => [item.id, item]));
  const baseMap = byId(base);
  const mineMap = byId(mine);
  const latestMap = byId(latest);

  const resolve = (id: string): IdItem | undefined => {
    const b = baseMap.get(id);
    const m = mineMap.get(id);
    const l = latestMap.get(id);
    if (m && l) return threeWayMerge<IdItem>(b as IdItem, m, l);
    if (m) {
      // 对方没有：要么对方删了（b 存在），要么我新增的（b 不存在）。
      if (b && deepEqual(m, b)) return undefined;
      return m;
    }
    if (l) {
      if (b && deepEqual(l, b)) return undefined;
      return l;
    }
    return undefined;
  };

  const ids = (list: IdItem[]) => list.map((item) => item.id);
  const mineReordered = !deepEqual(
    ids(mine).filter((id) => baseMap.has(id)),
    ids(base).filter((id) => mineMap.has(id)),
  );
  const primary = mineReordered ? mine : latest;
  const secondary = mineReordered ? latest : mine;
  const order = [
    ...ids(primary),
    ...ids(secondary).filter((id) => !(mineReordered ? mineMap : latestMap).has(id)),
  ];

  const out: IdItem[] = [];
  for (const id of order) {
    const item = resolve(id);
    if (item !== undefined) out.push(item);
  }
  return out;
}
