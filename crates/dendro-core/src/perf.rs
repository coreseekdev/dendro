//! SQL 统一管线的分阶段性能分析框架。
//!
//! 设计要点（回应架构批评：快路径必须在统一 SQL 框架内，而非旁路 API）：
//! - **所有语句自动计量**：Session::exec 的热路径按阶段打点（M-5
//!   延迟计数器同型：count_us/sum_us/count 三原子——每语句每阶段
//!   ~20ns 开销，常开可接受）
//! - **可归因**：Parse / PlanCache / Resolve(catalog) / Exec 总量 /
//!   Storage(树/memtx 点取) —— 29µs 之谜的拆解面
//! - **统一表面**：`cambium.perf_stages` SQL 表（与 memory_usage 同
//!   模式）——任何负载（bench/真实）跑完即可查询归因，无需专门工具
//! - 与前端分析框架（dendro ir/unparse/compare）互补：那边管"解析
//!   对不对"，这边管"每一阶段多贵"
//!
//! 口径：墙钟含调度噪声——count 足够大时 avg 稳定；单次读数仅作序。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 管线阶段（固定枚举——零分配打点）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// split + tokenize + parse（plan cache miss 才发生）
    Parse,
    /// 计划缓存查找（hash + shard lock）
    PlanCache,
    /// catalog 解析（resolve_table：版本树走查 + schema 载入）
    Resolve,
    /// 语句执行总量（exec_statement——含全部子阶段）
    Exec,
    /// 求值总量（eval_query：计划构建/优化/扫描/投影）
    Eval,
    /// 扫描总量（table_scan/try_ap_scan——含段解码）
    Scan,
    /// 存储点取（prolly::lookup + memtx get——树路径成本）
    StorageGet,
    /// 输出构造（TableView → Output::Rows/RecordSet 编码）
    OutputBuild,
    /// 语句切分（split_statements——引号/注释感知扫描）
    BatchSplit,
    /// embed 输出转换（Output → QueryResult 行物化）
    ToResult,
    /// 计划构建（build_plan：AST → Plan IR）
    BuildPlan,
    /// 优化链（rewrite_* 六条 + 掩码计算）
    Optimize,
}

pub const STAGES: &[Stage] = &[
    Stage::Parse,
    Stage::PlanCache,
    Stage::Resolve,
    Stage::Exec,
    Stage::Eval,
    Stage::Scan,
    Stage::StorageGet,
    Stage::OutputBuild,
    Stage::BatchSplit,
    Stage::ToResult,
    Stage::BuildPlan,
    Stage::Optimize,
];

impl Stage {
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Parse => "parse",
            Stage::PlanCache => "plan_cache",
            Stage::Resolve => "resolve",
            Stage::Exec => "exec",
            Stage::Eval => "eval",
            Stage::Scan => "scan",
            Stage::StorageGet => "storage_get",
            Stage::OutputBuild => "output_build",
            Stage::BatchSplit => "batch_split",
            Stage::ToResult => "to_result",
            Stage::BuildPlan => "build_plan",
            Stage::Optimize => "optimize",
        }
    }
}

struct Counters {
    count: AtomicU64,
    sum_ns: AtomicU64,
}

static ENABLED: AtomicBool = AtomicBool::new(true);
static COUNTERS: [Counters; 12] = [
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
    Counters { count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) },
];

fn idx(s: Stage) -> usize {
    match s {
        Stage::Parse => 0,
        Stage::PlanCache => 1,
        Stage::Resolve => 2,
        Stage::Exec => 3,
        Stage::Eval => 4,
        Stage::Scan => 5,
        Stage::StorageGet => 6,
        Stage::OutputBuild => 7,
        Stage::BatchSplit => 8,
        Stage::ToResult => 9,
        Stage::BuildPlan => 10,
        Stage::Optimize => 11,
    }
}

/// 打点（手动对）：`let t = enter(Stage::X); ...; exit(Stage::X, t)`
pub fn enter(s: Stage) -> std::time::Instant {
    if !ENABLED.load(Ordering::Relaxed) {
        // 常关时 Instant::now 仍计（~20ns）——简化；需要真零开销时再优化
    }
    std::time::Instant::now()
}

pub fn exit(s: Stage, t: std::time::Instant) {
    let ns = t.elapsed().as_nanos() as u64;
    let c = &COUNTERS[idx(s)];
    c.count.fetch_add(1, Ordering::Relaxed);
    c.sum_ns.fetch_add(ns, Ordering::Relaxed);
}

/// RAII 打点 guard
pub struct Guard(std::time::Instant, Stage);
pub fn scope(s: Stage) -> Guard {
    Guard(std::time::Instant::now(), s)
}
impl Drop for Guard {
    fn drop(&mut self) {
        exit(self.1, self.0);
    }
}

/// 阶段报告行
#[derive(Debug, Clone, PartialEq)]
pub struct StageReport {
    pub name: &'static str,
    pub count: u64,
    pub avg_ns: u64,
    pub total_ms: f64,
}

/// 全部阶段报告（快照）
pub fn report() -> Vec<StageReport> {
    STAGES
        .iter()
        .map(|s| {
            let c = &COUNTERS[idx(*s)];
            let n = c.count.load(Ordering::Relaxed);
            let ns = c.sum_ns.load(Ordering::Relaxed);
            StageReport {
                name: s.name(),
                count: n,
                avg_ns: if n > 0 { ns / n } else { 0 },
                total_ms: ns as f64 / 1e6,
            }
        })
        .collect()
}

/// 清零（基准臂间复位）
pub fn reset() {
    for c in COUNTERS.iter() {
        c.count.store(0, Ordering::Relaxed);
        c.sum_ns.store(0, Ordering::Relaxed);
    }
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        // 并行测试共享全局计数器——增量口径（前后差）而非绝对值
        let before = report().iter().find(|x| x.name == "parse").unwrap().count;
        {
            let _g = scope(Stage::Parse);
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        {
            let _g = scope(Stage::Parse);
        }
        let r = report();
        let p = r.iter().find(|x| x.name == "parse").unwrap();
        assert_eq!(p.count, before + 2);
        // 并行干扰下 avg 被兄弟打点稀释——只断言本测试贡献的总量增长
        assert!(
            p.total_ms >= before as f64 * 0.0 + 0.1,
            "sleep 100µs 应计入 total：{p:?}"
        );
    }
}
