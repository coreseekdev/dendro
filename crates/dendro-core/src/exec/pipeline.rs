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

/// 装配层默认批大小（行路径物化行 → 批的切分粒度；方向2 Source
/// 流式化后由 Source 自产批，此常量成为其缺省批大小）
pub const ROW_BATCH: usize = 1024;

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

// ---------------------------------------------------------------------------
// v2c-2：算子群扩展——SortOp（breaker）+ ProjectOp + OpAsSink 链组合
// ---------------------------------------------------------------------------

/// 排序算子（**pipeline breaker**——D1 的 finish()-emits 形态）：
/// push 阶段缓冲全部行；finish() 排序后一次性推给下游。
/// 排序语义 = apply_order 的比较器逐字节（ASC null-last / DESC
/// null-first——reverse 连 null 位一起翻）；键提取由装配层完成
/// （键-行配对入缓冲），算子只管排与吐。
pub struct SortOp {
    /// (sort_key, row) 对；key 为装配层预提取的值向量（全量模式）
    buffered: Vec<(Vec<SqlValue>, Vec<SqlValue>)>,
    /// 每键的 ASC 标记（与 key 同长度）
    asc: Vec<bool>,
    /// O-5 top-N 模式（Some(n)：有界堆只保最终序前 n 行；None：全量）
    limit: Option<usize>,
    /// top-N 有界堆（BinaryHeap 是 max-heap——堆顶 = 最终序最大者 =
    /// 淘汰位；Ord 即最终排序键，seq 保证并列行的稳定序）
    top: std::collections::BinaryHeap<TopEntry>,
    /// 压入序（top-N 稳定序的平局裁决——与全量 sort_by（稳定排序）
    /// 的并列保序逐字节一致）
    seq: u64,
    /// finish 后是否已推（幂等守卫）
    done: bool,
}

/// top-N 堆条目：Ord = 最终排序键（键序 + 压入序平局裁决）
struct TopEntry {
    key: Vec<SqlValue>,
    seq: u64,
    row: Vec<SqlValue>,
    asc: std::sync::Arc<Vec<bool>>,
}

impl PartialEq for TopEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for TopEntry {}
impl PartialOrd for TopEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TopEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        for (i, &asc) in self.asc.iter().enumerate() {
            let x = &self.key[i];
            let y = &other.key[i];
            let ord = if x.is_null() && y.is_null() {
                std::cmp::Ordering::Equal
            } else if x.is_null() {
                std::cmp::Ordering::Greater // null-last（ASC）
            } else if y.is_null() {
                std::cmp::Ordering::Less
            } else {
                crate::sql::expr::cmp_values(x, y).unwrap_or(std::cmp::Ordering::Equal)
            };
            let ord = if asc { ord } else { ord.reverse() };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        self.seq.cmp(&other.seq)
    }
}

impl SortOp {
    pub fn new(asc: Vec<bool>) -> Self {
        Self {
            buffered: Vec::new(),
            asc,
            limit: None,
            top: std::collections::BinaryHeap::new(),
            seq: 0,
            done: false,
        }
    }

    /// O-5：top-N 模式——内存上界 n 行（ORDER BY + LIMIT：全量缓冲的
    /// 替代；结果与"全量排序取前 n"逐字节一致，含并列行的稳定序）
    pub fn with_limit(asc: Vec<bool>, n: usize) -> Self {
        Self {
            buffered: Vec::new(),
            asc,
            limit: Some(n),
            top: std::collections::BinaryHeap::new(),
            seq: 0,
            done: false,
        }
    }
}

