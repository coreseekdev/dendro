//! 前置1（ir-spec 08 §0.1 / 评审 P1-1）：DbSnapshot 的 manifest version
//! 不滞后。update_manifest 曾存入提交前克隆（version 旧值）——本进程
//! DDL 后快照 version 滞后一次写，"读快照取 version"原语不成立
//! （L2 计划缓存失效键 / CatalogCache 换新的依赖，ir-spec 06）。

use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn tmp_db(tag: &str) -> Arc<Database> {
    let dir = std::env::temp_dir().join(format!(
        "dendro-snapver-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn snapshot_version_advances_exactly_once_per_ddl() {
    let db = tmp_db("exact");
    let mut s = db.new_session();
    let v0 = db.snapshot_version();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    let v1 = db.snapshot_version();
    assert_eq!(
        v1,
        v0 + 1,
        "DDL 后快照 version 必须恰好 +1（旧 bug：滞后一次写）"
    );
    s.exec("ALTER TABLE t ADD COLUMN v INT").unwrap();
    let v2 = db.snapshot_version();
    assert_eq!(v2, v1 + 1, "连续 DDL 逐次 +1");
    // 非 DDL（数据路径）不得推进
    s.exec("INSERT INTO t VALUES (1, 10)").unwrap();
    assert_eq!(db.snapshot_version(), v2, "INSERT 不推进 version");
}

#[test]
fn snapshot_version_matches_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "dendro-snapver-reopen-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let v_final;
    {
        let db = Database::open(DbOptions {
            store: StoreConfig::LocalDir(dir.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut s = db.new_session();
        s.exec("CREATE TABLE a (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("CREATE TABLE b (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("CREATE TABLE c (id BIGINT PRIMARY KEY)").unwrap();
        v_final = db.snapshot_version();
    }
    // 重开：load_latest 是权威版本——快照值必须与之一致（旧 bug 下差 1）
    let db2 = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        db2.snapshot_version(),
        v_final,
        "重开（load_latest 权威）与关库前快照必须一致——滞后即不一致"
    );
}
