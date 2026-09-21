//! 未 checkpoint 表的点查早退验证：同样 10 万行，checkpoint 前后点查速率
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::time::Instant;

fn bench<F: FnMut(u64)>(mut f: F) -> f64 {
    for i in 0..3000u64 { f(i); }
    let mut r = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed().as_secs_f64() < 1.0 { f(n); n += 1; }
        r.push(n as f64 / t.elapsed().as_secs_f64());
    }
    r.sort_by(|a, b| a.partial_cmp(&b).unwrap());
    r[2]
}

fn main() {
    let dir = std::env::temp_dir().join(format!("pearly-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // 阈值放大：不触发自动 checkpoint——"未物化表"场景
    let mut opts = DbOptions::embedded(StoreConfig::Memory);
    opts.checkpoint_threshold_bytes = 4 << 30;
    let mut c = Connection::open_with(&dir, opts).unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)").unwrap();
    // 连续插入 10 万行（跳批会漏 id——校验点查必须全连续）
    let mut id: u64 = 0;
    while id < 100_000 {
        let vals: Vec<String> = (0..10_000u64)
            .map(|i| format!("({}, {}, 'tag')", id + i + 1, (id + i) % 7))
            .collect();
        c.execute(&format!("INSERT INTO t VALUES {}", vals.join(","))).unwrap();
        id += 10_000;
    }
    // 裸 memtx 分解：snapshot/key/get/latest/visible
    {
        use dendro_core::types::SqlValue;
        let b = c.db().branch("main").unwrap();
        let tm = b.mem.table(1);
        let key = dendro_core::format::row::encode_key(&[SqlValue::Int64(50000)]);
        let snap = b.snapshot();
        println!("裸分解: key={:02x?} snap={:#x} get={:?} latest={:?} visible_ts={:?} entries={}",
            key, snap,
            tm.get(&key, snap).map(|v| v.len()),
            tm.latest_ts(&key),
            tm.visible_ts(&key, snap),
            tm.len());
    }
    // head 稳定性实证：INSERT 后 head.root 是否固定（resolve 缓存键）
    {
        let b = c.db().branch("main").unwrap();
        let h1 = b.head.load_full();
        let _ = c.query("SELECT v, tag FROM t WHERE id = 1");
        let h2 = b.head.load_full();
        let _ = c.query("SELECT v, tag FROM t WHERE id = 2");
        let h3 = b.head.load_full();
        println!("head 根: {:?} -> {:?} -> {:?}",
            h1.as_ref().as_ref().map(|x| x.root.as_u64()),
            h2.as_ref().as_ref().map(|x| x.root.as_u64()),
            h3.as_ref().as_ref().map(|x| x.root.as_u64()));
    }
    // 冷 key 裸 get（随机 10 万——SQL point 的真实键分布）
    {
        use dendro_core::types::SqlValue;
        let b = c.db().branch("main").unwrap();
        let tm = b.mem.table(1);
        let snap = b.snapshot();
        let keys: Vec<Vec<u8>> = (0..100_000u64)
            .map(|i| {
                let id = (i.wrapping_mul(7919) % 100_000) + 1;
                dendro_core::format::row::encode_key(&[SqlValue::Int64(id as i64)])
            })
            .collect();
        let t = std::time::Instant::now();
        let mut ops = 0u64;
        while t.elapsed().as_secs_f64() < 2.0 {
            for k in &keys {
                let _ = tm.get(k, snap);
                ops += 1;
            }
        }
        println!("memtx 冷 get: {:.0}ns/次", t.elapsed().as_secs_f64() / ops as f64 * 1e9);
    }
    let r = c.query("SELECT v, tag FROM t WHERE id = 50000").unwrap();
    println!("未物化 point 校验: {} 行 {:?}", r.rows().len(), r.rows().first().map(|x| format!("{:?}", x)));
    let p1 = bench(|i| { let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {}", (i%100_000)+1)); });
    println!("未物化 point: {p1:.0} ops/s ({:.2}µs)", 1e6/p1);
    // range100（mem 臂形态：~12 万驻留 + 每 100 行范围查询）
    {
        let r = bench(|i| {
            let lo = (i.wrapping_mul(7919) % 90_000) + 1;
            let _ = c.query(&format!("SELECT id, v FROM t WHERE id >= {lo} AND id < {}", lo + 100));
        });
        let cnt = c.query("SELECT id, v FROM t WHERE id >= 500 AND id < 600").unwrap();
        println!("range100（未物化驻留）: {r:.0} ops/s（校验 500-600 = {} 行）", cnt.rows().len());
    }
    c.execute("CHECKPOINT").unwrap();
    let p2 = bench(|i| { let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {}", (i%100_000)+1)); });
    println!("物化后 point: {p2:.0} ops/s ({:.2}µs)", 1e6/p2);
}
