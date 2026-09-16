#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! 递归 CTE：不动点迭代与 VALUES 注入。

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

/// 递归 CTE 迭代/行数上限（AST 注入路径与计划 IterativeScan 共用——
/// 到顶必须报错，绝不静默截断返回部分行）
pub(crate) const RECURSIVE_MAX_ITER: usize = 200;
pub(crate) const RECURSIVE_MAX_ROWS: usize = 1000;

// 阶段5：AST 注入路径（eval_recursive_cte + VALUES 字面量注入 +
// replace_rec_ref 全量重建）退役——递归 CTE 唯一路径 =
// Plan::IterativeScan（delta 工作集不动点，scan/plan_exec.rs）。
// 上限常量保留为两路径共用的发散防护（现仅 IterativeScan 消费）。
