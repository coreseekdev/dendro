//! 关键数据结构内存实测（关键数据结构内存分析的数据面）。
//! 独立进程逐结构测 RSS 增量 → 每条目字节 vs 裸数据尺寸 → 开销比。
//! 用法：cargo run --release --example memstruct -- [entries] [val_bytes]

use dendro_core::types::SqlValue;
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn rss_gb() -> f64 {
    dendro_core::engine::proc_rss_bytes().unwrap_or(0) as f64 / (1u64 << 30) as f64
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let vs: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);

    // ---- 1) memtx 版本链：N 键 × vs 字节值（单版本） ----
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        mem_sample_interval_ms: 0,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 1 << 20,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE m (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    let r0 = rss_gb();
    let mut txn = 0u64;
    for chunk_start in (0..n).step_by(1000) {
        let vals: Vec<String> = (0..1000.min(n - chunk_start))
            .map(|i| {
                let id = chunk_start + i;
                txn += 1;
                format!("({id}, '{}')", "x".repeat(vs.saturating_sub(2)))
            })
            .collect();
        s.exec(&format!("INSERT INTO m VALUES {}", vals.join(",")))
            .unwrap();
    }
    let r1 = rss_gb();
    let per_entry = (r1 - r0) * (1 << 30) as f64 / n as f64;
    println!(
        "memtx(单版本): N={n} val={vs}B  Δrss={:.0}MB  每条目={per_entry:.0}B（裸 {vs}B → 开销 {:.1}×）",
        (r1 - r0) * 1024.0,
        per_entry / vs as f64
    );
    println!(
        "  memprof memtx.pending = {}B/条目",
        dendro_core::memprof::memtx_pending()
            .bytes
            .load(std::sync::atomic::Ordering::Relaxed)
            / n as u64
    );

    // ---- 2) SqlValue 物化行：N 行 × 4 列（2 int + 2 text） ----
    let r2 = rss_gb();
    let rows: Vec<Vec<SqlValue>> = (0..n)
        .map(|i| {
            vec![
                SqlValue::Int64(i as i64),
                SqlValue::Int32((i % 97) as i32),
                SqlValue::Utf8("a".repeat(vs / 2)),
                SqlValue::Utf8("b".repeat(vs / 2)),
            ]
        })
        .collect();
    let r3 = rss_gb();
    std::mem::forget(rows);
    let per_row = (r3 - r2) * (1 << 30) as f64 / n as f64;
    let structural = {
        // 结构口径单行实测
        let one = vec![
            SqlValue::Int64(1),
            SqlValue::Int32(1),
            SqlValue::Utf8("a".repeat(vs / 2)),
            SqlValue::Utf8("b".repeat(vs / 2)),
        ];
        dendro_core::memprof::rows_bytes(&[one])
    };
    println!(
        "SqlValue 行(4列,2文本): RSS 每行={per_row:.0}B；rows_bytes 结构口径单行={structural}B → 分配器/对齐开销 {:.1}×",
        per_row / structural as f64
    );

    // ---- 3) checkpoint 后 prolly 树 + 段（分母 = 转储后的常驻） ----
    let r4 = rss_gb();
    s.exec("CHECKPOINT").unwrap();
    let r5 = rss_gb();
    println!(
        "checkpoint: Δrss={:+.0}MB（memtx→段物化的瞬时成本；完成后 pending 应归零：{}B）",
        (r5 - r4) * 1024.0,
        dendro_core::memprof::memtx_pending()
            .bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    println!("终局 rss={:.2}GB", rss_gb());
}
