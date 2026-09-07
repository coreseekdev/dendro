#![allow(clippy::all)]
//! SQL 语义回归（评审 §1.3 S 项）：
//! - S2：失败事务内后续语句被拒（25P02），ROLLBACK 放行（PG aborted-tx 语义）
//! - S6：WITH (CTE) 显式报错 0A000，不再静默丢弃
//! - S4：整数溢出报 22003（不回绕）；算术升宽 Int64（SPEC 07 偏离记录）

use dendro_core::{Database, DbOptions};

fn open_mem() -> std::sync::Arc<Database> {
    Database::open(DbOptions::memory()).unwrap()
}

#[test]
fn s2_aborted_transaction_rejects_statements_until_rollback() {
    let db = open_mem();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();

    s.exec("BEGIN").unwrap();
    // 事务内语句失败 → 事务进入 aborted 态
    let e1 = s.exec("INSERT INTO missing_table VALUES (1)").unwrap_err();
    assert_eq!(e1.state, "42P01");
    // 后续语句被拒（25P02），而非照常执行
    let e2 = s.exec("INSERT INTO t VALUES (2)").unwrap_err();
    assert_eq!(e2.state, "25P02", "失败事务内的语句应被拒绝");
    assert_eq!(e2.message, "current transaction is aborted, commands ignored until end of transaction block");
    // SELECT 也被拒（PG 语义：aborted 块内一切非回滚语句）
    let e3 = s.exec("SELECT count(*) FROM t").unwrap_err();
    assert_eq!(e3.state, "25P02");
    // ROLLBACK 放行，事务结束后恢复正常
    s.exec("ROLLBACK").unwrap();
    s.exec("INSERT INTO t VALUES (2)").unwrap();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(rs.text_rows()[0][0].as_deref(), Some("2"));
    }
}

#[test]
fn s2_commit_in_aborted_transaction_discards_writes() {
    // PG 语义：aborted 块上的 COMMIT = 丢弃（回报 ROLLBACK），不得私运半截写集
    let db = open_mem();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    s.exec("BEGIN").unwrap();
    s.exec("INSERT INTO t VALUES (2)").unwrap(); // 成功的前半截
    s.exec("INSERT INTO missing_table VALUES (9)").unwrap_err(); // 后半截失败
    let o = s.exec("COMMIT").unwrap();
    match &o[0] {
        dendro_core::Output::Command { tag, .. } => assert_eq!(tag, "ROLLBACK", "aborted 块 COMMIT 应回报 ROLLBACK"),
        other => panic!("expected Command, got {other:?}"),
    }
    // 半截写集（id=2）必须被丢弃
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1"), "aborted 事务的写集必须整体丢弃");
    }
}

#[test]
fn s6_with_cte_reports_feature_not_supported() {
    let db = open_mem();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    // 曾被静默丢弃（WHERE 丢失 → 全表）：现在必须显式 0A000
    let e = s
        .exec("WITH c AS (SELECT id FROM t) SELECT * FROM c")
        .unwrap_err();
    assert_eq!(e.state, "0A000", "WITH 必须显式不支持而非静默改写语义");
}

#[test]
fn s4_integer_overflow_reports_22003_not_wraparound() {
    let db = open_mem();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    // 升宽 Int64：int4 上界 +1 精确（不截断回绕为负数）
    let o = s.exec("SELECT 2147483647 + 1 FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(rs.text_rows()[0][0].as_deref(), Some("2147483648"), "整数算术升宽 Int64");
    }
    // i64 溢出 → 22003 out of range
    let e = s.exec("SELECT 9223372036854775807 + 1 FROM t").unwrap_err();
    assert_eq!(e.state, "22003", "i64 溢出应为 out of range 而非回绕/internal");
}

