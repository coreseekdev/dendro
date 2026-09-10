use dendro_core::{Database, DbOptions, StoreConfig};
#[test]
fn dbg_recovery() {
    let dir = std::env::temp_dir().join(format!("dendro-dbg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Database::open(DbOptions {
            store: StoreConfig::LocalDir(dir.clone()),
            wal_flush_interval_ms: 10,
            ..Default::default()
        })
        .unwrap();
        let mut s = db.new_session();
        s.exec("CREATE TABLE d (id BIGINT PRIMARY KEY)").unwrap();
        for i in 0..100 {
            s.exec(&format!("INSERT INTO d VALUES ({i})")).unwrap();
        }
        s.exec("CHECKPOINT").unwrap();
        s.exec("INSERT INTO d VALUES (99999)").unwrap();
        // drop without graceful close (模拟崩溃)
    }
    println!("wal files:");
    fn walk(p: &std::path::Path, depth: usize) {
        if depth > 3 {
            return;
        }
        for e in std::fs::read_dir(p).unwrap().flatten() {
            let ep = e.path();
            println!("  {}", ep.display());
            if ep.is_dir() {
                walk(&ep, depth + 1);
            }
        }
    }
    walk(&dir.join("wal"), 0);
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir.clone()),
        wal_flush_interval_ms: 10,
        ..Default::default()
    })
    .unwrap();
    let mut s = db.new_session();
    let o = s.exec("SELECT count(*) FROM d").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        println!("count={:?}", rs.text_rows());
    }
    let snap = db.manifest();
    for (n, h) in &snap.manifest.refs {
        println!(
            "ref {n}: wal_seg={} epoch={} covered={}",
            h.wal_seg, h.epoch, h.covered_seq
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
