//! S-4（方向4 + 评审修复）：GRANT/REVOKE 与语句权限门（PG 兼容子集，表级）。
//!
//! 模型（v1）：
//! - **角色 = 用户名**（不区分 role/user；pgwire startup `user` 参数 /
//!   embed `set_user`）；引导超户 `dendro`（embed/pgwire 缺省——缺省
//!   行为不变：全部放行，既有测试不受影响）。**标识符折叠小写**（PG
//!   语义；set_user/建表 owner/授权名统一 norm_user）；
//! - **ACL 存储**：TableEntry.acl（serde default 空——旧 manifest 反序列
//!   化兼容），role → 权限位；owner 字段（"" = 引导期建表 = 超户所有）；
//! - **表级权限位**：SELECT/INSERT/UPDATE/DELETE；列级清单（GRANT
//!   SELECT (col)）not_supported 诚实拒绝——不静默展平为全表（评审 P1）；
//! - **单点门**：exec_statement 在 Q-10 检查后调用 enforce——语句 AST
//!   走查表引用，**覆盖执行器实际支持的引用形态**（评审 3×P0 修复）：
//!   FROM/JOIN、派生表（Derived 子查询递归）、集合操作两侧、
//!   INSERT ... SELECT 源、视图（manifest.views 命中→parse 视图体递归，
//!   深度上限防环）、EXPLAIN 内层、CREATE VIEW 体；DROP/ALTER 走
//!   require_owner（属主或超户）；
//! - GRANT/REVOKE 本身 = catalog 写（owner 或超户可授；无 WITH GRANT
//!   OPTION 传递，v1 not_supported）；**读改写在 commit_mu 内**（评审
//!   P1：并发授权丢失更新）。

use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use crate::sql::scan;
use crate::types::Output;
use sqlparser::ast::{GrantObjects, Grantee, Query, SetExpr, Statement, TableFactor};

pub const PRIV_SELECT: u8 = 1;
pub const PRIV_INSERT: u8 = 2;
pub const PRIV_UPDATE: u8 = 4;
pub const PRIV_DELETE: u8 = 8;
pub const PRIV_ALL: u8 = PRIV_SELECT | PRIV_INSERT | PRIV_UPDATE | PRIV_DELETE;

/// 引导超户（embed/pgwire 缺省 user；owner 为空的旧表视为它所有）
pub const SUPERUSER: &str = "dendro";

/// 视图递归展开深度上限（防视图互引成环；与 eval 侧 VIEW_DEPTH 同动机）
const VIEW_WALK_CAP: usize = 8;

/// 标识符折叠（PG）：user/owner/grantee 统一小写
pub fn norm_user(s: &str) -> String {
    s.to_ascii_lowercase()
}

fn is_superuser(sess: &Session) -> bool {
    norm_user(&sess.user) == SUPERUSER
}

fn entry_owner(entry: &crate::versioned::TableEntry) -> String {
    if entry.owner.is_empty() {
        SUPERUSER.to_string()
    } else {
        norm_user(&entry.owner)
    }
}

/// 语句 → [(表名, 所需权限位)]。**纯 AST 走查**（无 db 访问——视图
/// 展开在 enforce 侧，见 check_table_ref）。覆盖：FROM/JOIN（含集合
/// 操作两侧、派生表递归）、INSERT 目标+源、UPDATE/DELETE/TRUNCATE、
/// CREATE VIEW 体、EXPLAIN 内层。
pub(crate) fn required_privs(stmt: &Statement) -> Vec<(String, u8)> {
    let mut out = Vec::new();
    collect_stmt(stmt, &mut out);
    out
}

fn collect_stmt(stmt: &Statement, out: &mut Vec<(String, u8)>) {
    match stmt {
        Statement::Query(q) => query_tables(q, PRIV_SELECT, out),
        Statement::Insert(ins) => {
            if let sqlparser::ast::TableObject::TableName(name) = &ins.table {
                out.push((obj_name(name), PRIV_INSERT));
            }
            // INSERT ... SELECT：源查询仍需 SELECT
            if let Some(src) = &ins.source {
                query_tables(src, PRIV_SELECT, out);
            }
        }
        Statement::Update(upd) => {
            factor_tables(&upd.table, PRIV_UPDATE, out);
        }
        Statement::Delete(del) => {
            for t in &del.tables {
                out.push((obj_name(t), PRIV_DELETE));
            }
            match &del.from {
                sqlparser::ast::FromTable::WithFromKeyword(from)
                | sqlparser::ast::FromTable::WithoutKeyword(from) => {
                    for twj in from {
                        factor_tables(twj, PRIV_DELETE, out);
                    }
                }
            }
        }
        Statement::Truncate(tr) => {
            for t in &tr.table_names {
                out.push((obj_name(&t.name), PRIV_DELETE));
            }
        }
        // CREATE VIEW：视图体是存储的查询——建视图即读底层表（评审 P0：
        // 原走查遗漏 → CREATE VIEW v AS SELECT * FROM secret 两步绕过）
        Statement::CreateView(cv) => query_tables(&cv.query, PRIV_SELECT, out),
        // EXPLAIN 内层语句同受门（防调度/形状泄漏；评审 P2）
        Statement::Explain { statement, .. } => collect_stmt(statement, out),
        _ => {}
    }
}

