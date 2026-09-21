//! mem 臂驻留实证 + mem/disk 树读路径分层（P0 前置归因）。
//! 臂一：复刻 sqlite-cmp mem 配置（embedded + StoreConfig::Memory），
//! 观察自动 checkpoint 行为与终态驻留；臂二：同负载 LocalDir——
//! 两臂各测 CHECKPOINT 后的 find_by_pk（L0 树点查）与 SQL 文本
//! （L1），加 perf 阶段分解与 NodeStore LRU 命中率。
//!
//! 用法：cargo run --release --example memres -- [rows]

use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::time::{Duration, Instant};

const DDL: &str = "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)";

fn rss_mb() -> f64 {
    dendro_core::engine::proc_rss_bytes().unwrap_or(0) as f64 / (1u64 << 20) as f64
}

fn resid(c: &Connection, tag: &str) {
    let b = c.db().branch("main").unwrap();
    let entries = b.mem.len();
    let pend = b.pending_bytes.load(std::sync::atomic::Ordering::Relaxed);
    let pend_meter = dendro_core::memprof::memtx_pending()
        .bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "[{tag}] rss={:.0}MB memtx.entries={entries} pending_bytes={pend} ({:.1}MB) memtx.pending meter={pend_meter}",
        rss_mb(),
        pend as f64 / (1u64 << 20) as f64
    );
}

fn cache_stats(c: &Connection, tag: &str, base: (u64, u64, u64)) {
    let (h0, m0, _) = base;
    let (h, m, resident) = dendro_core::prolly::store::global_cache_stats();
    let (h, m) = (h - h0, m - m0);
    let total = h + m;
    let rate = if total > 0 { h as f64 / total as f64 * 100.0 } else { 0.0 };
    println!("[{tag}] NodeStore LRU: hits={h} misses={m} 命中率={rate:.1}% resident={:.0}MB", resident as f64 / (1u64 << 20) as f64);
}

