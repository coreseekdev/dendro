//! 方言档案——**两个正交轴，分别独立实现**：
//!
//! 1. **方言**（SQL 语言面）：PG / MySQL / SQLite——解析与归一
//!    行为（本模块的 [`DialectProfile`]）；
//! 2. **传输**（访问面）：pgwire（PG 二进制线协议 v3）/ mywire
//!    （MySQL 客户端协议）/ dendro-sqlite（**C ABI**——SQLite 生态
//!    的"传输"即进程内调用约定）/ embed（Rust 原生进程内）。
//!
//! | 传输 \ 方言 | 声明 | 说明 |
//! |-------------|------|------|
//! | pgwire | Pg | TCP 二进制协议 |
//! | mywire | MySql | TCP 二进制协议 |
//! | dendro-sqlite | Sqlite | **C ABI 进程内**——与 embed 同一含义
//! | embed | **Sqlite（默认，可 set_dialect）** | Rust 进程内——进程内语义以 SQLite 方言为默认 |
//!
//! **dendro-sqlite 与 embed 共享同一个进程内语义**：前者是后者
//! （[`crate::embed::Connection`]）加 C ABI 暴露，方言同为 SQLite
//!（embed 默认即 Sqlite）。引擎层 Session 默认 Pg——网络传输由
//! 各 wire 显式声明，两层默认不同是有意的。
//!
//! 架构合同：**传输适配器在连接建立时声明方言一次**
//! （`Session::dialect`，经 `WireSession::set_dialect` / embed 的
//! `set_dialect`），全下游——parse（sqlparser 方言选择）、占位符
//! 归一（`?` → `$N`）、缓存键混合——经 [`DialectProfile`] **动态
//! 分派**，不散落 `match` 臂。内部工具（CHECK 重解析/计划方言回
//! parse/测试）固定 `Pg` 档案——它们不来自传输，无方言选择问题。
//!
//! 新增一个方言：新档案结构体（实现本 trait）+ `SqlDialect` 变体 +
//! `profile()` 一臂——方言的全部行为面收在档案里。新增一个传输：
//! 选定方言并在其连接入口声明——传输层不含任何方言行为。
//!
//! 未来方言行为面挂点（按需）：标识符折叠（PG 小写/SQLite 不敏感）、
//! 引用字面量风格、系统函数别名、`TRUE/FALSE` 字面量形态等——
//! 都进档案而非散点 match。

use sqlparser::ast::{Expr, VisitMut, VisitorMut};

/// 一条访问路径的完整方言行为面
pub trait DialectProfile: Send + Sync + 'static {
    /// 档案名（诊断/缓存键盐）
    fn name(&self) -> &'static str;
    /// sqlparser 解析方言（parse 分派）
    fn parser(&self) -> &'static dyn sqlparser::dialect::Dialect;
    /// 位置占位符（`?`）是否归一为 `$N`（MySQL/SQLite yes；PG 原生
    /// `$N` 不需要）。归一实现见 [`normalize_positional`]
    fn positional_placeholders(&self) -> bool {
        false
    }
    /// 行身份别名集（查询期解析为单列整数 PK 列）。**零新列原则**：
    /// 别名是已有存储身份（PK 编码键——prolly/memtx/WAL 的统一
    /// 寻址键）的引用层名字，不在存储/表结构加任何合成列：
    /// - SQLite：`rowid` / `_rowid_` / `oid`（官方语义：INTEGER
    ///   PRIMARY KEY 即 rowid 别名）
    /// - MySQL：`_rowid`（官方兼容特性——单列整数 PK 的同义名）
    /// - PG：`ctid`（Oracle ROWID 迁移指南语义——逻辑行身份 =
    ///   主键；dendro append-only 下比 PG 原生物理位置 ctid 更
    ///   稳定，无"UPDATE 后变化"陷阱）
    fn rowid_aliases(&self) -> &'static [&'static str] {
        &[]
    }
}

/// PG 档案（pgwire / embed 默认）：原生 `$N`、无归一
pub struct PgProfile;
impl DialectProfile for PgProfile {
    fn name(&self) -> &'static str {
        "pg"
    }
    fn parser(&self) -> &'static dyn sqlparser::dialect::Dialect {
        &sqlparser::dialect::PostgreSqlDialect {}
    }
    fn rowid_aliases(&self) -> &'static [&'static str] {
        // **ctid → 单列整数 PK**（Oracle ROWID 迁移指南语义——AWS
        // prescriptive guidance：长期行标识应用主键而非物理位置）。
        // 与 PG 原生 ctid 的差异（诚实记录）：PG ctid 是物理位置
        // （页+槽，UPDATE/VACUUM FULL 后变化，官方明确不建议当长期
        // 标识）；dendro 是内容寻址、append-only——映射目标（PK）
        // 永不变，**比原生 ctid 更符合 ctid 的使用意图**。非整数
        // PK 表上引用 → undefined column（响亮）。rowid/_rowid 在
        // PG 非系统列，不映射
        &["ctid"]
    }
}