impl PipeOp<Vec<Vec<SqlValue>>> for SortOp {
    fn push(
        &mut self,
        _cx: &mut PipeCtx,
        batch: Vec<Vec<SqlValue>>,
        _out: &mut dyn Sink<Vec<Vec<SqlValue>>>,
    ) -> FlowControl {
        // breaker：push 阶段只缓冲（键已在装配层配对——batch 每行
        // 前 asc.len() 个值为键，其余为数据；装配层合同）
        for mut row in batch {
            let klen = self.asc.len();
            let key: Vec<SqlValue> = row.drain(..klen).collect();
            match self.limit {
                None => self.buffered.push((key, row)),
                Some(n) => {
                    let e = TopEntry {
                        key,
                        seq: self.seq,
                        row,
                        asc: std::sync::Arc::new(self.asc.clone()),
                    };
                    self.seq += 1;
                    if self.top.len() < n {
                        self.top.push(e);
                    } else if self
                        .top
                        .peek()
                        .is_some_and(|worst| e.cmp(worst) == std::cmp::Ordering::Less)
                    {
                        self.top.pop();
                        self.top.push(e);
                    }
                }
            }
        }
        FlowControl::Continue
    }

    fn finish(&mut self, _cx: &mut PipeCtx, out: &mut dyn Sink<Vec<Vec<SqlValue>>>) -> FlowControl {
        if self.done {
            return FlowControl::Continue;
        }
        self.done = true;
        let rows: Vec<Vec<SqlValue>> = match self.limit {
            None => {
                self.buffered.sort_by(|(ka, _), (kb, _)| {
                    for (i, &asc) in self.asc.iter().enumerate() {
                        let x = &ka[i];
                        let y = &kb[i];
                        let ord = if x.is_null() && y.is_null() {
                            std::cmp::Ordering::Equal
                        } else if x.is_null() {
                            std::cmp::Ordering::Greater // null-last（ASC）
                        } else if y.is_null() {
                            std::cmp::Ordering::Less
                        } else {
                            crate::sql::expr::cmp_values(x, y).unwrap_or(std::cmp::Ordering::Equal)
                        };
                        let ord = if asc { ord } else { ord.reverse() };
                        if ord != std::cmp::Ordering::Equal {
                            return ord;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
                self.buffered.drain(..).map(|(_, r)| r).collect()
            }
            // top-N：堆内即最终序前 n（含稳定序平局裁决）——升序排出
            Some(_) => {
                let mut kept: Vec<TopEntry> = self.top.drain().collect();
                kept.sort();
                kept.into_iter().map(|e| e.row).collect()
            }
        };
        if rows.is_empty() {
            return FlowControl::Continue; // D3：零行不推
        }
        out.push(_cx, rows)
    }
}

/// 投影算子：每行按表达式列表求值产新行（行变换，非 breaker）
pub type ColResolver = Box<dyn Fn(&str) -> Option<usize> + Send>;

pub struct ProjectOp {
    pub exprs: Vec<sqlparser::ast::Expr>,
    /// 名字→列偏移（装配层闭包；行值经 eval 求值）
    pub cols: ColResolver,
}

impl PipeOp<Vec<Vec<SqlValue>>> for ProjectOp {
    fn push(
        &mut self,
        cx: &mut PipeCtx,
        batch: Vec<Vec<SqlValue>>,
        out: &mut dyn Sink<Vec<Vec<SqlValue>>>,
    ) -> FlowControl {
        let mut projected: Vec<Vec<SqlValue>> = Vec::with_capacity(batch.len());
        for row in &batch {
            let mut new_row = Vec::with_capacity(self.exprs.len());
            for e in &self.exprs {
                match crate::sql::expr::eval(e, row, &self.cols) {
                    Ok(v) => new_row.push(v),
                    Err(er) => return FlowControl::Err(er),
                }
            }
            projected.push(new_row);
        }
        let _ = cx;
        if projected.is_empty() {
            return FlowControl::Continue;
        }
        out.push(cx, projected)
    }
}

/// OpAsSink：把下游算子包装为上游的 Sink（链组合的适配器）。
/// 上游 push → 转发给被包装算子的 push，其输出再推给真正的终端 Sink。
/// **限制**（v1）：单层适配（A→B→terminal），多层需嵌套——借用检查
/// 使嵌套复杂化，v1 链深 ≤2 够用（Filter→Sort→Collect）。
pub struct OpAsSink<'a> {
    pub op: &'a mut dyn PipeOp<Vec<Vec<SqlValue>>>,
    pub terminal: &'a mut dyn Sink<Vec<Vec<SqlValue>>>,
}

impl Sink<Vec<Vec<SqlValue>>> for OpAsSink<'_> {
    fn push(&mut self, cx: &mut PipeCtx, batch: Vec<Vec<SqlValue>>) -> FlowControl {
        self.op.push(cx, batch, self.terminal)
    }
}

/// 链驱动：source → op1 → op2 → sink（两算子链；OpAsSink 适配中间层）。
/// EOS 后按装配序 finish（D1：breaker 算子的吐出时机）。
pub fn drive_pair(
    cx: &mut PipeCtx,
    source: &mut dyn Iterator<Item = Result<Vec<Vec<SqlValue>>>>,
    op1: &mut dyn PipeOp<Vec<Vec<SqlValue>>>,
    op2: &mut dyn PipeOp<Vec<Vec<SqlValue>>>,
    sink: &mut dyn Sink<Vec<Vec<SqlValue>>>,
) -> Result<()> {
    // 取前判停（同 drive——LIMIT 满员后不再拉）
    loop {
        if cx.stopped {
            break;
        }
        let batch = match source.next() {
            None => break,
            Some(item) => item?,
        };
        cx.checkpoint()?;
        if batch.is_empty() {
            continue;
        }
        let mut mid = OpAsSink {
            op: op2,
            terminal: sink,
        };
        match op1.push(cx, batch, &mut mid) {
            FlowControl::Err(e) => return Err(e),
            FlowControl::Stop => {
                cx.stop();
                break;
            }
            FlowControl::Continue => {}
        }
    }
    if !cx.stopped {
        // finish 按装配序：op1 先（其 finish 产出流入 op2 → sink）
        let mut mid = OpAsSink {
            op: op2,
            terminal: sink,
        };
        match op1.finish(cx, &mut mid) {
            FlowControl::Err(e) => return Err(e),
            FlowControl::Stop => cx.stop(),
            FlowControl::Continue => {}
        }
        if !cx.stopped {
            match op2.finish(cx, sink) {
                FlowControl::Err(e) => return Err(e),
                FlowControl::Stop => cx.stop(),
                FlowControl::Continue => {}
            }
        }
    }
    Ok(())
}

/// 算子群测试：Filter→Sort→Collect 链 vs 手写等价路径
#[cfg(test)]
mod operator_tests {
    use super::*;

    fn rows_from(pairs: &[(i64, Option<i64>)]) -> Vec<Vec<SqlValue>> {
        pairs
            .iter()
            .map(|(a, b)| {
                vec![
                    SqlValue::Int64(*a),
                    match b {
                        Some(v) => SqlValue::Int64(*v),
                        None => SqlValue::Null,
                    },
                ]
            })
            .collect()
    }

    #[test]
    fn sort_breaker_null_semantics() {
        // ASC：null-last；DESC：null-first（reverse 连 null 位翻）
        let data = rows_from(&[(1, Some(3)), (2, None), (3, Some(1)), (4, Some(2))]);
        let asc = vec![true];
        let mut sort = SortOp::new(asc.clone());
        let mut sink = CollectSink::new(None);
        // 装配层合同：每行前 asc.len() 个值为键——这里键=第 2 列
        let keyed: Vec<Vec<SqlValue>> = data
            .iter()
            .map(|r| vec![r[1].clone(), r[0].clone(), r[1].clone()])
            .collect();
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(keyed)];
        let mut it = src.into_iter();
        let mut cx = ctx();
        drive(&mut cx, &mut it, &mut sort, &mut sink).unwrap();
        // ASC null-last：[3,1],[4,2],[1,3],[2,NULL]
        let vals: Vec<&SqlValue> = sink.rows.iter().map(|r| &r[1]).collect();
        assert_eq!(
            vals,
            vec![
                &SqlValue::Int64(1),
                &SqlValue::Int64(2),
                &SqlValue::Int64(3),
                &SqlValue::Null,
            ],
            "ASC null-last：{:?}",
            sink.rows
        );

        // DESC null-first
        let mut sort2 = SortOp::new(vec![false]);
        let mut sink2 = CollectSink::new(None);
        let keyed2: Vec<Vec<SqlValue>> = data
            .iter()
            .map(|r| vec![r[1].clone(), r[0].clone(), r[1].clone()])
            .collect();
        let src2: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(keyed2)];
        let mut it2 = src2.into_iter();
        let mut cx2 = ctx();
        drive(&mut cx2, &mut it2, &mut sort2, &mut sink2).unwrap();
        let vals2: Vec<&SqlValue> = sink2.rows.iter().map(|r| &r[1]).collect();
        assert_eq!(
            vals2[0],
            &SqlValue::Null,
            "DESC null-first：{:?}",
            sink2.rows
        );
    }

