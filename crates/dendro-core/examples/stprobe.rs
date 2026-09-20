//! 存储层天花板探针：裸树点查 vs SQL 点查路径（memtx 驻留 vs 树驻留）
//! 回答：A/B 持平是"存储无差"还是"SQL 层封顶"
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::time::Instant;

fn bench<F: FnMut(u64)>(mut f: F) -> f64 {
    for i in 0..10_000u64 {
        f(i);
    } // warmup
    let mut rates = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed().as_secs_f64() < 1.0 {
            f(n);
            n += 1;
        }
        rates.push(n as f64 / t.elapsed().as_secs_f64());
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    rates[2]
}

fn main() {
    let dir = std::env::var("PROBE_DB").unwrap_or("/tmp/dendro-tp10m".into());
    let mut c = Connection::open_with(
        &dir,
        DbOptions::embedded(StoreConfig::LocalDir(dir.clone().into())),
    )
    .unwrap();
    // 0) 裸树点查（绕过 SQL——存储层天花板）
    let db = c.db().clone();
    let raw_tree = bench(|i| {
        let _ = db.debug_tree_get("main", "t", ((i * 7919) % 10_000_000 + 1) as i64);
    });
    println!(
        "裸树点查 × 10M(页缓存热):  {raw_tree:.0} ops/s（{:.2}µs/op）",
        1e6 / raw_tree
    );
    // 1) 裸树点查（绕过 SQL）
    // 通过 SQL 侧建 memtx 驻留对照表 m（不 checkpoint）
    let _ = c.execute("DROP TABLE m");
    c.execute("CREATE TABLE m (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)")
        .unwrap();
    for ch in 0..10 {
        let vals: Vec<String> = (0..10_000)
            .map(|i| {
                let id = ch * 10_000 + i + 1;
                format!("({id}, {}, 'x')", id % 7)
            })
            .collect();
        c.execute(&format!("INSERT INTO m VALUES {}", vals.join(",")))
            .unwrap();
    }
    // 2) SQL 路径 × 两存储
    let sql_tree = bench(|i| {
        let _ = c.query(&format!(
            "SELECT v FROM t WHERE id = {}",
            (i * 7919) % 10_000_000 + 1
        ));
    });
    let sql_memtx = bench(|i| {
        let _ = c.query(&format!(
            "SELECT v FROM m WHERE id = {}",
            (i * 7919) % 100_000 + 1
        ));
    });
    println!(
        "SQL 点查 × 树驻留(10M):  {sql_tree:.0} ops/s（{:.1}µs/op）",
        1e6 / sql_tree
    );
    println!(
        "SQL 点查 × memtx驻留(100K): {sql_memtx:.0} ops/s（{:.1}µs/op）",
        1e6 / sql_memtx
    );
    println!(
        "SQL 层封顶判定：树/memtx 比 = {:.2}（≈1 ⇒ SQL 层封顶）",
        sql_tree / sql_memtx
    );
    // 统一 perf 框架拆解（SQL×树 臂的 29µs 去向）
    dendro_core::perf::reset();
    let _re = bench(|i| {
        let _ = c.query(&format!(
            "SELECT v FROM t WHERE id = {}",
            (i * 7919) % 10_000_000 + 1
        ));
    });
    println!("\n== perf 拆解（SQL×树，统一框架内；含包装层）==");
    for r in dendro_core::perf::report() {
        if r.count > 0 {
            println!(
                "  {:<12} count={:<9} avg={:>6}ns total={:>8.1}ms",
                r.name, r.count, r.avg_ns, r.total_ms
            );
        }
    }
    // 3) find_by_pk 快路径（SQL 层绕过）
    let fp = bench(|i| {
        let _ = c.find_by_pk("t", ((i * 7919) % 10_000_000 + 1) as i64);
    });
    println!(
        "find_by_pk × 树(10M):      {fp:.0} ops/s（{:.2}µs/op，SQL 路径 {:.1}×）",
        1e6 / fp,
        sql_tree / fp
    );
}
// （追加）update 分解 + 双限流探测：mode 参数 upd 时输出
