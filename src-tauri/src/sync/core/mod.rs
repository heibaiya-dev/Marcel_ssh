//! 同步核心库：纯逻辑，没有网络 / 文件 IO，整个跨设备同步的正确性地基。
//!
//! 数据流（一台设备上，以单个同步源为例）：
//!
//! ```text
//!  本机文档 JSON ──flatten──▶ 叶子        基线（上次自己序列化的结果）
//!        │                      │                    │
//!        └──────────── diff（只比基线）◀──────────────┘
//!                         │
//!                         ▼ Op（Set / Remove / Tombstone，带 HLC）
//!   ┌── 本机 op ──▶ Mirror.merge ◀── 远端 op / 快照（merge_remote_ops / ingest_remote*）
//!   │                      │
//!   │        affected_vs_doc（文档现状 vs 镜像可见集）→ 受影响路径
//!   │                      ▼
//!   └──────── apply_to_doc（只动 affected，保留本机专属字段）──▶ 新文档 → 新基线
//! ```
//!
//! 模块一览：
//! - [`hlc`]：混合逻辑时钟；
//! - [`path`]：叶子路径 `Seg`、id 映射登记表 `IdMapRegistry`、平台前缀；
//! - [`scope`]：作用域规则 `ScopeRules`（Shared / Local / Platform + 元素级过滤）；
//! - [`flatten`]：JSON ↔ 扁平叶子；
//! - [`mirror`]：`Op`、`Mirror`（LWW + 墓碑 + 整元素复活）、`affected`、`ingest_remote*`；
//! - [`diff`]：本机改动捕获；
//! - [`apply`]：远端改动落地；
//! - [`crypto`]：恢复密钥 / 数据密钥 / 密文信封；
//! - [`format`]：远端目录布局、明文 JSON 结构、同步码。

pub mod apply;
pub mod crypto;
pub mod diff;
pub mod flatten;
pub mod format;
pub mod hlc;
pub mod mirror;
pub mod path;
pub mod scope;

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod test_util;

pub use apply::{affected_vs_doc, apply_to_doc};
pub use crypto::{CryptoError, DataKey, FileKind, RecoveryKey};
pub use diff::{diff, export_all, export_all_at, DiffCtx};
pub use flatten::{flatten, flatten_scoped, learn_id_maps};
pub use format::{DeviceInfo, FormatError, SyncMeta};
pub use hlc::{wall_clock_ms, Hlc, HlcClock};
pub use mirror::{
    affected, ingest_remote, ingest_remote_mirror, merge_remote_mirror, merge_remote_ops,
    LeafEntry, Mirror, Op, OpKind,
};
pub use path::{IdMapRegistry, Path, Platform, Seg};
pub use scope::{ItemFilter, ItemKeep, Scope, ScopeRules};
