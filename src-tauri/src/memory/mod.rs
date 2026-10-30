//! 长期记忆（manager）。
//!
//! 让 agent 跨会话记住三类东西：关于用户本人的（全局）、一批机器共有的（分组）、
//! 某台机器的（按连接）。它协调的东西：
//!
//! - **存储** `store.rs`：`{config_dir}/memory.db`，条目 + 操作日志（撤销）+ 复盘
//!   进度与提示行 + 每会话冻结的注入快照；
//! - **注入** `inject.rs`：系统提示词里的「长期记忆」段，**按会话冻结**（前缀缓存不被
//!   agent 自己写的记忆打穿；用户亲手改的立刻生效）；
//! - **工具** `agent/tools/memory.rs`：`memory`（写）与 `memory_recall`（搜记忆 + 这台
//!   机器的历史会话）；
//! - **后台复盘** `reflect.rs`：一段对话告一段落后，用记忆模型整理值得长期记住的；
//! - **整理助手** `assistant.rs`：记忆管理页的对话框，一句话批量整理；
//! - **作用域** `scope.rs`：连接 / 会话 → 作用域的唯一翻译处；
//! - 对外接线：任务开始 / 收尾（`agent::manager`）、压缩落库、连接删除与分组改名
//!   （`commands::connections`）、会话删除（`commands::agent_conversation`）、
//!   会话断开（`lib.rs` 的断连观察者）、设置页与管理页（`commands::memory`）。
//!
//! 不做的事（刻意）：不跨设备同步（手机、电脑各记各的）；不扫描机密（由提示词约束：
//! 只记「放在哪」）；agent 写入不弹审批（对话里有可撤销的卡片）。

pub mod assistant;
pub mod inject;
pub mod model;
pub mod reflect;
pub mod scope;
pub mod store;

use std::path::Path;
use std::sync::{Arc, OnceLock};

use tauri::AppHandle;

use crate::data::DataSourceId;
use crate::error::AppError;
use crate::llm::manager::LlmManager;
use crate::AppState;

pub use model::{MemoryItem, Scope, ScopeRef, Source};
pub use store::MemoryStore;

pub struct MemoryManager {
    pub store: Arc<MemoryStore>,
    pub reflector: reflect::Reflector,
    pub assistant: assistant::AssistantRuns,
    /// 应用启动时挂上（`lib.rs` 的 setup）。各处钩子只拿得到 `AppState`，发事件
    /// 要用它；挂上之前（以及单测里）发不出事件，但写库照常。
    app: OnceLock<AppHandle>,
}

impl MemoryManager {
    /// 打开 `DataSourceId::Memory`（`{config_dir}/memory.db`）；打不开时退回内存库
    /// （记忆不落盘，但功能可用）并记日志，不让启动失败。
    pub fn open(config_dir: &Path) -> Self {
        let path = DataSourceId::Memory.path(config_dir);
        let store = match MemoryStore::open(&path) {
            Ok(s) => {
                log::info!("✓ 记忆库已加载: {}", path.display());
                s
            }
            Err(e) => {
                log::warn!("记忆库打开失败（{}），本次退回内存库: {e}", path.display());
                MemoryStore::in_memory()
            }
        };
        Self::with_store(store)
    }

    pub fn with_store(store: MemoryStore) -> Self {
        Self {
            store: Arc::new(store),
            reflector: reflect::Reflector::default(),
            assistant: assistant::AssistantRuns::default(),
            app: OnceLock::new(),
        }
    }

    pub fn attach_app(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    pub fn app(&self) -> Option<&AppHandle> {
        self.app.get()
    }
}

/// 整理助手的模型：用户在助手里**明确选的** → 记忆槽位 → 全局默认模型。
///
/// 明确选的模型解析不了（被删了）就报错、不悄悄换一个——用户点名要的模型，换了
/// 他自己都不知道花的是哪个的钱。没选时走 [`resolve_memory_model`]（助手没有会话，
/// 所以最后落到全局默认）。
pub(crate) async fn resolve_assistant_model(
    state: &AppState,
    explicit: Option<&str>,
) -> Result<LlmManager, AppError> {
    if let Some(id) = explicit.map(str::trim).filter(|id| !id.is_empty()) {
        let reg = state.settings.read().await.llm_registry.clone();
        let resolved = reg.resolve_model(id)?;
        return LlmManager::new(resolved.config);
    }
    resolve_memory_model(state, None).await
}

/// 记忆模型：记忆槽位 → 会话主模型（会话级记忆 → 全局最近使用 → 第一个）。
///
/// **不**回落压缩模型（用户拍板）。槽位指向的模型解析失败时退回主模型并记日志，
/// 不让复盘因为一个过期的槽位整个停摆。
pub(crate) async fn resolve_memory_model(
    state: &AppState,
    conversation_id: Option<&str>,
) -> Result<LlmManager, AppError> {
    let reg = state.settings.read().await.llm_registry.clone();
    if !reg.slots.memory_model_id.is_empty() {
        match reg.resolve_model(&reg.slots.memory_model_id) {
            Ok(r) => return LlmManager::new(r.config),
            Err(e) => log::warn!("记忆模型解析失败，退回主模型: {e}"),
        }
    }
    let session_model = conversation_id.and_then(|cid| {
        state
            .session_models
            .read()
            .get(cid)
            .filter(|s| !s.is_empty())
            .cloned()
    });
    let resolved = match session_model {
        Some(id) => reg.resolve_override(&id)?,
        None => reg.resolve_default()?,
    };
    LlmManager::new(resolved.config)
}
