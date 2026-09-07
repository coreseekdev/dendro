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