/// sqlite-cmp 同款：median of 5×1s
fn bench<F: FnMut(u64)>(mut f: F) -> f64 {
    for i in 0..2000u64 {
        f(i);
    }
    let mut rates = Vec::with_capacity(5);
    for _ in 0..5 {
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed() < Duration::from_secs(1) {
            f(n);
            n += 1;
        }
        rates.push(n as f64 / t.elapsed().as_secs_f64());
    }
    rates.sort_by(|a, b| a.partial_cmp(&b).unwrap());
    rates[2]
}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);

    for (mode, store, big_threshold) in [
        ("mem", StoreConfig::Memory, false),
        ("disk", StoreConfig::LocalDir(std::path::PathBuf::from("/tmp/dendro-memres-disk")), false),
        ("memfull", StoreConfig::Memory, true),
    ] {
        println!("==== 臂 {mode} ====");
        let cache_base = dendro_core::prolly::store::global_cache_stats();
        let dir = format!("/tmp/dendro-memres-{mode}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all("/tmp/dendro-memres-disk");
        let mut opts = DbOptions::embedded(store);
        if big_threshold {
            // 全驻 memtx：阈值放大到 4GB，自动 checkpoint 永不触发——
            // "内存引擎"语义（HANA delta store 全内存）的极限实验
            opts.checkpoint_threshold_bytes = 4 << 30;
        }
        let mut c = Connection::open_with(&dir, opts).unwrap();
        c.execute(DDL).unwrap();
        resid(&c, "建表后");

        let t0 = Instant::now();
        for ch in 0..(rows / 10_000) {
            let vals: Vec<String> = (0..10_000u64)
                .map(|i| {
                    let id = ch * 10_000 + i + 1;
                    format!("({id}, {}, 'tag{}')", id % 1000, id % 97)
                })
                .collect();
            c.execute(&format!("INSERT INTO t VALUES {}", vals.join(",")))
                .unwrap();
            if (ch + 1) % 20 == 0 {
                resid(&c, &format!("load {}/{} ({:.1}s)", (ch + 1) * 10_000, rows, t0.elapsed().as_secs_f64()));
            }
        }
        println!("load 完成：{:.1}s", t0.elapsed().as_secs_f64());
        std::thread::sleep(Duration::from_secs(3));
        resid(&c, "load 后 3s（等自动 checkpoint）");

        // point：驻留态（自动 checkpoint 后）测速
        let p1 = bench(|i| {
            let id = ((i * 7919) % rows + 1) as i64;
            let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {id}"));
        });
        println!("point.{mode}（checkpoint 前）= {p1:.0} ops/s");
        resid(&c, "point 后");

        if big_threshold {
            // 全驻臂：memtx 即真相——只测 SQL 文本点查（不 CHECKPOINT）
            resid(&c, "全驻终态");
            let p2 = bench(|i| {
                let id = ((i * 7919) % rows + 1) as i64;
                let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {id}"));
            });
            println!("point.{mode}（全驻 memtx）= {p2:.0} ops/s ({:.2}µs)", 1e6 / p2);
            // 单次查询校验：行数 + 值
            let r = c.query("SELECT v, tag FROM t WHERE id = 500000").unwrap();
            println!("point 校验 id=500000: rows={} {:?}", r.rows().len(),
                r.rows().first().map(|row| format!("{:?}", row)));
            // 裸 TableMem::get 微基准（绕过 SQL——纯存储层分解）
            let b = c.db().branch("main").unwrap();
            let tm = b.mem.table(1);
            let snap = u64::MAX - 1;
            let keys: Vec<Vec<u8>> = (0..1000u64)
                .map(|i| {
                    dendro_core::format::row::encode_key(&[dendro_core::types::SqlValue::Int64(
                        ((i * 7919) % rows + 1) as i64,
                    )])
                })
                .collect();
            let g = bench(|i| {
                let _ = tm.get(&keys[(i as usize) % 1000], snap);
            });
            println!("memtx 裸 get（全驻）= {g:.0} ops/s ({:.0}ns)", 1e9 / g);
            dendro_core::perf::reset();
            let _ = bench(|i| {
                let id = ((i * 7919) % rows + 1) as i64;
                let _ = c.query(&format!("SELECT v FROM t WHERE id = {id}"));
            });
            for x in dendro_core::perf::report() {
                if x.count > 0 && x.avg_ns > 50 {
                    println!("    {:<16} {:>7}ns ×{}", x.name, x.avg_ns, x.count);
                }
            }
            let n: i64 = c
                .query_scalar_i64("SELECT count(*) FROM t")
                .unwrap()
                .unwrap();
            println!("count={n}");
            drop(c);
            continue;
        }

        c.execute("CHECKPOINT").unwrap();
        resid(&c, "显式 CHECKPOINT 后");
        cache_stats(&c, "CHECKPOINT 后", cache_base);

        // L0：树点查（旁路基线参照，仅归因用）
        let l0 = bench(|i| {
            let _ = c.find_by_pk("t", ((i * 7919) % rows + 1) as i64);
        });
        println!("tree L0 find_by_pk: {l0:>8.0} ops/s ({:.2}µs)", 1e6 / l0);
        cache_stats(&c, "L0 后", cache_base);

        // L1：SQL 文本（与 sqlite-cmp 同口径）
        let p2 = bench(|i| {
            let id = ((i * 7919) % rows + 1) as i64;
            let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {id}"));
        });
        println!("point.{mode}（checkpoint 后）= {p2:.0} ops/s");
        cache_stats(&c, "L1 后", cache_base);

        // perf 阶段分解（L1 路径）
        dendro_core::perf::reset();
        let _ = bench(|i| {
            let id = ((i * 7919) % rows + 1) as i64;
            let _ = c.query(&format!("SELECT v FROM t WHERE id = {id}"));
        });
        for x in dendro_core::perf::report() {
            if x.count > 0 && x.avg_ns > 50 {
                println!("    {:<16} {:>7}ns ×{}", x.name, x.avg_ns, x.count);
            }
        }
        let n: i64 = c
            .query_scalar_i64("SELECT count(*) FROM t")
            .unwrap()
            .unwrap();
        println!("count={n}");
        drop(c);
    }
}
