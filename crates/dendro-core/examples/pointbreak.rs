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
    let rows: u64 = std::env::args().nth(1).and_then(|x| x.parse().ok()).unwrap_or(100_000);
    let mut opts = DbOptions::embedded(StoreConfig::Memory);
    if std::env::args().any(|a| a == "full") { opts.checkpoint_threshold_bytes = 4 << 30; }
    let mut c = Connection::open_with(&dir, opts).unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)").unwrap();
    let mut id: u64 = 0;
    while id < rows {
        let n = (rows - id).min(10_000);
        let vals: Vec<String> = (0..n).map(|i| format!("({}, {}, 'tag')", id+i+1, (id+i)%7)).collect();
        c.execute(&format!("INSERT INTO t VALUES {}", vals.join(","))).unwrap();
        id += n;
    }
    // === memtx 驻留（无 checkpoint）===
    // L0: 裸 find_by_pk
    let r = bench(|i| { let _ = c.find_by_pk("t", ((i % rows) + 1) as i64); });
    println!("memtx L0 find_by_pk:        {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
    // L1: SQL 文本
    let r = bench(|i| { let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {}", (i%rows)+1)); });
    println!("memtx L1 SQL 文本:          {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
    // perf 分解
    dendro_core::perf::reset();
    let _ = bench(|i| { let _ = c.query(&format!("SELECT v FROM t WHERE id = {}", (i%rows)+1)); });
    for x in dendro_core::perf::report() {
        if x.count > 0 && x.avg_ns > 100 { println!("    {:<12} {:>6}ns", x.name, x.avg_ns); }
    }
    // 早退触发证明：裸列投影（早退覆盖）vs 表达式投影（早退回落计划）
    {
        let p_expr = bench(|i| { let _ = c.query(&format!("SELECT v+0, tag FROM t WHERE id = {}", (i%rows)+1)); });
        println!("表达式投影 point（早退不覆盖）: {p_expr:.0} ops/s ({:.2}µs)", 1e6/p_expr);
    }
    // 早退差分轴：optimize=off 关闭 try_point_early（纯计划路径对照）
    if std::env::args().any(|a| a == "diff") {
        c.execute("SET dendro.optimize = off").unwrap();
        let p_off = bench(|i| { let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {}", (i%rows)+1)); });
        c.execute("SET dendro.optimize = on").unwrap();
        println!("optimize=off point: {p_off:.0} ops/s ({:.2}µs)", 1e6/p_off);
    }
    // === checkpoint 后（树驻留）===
    c.execute("CHECKPOINT").unwrap();
    let r = bench(|i| { let _ = c.find_by_pk("t", ((i % rows) + 1) as i64); });
    println!("tree  L0 find_by_pk:        {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
    let r = bench(|i| { let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {}", (i%rows)+1)); });
    println!("tree  L1 SQL 文本:          {r:>8.0} ops/s ({:.2}µs)", 1e6/r);
    dendro_core::perf::reset();
    let _ = bench(|i| { let _ = c.query(&format!("SELECT v FROM t WHERE id = {}", (i%rows)+1)); });
    for x in dendro_core::perf::report() {
        if x.count > 0 && x.avg_ns > 50 {
            println!("    tree {:<16} {:>7}ns ×{}", x.name, x.avg_ns, x.count);
        }
    }
}
