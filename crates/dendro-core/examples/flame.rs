//! 火焰图剖析探针（pprof-rs：SIGPROF 信号采样，无 perf 权限环境可用）。
//! 复刻 pointearly/pointbreak 场景，负载跑 N 秒后导出 SVG 火焰图。
//! 用法：cargo run --release --example flame -- [rows] [secs] [mode]
//!   mode: point（未物化全驻点查）| tree（物化后点查）| insert

use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::time::{Duration, Instant};

fn main() {
    let rows: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let mode: String = std::env::args().nth(3).unwrap_or_else(|| "point".into());

    let dir = std::env::temp_dir().join(format!("flame-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut opts = DbOptions::embedded(if mode == "updatedisk" {
        StoreConfig::LocalDir(std::env::temp_dir().join(format!("flamedisk-{}", std::process::id())))
    } else {
        StoreConfig::Memory
    });
    if mode == "point" {
        opts.checkpoint_threshold_bytes = 4 << 30; // 未物化全驻
    }
    let mut c = Connection::open_with(&dir, opts).unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)").unwrap();
    let mut id: u64 = 0;
    while id < rows {
        let n = (rows - id).min(10_000);
        let vals: Vec<String> = (0..n).map(|i| format!("({}, {}, 'tag{}')", id + i + 1, (id + i) % 1000, (id + i) % 97)).collect();
        c.execute(&format!("INSERT INTO t VALUES {}", vals.join(","))).unwrap();
        id += n;
    }
    if mode == "tree" || mode == "update" || mode == "updatedisk" {
        c.execute("CHECKPOINT").unwrap();
    }
    println!("load 完成（{mode} 模式，{rows} 行）——采样 {secs}s…");

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(997)
        .build()
        .unwrap();
    let t = Instant::now();
    let mut i = 0u64;
    let mut cnt = 0u64;
    while t.elapsed().as_secs_f64() < secs as f64 {
        match mode.as_str() {
            "update" | "updatedisk" => {
                let nid = rows + (i % 50_000) + 1;
                let _ = c.execute(&format!("INSERT OR REPLACE INTO t VALUES ({nid}, 1, 'z')"));
            }
            "update" => {
                let uid = (i.wrapping_mul(104729) % rows) + 1;
                let _ = c.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {uid}"));
            }
            _ => {
                let qid = (i.wrapping_mul(7919) % rows) + 1;
                let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {qid}"));
            }
        }
        i += 1;
        cnt += 1;
    }
    let rate = cnt as f64 / t.elapsed().as_secs_f64();
    drop(guard.report().build().unwrap().flamegraph(std::fs::File::create(format!("/home/nzinfo/bench-p0/flame-{mode}.svg")).unwrap()).unwrap());
    println!("完成：{rate:.0} ops/s——火焰图 /home/nzinfo/bench-p0/flame-{mode}.svg");
    let _ = Duration::from_secs(0);
}
