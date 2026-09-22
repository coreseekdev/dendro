//! 可扩展索引元数据面测试（PR3 验收）：CREATE/DROP INDEX 语句、
//! entry.indexes 持久化、schema_version bump、重复/不存在/多表歧义。

use dendro_core::{Database, DbOptions, StoreConfig};

fn db() -> (Arc<Database>, dendro_core::engine::Session) {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        mem_sample_interval_ms: 0,
        ..Default::default()
    })
    .unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)").unwrap();
    (db, s)
}

#[test]
fn create_drop_and_metadata_persist() {
    let (db, mut s) = db();
    let v0 = db.schema_version();
    s.exec("CREATE UNIQUE INDEX ix_v ON t (v)").unwrap();
    assert_eq!(db.schema_version(), v0 + 1);
    s.exec("CREATE INDEX ix_tag ON t (tag)").unwrap();
    assert_eq!(db.schema_version(), v0 + 2);

    // 元数据经 checkpoint 存活（重复创建仍冲突 = 索引清单还在）
    db.checkpoint_branch("main").unwrap();
    let e = s.exec("CREATE INDEX ix_v ON t (v)").unwrap_err();
    assert!(e.message.contains("already"), "{e:?}");

    // DROP（表限定形态）后再建同名成功
    s.exec("DROP INDEX t.ix_v").unwrap();
    assert_eq!(db.schema_version(), v0 + 3);
    s.exec("CREATE INDEX ix_v ON t (v)").unwrap(); // 同名再建 ✓
    s.exec("DROP INDEX ix_v").unwrap();
    // 纯名 DROP（唯一命中）
    s.exec("DROP INDEX ix_tag").unwrap();
    let e = s.exec("DROP INDEX ix_tag").unwrap_err();
    assert_eq!(e.state, "42704");
}

#[test]
fn duplicate_missing_and_if_not_exists() {
    let (_db, mut s) = db();
    s.exec("CREATE INDEX ix ON t (v)").unwrap();
    // 重复
    let e = s.exec("CREATE INDEX ix ON t (v)").unwrap_err();
    assert!(e.state == "23505" || e.state == "42P07" || e.message.contains("already"), "{e:?}");
    // IF NOT EXISTS 静默
    s.exec("CREATE INDEX IF NOT EXISTS ix ON t (v)").unwrap();
    // 不存在
    let e = s.exec("DROP INDEX nope").unwrap_err();
    assert_eq!(e.state, "42704");
    // 未知列
    let e = s.exec("CREATE INDEX bad ON t (missing_col)").unwrap_err();
    assert!(e.message.contains("missing_col"), "{e:?}");
    // 多列索引
    s.exec("CREATE INDEX multi ON t (v, tag)").unwrap();
}

#[test]
fn same_name_on_two_tables_ambiguous_drop() {
    let (_db, mut s) = db();
    s.exec("CREATE TABLE u (id BIGINT PRIMARY KEY, w BIGINT)").unwrap();
    s.exec("CREATE INDEX ix ON t (v)").unwrap();
    s.exec("CREATE INDEX ix ON u (w)").unwrap();
    // 纯名 DROP 歧义
    let e = s.exec("DROP INDEX ix").unwrap_err();
    assert_eq!(e.state, "42702");
    // 表限定消除歧义
    s.exec("DROP INDEX u.ix").unwrap();
}

use std::sync::Arc;
