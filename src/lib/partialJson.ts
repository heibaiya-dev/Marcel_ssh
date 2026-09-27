/**
 * 增量 JSON 完整性追踪。
 *
 * 流式 tool call arguments 每个 delta 到达时，判断「累计文本是否已是语法
 * 完整的 JSON」只需要看**新增字符**：引号/转义/括号深度都是流式状态。
 * 这让「完整才 JSON.parse 一次」成为可能——否则每个 delta 都要对累计全文
 * parse 一次（Θ(B²)，长参数流式期间全是注定失败的 SyntaxError）。
 *
 * 状态机与上面的词法扫描共享同一套 JSON 规则；只跟踪结构性状态，
 * 不产出内容。同一规则只此一处实现。
 */

export interface JsonStreamTracker {
  /** 是否已见到首个结构字符（`{`）——空串/纯空白不算完整。 */
  started: boolean;
  /** 是否在字符串字面量内（引号未闭合）。 */
  inString: boolean;
  /** 转义尾巴剩余字符数（0=不在转义中；1=反斜杠后的说明符；`\u` 后为 4 个 hex）。 */
  escape: number;
  /** 当前转义尾巴是 `\uXXXX` 的 hex 段（说明符 `u` 已消费）。 */
  unicode: boolean;
  /** 未闭合的 `{`/`[` 总深度。 */
  depth: number;
  /** 结构已坏（深度为负等）：永不判完整，等 toolResult 收尾。 */
  broken: boolean;
}

export function createJsonStreamTracker(): JsonStreamTracker {
  return { started: false, inString: false, escape: 0, unicode: false, depth: 0, broken: false };
}

/** 喂入一段新增字符（按 UTF-16 单元前进；结构性字符都是 ASCII）。 */
export function feedJsonChunk(tracker: JsonStreamTracker, delta: string): void {
  if (tracker.broken) return;
  for (let i = 0; i < delta.length; i++) {
    const ch = delta[i];
    if (tracker.inString) {
      if (tracker.escape > 0) {
        if (tracker.escape === 1 && !tracker.unicode && ch === 'u') {
          // 这个字符是 \u 的说明符；其后还有 4 个 hex
          tracker.unicode = true;
          tracker.escape = 4;
        } else {
          tracker.escape--;
          if (tracker.escape === 0) tracker.unicode = false;
        }
        continue;
      }
      if (ch === '\\') {
        tracker.escape = 1;
      } else if (ch === '"') {
        tracker.inString = false;
      }
      continue;
    }
    if (ch === '"') {
      tracker.inString = true;
    } else if (ch === '{' || ch === '[') {
      tracker.started = true;
      tracker.depth++;
    } else if (ch === '}' || ch === ']') {
      tracker.depth--;
      if (tracker.depth < 0) {
        tracker.broken = true;
        return;
      }
    }
    // 标量字面量/空白：不影响完整性
  }
}

/** 累计文本是否已构成语法完整的 JSON 顶层值。 */
export function isJsonComplete(tracker: JsonStreamTracker): boolean {
  return (
    tracker.started &&
    !tracker.broken &&
    !tracker.inString &&
    tracker.escape === 0 &&
    tracker.depth === 0
  );
}

/**
 * Partial-JSON 字符串字段提取。
 *
 * 背景：LLM 流式输出 tool call arguments 时（toolCallDelta 事件累积），
 * JSON 在整个生成期间都是不完整的（如 `{"title":"图表","fragment":"<div>...`），
 * `JSON.parse` 会一直失败到最后一刻。对于 render_html 这类"长字符串字段
 * 需要实时预览"的工具，必须能从不完整的 JSON 中提取已生成的字符串前缀。
 *
 * 这是一个通用能力（按字段名提取），不与任何具体工具耦合；未来其他
 * 工具（如 write_file 内容预览）可复用。
 */

/**
 * 从（可能不完整的）JSON 对象文本中提取顶层字符串字段的当前值。
 *
 * - 字段已完整闭合 → 返回完整解码值
 * - 字段值仍在生成中（未遇到闭合引号）→ 返回已生成部分的解码值
 * - 字段尚未出现 / 不是字符串 → 返回 null
 *
 * 只做顶层扫描（LLM 工具参数都是扁平对象）；正确跳过其他字符串值中
 * 恰好包含 `"field":` 字样的干扰（因为扫描器按 JSON 词法逐字符前进，
 * 不做正则匹配）。
 */