    #[test]
    fn filter_sort_chain() {
        // Filter(>2) → Sort(DESC on key) → Collect —— drive_pair 链
        let data = rows_from(&[(1, Some(5)), (2, Some(1)), (3, Some(4)), (4, Some(2))]);
        let mut filter = FilterOp::new({
            let e = sqlparser::ast::Expr::BinaryOp {
                left: Box::new(sqlparser::ast::Expr::Identifier(
                    sqlparser::ast::Ident::new("k"),
                )),
                op: sqlparser::ast::BinaryOperator::Gt,
                right: Box::new(sqlparser::ast::Expr::Value(
                    sqlparser::ast::Value::Number("2".into(), false).into(),
                )),
            };
            let cols = |n: &str| (n == "k").then_some(2usize); // 键前置后 k 在索引 2
                                                               // n_cols 必须覆盖键列（verify 拒绝 Col idx ≥ n_cols 的装配——
                                                               // 原测试声明 2 与 cols 解析 2 矛盾，构造期校验接线后暴露）
            crate::sql::scalar::compile_predicate(&e, &cols, 3)
                .unwrap()
                .prog
        });
        let mut sort = SortOp::new(vec![false]); // DESC
        let mut sink = CollectSink::new(None);
        // 装配层：键前置（第 2 列为键），键后跟原行
        let keyed: Vec<Vec<SqlValue>> = data
            .iter()
            .map(|r| {
                let mut kr = vec![r[1].clone()];
                kr.extend(r.iter().cloned());
                kr
            })
            .collect();
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(keyed)];
        let mut it = src.into_iter();
        let mut cx = ctx();
        drive_pair(&mut cx, &mut it, &mut filter, &mut sort, &mut sink).unwrap();
        // >2 的 k：5,4 → DESC → 5,4；行值列（去掉键前缀后）= [1,5],[3,4]
        assert_eq!(sink.rows.len(), 2, "{:?}", sink.rows);
        assert_eq!(sink.rows[0][1], SqlValue::Int64(5)); // 排序后 [id, k]——k 在索引 1
        assert_eq!(sink.rows[1][1], SqlValue::Int64(4));
    }

    fn ctx() -> PipeCtx {
        PipeCtx::new(vec![], None, Arc::new(AtomicBool::new(false)))
    }
}

