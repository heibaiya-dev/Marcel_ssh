import { create } from 'zustand';
import type { ParsedSkill, Skill, SkillDiscovery, SkillResource } from '@/lib/types';
import * as tauri from '@/lib/tauri';
import { getErrorMessage } from '@/lib/errors';
import { useMarketStore } from './marketStore';

interface SkillState {
  skills: Skill[];
  loading: boolean;
  error: string | null;

  /** `silent`：后台静默刷新（跨设备同步落地后用），不翻 loading、不闪动列表。 */
  fetchSkills: (opts?: { silent?: boolean }) => Promise<void>;
  addSkill: (
    name: string,
    description: string,
    prompt: string,
    resources?: SkillResource[],
  ) => Promise<void>;
  /** resources 省略 = 保持原样；传数组 = 整体替换（空数组清空）。 */
  updateSkill: (
    id: string,
    name?: string,
    description?: string,
    prompt?: string,
    resources?: SkillResource[],
  ) => Promise<void>;
  toggleSkill: (id: string) => Promise<void>;
  deleteSkill: (id: string) => Promise<void>;
  reorderSkills: (orderedIds: string[]) => Promise<void>;
  /** 列出 GitHub 仓库里的 skill（走市场配置的镜像）。失败时抛错，由调用方展示。 */
  discoverFromUrl: (url: string, importId?: string) => Promise<SkillDiscovery>;
  /** 从 GitHub 拉取并解析指定 skill（不入库）。失败时抛错，由调用方展示。 */
  importFromUrl: (url: string, subpath?: string, importId?: string) => Promise<ParsedSkill>;
}

export const useSkillStore = create<SkillState>((set, get) => ({
  skills: [],
  loading: false,
  error: null,

  fetchSkills: async (opts) => {
    const silent = opts?.silent === true;
    if (!silent) set({ loading: true, error: null });
    try {
      const skills = await tauri.skillList();
      set({ skills });
    } catch (err) {
      set({ error: getErrorMessage(err) });
    } finally {
      if (!silent) set({ loading: false });
    }
  },

  addSkill: async (name, description, prompt, resources) => {
    set({ loading: true, error: null });
    try {
      await tauri.skillAdd(name, description, prompt, resources);
      await get().fetchSkills();
    } catch (err) {
      set({ error: getErrorMessage(err), loading: false });
    }
  },

  updateSkill: async (id, name, description, prompt, resources) => {
    set({ loading: true, error: null });
    try {
      await tauri.skillUpdate(id, name, description, prompt, resources);
      await get().fetchSkills();
    } catch (err) {
      set({ error: getErrorMessage(err), loading: false });
    }
  },

  toggleSkill: async (id) => {
    // Optimistic update
    set((state) => ({
      skills: state.skills.map((s) =>
        s.id === id ? { ...s, enabled: !s.enabled } : s
      ),
    }));
    try {
      await tauri.skillToggle(id);
    } catch (err) {
      // Revert on failure
      set((state) => ({
        skills: state.skills.map((s) =>
          s.id === id ? { ...s, enabled: !s.enabled } : s
        ),
        error: getErrorMessage(err),
      }));
    }
  },

  deleteSkill: async (id) => {
    set({ loading: true, error: null });
    try {
      await tauri.skillDelete(id);
      await get().fetchSkills();
    } catch (err) {
      set({ error: getErrorMessage(err), loading: false });
    }
  },

  reorderSkills: async (orderedIds) => {
    // 乐观更新：与后端 apply_user_order 相同的 position 分配规则
    const prevSkills = get().skills;
    const posById = new Map(orderedIds.map((id, i) => [id, (i + 1) * 1000]));
    set((state) => ({
      skills: state.skills.map((s) =>
        posById.has(s.id) ? { ...s, position: posById.get(s.id)! } : s,
      ),
    }));
    try {
      await tauri.skillReorder(orderedIds);
    } catch (err) {
      set({ skills: prevSkills, error: getErrorMessage(err) });
    }
  },

  discoverFromUrl: (url, importId) =>
    tauri.skillDiscoverFromUrl(url, useMarketStore.getState().sourceUrl || undefined, importId),

  importFromUrl: (url, subpath, importId) =>
    tauri.skillImportFromUrl(
      url,
      subpath,
      useMarketStore.getState().sourceUrl || undefined,
      importId,
    ),
}));
