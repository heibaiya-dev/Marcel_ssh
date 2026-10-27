import { useEffect, useState } from 'react';
import * as tauri from '@/lib/tauri';
import { getErrorMessage } from '@/lib/errors';

/**
 * 从后端取一份内置提示词文本。取不到时返回空串，输入框回落到 placeholder，不阻塞用户自定义。
 */
function useBackendPrompt(fetchPrompt: () => Promise<string>, what: string): string {
  const [text, setText] = useState('');
  useEffect(() => {
    let cancelled = false;
    fetchPrompt()
      .then((value) => {
        if (!cancelled) setText(value);
      })
      .catch((e) => {
        console.warn(`读取${what}失败：`, getErrorMessage(e));
      });
    return () => {
      cancelled = true;
    };
    // fetchPrompt 是模块级函数，恒定不变
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return text;
}

/**
 * 内置的命令审批系统提示词，取自后端模板 `templates/approval/审批.hbs`。
 *
 * 前端不再自带副本（以前 TS 里那份与模板逐字节相同，两边会各自漂移）：
 * 设置项留空时用它的内容做展示默认值，用户编辑后与它比较——与内置文本完全
 * 相同则存回空串，即"未自定义"。
 */
export function useDefaultApprovalPrompt(): string {
  return useBackendPrompt(() => tauri.agentDefaultApprovalPrompt(), '内置审批提示词');
}

/**
 * 内置的 Jev 判据，取自 `templates/approval/审批Jev.hbs`。用法与 {@link useDefaultApprovalPrompt}
 * 完全一致：预填在输入框里，改回原文就存回空串。
 */
export function useDefaultJevPrompt(): string {
  return useBackendPrompt(() => tauri.agentDefaultJevPrompt(), '内置 Jev 判据');
}
