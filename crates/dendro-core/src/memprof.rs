//! 内存计量与持续监控（memory profiling / monitoring / analysis）。
//!
//! 参考 tree-sitter generate-mem-prof（阶段 time+RSS + 节流结构普查）
//! 的经验，按内存型 TP 库的需求升级为**常开、分用途、可查询**的三层：
//!
//! 1. **计量层（meters）**：全局注册表 `Registry`，命名计量器 =
//!    AtomicU64 字节 + AtomicU64 条目数。热路径更新是relaxed 原子加
//!    （纳秒级），RAII guard 支持作用域追踪（如列存扫描的活动批）。
//!    零开销可关：`DENDRO_MEMPROF=off` 时 guard 仍安全（注册表
//!    常在，仅采样器停）。
//! 2. **包络层（envelope）**：进程 RSS/VmHWM 作为真值——计量和与
//!    包络的差 = `unattributed`（未归因堆/碎片/第三方），这是计量
//!    完整性的持续自检。
//! 3. **采样层（sampler）**：后台线程按可配间隔拍快照进环形窗口
//!    （默认保留 600 帧）；维护**峰值归因**（RSS 峰值时刻各 meter
//!    的贡献）与每 meter 的历史峰值——事后分析不需要预先在场。
//!
//! 表面：`cambium.memory_usage`（SQL 自省表）/ `/metrics`（HTTP）/
//! `Connection::memory_snapshot_json`（embed）。
//!
//! 诚实边界：缓存类 meter 上报**条目数**（精确）与注册方提供的
//! 字节估计（标注 est）；memtx/扫描缓冲上报精确字节。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 单个命名计量器（字节 + 条目；条目可为缓存条数/行数/批数）
pub struct Meter {
    pub name: &'static str,
    pub bytes: AtomicU64,
    pub items: AtomicU64,
    /// bytes 是否为估计值（缓存条目估算是 est；memtx/缓冲是精确）
    pub estimated: bool,
    pub desc: &'static str,
}

impl Meter {
    const fn new(name: &'static str, estimated: bool, desc: &'static str) -> Self {
        Meter {
            name,
            bytes: AtomicU64::new(0),
            items: AtomicU64::new(0),
            estimated,
            desc,
        }
    }
}

/// 进程级注册表（静态；各子系统启动时注册一次）
pub struct Registry {
    meters: Mutex<Vec<&'static Meter>>,
    /// 采样窗口（环形；含峰值归因）
    history: Mutex<Vec<Snapshot>>,
    /// 窗口容量
    capacity: usize,
    enabled: AtomicBool,
}

impl Registry {
    fn new() -> Self {
        Registry {
            meters: Mutex::new(Vec::new()),
            history: Mutex::new(Vec::new()),
            capacity: 600,
            enabled: AtomicBool::new(true),
        }
    }

    /// 注册（幂等：同名只注册一次——返回既有引用）
    pub fn register(name: &'static str, estimated: bool, desc: &'static str) -> &'static Meter {
        let reg = get();
        {
            let g = reg.meters.lock().unwrap();
            if g.iter().any(|m| m.name == name) {
                return g.iter().find(|m| m.name == name).unwrap();
            }
        }
        let m: &'static Meter = Box::leak(Box::new(Meter::new(name, estimated, desc)));
        reg.meters.lock().unwrap().push(m);
        m
    }

    /// 当前快照（含包络与未归因差）
    pub fn snapshot(&self) -> Snapshot {
        let rss = crate::engine::proc_rss_bytes().unwrap_or(0);
        let hwm = proc_hwm_bytes().unwrap_or(0);
        let mut meters: Vec<MeterSample> = self
            .meters
            .lock()
            .unwrap()
            .iter()
            .map(|m| MeterSample {
                name: m.name,
                bytes: m.bytes.load(Ordering::Relaxed),
                items: m.items.load(Ordering::Relaxed),
                estimated: m.estimated,
                desc: m.desc,
            })
            .collect();
        meters.extend(census_samples());
        let attributed: u64 = meters.iter().map(|m| m.bytes).sum();
        Snapshot {
            ts_ms: now_ms(),
            rss_bytes: rss,
            hwm_bytes: hwm,
            meters,
            unattributed: rss.saturating_sub(attributed),
            allocator: allocator_detail().or_else(glibc_detail),
        }
    }

    /// 采样入窗（sampler 调用；容量环形覆盖）
    pub fn record(&self, s: Snapshot) {
        let mut h = self.history.lock().unwrap();
        if h.len() >= self.capacity {
            h.remove(0);
        }
        h.push(s);
    }

    /// 历史窗口拷贝（分析用：趋势/峰值）
    pub fn history(&self) -> Vec<Snapshot> {
        self.history.lock().unwrap().clone()
    }

    /// RSS 峰值帧及其各 meter 贡献（事后归因）
    pub fn peak_attribution(&self) -> Option<Snapshot> {
        self.history().into_iter().max_by_key(|s| s.rss_bytes)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
}

