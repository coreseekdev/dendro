//! v2c-1b（C3/C5，ir-spec 04 §2）：push 管线协议。
//!
//! 设计（评审 D1/D2/D7 落地）：
//! - `FlowControl{Continue, Stop, Err}`：Stop=正常停止（LIMIT/取消），
//!   Err=语句失败（求值错误逐层上抛）；**二分不合并**；
//! - `PipeOp::finish()`：EOS 收尾（pipeline breaker——聚合/排序吐结果的
//!   唯一通道；DuckDB Sink→Combine→Finalize 同构）；
//! - 停止令牌随 Ctx 走（D2）：Stop 置位后 Source 不再产批；跨线程化后
//!   返回值传播失效，令牌是唯一通道（Polars SourceToken 同构的预留）；
//! - **载荷泛型 `T`**（工程裁量，08 §0.4 记录）：TP 行路径
//!   `T = Vec<Vec<SqlValue>>`（免 arrow 物化——85% 性能线纪律），AP 侧
//!   `T = Chunk`（04 §1 货币）；协议/算子/汇对 T 统一；
//! - 零行不变量（D3）：算子不产不收空批；驱动器跳过空批；
//! - metrics 挂点（D7）：驱动器按算子计数（v1 空实现，EXPLAIN ANALYZE
//!   预留位）。

use crate::error::{Result, SqlError};
use crate::sql::scalar::{eval_row, ScalarProgram};
use crate::types::SqlValue;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug)]
pub enum FlowControl {
    Continue,
    Stop,
    Err(SqlError),
}

/// 管线上下文：会话级守卫（deadline/cancel/停止令牌）+ 参数数组
/// （ScalarProgram 的 Param 步取值点）。
pub struct PipeCtx {
    pub params: Vec<SqlValue>,
    pub deadline: Option<Instant>,
    pub cancel: Arc<AtomicBool>,
    /// 停止令牌（D2）：任一算子/汇返回 Stop 时置位
    stopped: bool,
    /// metrics 挂点（D7）：每算子批数/行数（v1 收集，EXPLAIN ANALYZE 预留）
    pub rows_through: u64,
}

impl PipeCtx {
    pub fn new(params: Vec<SqlValue>, deadline: Option<Instant>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            params,
            deadline,
            cancel,
            stopped: false,
            rows_through: 0,
        }
    }
    /// 每批检查点（S-3 同源：57014 超时 / 取消令牌）
    fn checkpoint(&self) -> Result<()> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(SqlError::new("57014", "query canceled"));
        }
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                return Err(SqlError::new("57014", "statement timeout"));
            }
        }
        Ok(())
    }
    fn stop(&mut self) {
        self.stopped = true;
    }
}

pub trait Sink<T> {
    fn push(&mut self, cx: &mut PipeCtx, batch: T) -> FlowControl;
}

pub trait PipeOp<T> {
    fn push(&mut self, cx: &mut PipeCtx, batch: T, out: &mut dyn Sink<T>) -> FlowControl;
    /// EOS 收尾（D1：breaker 算子在此吐结果；默认无收尾）
    fn finish(&mut self, _cx: &mut PipeCtx, _out: &mut dyn Sink<T>) -> FlowControl {
        FlowControl::Continue
    }
}