fn obj_name(n: &sqlparser::ast::ObjectName) -> String {
    n.0.iter()
        .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
        .collect::<Vec<_>>()
        .join(".")
        .to_ascii_lowercase()
}

fn query_tables(q: &Query, bits: u8, out: &mut Vec<(String, u8)>) {
    setexpr_tables(&q.body, bits, out);
}

fn setexpr_tables(se: &SetExpr, bits: u8, out: &mut Vec<(String, u8)>) {
    match se {
        SetExpr::Select(sel) => {
            for twj in &sel.from {
                factor_tables(twj, bits, out);
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            setexpr_tables(left, bits, out);
            setexpr_tables(right, bits, out);
        }
        SetExpr::Query(inner) => query_tables(inner, bits, out),
        _ => {}
    }
}

fn factor_tables(twj: &sqlparser::ast::TableWithJoins, bits: u8, out: &mut Vec<(String, u8)>) {
    factor_table(&twj.relation, bits, out);
    for j in &twj.joins {
        factor_table(&j.relation, bits, out);
    }
}

fn factor_table(tf: &TableFactor, bits: u8, out: &mut Vec<(String, u8)>) {
    match tf {
        TableFactor::Table { name, .. } => out.push((obj_name(name), bits)),
        // 派生表（评审 P0：原走查遗漏 → SELECT * FROM (SELECT * FROM
        // secret) x 全量绕过）
        TableFactor::Derived { subquery, .. } => query_tables(subquery, bits, out),
        _ => {}
    }
}

/// 单点权限门（exec_statement / prepare 调用）：超户直通；其余按语句
/// 形态检查。表/视图不存在 → 放行至执行路径按其原生口径报错（42P01）；
/// 其他 resolve 错误（分支故障等瞬态）如实上抛（评审 P2：不吞错放行）。
pub(crate) fn enforce(db: &Database, sess: &Session, stmt: &Statement) -> Result<()> {
    if is_superuser(sess) {
        return Ok(());
    }
    let user = norm_user(&sess.user);

    // DROP / ALTER：属主或超户（require_owner）；DROP VIEW 等非表对象
    // 无 owner 追踪 → 非超户拒绝（诚实限制，v2 补属主后放开）
    match stmt {
        Statement::Drop {
            object_type, names, ..
        } => {
            use sqlparser::ast::ObjectType;
            for n in names {
                match object_type {
                    ObjectType::Table => {
                        let t = obj_name(n);
                        if let Ok((_, entry)) = scan::resolve_table(db, &sess.branch, &t) {
                            require_owner(&user, &entry, &t)?;
                        }
                    }
                    _ => {
                        return Err(SqlError::new(
                            "42501",
                            format!("permission denied to drop {n} (owner tracking pending v2)"),
                        ));
                    }
                }
            }
        }
        Statement::AlterTable(alt) => {
            let t = obj_name(&alt.name);
            if let Ok((_, entry)) = scan::resolve_table(db, &sess.branch, &t) {
                require_owner(&user, &entry, &t)?;
            }
        }
        _ => {}
    }

    for (table, bits) in required_privs(stmt) {
        check_table_ref(db, sess, &user, &table, bits, 0)?;
    }
    Ok(())
}

/// 单个表引用的检查：表 → owner/ACL；视图 → parse 视图体递归。
fn check_table_ref(
    db: &Database,
    sess: &Session,
    user: &str,
    table: &str,
    bits: u8,
    depth: usize,
) -> Result<()> {
    if depth > VIEW_WALK_CAP {
        return Err(SqlError::internal(
            "view nesting too deep in privilege walk",
        ));
    }
    match scan::resolve_table(db, &sess.branch, table) {
        Ok((_, entry)) => {
            if entry_owner(&entry) == user {
                return Ok(());
            }
            let have = entry.acl.get(user).copied().unwrap_or(0);
            if have & bits != bits {
                return Err(SqlError::new(
                    "42501",
                    format!("permission denied for table {table}"),
                ));
            }
            Ok(())
        }
        Err(e) if e.state == "42P01" => {
            // 非表名：视图命中 → 展开视图体递归（评审 P0：视图两步链）
            if let Some(qtext) = db.manifest().manifest.views.get(table) {
                let stmts = crate::sql::parse_batch(qtext, crate::sql::SqlDialect::Pg)?;
                let inner = stmts
                    .first()
                    .ok_or_else(|| SqlError::internal("empty view body"))?;
                for (t2, b2) in required_privs(inner) {
                    check_table_ref(db, sess, user, &t2, b2, depth + 1)?;
                }
                Ok(())
            } else {
                // 表不存在 → 放行至执行路径按其口径报错
                Ok(())
            }
        }
        Err(e) => Err(e), // 瞬态错误不吞（fail-closed）
    }
}

// ---------------------------------------------------------------------------
// GRANT / REVOKE 执行
// ---------------------------------------------------------------------------

fn privileges_bits(p: &sqlparser::ast::Privileges) -> Result<u8> {
    use sqlparser::ast::{Action, Privileges};
    match p {
        Privileges::All { .. } => Ok(PRIV_ALL),
        Privileges::Actions(actions) => {
            let mut bits = 0u8;
            for a in actions {
                bits |= match a {
                    Action::Select { columns } => col_guard(columns, PRIV_SELECT, "SELECT")?,
                    Action::Insert { columns } => col_guard(columns, PRIV_INSERT, "INSERT")?,
                    Action::Update { columns } => col_guard(columns, PRIV_UPDATE, "UPDATE")?,
                    Action::Delete => PRIV_DELETE,
                    other => return Err(SqlError::not_supported(format!("privilege {other:?}"))),
                };
            }
            Ok(bits)
        }
    }
}

/// 列级清单（GRANT SELECT (col)）v1 不支持——**诚实拒绝**，不静默展平为
/// 全表权限（评审 P1：过度授权比拒绝更糟）
fn col_guard(cols: &Option<Vec<sqlparser::ast::Ident>>, bit: u8, what: &str) -> Result<u8> {
    match cols {
        None => Ok(bit),
        Some(cs) if cs.is_empty() => Ok(bit),
        Some(_) => Err(SqlError::not_supported(format!(
            "column-level {what} privileges"
        ))),
    }
}

fn grantee_name(g: &Grantee) -> Result<String> {
    match &g.name {
        Some(sqlparser::ast::GranteeName::ObjectName(n)) => Ok(norm_user(
            &n.0.iter()
                .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
                .collect::<Vec<_>>()
                .join("."),
        )),
        _ => Err(SqlError::not_supported("grantee form")),
    }
}

fn grant_tables(objs: &Option<GrantObjects>) -> Result<Vec<String>> {
    match objs {
        Some(GrantObjects::Tables(names)) => Ok(names.iter().map(obj_name).collect()),
        _ => Err(SqlError::not_supported(
            "GRANT target: v1 supports TABLE only",
        )),
    }
}

/// owner/超户守卫（GRANT/REVOKE/ALTER/DROP 共用口径；调用方已滤超户）
fn require_owner(user: &str, entry: &crate::versioned::TableEntry, table: &str) -> Result<()> {
    if entry_owner(entry) == user {
        return Ok(());
    }
    Err(SqlError::new(
        "42501",
        format!("permission denied for table {table}"),
    ))
}

/// 读改写 + 提交整体持 commit_mu（评审 P1：原 resolve 在锁外，并发
/// GRANT 互相覆盖丢更新）。catalog_commit_locked 假定调用方持锁。
fn apply_grant_revoke(
    db: &Database,
    sess: &Session,
    tables: &[String],
    grantees: &[String],
    bits: u8,
    grant: bool,
    tag: &str,
) -> Result<()> {
    let b = db.branch(&sess.branch)?;
    let _g = b.commit_mu.lock();
    let user = norm_user(&sess.user);
    let mut changes = Vec::with_capacity(tables.len());
    for t in tables {
        let (_, mut entry) = scan::resolve_table(db, &sess.branch, t)?;
        require_owner(&user, &entry, t)?;
        for gr in grantees {
            if grant {
                *entry.acl.entry(gr.clone()).or_insert(0) |= bits;
            } else if let Some(v) = entry.acl.get_mut(gr) {
                *v &= !bits;
                if *v == 0 {
                    entry.acl.remove(gr);
                }
            }
        }
        changes.push((t.clone(), Some(entry)));
    }
    if !changes.is_empty() {
        crate::sql::ddl::catalog_commit_locked(db, sess, changes, tag, &_g)?;
    }
    Ok(())
}

pub(crate) fn exec_grant(
    db: &Database,
    sess: &Session,
    g: &sqlparser::ast::Grant,
) -> Result<Option<Output>> {
    if g.with_grant_option {
        return Err(SqlError::not_supported("WITH GRANT OPTION"));
    }
    let bits = privileges_bits(&g.privileges)?;
    let tables = grant_tables(&g.objects)?;
    let grantees: Vec<String> = g.grantees.iter().map(grantee_name).collect::<Result<_>>()?;
    apply_grant_revoke(db, sess, &tables, &grantees, bits, true, "GRANT")?;
    Ok(Some(Output::Command {
        tag: "GRANT".into(),
        affected: 0,
    }))
}

pub(crate) fn exec_revoke(
    db: &Database,
    sess: &Session,
    r: &sqlparser::ast::Revoke,
) -> Result<Option<Output>> {
    let bits = privileges_bits(&r.privileges)?;
    let tables = grant_tables(&r.objects)?;
    let grantees: Vec<String> = r.grantees.iter().map(grantee_name).collect::<Result<_>>()?;
    apply_grant_revoke(db, sess, &tables, &grantees, bits, false, "REVOKE")?;
    Ok(Some(Output::Command {
        tag: "REVOKE".into(),
        affected: 0,
    }))
}
