//! 点查机器逐层分解：SQL 路径各子步骤单独计时（定位真瓶颈）
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::time::Instant;

fn bench<F: FnMut(u64)>(mut f: F) -> f64 {
    for i in 0..5000u64 { f(i); }
    let mut r = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut n = 0;
        while t.elapsed().as_secs_f64() < 1.0 { f(n); n += 1; }
        r.push(n as f64 / t.elapsed().as_secs_f64());
    }
    r.sort_by(|a, b| a.partial_cmp(b).unwrap());
    r[2]
}

fn main() {
    let dir = std::env::temp_dir().join(format!("pbrk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut c = Connection::open_with(&dir, DbOptions::embedded(StoreConfig::Memory)).unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)").unwrap();
    let vals: Vec<String> = (0..100_000).map(|i| format!("({}, {}, 'tag')", i+1, i%7)).collect();
    for ch in (0..100).step_by(10) {
        c.execute(&format!("INSERT INTO t VALUES {}", vals[ch*1000..(ch+1)*1000].join(","))).unwrap();
    }
    // === memtx 驻留（无 checkpoint）===
    // L0: 裸 find_by_pk
    let r = bench(|i| { let _ = c.find_by_pk("t", ((i % 100_000) + 1) as i64); });
    println!("memtx L0 find_by_pk:        {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
    // L1: SQL 文本
    let r = bench(|i| { let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {}", (i%100_000)+1)); });
    println!("memtx L1 SQL 文本:          {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
    // perf 分解
    dendro_core::perf::reset();
    let _ = bench(|i| { let _ = c.query(&format!("SELECT v FROM t WHERE id = {}", (i%100_000)+1)); });
    for x in dendro_core::perf::report() {
        if x.count > 0 && x.avg_ns > 100 { println!("    {:<12} {:>6}ns", x.name, x.avg_ns); }
    }
    // === checkpoint 后（树驻留）===
    c.execute("CHECKPOINT").unwrap();
    let r = bench(|i| { let _ = c.find_by_pk("t", ((i % 100_000) + 1) as i64); });
    println!("tree  L0 find_by_pk:        {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
    let r = bench(|i| { let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {}", (i%100_000)+1)); });
    println!("tree  L1 SQL 文本:          {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
}