// ---------------------------------------------------------------------------
// v2c-2：AggOp（聚合算子——第三个 breaker，与 SortOp 同型）
// ---------------------------------------------------------------------------

/// 聚合算子：push 缓冲组哈希 + 累加器；finish() 按首见序吐组行。
/// 语义 = group_aggregate 逐字节（首见序、组键 to_text 哈希、NULL 跳过
/// 累加、无分组空输入全局聚合仍出一行、Accum 状态机）——**不含**投影/
/// HAVING（装配层职责，与 group_aggregate 调用方一致）。
pub struct AggOp {
    /// 组表达式列（列索引——由装配层从 group_exprs 解析）
    pub group_col_indices: Vec<usize>,
    /// 聚合调用
    pub calls: Vec<AggSpec>,
    /// 组状态：hashkey → 累加器行
    groups: std::collections::HashMap<Vec<String>, Vec<AggAccum>>,
    /// 首见序
    order: Vec<(Vec<String>, Vec<SqlValue>)>,
    done: bool,
}

/// 管线侧聚合调用描述（装配层从 AggCall 翻译）
#[derive(Clone, Debug)]
pub struct AggSpec {
    pub func: AggFunc,
    /// 参数列索引（None = count(*)）
    pub arg_col: Option<usize>,
    pub distinct: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// 累加器（= agg.rs Accum 的管线侧镜像——语义逐字节一致）。
/// v2c-3 语义合同（两路径共用）：count 只计数（任意类型，含文本——
/// 原 `count(文本列)` 因 as_i64 报错的缺陷在两侧同步修复）；
/// sum/avg 才数值化（非数值报 42804）；min/max 只比较；
/// DISTINCT 去重门在累加前（对所有函数生效，非仅 count）。
#[derive(Clone)]
pub struct AggAccum {
    func: AggFunc,
    distinct: bool,
    pub count: u64,
    pub sum_f: f64,
    pub sum_i: i64,
    pub is_float: bool,
    pub min: Option<SqlValue>,
    pub max: Option<SqlValue>,
    pub seen: Option<std::collections::HashSet<String>>,
}

impl AggAccum {
    fn new(func: AggFunc, distinct: bool) -> Self {
        Self {
            func,
            distinct,
            count: 0,
            sum_f: 0.0,
            sum_i: 0,
            is_float: false,
            min: None,
            max: None,
            seen: if distinct {
                Some(std::collections::HashSet::new())
            } else {
                None
            },
        }
    }

