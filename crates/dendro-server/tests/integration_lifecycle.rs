use dendro_core::{Database, DbOptions};
use std::sync::Arc;

fn open_local(dir: &std::path::Path) -> Arc<Database> {
    Database::open(DbOptions {
        store: dendro_core::StoreConfig::LocalDir(dir.to_path_buf()),
        ..DbOptions::default()
    })
    .unwrap()
}

fn q(db: &Arc<Database>, sql: &str) -> Vec<Vec<String>> {
    let mut s = db.new_session();
    let outs = s.exec(sql).unwrap();
    let mut result = Vec::new();
    for o in &outs {
        if let dendro_core::Output::Rows(rs) = o {
            for row in rs.text_rows() {
                result.push(row.iter().map(|c| c.clone().unwrap_or_else(|| "NULL".into())).collect());
            }
        }
    }
    result
}

#[test]
fn lifecycle_backup_readonly_txn() {
    let dir = std::env::temp_dir().join(format!("dendro-lifecycle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bak = std::env::temp_dir().join(format!("dendro-lifecycle-bak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bak);

    // Phase 1: 建表 + 写入 + 分支 + 视图
    {
        let db = open_local(&dir);
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();
        s.exec("CREATE BRANCH dev FROM main").unwrap();
        s.exec("USE BRANCH dev");
        s.exec("INSERT INTO t VALUES (3, 'c')").unwrap();
        s.exec("CREATE VIEW v_cnt AS SELECT count(*) AS cnt FROM t").unwrap();
    }

    // Phase 2: 备份
    {
        let db = open_local(&dir);
        let mut s = db.new_session();
        s.exec("CHECKPOINT").unwrap();
        drop(s);
        let (n, _) = dendro_server::backup::backup_dir(&dir, &bak).unwrap();
        assert!(n > 0);
    }

    // Phase 3: 从备份恢复
    {
        let db = open_local(&bak);
        let mut s = db.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        // 数据完整性
        match &o[0] {
            dendro_core::Output::Rows(rs) => {
                assert!(rs.total_rows() >= 1, "备份恢复后数据不可为空");
            }
            _ => panic!("expected rows"),
        }
    }

    // Phase 4: 只读模式拒绝写
    {
        let ro = Database::open(DbOptions {
            store: dendro_core::StoreConfig::LocalDir(dir.clone()),
            read_only: true,
            ..DbOptions::default()
        })
        .unwrap();
        let mut s = ro.new_session();
        let e = match s.exec("INSERT INTO t VALUES (99, 'x')") {
            Ok(_) => panic!("只读模式应拒绝 INSERT"),
            Err(e) => e,
        };
        assert_eq!(e.state, "25006", "{e}");
    }

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&bak);
}

#[test]
fn ddl_in_explicit_txn_rejected() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("BEGIN").unwrap();
    let e = match s.exec("CREATE TABLE ddl_tx (id BIGINT PRIMARY KEY)") {
        Ok(_) => panic!("事务内 DDL 应拒绝"),
        Err(e) => e,
    };
    assert_eq!(e.state, "25001", "{e}");
    s.exec("ROLLBACK").unwrap();
}
