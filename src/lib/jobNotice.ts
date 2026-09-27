/**
 * 「作业结算告知」文本的解析。
 *
 * 后端 `build_job_settlement_notice`（`agent_loop.rs`）把告知写成两段：每个作业
 * 一行 `后台作业 {id}（{描述}）{状态}`，末尾一行是给模型的指令（去 `job_output`
 * 读输出）。这条文本既要喂给模型，也要在界面上长成一张卡 —— 标题行给
 * id + 描述 + 状态，给模型的指令收进展开区。这里只做「拆行 + 认作业行」，
 * **认不出来的一律当普通文本**：显示层绝不能因为后端换了措辞就把内容吞掉。
 *
 * 格式的权威在后端；`jobNotice.test.ts` 读那份源码钉住格式串（与 `role`、
 * `origin` 同一手法），改了那边这里就会红。
 */

/**
 * 一行作业：`后台作业 job_3（构建）已完成`。
 *
 * id 与状态都**不许含全角括号**（id 是 `job_N`，状态是后端那几个中文词），描述
 * 不限——描述里带括号（`构建（release）`）时靠这两条边界认，不靠贪婪：
 * `(\S+)` 若允许吃括号，`（构建（release））` 会被切成 id=`job_7（构建`、
 * 描述=`release）`（实测踩过）。
 */
const JOB_LINE = /^后台作业\s+([^（）\s]+)（(.*)）([^（）\s]+)$/;

export interface JobNoticeItem {
  jobId: string;
  /** 描述（后端在描述为空时回退成命令原文，这里原样收着）。 */
  description: string;
  /** 后端给的中文状态：已完成 / 执行失败 / 已被终止 / 随应用退出中断 / 仍在运行。 */
  status: string;
}

export interface JobNoticeParts {
  jobs: JobNoticeItem[];
  /** 其余非空行（给模型的那句指令），保持原顺序。 */
  notes: string[];
}

export function parseJobNotice(content: string): JobNoticeParts {
  const jobs: JobNoticeItem[] = [];
  const notes: string[] = [];
  for (const raw of content.split('\n')) {
    const line = raw.trim();
    if (!line) continue;
    const match = JOB_LINE.exec(line);
    if (match) {
      jobs.push({ jobId: match[1], description: match[2].trim(), status: match[3] });
    } else {
      notes.push(line);
    }
  }
  return { jobs, notes };
}
