//! 单条 UPDATE 耗时分解（fetch 点查 vs txn 提交）
use dendro_core::{DbOptions, StoreConfig};
use std::sync::Arc;
use std::time::Instant;
fn main() {
    let dir = std::env::var("PROBE_DB").unwrap_or("/tmp/dendro-tpbench".into());
    let db = dendro_core::Database::open(DbOptions::embedded(StoreConfig::LocalDir(
        dir.clone().into(),
    )))
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 1 << 20,
    }));
    let mut s = db.new_session();
    // 点查对照
    for i in 1..=3 {
        let t = Instant::now();
        let _ = s
            .exec(&format!("SELECT v, tag FROM t WHERE id = {i}"))
            .unwrap();
        eprintln!("SELECT id={i}: {:.1}ms", t.elapsed().as_secs_f64() * 1e3);
    }
    for i in 1..=5 {
        let t = Instant::now();
        let n = s
            .exec(&format!("UPDATE t SET v = v + 1 WHERE id = {i}"))
            .unwrap();
        let el = t.elapsed().as_secs_f64() * 1e3;
        let affected = n
            .iter()
            .map(|o| match o {
                dendro_core::types::Output::Command { affected, .. } => *affected,
                _ => 0,
            })
            .sum::<u64>();
        eprintln!("UPDATE id={i}: {el:.1}ms (affected={affected})",);
    }
    // embed Connection 对照（tp-bench 的路径——SQLite 方言）
    {
        use dendro_core::embed::Connection;
        let mut c = Connection::open_with(
            &dir,
            DbOptions::embedded(StoreConfig::LocalDir(dir.clone().into())),
        )
        .unwrap();
        for i in 1..=5 {
            let t = Instant::now();
            let n = c
                .execute(&format!("UPDATE t SET v = v + 1 WHERE id = {i}"))
                .unwrap();
            eprintln!(
                "embed UPDATE id={i}: {:.1}ms (affected={n})",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}
