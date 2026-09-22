//! 元数据 Provider 接口（SQL 前端与存储的解耦缝）。
//!
//! 设计定案（2026-09-22 owner 批准，勘探报告见会话档案）：
//! - SQL 层的元数据消费收口在 resolve_table 家族（36+ 调用点）——
//!   本 trait 是它的抽象形态，让 SQL 前端（解析/校验/审计/规划）可
//!   脱离 dendro 存储运行（SQL 审计网关：对接外部系统的表/列/索引
//!   元数据，如 SAP/Oracle 的字典扩展机制）。
//! - **语义面 / 治理面 / 统计面 / 索引面按 capability 分层**，可选
//!   能力全部默认回落（ColumnarStore 惯例）——审计网关实现者只需
//!   覆盖语义面。**数据平面绑定（entry.id/table_root/col_segments）
//!   不进本接口**：执行器的存储寻址由 dendro 内部实现继续提供，
//!   网关场景零存储负担。
//! - 双失效轴都显式暴露：`Version`（内容寻址版本令牌，dendro 实现
//!   = catalog root；外部源 = epoch/generation）+ `schema_epoch`
//!   （单调，prepared 透明重编译轴——schema_version.rs 测试基线）。
//! - 缓存合同：实现侧私有（dendro 的 RESOLVE_CACHE 键 = root，留
//!   在 DendroSemantics 内）；trait 不暴露缓存，外部实现自带失效。

use crate::error::Result;
use crate::sql::stats::TableAnalyze;
use crate::versioned::{ColumnDef, ForeignKeyDef, TableSchema};
use std::collections::HashMap;
use std::sync::Arc;

/// 表语义视图：SQL 前端需要的全部"表是什么"信息（不含存储路由）。
#[derive(Debug, Clone)]
pub struct TableSemantics {
    pub schema: TableSchema,
    pub constraints: ConstraintSet,
    /// SAP 式扩展层（append structure）：基表定义之外的追加列集，
    /// 多套并存、独立卸载（resolve 时合并进 schema.columns 的顺序
    /// = 基表列 ++ 各扩展列按挂载序）——dendro 自身实现按
    /// TableEntry.extensions 构建；外部源可直接给合并后视图
    pub extensions: Vec<ExtensionDef>,
}

/// 约束集（从 TableEntry 投影的纯 SQL 语义字段）
#[derive(Debug, Clone, Default)]
pub struct ConstraintSet {
    pub unique_sets: Vec<Vec<u16>>,
    /// CHECK 表达式（opaque SQL 文本——方言载荷，接口层不解释）
    pub check_exprs: Vec<String>,
    pub foreign_keys: Vec<ForeignKeyDef>,
}

/// 扩展层定义（SAP append structure 语义）
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExtensionDef {
    pub name: String,
    pub owner: String,
    pub columns: Vec<ColumnDef>,
}

/// 索引定义（可扩展索引的元数据面）
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IndexDef {
    pub name: String,
    /// 索引列（schema 列下标，含扩展列合并后的下标空间）
    pub columns: Vec<u16>,
    pub unique: bool,
    /// 方法占位（"prolly" = 索引树；外部源可为任意标识）
    pub method: String,
}

/// 治理面：属主 + 表级 ACL（privs.rs 的消费口径）
pub type Governance = (String, HashMap<String, u8>);

/// 语义元数据供给接口。**实现绑定单一分支/数据源视图**（多分支 =
/// 多 provider 实例；dendro 的 Session 按会话分支取内建实现）。
///
/// 实现方合同：
/// - `resolve` 对同 `(Version, name)` 必须**内容稳定**（同版本同值，
///   可缓存）——dendro 靠内容寻址天然满足；外部源需自证（等价于
///   HelixDB CatalogRefreshProof 的"不可转移凭证"语义）。
/// - 名字规约统一走 `name_normalize`（dendro 约定：`rsplit(['.','@'])`
///   截短名——schema 前缀与 `@` 分支后缀剥离）。
/// - 语义面方法失败 = 响亮错误（表不存在等）；可选面（治理/统计/
///   索引）默认回落，永不阻塞查询。
pub trait SemanticsProvider: Send + Sync {
    /// 版本令牌（固定 u64：dyn 注入位的统一类型）。**合同**：同令牌 ⇒
    /// 同元数据视图；语句/事务取一次 head_version 后续 resolve 复用；
    /// resolve 收到过期令牌应 Err（调用方重取）。dendro 实现 =
    /// catalog root 的 u64 投影（与 RESOLVE_CACHE 键同口径）；显式
    /// 事务的冻结根语义（R7-3）走保留的 resolve_table_at(frozen)
    /// 专用通道，不经本接口
    fn head_version(&self) -> Result<u64>;

    /// 解析表语义（规范名 → 合并视图）。不存在 = 响亮错误；
    /// ver != 当前视图 = Err（过期）
    fn resolve(&self, ver: &u64, table: &str) -> Result<Arc<TableSemantics>>;

    /// 全表清单（SHOW TABLES / information_schema 的收口）
    fn list(&self, ver: &u64) -> Result<Vec<(String, Arc<TableSemantics>)>>;

    /// 视图定义文本（manifest.views 的收口；无视图能力返回 None）
    fn view_text(&self, _name: &str) -> Result<Option<Arc<str>>> {
        Ok(None)
    }

    /// prepared 失效轴：单调递增的 schema 世代（dendro =
    /// manifest.schema_version；比较语义见 engine::Prepared）
    fn schema_epoch(&self) -> u64;

    /// 名字规约（默认 = dendro 短名截断约定）
    fn name_normalize(&self, name: &str) -> String {
        name.rsplit(['.', '@']).next().unwrap_or(name).to_string()
    }

    // ---- 可选面（默认回落；审计网关可不实现）----

    /// 治理面：属主 + ACL（无治理能力 = None，调用方按超户直通）
    fn owner_acl(&self, _ver: &u64, _table: &str) -> Option<Governance> {
        None
    }

    /// 统计面（fail-open：无统计 = None，估算走回退）
    fn stats(&self, _ver: &u64, _table: &str) -> Option<Arc<TableAnalyze>> {
        None
    }

    /// 索引面（无索引能力 = 空）
    fn indexes(&self, _ver: &u64, _table: &str) -> Vec<Arc<IndexDef>> {
        Vec::new()
    }
}