    fn push(&mut self, v: Option<SqlValue>) -> crate::error::Result<()> {
        match v {
            None => {
                self.count += 1; // count(*) 路径
                Ok(())
            }
            Some(val) => {
                if val.is_null() {
                    return Ok(()); // NULL 不参与聚合
                }
                if self.distinct {
                    // 带类型标签（评审 P1：to_text 跨类型碰撞——
                    // Int64(1) 与 Utf8("1") 判重合并）
                    let key = format!("{val:?}");
                    if !self.seen.as_mut().unwrap().insert(key) {
                        return Ok(()); // DISTINCT 重复
                    }
                }
                self.count += 1;
                if matches!(self.func, AggFunc::Sum | AggFunc::Avg) {
                    match &val {
                        SqlValue::Float64(f) => {
                            self.is_float = true;
                            self.sum_f += *f;
                        }
                        other => self.sum_i += crate::sql::expr::as_i64(other)?,
                    }
                }
                // 评审 P2：比较错误传播（原 unwrap_or(false) 吞 42804——
                // 混型列下行式报错、管线静默取首见值的路径分歧）
                if self.func == AggFunc::Min {
                    let less = match self.min.as_ref() {
                        None => true,
                        Some(m) => {
                            matches!(
                                crate::sql::expr::cmp_values(&val, m)?,
                                std::cmp::Ordering::Less
                            )
                        }
                    };
                    if less {
                        self.min = Some(val.clone());
                    }
                }
                if self.func == AggFunc::Max {
                    let greater = match self.max.as_ref() {
                        None => true,
                        Some(m) => matches!(
                            crate::sql::expr::cmp_values(&val, m)?,
                            std::cmp::Ordering::Greater
                        ),
                    };
                    if greater {
                        self.max = Some(val);
                    }
                }
                Ok(())
            }
        }
    }