/// 驱动器：Source（批迭代器）→ 算子 → Sink；EOS 后 finish。
/// **v1 链深 1**（Filter）：多算子链的顺序组合（OpAsSink 适配）随真实
/// 算子群（v2c-2：Project/Agg/Sort）到来——不提前假装支持深链语义。
/// 零行批在驱动器跳过（D3：算子无需处理空批）。
pub trait BatchEmpty {
    fn batch_is_empty(&self) -> bool;
}
impl BatchEmpty for Vec<Vec<SqlValue>> {
    fn batch_is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

pub fn drive<T: BatchEmpty>(
    cx: &mut PipeCtx,
    source: &mut dyn Iterator<Item = Result<T>>,
    op: &mut dyn PipeOp<T>,
    sink: &mut dyn Sink<T>,
) -> Result<()> {
    // 取批**之前**判停（LIMIT 满员后不再拉 Source——D2 的停拉语义；
    // for-in 会先取再判，故手写 loop）
    loop {
        if cx.stopped {
            break;
        }
        let batch = match source.next() {
            None => break,
            Some(item) => item?,
        };
        cx.checkpoint()?;
        if batch.batch_is_empty() {
            continue;
        }
        match op.push(cx, batch, sink) {
            FlowControl::Err(e) => return Err(e),
            FlowControl::Stop => {
                cx.stop();
                break;
            }
            FlowControl::Continue => {}
        }
    }
    if !cx.stopped {
        if let FlowControl::Err(e) = op.finish(cx, sink) {
            return Err(e);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 算子与汇（v2c-1b 行路径实例）
// ---------------------------------------------------------------------------

/// 谓词过滤（C5：eval_chunk v1 = 行循环调 eval_row——先正确后向量化；
/// 列式直线段（含常量提升）是 03 §5 v2 清单）
pub struct FilterOp {
    pub prog: ScalarProgram,
}

impl FilterOp {
    pub fn new(prog: ScalarProgram) -> Self {
        Self { prog }
    }
}

impl PipeOp<Vec<Vec<SqlValue>>> for FilterOp {
    fn push(
        &mut self,
        cx: &mut PipeCtx,
        batch: Vec<Vec<SqlValue>>,
        out: &mut dyn Sink<Vec<Vec<SqlValue>>>,
    ) -> FlowControl {
        let mut kept: Vec<Vec<SqlValue>> = Vec::with_capacity(batch.len());
        for row in batch {
            let mut v = SqlValue::Null;
            match eval_row(&self.prog, &row, &cx.params, &mut v) {
                Ok(_) => {
                    if matches!(v, SqlValue::Bool(true)) {
                        kept.push(row);
                    }
                    // 谓词终结：NULL/false 丢行（Qual 语义，B1 差分固化）
                }
                Err(e) => return FlowControl::Err(e),
            }
        }
        if kept.is_empty() {
            return FlowControl::Continue; // 零行不推（D3）
        }
        cx.rows_through += kept.len() as u64;
        out.push(cx, kept)
    }
}

/// 行收集汇：物化回 TableView 行（unwind 边界——下游行算子的接口）
#[derive(Default)]
pub struct CollectSink {
    pub rows: Vec<Vec<SqlValue>>,
    /// 上限（LIMIT 语义在汇端：满员即 Stop——短路的实现点，04 §2）
    pub limit: Option<usize>,
}

impl CollectSink {
    pub fn new(limit: Option<usize>) -> Self {
        Self {
            rows: Vec::new(),
            limit,
        }
    }
}

impl Sink<Vec<Vec<SqlValue>>> for CollectSink {
    fn push(&mut self, _cx: &mut PipeCtx, batch: Vec<Vec<SqlValue>>) -> FlowControl {
        self.rows.extend(batch);
        if let Some(l) = self.limit {
            if self.rows.len() >= l {
                // LIMIT 截断到精确值再停（与引擎 pushdown_limit 截断语义一致）
                self.rows.truncate(l);
                return FlowControl::Stop; // 满员：驱动器停拉
            }
        }
        FlowControl::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::scalar::compile_predicate;
    use sqlparser::ast::{BinaryOperator as BO, Expr, Ident, Value as PV};

    fn ctx() -> PipeCtx {
        PipeCtx::new(vec![], None, Arc::new(AtomicBool::new(false)))
    }
    fn rows(vs: &[i64]) -> Vec<Vec<SqlValue>> {
        vs.iter().map(|v| vec![SqlValue::Int64(*v)]).collect()
    }
    fn pred_gt(n: i64) -> ScalarProgram {
        let e = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("x"))),
            op: BO::Gt,
            right: Box::new(Expr::Value(PV::Number(n.to_string(), false).into())),
        };
        let cols = |name: &str| (name == "x").then_some(0usize);
        compile_predicate(&e, &cols, 1).unwrap().prog
    }

    #[test]
    fn filter_collect_and_limit_stop() {
        let prog = pred_gt(2);
        let mut op = FilterOp::new(prog);
        let mut sink = CollectSink::new(Some(2)); // LIMIT 2
        let batches = vec![rows(&[1, 2, 3, 4, 5]), rows(&[6, 7])];
        let batches2: Vec<Result<Vec<Vec<SqlValue>>>> = batches.into_iter().map(Ok).collect();
        let mut it = batches2.into_iter();
        let mut cx = ctx();
        drive(&mut cx, &mut it, &mut op, &mut sink).unwrap();
        // LIMIT 2 满员 Stop：恰好 2 行（3 与 4；第二批不再拉取）
        assert_eq!(sink.rows.len(), 2, "LIMIT 短路");
        assert_eq!(sink.rows[0][0], SqlValue::Int64(3));
    }

    #[test]
    fn zero_row_batch_skipped_and_err_propagates() {
        // 空批不达算子；除零错误中途 → Err 上抛
        let prog = {
            // x/0 > 0
            let e = Expr::BinaryOp {
                left: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::Identifier(Ident::new("x"))),
                    op: BO::Divide,
                    right: Box::new(Expr::Value(PV::Number("0".into(), false).into())),
                }),
                op: BO::Gt,
                right: Box::new(Expr::Value(PV::Number("0".into(), false).into())),
            };
            let cols = |n: &str| (n == "x").then_some(0usize);
            compile_predicate(&e, &cols, 1).unwrap().prog
        };
        let mut op = FilterOp::new(prog);
        let mut sink = CollectSink::new(None);
        let data = vec![Ok(rows(&[])), Ok(rows(&[1]))]; // 空批 + 触发除零
        let mut it = data.into_iter();
        let mut cx = ctx();
        let r = drive(&mut cx, &mut it, &mut op, &mut sink);
        assert!(r.is_err(), "除零必须沿管线失败");
        assert!(sink.rows.is_empty());
    }

    #[test]
    fn cancel_token_stops_pipeline() {
        let prog = pred_gt(0);
        let mut op = FilterOp::new(prog);
        let mut sink = CollectSink::new(None);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cx = PipeCtx::new(vec![], None, cancel.clone());
        let mut produced = 0usize;
        let mut src = std::iter::from_fn(move || {
            produced += 1;
            if produced > 100 {
                None
            } else {
                Some(Ok(rows(&[1, 2, 3])))
            }
        });
        // 先取消再驱动：第一批检查点即 57014
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = drive(&mut cx, &mut src, &mut op, &mut sink);
        assert!(r.is_err());
        assert_eq!(r.err().unwrap().state, "57014");
    }
}