/// 单 meter 快照样本
#[derive(Debug, Clone, PartialEq)]
pub struct MeterSample {
    pub name: &'static str,
    pub bytes: u64,
    pub items: u64,
    pub estimated: bool,
    pub desc: &'static str,
}

/// 全量快照（包络 + 分用途 + 未归因差 + 分配器物理明细）
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub ts_ms: u64,
    pub rss_bytes: u64,
    pub hwm_bytes: u64,
    pub meters: Vec<MeterSample>,
    pub unattributed: u64,
    pub allocator: Option<AllocatorDetail>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub fn get() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// VmHWM（进程峰值 RSS，/proc/self/status——诊断真实高水位）
pub fn proc_hwm_bytes() -> Option<u64> {
    let st = std::fs::read_to_string("/proc/self/status").ok()?;
    for l in st.lines() {
        if let Some(v) = l.strip_prefix("VmHWM:") {
            let kb: u64 = v.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// RAII 作用域计量 guard：构造加、析构减（列存扫描活动批等
/// 生命周期明确的缓冲）。guard 克隆 = 追加持有计数（Arc 共享端点）。
pub struct MeterGuard {
    meter: &'static Meter,
    bytes: u64,
    items: u64,
}

impl MeterGuard {
    pub fn new(meter: &'static Meter, bytes: u64, items: u64) -> Self {
        meter.bytes.fetch_add(bytes, Ordering::Relaxed);
        meter.items.fetch_add(items, Ordering::Relaxed);
        MeterGuard {
            meter,
            bytes,
            items,
        }
    }
}

impl Drop for MeterGuard {
    fn drop(&mut self) {
        self.meter.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
        self.meter.items.fetch_sub(self.items, Ordering::Relaxed);
    }
}

/// 便捷注册 + 取 meter（子系统初始化用）
pub fn meter(name: &'static str, estimated: bool, desc: &'static str) -> &'static Meter {
    Registry::register(name, estimated, desc)
}

/// 行集结构字节（Vec 内容 + 变长载荷；分配器松弛不计——口径注明）
pub fn rows_bytes(rows: &[Vec<crate::types::SqlValue>]) -> u64 {
    use crate::types::SqlValue;
    let fixed = std::mem::size_of::<SqlValue>() as u64;
    let mut b = 0u64;
    for r in rows {
        b += fixed * r.len() as u64 + 48; // Vec 头/容量余量
        for v in r {
            b += match v {
                SqlValue::Utf8(s) => s.capacity() as u64,
                SqlValue::Bytes(x) => x.capacity() as u64,
                _ => 0,
            };
        }
    }
    b
}

/// 后台采样器（线程持有 Arc 句柄；Drop 停止）
pub struct Sampler {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    /// interval_ms = 0 → 不启动（默认）
    pub fn start(interval_ms: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = if interval_ms > 0 {
            Some(
                std::thread::Builder::new()
                    .name("dendro-memprof".into())
                    .spawn(move || {
                        let reg = get();
                        let mut tick: u64 = 0;
                        while !stop2.load(Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_millis(
                                interval_ms.min(50),
                            ));
                            tick += interval_ms.min(50);
                            if tick >= interval_ms {
                                tick = 0;
                                if reg.enabled() {
                                    let s = reg.snapshot();
                                    reg.record(s);
                                }
                            }
                        }
                    })
                    .expect("memprof sampler thread"),
            )
        } else {
            None
        };
        Sampler { stop, handle }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 分配器物理明细（语义 meters 之外的物理归因：arena 滞留/碎片）
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AllocatorDetail {
    /// 分配器品牌（"glibc" | "jemalloc" | ...）
    pub flavor: &'static str,
    /// 在用字节（glibc uordblks / jemalloc allocated）
    pub allocated: u64,
    /// 已释放但未归还 OS 的可回收字节（glibc fordblks / jemalloc retained）
    pub retained: u64,
}

type DetailFn = Box<dyn Fn() -> Option<AllocatorDetail> + Send + Sync>;
static DETAIL_HOOK: Mutex<Option<DetailFn>> = Mutex::new(None);

/// 注册分配器明细读取器（server 启动时按 feature/平台注册；
/// 未注册 = None——embed/cdylib 默认无）
pub fn set_allocator_detail(f: DetailFn) {
    *DETAIL_HOOK.lock().unwrap() = Some(f);
}

/// 缓存普查 provider（条目数——census；字节口径不可得时不虚报）
type CensusFn = Box<dyn Fn() -> Vec<(&'static str, u64, &'static str)> + Send + Sync>;
static CENSUS: Mutex<Vec<CensusFn>> = Mutex::new(Vec::new());

/// 注册缓存普查（子系统懒注册或初始化时注册）
pub fn add_census(f: CensusFn) {
    CENSUS.lock().unwrap().push(f);
}

fn census_samples() -> Vec<MeterSample> {
    let g = CENSUS.lock().unwrap();
    g.iter()
        .flat_map(|f| {
            f().into_iter()
                .map(|(name, items, desc)| MeterSample {
                    name,
                    bytes: 0, // 条目 census——字节不可得，不虚报
                    items,
                    estimated: true,
                    desc,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn allocator_detail() -> Option<AllocatorDetail> {
    let g = DETAIL_HOOK.lock().unwrap();
    g.as_ref()?()
}

/// glibc 明细（Linux 专属；mallinfo2 需 glibc ≥ 2.33——失败即 None，
/// 平台/版本两重可选）
#[cfg(target_os = "linux")]
fn glibc_detail() -> Option<AllocatorDetail> {
    // extern "C" 直接绑 glibc（无 libc crate 依赖面变化）
    #[repr(C)]
    struct MallInfo2 {
        arena: i64,
        ordblks: i64,
        smblks: i64,
        hblks: i64,
        hblkhd: i64,
        usmblks: i64,
        fsmblks: i64,
        uordblks: i64,
        fordblks: i64,
        keepcost: i64,
    }
    extern "C" {
        fn mallinfo2() -> MallInfo2;
    }
    let mi = unsafe { mallinfo2() };
    if mi.arena < 0 {
        return None;
    }
    Some(AllocatorDetail {
        flavor: "glibc",
        allocated: mi.uordblks.max(0) as u64,
        retained: mi.fordblks.max(0) as u64,
    })
}

#[cfg(not(target_os = "linux"))]
fn glibc_detail() -> Option<AllocatorDetail> {
    None
}

/// 快照 → JSON（HTTP/embed 表面共用；紧凑可 grep）
pub fn snapshot_json(s: &Snapshot) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(256);
    let _ = write!(
        out,
        "{{\"ts_ms\":{},\"rss\":{},\"hwm\":{},\"unattributed\":{},\"meters\":[",
        s.ts_ms, s.rss_bytes, s.hwm_bytes, s.unattributed
    );
    for (i, m) in s.meters.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"name\":\"{}\",\"bytes\":{},\"items\":{}{}}}",
            m.name,
            m.bytes,
            m.items,
            if m.estimated { ",\"est\":true" } else { "" }
        );
    }
    out.push_str("]"); // meters 数组先闭合——allocator 在对象层
    if let Some(a) = &s.allocator {
        let _ = write!(
            out,
            ",\"allocator\":{{\"flavor\":\"{}\",\"allocated\":{},\"retained\":{}}}",
            a.flavor, a.allocated, a.retained
        );
    }
    out.push('}');
    out
}

// ---------- 内建 meter（核心子系统；其余按需注册） ----------

/// memtx pending 载荷字节（分支聚合，engine 写路径更新）。
/// 口径（docs/research/关键数据结构内存分析.md）：**载荷**（key+val
/// 编码长）；全链路结构驻留实测 ~20× 载荷——不在本 meter，
/// 用 memtx.entries census × 系数外推
pub static MEMTX_PENDING: OnceLock<&'static Meter> = OnceLock::new();

pub fn memtx_pending() -> &'static Meter {
    MEMTX_PENDING.get_or_init(|| {
        meter(
            "memtx.pending",
            false,
            "未 checkpoint 的载荷字节（结构开销 ~20× 载荷——见关键数据结构内存分析）",
        )
    })
}

pub fn memtx_add(bytes: u64) {
    memtx_pending().bytes.fetch_add(bytes, Ordering::Relaxed);
}
pub fn memtx_sub(bytes: u64) {
    memtx_pending().bytes.fetch_sub(bytes, Ordering::Relaxed);
}

/// 列存扫描活动批（精确；RAII——try_ap_scan/lazy/global_agg 的段批）
pub static COLSCAN_ACTIVE: OnceLock<&'static Meter> = OnceLock::new();

pub fn colscan_active() -> &'static Meter {
    COLSCAN_ACTIVE.get_or_init(|| {
        meter(
            "columnar.scan_active",
            false,
            "活动扫描的 Arrow 批字节（RAII 作用域）",
        )
    })
}

/// 查询中间行（TableView rows；精确字节按行宽实测口径）
pub static QUERY_ROWS: OnceLock<&'static Meter> = OnceLock::new();

pub fn query_rows() -> &'static Meter {
    QUERY_ROWS.get_or_init(|| meter("query.rows", false, "物化中间行字节（TableView 收集期）"))
}

/// 打开库的弱引用注册表（census 汇总 plan 缓存——无环：Weak）
static DBS: Mutex<Vec<std::sync::Weak<crate::engine::Database>>> = Mutex::new(Vec::new());

pub(crate) fn register_db(db: &std::sync::Arc<crate::engine::Database>) {
    let mut g = DBS.lock().unwrap();
    g.retain(|w| w.upgrade().is_some());
    g.push(std::sync::Arc::downgrade(db));
}

fn open_dbs_memtx_entries() -> usize {
    let mut g = DBS.lock().unwrap();
    g.retain(|w| w.upgrade().is_some());
    g.iter()
        .filter_map(|w| w.upgrade())
        .map(|d| d.memtx_entries())
        .sum()
}

fn open_dbs_plan_cache_len() -> usize {
    let mut g = DBS.lock().unwrap();
    g.retain(|w| w.upgrade().is_some());
    g.iter()
        .filter_map(|w| w.upgrade())
        .map(|d| d.plan_cache_len())
        .sum()
}

pub fn init_builtin_meters() {
    let _ = memtx_pending();
    let _ = colscan_active();
    let _ = query_rows();
    // 缓存普查：条目数（有界性/泄漏趋势的观测面——字节不虚报为 0）
    add_census(Box::new(|| {
        vec![
            (
                "memtx.entries",
                open_dbs_memtx_entries() as u64,
                "memtx 全表键数（结构驻留 = 条目 × 系数，见关键数据结构内存分析）",
            ),
            (
                "cache.plan_entries",
                open_dbs_plan_cache_len() as u64,
                "计划缓存条目（SQL hash→AST，16 分片界 4096）",
            ),
            (
                "cache.predicate_entries",
                crate::sql::scalar::predicate_cache_len() as u64,
                "谓词程序缓存条目（文本+布局→ScalarProgram，界 1024）",
            ),
            (
                "cache.analyze_entries",
                crate::sql::stats::analyze_cache_len() as u64,
                "ANALYZE 产物缓存条目（stats_addr→TableAnalyze）",
            ),
        ]
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_scoped_accounting() {
        let m = meter("test.scoped", false, "测试");
        let before = m.bytes.load(Ordering::Relaxed);
        {
            let _g = MeterGuard::new(m, 1024, 4);
            assert_eq!(m.bytes.load(Ordering::Relaxed), before + 1024);
            assert_eq!(m.items.load(Ordering::Relaxed), 4);
        }
        assert_eq!(m.bytes.load(Ordering::Relaxed), before);
        assert_eq!(m.items.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn snapshot_envelope_and_json() {
        init_builtin_meters();
        let s = get().snapshot();
        assert!(s.rss_bytes > 0, "包络存在");
        assert!(s.meters.iter().any(|m| m.name == "memtx.pending"));
        let j = snapshot_json(&s);
        assert!(j.contains("\"unattributed\":"), "{j}");
        assert!(j.contains("memtx.pending"));
    }

    #[test]
    fn history_window_and_peak() {
        let reg = get();
        let s1 = reg.snapshot();
        reg.record(s1.clone());
        let m = meter("test.hist", false, "历史");
        m.bytes.fetch_add(4096, Ordering::Relaxed);
        let s2 = reg.snapshot();
        reg.record(s2.clone());
        let h = reg.history();
        assert!(h.len() >= 2);
        // 并行测试共享全局注册表——峰值帧可能来自其他测试：确定性
        // 断言改为"历史含归因帧 + 峰值帧 RSS ≥ 归因帧"
        assert!(
            reg.history().iter().any(|s| s
                .meters
                .iter()
                .any(|m| m.name == "test.hist" && m.bytes == 4096)),
            "历史窗口含归因帧"
        );
        let peak = reg.peak_attribution().expect("有峰值帧");
        assert!(peak.rss_bytes >= s2.rss_bytes, "峰值帧 RSS ≥ 归因帧");
        m.bytes.fetch_sub(4096, Ordering::Relaxed);
    }
}