    fn finish(&self) -> SqlValue {
        match self.func {
            AggFunc::Count => {
                let n = if self.distinct {
                    self.seen.as_ref().map(|s| s.len() as u64).unwrap_or(0)
                } else {
                    self.count
                };
                SqlValue::Int64(n as i64)
            }
            AggFunc::Sum => {
                if self.count == 0 {
                    SqlValue::Null
                } else if self.is_float {
                    SqlValue::Float64(self.sum_f + self.sum_i as f64)
                } else {
                    SqlValue::Int64(self.sum_i)
                }
            }
            AggFunc::Avg => {
                if self.count == 0 {
                    SqlValue::Null
                } else {
                    SqlValue::Float64((self.sum_f + self.sum_i as f64) / self.count as f64)
                }
            }
            AggFunc::Min => self.min.clone().unwrap_or(SqlValue::Null),
            AggFunc::Max => self.max.clone().unwrap_or(SqlValue::Null),
        }
    }
}

impl AggOp {
    pub fn new(group_col_indices: Vec<usize>, calls: Vec<AggSpec>) -> Self {
        Self {
            group_col_indices,
            calls,
            groups: std::collections::HashMap::new(),
            order: Vec::new(),
            done: false,
        }
    }
}

impl PipeOp<Vec<Vec<SqlValue>>> for AggOp {
    fn push(
        &mut self,
        _cx: &mut PipeCtx,
        batch: Vec<Vec<SqlValue>>,
        _out: &mut dyn Sink<Vec<Vec<SqlValue>>>,
    ) -> FlowControl {
        for row in &batch {
            let mut hashkey = Vec::with_capacity(self.group_col_indices.len());
            let mut keyvals = Vec::with_capacity(self.group_col_indices.len());
            for &gi in &self.group_col_indices {
                let v = row.get(gi).cloned().unwrap_or(SqlValue::Null);
                // 带类型标签（与 agg.rs group_aggregate 同口径——评审 P1）
                hashkey.push(format!("{v:?}"));
                keyvals.push(v);
            }
            let specs = self.calls.clone();
            let accums = self.groups.entry(hashkey.clone()).or_insert_with(|| {
                self.order.push((hashkey.clone(), keyvals.clone()));
                specs
                    .iter()
                    .map(|s| AggAccum::new(s.func, s.distinct))
                    .collect()
            });
            for (ai, spec) in self.calls.iter().enumerate() {
                let v = if let Some(col) = spec.arg_col {
                    row.get(col).cloned()
                } else {
                    None // count(*)
                };
                if let Err(e) = accums[ai].push(v) {
                    return FlowControl::Err(e);
                }
            }
        }
        FlowControl::Continue
    }

    fn finish(&mut self, _cx: &mut PipeCtx, out: &mut dyn Sink<Vec<Vec<SqlValue>>>) -> FlowControl {
        if self.done {
            return FlowControl::Continue;
        }
        self.done = true;

        let mut result_rows: Vec<Vec<SqlValue>> = Vec::with_capacity(self.order.len());
        for (hk, kv) in &self.order {
            if let Some(accums) = self.groups.get(hk) {
                let mut row = kv.clone(); // 组键值
                for a in accums {
                    row.push(a.finish());
                }
                result_rows.push(row);
            }
        }

        // **无分组 + 空输入 → 全局聚合仍出一行**（零值聚合——ir-spec 02
        // §1 算子合同，#23 修复后的唯一语义）
        if self.group_col_indices.is_empty() && result_rows.is_empty() {
            let row: Vec<SqlValue> = self
                .calls
                .iter()
                .map(|spec| AggAccum::new(spec.func, spec.distinct).finish())
                .collect();
            result_rows.push(row);
        }

        if result_rows.is_empty() {
            return FlowControl::Continue; // D3：零行不推
        }
        out.push(_cx, result_rows)
    }
}

/// AggOp 测试：分组/全局/空输入/DISTINCT/NULL 语义
#[cfg(test)]
mod agg_tests {
    use super::*;

    fn rows(pairs: &[(i64, i64)]) -> Vec<Vec<SqlValue>> {
        pairs
            .iter()
            .map(|(g, v)| vec![SqlValue::Int64(*g), SqlValue::Int64(*v)])
            .collect()
    }

