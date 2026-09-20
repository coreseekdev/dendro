//! UPDATE 提交路径耗时分解（目标 SQLite 量级的第一手数据）
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::time::Instant;
fn main() {
    let dir = std::env::temp_dir().join(format!("updpath-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut c = Connection::open_with(&dir, DbOptions::embedded(StoreConfig::Memory)).unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)").unwrap();
    let vals: Vec<String> = (0..100_000).map(|i| format!("({}, {})", i+1, i)).collect();
    for ch in (0..100).step_by(10) {
        c.execute(&format!("INSERT INTO t VALUES {}", vals[ch*1000..(ch+1)*1000].join(","))).unwrap();
    }
    c.execute("CHECKPOINT").unwrap();
    // 分解：SQL 机器 vs 提交管道（durability 对比）
    let run = |tag: &str, dur| {
        let mut o = DbOptions::embedded(StoreConfig::Memory);
        o.durability = dur;
        let mut c2 = Connection::open_with(&dir, o).unwrap();
        for i in 1..=2000 { let _ = c2.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {i}")); }
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed().as_secs_f64() < 2.0 {
            let _ = c2.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {}",(n%100_000)+1));
            n += 1;
        }
        let r = n as f64 / t.elapsed().as_secs_f64();
        println!("{tag:12} {r:>9.0} ops/s（{:.1}µs/op）", 1e6/r);
    };
    run("Group(20ms)", dendro_core::Durability::Group);
    run("NoWait", dendro_core::Durability::NoWait);
    // perf 分解（Group 臂）
    dendro_core::perf::reset();
    let t = Instant::now();
    let mut n = 0u64;
    while t.elapsed().as_secs_f64() < 2.0 {
        let _ = c.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {}",(n%100_000)+1));
        n += 1;
    }
    println!("perf 分解（Group，{}op）：", n);
    for r in dendro_core::perf::report() {
        if r.count > 0 { println!("  {:<12} avg={:>6}ns total={:>7.1}ms", r.name, r.avg_ns, r.total_ms); }
    }
}
