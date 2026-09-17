#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! 伪表：branches/tables/columns/commit_log。

use super::*;

use super::agg::{self, AggCall};
use super::expr;
use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use crate::format::row::decode_row;
use crate::types::{ColType, ColumnMeta, Output, RecordSet, SqlValue};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, GroupByExpr, JoinOperator, ObjectName, OrderByExpr, Query,
    Select, SelectItem, SetExpr, TableFactor, Value as PV,
};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) fn pseudo_branches(db: &Database) -> Result<TableView> {
    let names = vec!["branch".into(), "commit".into(), "parent".into()];
    let mut rows = Vec::new();
    for (n, h) in db.manifest().manifest.refs.clone() {
        rows.push(vec![
            SqlValue::Utf8(n),
            SqlValue::Utf8(h.commit.unwrap_or_default()),
            h.parent.map(SqlValue::Utf8).unwrap_or(SqlValue::Null),
        ]);
    }
    Ok(TableView { names, rows })
}

pub(crate) fn pseudo_tables(db: &Database) -> Result<TableView> {
    let names = vec!["table_schema".into(), "table_name".into()];
    let mut rows = Vec::new();
    let branch = db.branch("main")?;
    let _ = branch;
    for sess_name in ["main"] {
        let b = db.branch(sess_name)?;
        let head = b.head.load_full();
        let catalog = crate::versioned::Versioned::new(db.store.clone());
        for (n, _) in catalog.catalog_entries(head.as_ref().as_ref().map(|c| c.root).as_ref())? {
            rows.push(vec![SqlValue::Utf8("public".into()), SqlValue::Utf8(n)]);
        }
    }
    Ok(TableView { names, rows })
}

pub(crate) fn pseudo_columns(db: &Database, sess: &mut Session) -> Result<TableView> {
    let names = vec![
        "table_schema".into(),
        "table_name".into(),
        "column_name".into(),
        "data_type".into(),
    ];
    let mut rows = Vec::new();
    let b = db.branch("main")?;
    let head = b.head.load_full();
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    for (n, _) in catalog.catalog_entries(head.as_ref().as_ref().map(|c| c.root).as_ref())? {
        let (schema, _) = resolve_table(db, &sess.branch, &n)?;
        for c in schema.columns {
            rows.push(vec![
                SqlValue::Utf8("public".into()),
                SqlValue::Utf8(n.clone()),
                SqlValue::Utf8(c.name),
                SqlValue::Utf8(c.ty.type_name().into()),
            ]);
        }
    }
    Ok(TableView { names, rows })
}

pub(crate) fn pseudo_commit_log(db: &Database, sess: &mut Session) -> Result<TableView> {
    let names = vec![
        "commit".into(),
        "height".into(),
        "ts_ms".into(),
        "message".into(),
    ];
    let mut rows = Vec::new();
    let b = db.branch(&sess.branch)?;
    let mut cur = b.head.load_full();
    let mut guard = 0;
    while let Some(c) = cur.as_ref() {
        rows.push(vec![
            SqlValue::Utf8(c.addr().to_base32()),
            SqlValue::Int64(c.height as i64),
            SqlValue::TimestampMs(c.ts_ms),
            SqlValue::Utf8(c.message.clone()),
        ]);
        guard += 1;
        if guard > 1000 || c.parents.is_empty() {
            break;
        }
        let (_ty, data) = db.cas.get(&c.parents[0])?;
        cur = Arc::new(Some(crate::versioned::commit::Commit::decode(&data)?));
    }
    Ok(TableView { names, rows })
}

// ---------- 输出 ----------