    fn agg_ctx() -> PipeCtx {
        PipeCtx::new(vec![], None, Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn group_by_first_seen_order_and_agg() {
        // 组序 = 首见序（3 在 1 之前出现）
        let data = rows(&[(3, 30), (1, 10), (3, 40), (1, 20)]);
        let mut agg = AggOp::new(
            vec![0], // group by 列 0
            vec![AggSpec {
                func: AggFunc::Sum,
                arg_col: Some(1),
                distinct: false,
            }],
        );
        let mut sink = CollectSink::new(None);
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(data)];
        let mut it = src.into_iter();
        let mut cx = agg_ctx();
        drive(&mut cx, &mut it, &mut agg, &mut sink).unwrap();
        // 首见序：组 3 先出现
        assert_eq!(sink.rows.len(), 2, "{:?}", sink.rows);
        assert_eq!(sink.rows[0][0], SqlValue::Int64(3), "首见序");
        assert_eq!(sink.rows[0][1], SqlValue::Int64(70), "sum(30+40)");
        assert_eq!(sink.rows[1][0], SqlValue::Int64(1));
        assert_eq!(sink.rows[1][1], SqlValue::Int64(30), "sum(10+20)");
    }

    #[test]
    fn global_agg_empty_input_one_row() {
        // 无分组 + 空输入 → 单行零值聚合（#23 修复后的唯一语义）
        let mut agg = AggOp::new(
            vec![],
            vec![
                AggSpec {
                    func: AggFunc::Count,
                    arg_col: None,
                    distinct: false,
                },
                AggSpec {
                    func: AggFunc::Sum,
                    arg_col: Some(0),
                    distinct: false,
                },
            ],
        );
        let mut sink = CollectSink::new(None);
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![];
        let mut it = src.into_iter();
        let mut cx = agg_ctx();
        drive(&mut cx, &mut it, &mut agg, &mut sink).unwrap();
        assert_eq!(sink.rows.len(), 1, "空输入全局聚合单行：{:?}", sink.rows);
        assert_eq!(sink.rows[0][0], SqlValue::Int64(0), "count=0");
        assert_eq!(sink.rows[0][1], SqlValue::Null, "sum=NULL");
    }

    #[test]
    fn null_skips_aggregation() {
        let data = vec![
            vec![SqlValue::Int64(1), SqlValue::Null],
            vec![SqlValue::Int64(1), SqlValue::Int64(5)],
        ];
        let mut agg = AggOp::new(
            vec![0],
            vec![
                AggSpec {
                    func: AggFunc::Count,
                    arg_col: Some(1),
                    distinct: false,
                },
                AggSpec {
                    func: AggFunc::Sum,
                    arg_col: Some(1),
                    distinct: false,
                },
            ],
        );
        let mut sink = CollectSink::new(None);
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(data)];
        let mut it = src.into_iter();
        let mut cx = agg_ctx();
        drive(&mut cx, &mut it, &mut agg, &mut sink).unwrap();
        assert_eq!(sink.rows[0][1], SqlValue::Int64(1), "count 跳过 NULL");
        assert_eq!(sink.rows[0][2], SqlValue::Int64(5), "sum 跳过 NULL");
    }

    #[test]
    fn distinct_dedup() {
        let data = rows(&[(1, 10), (1, 10), (1, 20), (1, 10)]);
        let mut agg = AggOp::new(
            vec![0],
            vec![AggSpec {
                func: AggFunc::Count,
                arg_col: Some(1),
                distinct: true,
            }],
        );
        let mut sink = CollectSink::new(None);
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(data)];
        let mut it = src.into_iter();
        let mut cx = agg_ctx();
        drive(&mut cx, &mut it, &mut agg, &mut sink).unwrap();
        assert_eq!(
            sink.rows[0][1],
            SqlValue::Int64(2),
            "count DISTINCT: {:?}",
            sink.rows
        );
    }