/// MySQL 档案（mywire）：`?` 位置参数归一
pub struct MySqlProfile;
impl DialectProfile for MySqlProfile {
    fn name(&self) -> &'static str {
        "mysql"
    }
    fn parser(&self) -> &'static dyn sqlparser::dialect::Dialect {
        &sqlparser::dialect::MySqlDialect {}
    }
    fn positional_placeholders(&self) -> bool {
        true
    }
    fn rowid_aliases(&self) -> &'static [&'static str] {
        &["_rowid"] // MySQL 官方兼容名（单列整数 PK 别名）
    }
}

/// SQLite 档案（dendro-sqlite C ABI）：`?` 位置参数归一、双引号
/// 标识符（sqlparser SQLiteDialect 处理）
pub struct SqliteProfile;
impl DialectProfile for SqliteProfile {
    fn name(&self) -> &'static str {
        "sqlite"
    }
    fn parser(&self) -> &'static dyn sqlparser::dialect::Dialect {
        &sqlparser::dialect::SQLiteDialect {}
    }
    fn positional_placeholders(&self) -> bool {
        true
    }
    fn rowid_aliases(&self) -> &'static [&'static str] {
        &["rowid", "_rowid_", "oid"]
    }
}

/// 访问路径判别（会话携带的紧凑键——Hash/Eq 供缓存键；行为分派
/// 一律经 [`SqlDialect::profile`]，不得 match 此枚举）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlDialect {
    Pg,
    MySql,
    /// dendro-sqlite（C ABI）线
    Sqlite,
}

impl SqlDialect {
    /// **唯一分派点**：枚举 → 行为档案。新增方言 = 新档案 + 新变体 +
    /// 此处一臂；下游任何新方言行为都加在 trait 上而非散点 match
    pub fn profile(&self) -> &'static dyn DialectProfile {
        match self {
            SqlDialect::Pg => &PgProfile,
            SqlDialect::MySql => &MySqlProfile,
            SqlDialect::Sqlite => &SqliteProfile,
        }
    }
}

/// 位置占位符归一（`?` → `$N`，出现序 = 绑定序）。AST 级
/// VisitorMut 改写——字符串字面量天然免疫（对比 wire 层字符串扫描
/// 的历史方案）。**每语句独立计数**（prepared 绑定序按单语句）。
/// 命名/编号变体（`?5`、`:name`、`@p`）v1 不归一——按未绑定参数
/// 在执行期 08P01 响亮报错
pub fn normalize_positional(stmts: &mut [sqlparser::ast::Statement]) {
    struct QNorm {
        n: u32,
    }
    impl VisitorMut for QNorm {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &mut Expr) -> std::ops::ControlFlow<Self::Break> {
            if let Expr::Value(v) = e {
                if let sqlparser::ast::Value::Placeholder(id) = &v.value {
                    if id == "?" {
                        self.n += 1;
                        v.value = sqlparser::ast::Value::Placeholder(format!("${}", self.n));
                    }
                }
            }
            std::ops::ControlFlow::Continue(())
        }
    }
    for st in stmts {
        let _ = st.visit(&mut QNorm { n: 0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse_batch;

    /// 三档案分派：同名 SQL 按各自 sqlparser 方言解析；`?` 只在
    /// 位置参数档案归一为 `$N`（PG 方言 `?` 直接解析失败）
    #[test]
    fn profile_dispatch_and_placeholder() {
        // SQLite：`?` 解析 + 归一（字符串内的 ? 免疫）
        let mut st = parse_batch(
            "SELECT a FROM t WHERE b = ? AND c <> 'why?' AND d = ?",
            SqlDialect::Sqlite,
        )
        .unwrap();
        let sql_text = format!("{}", st.remove(0));
        assert!(sql_text.contains("$1"), "{sql_text}");
        assert!(sql_text.contains("$2"), "{sql_text}");
        // MySQL 同归一
        let mut st2 = parse_batch("SELECT a FROM t WHERE b = ?", SqlDialect::MySql).unwrap();
        assert!(format!("{}", st2.remove(0)).contains("$1"));
        // PG：`?` 原生拒绝（未归一路径不受影响）
        assert!(parse_batch("SELECT a FROM t WHERE b = ?", SqlDialect::Pg).is_err());
        // PG：`$1` 原生形态不受归一影响
        assert!(parse_batch("SELECT a FROM t WHERE b = $1", SqlDialect::Pg).is_ok());
    }

    /// 每语句独立计数：批内两语句的 `?` 各自从 $1 起
    #[test]
    fn per_statement_counter() {
        let mut st = parse_batch(
            "INSERT INTO t VALUES (?); INSERT INTO t VALUES (?, ?)",
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert_eq!(st.len(), 2);
        let s1 = format!("{}", st.remove(0));
        let s2 = format!("{}", st.remove(0));
        assert!(s1.contains("$1") && !s1.contains("$2"), "{s1}");
        assert!(s2.contains("$1") && s2.contains("$2"), "{s2}");
    }
}