export function extractPartialStringField(buffer: string, field: string): string | null {
  const len = buffer.length;
  let i = 0;

  // 找到对象开头
  while (i < len && buffer[i] !== '{') i++;
  if (i >= len) return null;
  i++;

  // 顶层扫描循环：期待 "key" : value , ...
  while (i < len) {
    // 跳过空白与逗号
    while (i < len && (buffer[i] === ',' || isWs(buffer[i]))) i++;
    if (i >= len || buffer[i] === '}') return null;
    if (buffer[i] !== '"') return null; // 非法/意外形态，放弃

    // 读 key
    const keyResult = readString(buffer, i);
    if (keyResult.value === null && !keyResult.terminated) {
      // key 本身被截断，无法判断
      return null;
    }
    const key = keyResult.value;
    i = keyResult.end;
    if (!keyResult.terminated) return null;

    // 冒号
    while (i < len && isWs(buffer[i])) i++;
    if (i >= len || buffer[i] !== ':') return null;
    i++;
    while (i < len && isWs(buffer[i])) i++;
    if (i >= len) return null;

    if (key === field) {
      if (buffer[i] !== '"') return null; // 目标字段不是字符串
      const val = readString(buffer, i);
      // 无论是否闭合，都返回已解码部分（partial 语义）
      return val.value ?? '';
    }

    // 跳过这个（非目标）值
    const skipped = skipValue(buffer, i);
    if (skipped === -1) return null; // 值被截断在中途，目标字段还没出现
    i = skipped;
  }
  return null;
}

function isWs(c: string): boolean {
  return c === ' ' || c === '\t' || c === '\n' || c === '\r';
}

/**
 * 从 `start`（指向开头引号）读取 JSON 字符串。
 * 返回解码后的值、扫描结束位置（闭合引号后一位，或缓冲区末尾）、是否闭合。
 * 尾部悬挂的不完整转义序列（如 `\` 或 `\u12`）被丢弃而不是报错。
 */
function readString(
  buffer: string,
  start: number,
): { value: string | null; end: number; terminated: boolean } {
  const len = buffer.length;
  let i = start + 1; // 跳过开头引号
  let out = '';
  while (i < len) {
    const c = buffer[i];
    if (c === '"') {
      return { value: out, end: i + 1, terminated: true };
    }
    if (c === '\\') {
      if (i + 1 >= len) break; // 悬挂转义符，丢弃
      const e = buffer[i + 1];
      switch (e) {
        case '"': out += '"'; i += 2; break;
        case '\\': out += '\\'; i += 2; break;
        case '/': out += '/'; i += 2; break;
        case 'b': out += '\b'; i += 2; break;
        case 'f': out += '\f'; i += 2; break;
        case 'n': out += '\n'; i += 2; break;
        case 'r': out += '\r'; i += 2; break;
        case 't': out += '\t'; i += 2; break;
        case 'u': {
          if (i + 6 > len) {
            // 不完整的 \uXXXX，丢弃尾部
            return { value: out, end: len, terminated: false };
          }
          const hex = buffer.slice(i + 2, i + 6);
          const code = Number.parseInt(hex, 16);
          if (Number.isNaN(code)) {
            // 非法转义——按原样保守输出，避免丢内容
            out += '\\u' + hex;
          } else {
            out += String.fromCharCode(code);
          }
          i += 6;
          break;
        }
        default:
          // 非法转义，按原样输出
          out += e;
          i += 2;
      }
      continue;
    }
    out += c;
    i++;
  }
  return { value: out, end: len, terminated: false };
}

/**
 * 跳过一个完整的 JSON 值（字符串/数字/对象/数组/字面量）。
 * 返回值结束后的位置；值被截断（缓冲区结束仍未闭合）返回 -1。
 */
function skipValue(buffer: string, start: number): number {
  const len = buffer.length;
  const c = buffer[start];
  if (c === '"') {
    const r = readString(buffer, start);
    return r.terminated ? r.end : -1;
  }
  if (c === '{' || c === '[') {
    const open = c;
    const close = open === '{' ? '}' : ']';
    let depth = 0;
    let i = start;
    while (i < len) {
      const ch = buffer[i];
      if (ch === '"') {
        const r = readString(buffer, i);
        if (!r.terminated) return -1;
        i = r.end;
        continue;
      }
      if (ch === open || (ch === '{' || ch === '[')) depth++;
      else if (ch === close || ch === '}' || ch === ']') {
        depth--;
        if (depth === 0) return i + 1;
      }
      i++;
    }
    return -1;
  }
  // 数字 / true / false / null：读到分隔符
  let i = start;
  while (i < len && buffer[i] !== ',' && buffer[i] !== '}' && !isWs(buffer[i])) i++;
  // 截断的字面量无法与完整字面量区分（如 `tru`）；保守地认为
  // 只有后面还出现了分隔符才算完整
  if (i >= len) return -1;
  return i;
}
