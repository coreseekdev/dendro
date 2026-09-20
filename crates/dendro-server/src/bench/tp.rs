//! TP 基准（opt1-prolly-tp-base 的 A/B 数据面）：
//! 装载 → CHECKPOINT（树驻留）→ 点查 / 短范围 / 随机写 三场景。
//! 双臂对比：`--preset default`（memtx 全内存惯性形态）vs
//! `--preset embedded`（有界 memtx + 字节预算页缓存）。
//! 输出：每场景 ops/s（median of 5 × 1s 窗）+ RSS/memprof 快照。

use crate::bench::{BenchResult, BenchRow};
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn bench_tp(data_dir: &PathBuf, preset: &str, rows: u64, out: &PathBuf) -> BenchResult {
    let mut rows_out: Vec<BenchRow> = Vec::new();
    let mut conn = if preset == "embedded" {
        Connection::open_with(
            data_dir,
            DbOptions::embedded(StoreConfig::LocalDir(data_dir.clone())),
        )
        .unwrap()
    } else {
        // default 臂：阈值调大模拟"memtx 全内存"惯性形态（页缓存同预算
        // ——只让 memtx 维度变化）
        let mut o = DbOptions::embedded(StoreConfig::LocalDir(data_dir.clone()));
        o.checkpoint_threshold_bytes = 8 << 30; // 8GB ≈ 不自动物化
        o.checkpoint_interval_s = 3600;
        Connection::open_with(data_dir, o).unwrap()
    };

    // ---- 装载 + 显式物化（两臂同界：点查走树路径的可比前提）----
    let t0 = Instant::now();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)")
        .unwrap();
    for chunk in 0..(rows / 10_000) {
        let vals: Vec<String> = (0..10_000)
            .map(|i| {
                let id = chunk * 10_000 + i + 1;
                format!("({id}, {}, 'tag{}')", id % 1000, id % 97)
            })
            .collect();
        conn.execute(&format!("INSERT INTO t VALUES {}", vals.join(",")))
            .unwrap();
    }
    conn.execute("CHECKPOINT").unwrap();
    rows_out.push(BenchRow {
        name: "load_seconds".into(),
        value: t0.elapsed().as_secs_f64(),
        unit: "s",
    });

    let rss = || dendro_core::engine::proc_rss_bytes().unwrap_or(0) as f64 / (1u64 << 30) as f64;

    // ---- 场景 1：随机点查（树路径——pk 等值）----
    let scenarios: Vec<(&str, Box<dyn Fn(&mut Connection, u64) -> bool>)> = vec![
        (
            "point",
            Box::new(|c: &mut Connection, i: u64| {
                let r = c
                    .query(&format!(
                        "SELECT v, tag FROM t WHERE id = {}",
                        (i * 7919) % rows + 1
                    ))
                    .unwrap();
                !r.rows.is_empty()
            }),
        ),
        (
            "range100",
            Box::new(|c: &mut Connection, i: u64| {
                let lo = (i * 7919) % (rows - 100_000).max(1) + 1;
                let r = c
                    .query(&format!(
                        "SELECT id, v FROM t WHERE id >= {lo} AND id < {}",
                        lo + 100
                    ))
                    .unwrap();
                r.rows.len() == 100
            }),
        ),
        (
            "write",
            Box::new(|c: &mut Connection, i: u64| {
                let id = (i * 104729) % rows + 1;
                c.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {id}"))
                    .unwrap()
                    > 0
            }),
        ),
    ];
    for (name, f) in &scenarios {
        // warmup
        for i in 0..1000u64 {
            let _ = f(&mut conn, i);
        }
        let mut rates: Vec<f64> = Vec::with_capacity(5);
        for _round in 0..5 {
            let t = Instant::now();
            let win = Duration::from_secs(1);
            let mut n = 0u64;
            while t.elapsed() < win {
                let _ = f(&mut conn, n);
                n += 1;
            }
            rates.push(n as f64 / t.elapsed().as_secs_f64());
        }
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
        rows_out.push(BenchRow {
            name: format!("{name}.ops_per_s.median"),
            value: rates[2],
            unit: "ops/s",
        });
        rows_out.push(BenchRow {
            name: format!("{name}.rss_gb"),
            value: rss(),
            unit: "gb",
        });
    }
    rows_out.push(BenchRow {
        name: "final_rss_gb".into(),
        value: rss(),
        unit: "gb",
    });
    // 页缓存 census：命中率（opt1 验证面——预算 vs 工作集的缓存有效性）
    {
        let (h, m, resident) = dendro_core::prolly::store::global_cache_stats();
        let total = h + m;
        rows_out.push(BenchRow { name: "nodecache.hits".into(), value: h as f64, unit: "cnt" });
        rows_out.push(BenchRow { name: "nodecache.misses".into(), value: m as f64, unit: "cnt" });
        rows_out.push(BenchRow {
            name: "nodecache.hit_rate".into(),
            value: if total > 0 { (h as f64 / total as f64 * 1000.0).round() / 1000.0 } else { 0.0 },
            unit: "ratio",
        });
        rows_out.push(BenchRow {
            name: "nodecache.resident_mb".into(),
            value: resident as f64 / (1u64 << 20) as f64,
            unit: "mb",
        });
    }
    // memprof 快照（unattributed + 分配器 + 页缓存命中）
    rows_out.push(BenchRow {
        name: "memprof.unattributed_gb".into(),
        value: dendro_core::memprof::get().snapshot().unattributed as f64 / (1u64 << 30) as f64,
        unit: "gb",
    });

    let r = BenchResult {
        suite: "tp".into(),
        rows: rows_out,
    };
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(out, r.to_json()).unwrap();
    r
}