    #[test]
    fn min_max_across_groups() {
        let data = rows(&[(2, 20), (1, 15), (2, 10), (1, 25)]);
        let mut agg = AggOp::new(
            vec![0],
            vec![
                AggSpec {
                    func: AggFunc::Min,
                    arg_col: Some(1),
                    distinct: false,
                },
                AggSpec {
                    func: AggFunc::Max,
                    arg_col: Some(1),
                    distinct: false,
                },
                AggSpec {
                    func: AggFunc::Avg,
                    arg_col: Some(1),
                    distinct: false,
                },
            ],
        );
        let mut sink = CollectSink::new(None);
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(data)];
        let mut it = src.into_iter();
        let mut cx = agg_ctx();
        drive(&mut cx, &mut it, &mut agg, &mut sink).unwrap();
        // 首见序：组 2 先
        assert_eq!(sink.rows[0][0], SqlValue::Int64(2));
        assert_eq!(sink.rows[0][1], SqlValue::Int64(10), "min");
        assert_eq!(sink.rows[0][2], SqlValue::Int64(20), "max");
        match &sink.rows[0][3] {
            SqlValue::Float64(f) => assert!((f - 15.0).abs() < 1e-9, "avg"),
            other => panic!("avg 应为 float：{other:?}"),
        }
    }
}

/// O-5 top-N：有界堆 vs 全量排序截断——**行序严格相等**（含并列行的
/// 压入序稳定；keyed 载荷形态与装配层合同一致：前 k 列键 + 数据）
#[cfg(test)]
mod topn_tests {
    use super::*;

    fn keyed(pairs: &[(i64, Option<i64>, &str)]) -> Vec<Vec<SqlValue>> {
        // (key, key2 nullable, data)
        pairs
            .iter()
            .map(|(k, k2, d)| {
                vec![
                    SqlValue::Int64(*k),
                    match k2 {
                        Some(v) => SqlValue::Int64(*v),
                        None => SqlValue::Null,
                    },
                    SqlValue::Utf8(d.to_string()),
                ]
            })
            .collect()
    }

    fn drive_sorted(
        op: &mut dyn PipeOp<Vec<Vec<SqlValue>>>,
        data: Vec<Vec<SqlValue>>,
    ) -> Vec<Vec<SqlValue>> {
        let mut sink = CollectSink::new(None);
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(data)];
        let mut it = src.into_iter();
        let mut cx = PipeCtx::new(
            vec![],
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        drive(&mut cx, &mut it, op, &mut sink).unwrap();
        sink.rows
    }

    #[test]
    fn topn_equals_full_sort_prefix_with_ties() {
        // 并列键大量出现（v=5 八行）——稳定序 = 压入序（a..h）
        let mut pairs: Vec<(i64, Option<i64>, &str)> = vec![
            (5, Some(1), "a"),
            (9, None, "b"),
            (5, Some(2), "c"),
            (1, Some(3), "d"),
            (5, None, "e"),
            (7, Some(4), "f"),
            (5, Some(5), "g"),
            (3, None, "h"),
            (5, Some(6), "i"),
            (2, Some(7), "j"),
        ];
        pairs.sort_by_key(|(k, _, _)| *k); // 与压入无关——压入序即上表序
        let data = keyed(&pairs);
        for asc2 in [true, false] {
            for n in [1usize, 3, 5, 10] {
                let mut top = SortOp::with_limit(vec![true, asc2], n);
                let out_top = drive_sorted(&mut top, data.clone());
                let mut full = SortOp::new(vec![true, asc2]);
                let mut out_full = drive_sorted(&mut full, data.clone());
                out_full.truncate(n);
                assert_eq!(
                    out_top, out_full,
                    "asc2={asc2} n={n}：top-N 必须与全量前缀逐字节一致"
                );
            }
        }
    }

    #[test]
    fn topn_zero_and_beyond_size() {
        let data = keyed(&[(3, Some(1), "x"), (1, Some(2), "y")]);
        // n=0：堆空 → 零行不推（D3）
        let mut t0 = SortOp::with_limit(vec![true], 0);
        assert!(drive_sorted(&mut t0, data.clone()).is_empty());
        // n 超行数：全量
        let mut t9 = SortOp::with_limit(vec![true], 9);
        assert_eq!(drive_sorted(&mut t9, data.clone()).len(), 2);
    }
}