#[test]
fn r7_2_read_only_rejects_catalog_writes() {
    // 第七轮 R7-2：只读副本此前可执行 DROP BRANCH（manifest CAS 在副本上
    // 成功 → 持久删除分支 + 墓碑化对象）。守卫在 update_manifest 单一咽喉。
    let dir = std::env::temp_dir().join(format!("dendro-ro-cat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Database::open(DbOptions {
            store: dendro_core::StoreConfig::LocalDir(dir.clone()),
            ..DbOptions::default()
        })
        .unwrap();
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("CREATE BRANCH b2 FROM main").unwrap();
    }
    let ro = Database::open(DbOptions {
        store: dendro_core::StoreConfig::LocalDir(dir.clone()),
        read_only: true,
        ..DbOptions::default()
    })
    .unwrap();
    let mut s = ro.new_session();
    for sql in ["DROP BRANCH b2", "CREATE TABLE x (id BIGINT PRIMARY KEY)"] {
        let e = match s.exec(sql) {
            Ok(_) => panic!("只读副本不应允许：{sql}"),
            Err(e) => e,
        };
        assert_eq!(e.state, "25006", "{sql}: {e}");
    }
    // 数据完好：正常实例重新打开，b2 仍在
    drop(ro);
    let w = Database::open(DbOptions {
        store: dendro_core::StoreConfig::LocalDir(dir.clone()),
        ..DbOptions::default()
    })
    .unwrap();
    let mut s = w.new_session();
    s.exec("USE BRANCH b2").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn r7_3_explicit_txn_read_visibility_frozen() {
    // 第七轮 R7-3：显式事务内树的可见性以 BEGIN 冻结的 catalog 根为准，
    // 不随并发 checkpoint 推进翻转（此前同 一 count(*) 在事务内从空翻 1）。
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    }
    let mut s = db.new_session();
    s.exec("BEGIN").unwrap();
    let q = |s: &mut dendro_core::Session| -> String {
        match &s.exec("SELECT count(*) FROM t").unwrap()[0] {
            dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
            _ => panic!(),
        }
    };
    assert_eq!(q(&mut s), "0", "事务内初始不可见");
    // 并发提交 + checkpoint（另一会话推进树）
    {
        let mut s2 = db.new_session();
        s2.exec("INSERT INTO t VALUES (1)").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    assert_eq!(q(&mut s), "0", "显式事务内可见性被 checkpoint 翻转");
    s.exec("COMMIT").unwrap();
    // 新快照可见
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    match &o[0] {
        dendro_core::Output::Rows(rs) => assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1")),
        _ => panic!(),
    }
}

#[test]
fn r8_1_explicit_txn_reads_own_writes() {
    // 第八轮 R8-1：SQL 读路径从不合并 sess.txn.writes——
    // BEGIN;INSERT 后 SELECT 看不到、BEGIN;DELETE 后 UPDATE 空转且 COMMIT
    // 后被删行复活。显式事务必须读自己的写。
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    s.exec("INSERT INTO t VALUES (1, 'a')").unwrap();
    s.exec("BEGIN").unwrap();
    s.exec("INSERT INTO t VALUES (2, 'b')").unwrap();
    // 读自己的 INSERT
    let q = |s: &mut dendro_core::Session, sql: &str| -> Vec<Vec<String>> {
        match &s.exec(sql).unwrap()[0] {
            dendro_core::Output::Rows(rs) => rs
                .text_rows()
                .iter()
                .map(|r| r.iter().map(|c| c.clone().unwrap_or_default()).collect())
                .collect(),
            _ => panic!(),
        }
    };
    assert_eq!(q(&mut s, "SELECT count(*) FROM t")[0][0], "2", "事务内读自己的 INSERT");
    assert_eq!(q(&mut s, "SELECT v FROM t WHERE id = 2")[0][0], "b", "事务内新行可点查");
    // 读自己的 DELETE：行消失，且 UPDATE 空转（匹配 0 行）而非报错/复活
    s.exec("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(q(&mut s, "SELECT count(*) FROM t")[0][0], "1");
    assert_eq!(q(&mut s, "SELECT id FROM t WHERE id = 1").len(), 0, "事务内被删行不可见");
    s.exec("UPDATE t SET v = 'x' WHERE id = 1").unwrap(); // 匹配 0 行，不报错
    assert_eq!(q(&mut s, "SELECT count(*) FROM t")[0][0], "1");
    // 读自己的 UPDATE
    s.exec("UPDATE t SET v = 'B' WHERE id = 2").unwrap();
    assert_eq!(q(&mut s, "SELECT v FROM t WHERE id = 2")[0][0], "B");
    s.exec("COMMIT").unwrap();
    // COMMIT 后与事务内一致（无幽灵行、写入持久）
    assert_eq!(q(&mut s, "SELECT count(*) FROM t")[0][0], "1");
    assert_eq!(q(&mut s, "SELECT v FROM t WHERE id = 2")[0][0], "B");
    assert_eq!(q(&mut s, "SELECT id FROM t WHERE id = 1").len(), 0);
}
