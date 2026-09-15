//! S-4（方向4）：GRANT/REVOKE 与语句权限门（PG 兼容子集，表级粒度）。
//!
//! 模型（v1）：
//! - **角色 = 用户名**（不区分 role/user；pgwire startup `user` 参数 /
//!   embed `set_user`）；引导超户 `dendro`（embed/pgwire 缺省——缺省
//!   行为不变：全部放行，既有测试不受影响）；
//! - **ACL 存储**：TableEntry.acl（serde default 空——旧 manifest 反序列
//!   化兼容），role → 权限位；owner 字段（"" = 引导期建表 = 超户所有）；
//! - **表级权限位**：SELECT/INSERT/UPDATE/DELETE（列级/其他对象类型
//!   v1 不做，not_supported 诚实拒绝）；
//! - **单点门**：exec_statement 在 Q-10 检查后调用 enforce——语句 AST
//!   静态走查表引用与所需位（散点检查的遗漏面大，单点是唯一守卫）；
//! - GRANT/REVOKE 本身 = catalog 写（owner 或超户可授；无 WITH GRANT
//!   OPTION 传递，v1 记 not_supported）。

use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use crate::sql::scan;
use crate::types::Output;
use sqlparser::ast::{Grantee, GrantObjects, Query, SetExpr, Statement, TableFactor};

pub const PRIV_SELECT: u8 = 1;
pub const PRIV_INSERT: u8 = 2;
pub const PRIV_UPDATE: u8 = 4;
pub const PRIV_DELETE: u8 = 8;
pub const PRIV_ALL: u8 = PRIV_SELECT | PRIV_INSERT | PRIV_UPDATE | PRIV_DELETE;

/// 引导超户（embed/pgwire 缺省 user；owner 为空的旧表视为它所有）
pub const SUPERUSER: &str = "dendro";

fn is_superuser(sess: &Session) -> bool {
    sess.user == SUPERUSER
}

fn entry_owner(entry: &crate::versioned::TableEntry) -> &str {
    if entry.owner.is_empty() {
        SUPERUSER
    } else {
        &entry.owner
    }
}

/// 语句 → [(表名, 所需权限位)]。表引用走查：FROM/JOIN（含集合操作两侧）。
pub(crate) fn required_privs(stmt: &Statement) -> Vec<(String, u8)> {
    let mut out = Vec::new();
    match stmt {
        Statement::Query(q) => query_tables(q, PRIV_SELECT, &mut out),
        Statement::Insert(ins) => {
            if let sqlparser::ast::TableObject::TableName(name) = &ins.table {
                out.push((obj_name(name), PRIV_INSERT));
            }
            // INSERT ... SELECT：源查询仍需 SELECT
            if let Some(src) = &ins.source {
                query_tables(src, PRIV_SELECT, &mut out);
            }
        }
        Statement::Update(upd) => {
            factor_tables(&upd.table, PRIV_UPDATE, &mut out);
        }
        Statement::Delete(del) => {
            for t in &del.tables {
                out.push((obj_name(t), PRIV_DELETE));
            }
            // DELETE FROM / USING 形态（tables 为空时 from 是主目标）
            match &del.from {
                sqlparser::ast::FromTable::WithFromKeyword(from)
                | sqlparser::ast::FromTable::WithoutKeyword(from) => {
                    for twj in from {
                        factor_tables(twj, PRIV_DELETE, &mut out);
                    }
                }
            }
        }
        Statement::Truncate(tr) => {
            for t in &tr.table_names {
                out.push((obj_name(&t.name), PRIV_DELETE));
            }
        }
        _ => {}
    }
    out
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
    if let TableFactor::Table { name, .. } = tf {
        out.push((obj_name(name), bits));
    }
}

/// 单点权限门（exec_statement 调用）：超户直通；owner 直通；否则 ACL
/// 位全含才放行。表不存在 → 底层执行路径的报错口径（不在权限门重复）。
pub(crate) fn enforce(db: &Database, sess: &Session, stmt: &Statement) -> Result<()> {
    if is_superuser(sess) {
        return Ok(());
    }
    for (table, bits) in required_privs(stmt) {
        if let Ok((_, entry)) = scan::resolve_table(db, &sess.branch, &table) {
            if entry_owner(&entry) == sess.user {
                continue;
            }
            let have = entry.acl.get(&sess.user).copied().unwrap_or(0);
            if have & bits != bits {
                return Err(SqlError::new(
                    "42501",
                    format!("permission denied for table {table}"),
                ));
            }
        }
        // resolve 失败（表不存在）→ 放行到执行路径按其口径报错
    }
    Ok(())
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
                    Action::Select { .. } => PRIV_SELECT,
                    Action::Insert { .. } => PRIV_INSERT,
                    Action::Update { .. } => PRIV_UPDATE,
                    Action::Delete => PRIV_DELETE,
                    other => {
                        return Err(SqlError::not_supported(format!(
                            "privilege {other:?}"
                        )))
                    }
                };
            }
            Ok(bits)
        }
    }
}

fn grantee_name(g: &Grantee) -> Result<String> {
    match &g.name {
        Some(sqlparser::ast::GranteeName::ObjectName(n)) => {
            Ok(n.0
                .iter()
                .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
                .collect::<Vec<_>>()
                .join(".")
                .to_ascii_lowercase())
        }
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

/// owner/超户守卫（GRANT/REVOKE/ALTER/DROP 共用口径）
fn require_owner(sess: &Session, entry: &crate::versioned::TableEntry, table: &str) -> Result<()> {
    if is_superuser(sess) || entry_owner(entry) == sess.user {
        return Ok(());
    }
    Err(SqlError::new(
        "42501",
        format!("permission denied for table {table}"),
    ))
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
    let mut changes = Vec::with_capacity(tables.len());
    for t in &tables {
        let (_, mut entry) = scan::resolve_table(db, &sess.branch, t)?;
        require_owner(sess, &entry, t)?;
        for gr in &grantees {
            *entry.acl.entry(gr.clone()).or_insert(0) |= bits;
        }
        changes.push((t.clone(), Some(entry)));
    }
    if !changes.is_empty() {
        crate::sql::ddl::catalog_commit(db, sess, changes, "GRANT")?;
    }
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
    let mut changes = Vec::with_capacity(tables.len());
    for t in &tables {
        let (_, mut entry) = scan::resolve_table(db, &sess.branch, t)?;
        require_owner(sess, &entry, t)?;
        for gr in &grantees {
            if let Some(v) = entry.acl.get_mut(gr) {
                *v &= !bits;
                if *v == 0 {
                    entry.acl.remove(gr);
                }
            }
        }
        changes.push((t.clone(), Some(entry)));
    }
    if !changes.is_empty() {
        crate::sql::ddl::catalog_commit(db, sess, changes, "REVOKE")?;
    }
    Ok(Some(Output::Command {
        tag: "REVOKE".into(),
        affected: 0,
    }))
}
