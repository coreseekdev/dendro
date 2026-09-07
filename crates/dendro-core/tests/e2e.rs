#![allow(clippy::all)]
//! 端到端冒烟测试：建表 → 插入 → 查询 → 分支 → 合并 → 恢复。

use dendro_core::{Database, DbOptions, SqlValue, StoreConfig};
type Row = Vec<String>;

fn open_mem() -> std::sync::Arc<Database> {
    Database::open(DbOptions::memory()).unwrap()
}

fn rows(outs: &[dendro_core::Output]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for o in outs {
        if let dendro_core::types::Output::Rows(rs) = o {
            for r in rs.text_rows() {
                out.push(r.into_iter().map(|c| c.unwrap_or_else(|| "NULL".into())).collect());
            }
        }
    }
    out
}

fn exec(_db: &std::sync::Arc<Database>, s: &mut dendro_core::Session, sql: &str) -> Vec<Vec<String>> {
    let outs = s.exec(sql).unwrap();
    rows(&outs)
}

#[test]
fn smoke_create_insert_select() {
    let db = open_mem();
    let mut s = db.new_session();
    exec(&db, &mut s, "CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT NOT NULL, score DOUBLE)");
    exec(&db, &mut s, "INSERT INTO t VALUES (1, 'a', 1.5), (2, 'b', 2.5), (3, 'c', 3.5)");
    let r = exec(&db, &mut s, "SELECT id, v, score FROM t ORDER BY id");
    let expect: Vec<Row> = vec![
        vec!["1".into(), "a".into(), "1.5".into()],
        vec!["2".into(), "b".into(), "2.5".into()],
        vec!["3".into(), "c".into(), "3.5".into()],
    ];
    assert_eq!(r, expect);
    // 过滤 + 表达式
    let r = exec(&db, &mut s, "SELECT v FROM t WHERE score > 2 ORDER BY v");
        let expect: Vec<Row> = vec![vec!["b".into()], vec!["c".into()]];
    assert_eq!(r, expect);
    // 聚合
    let r = exec(&db, &mut s, "SELECT count(*), sum(score), avg(score) FROM t");
    assert_eq!(r[0][0], "3");
    // UPDATE / DELETE
    exec(&db, &mut s, "UPDATE t SET score = 9.5 WHERE id = 1");
    let r = exec(&db, &mut s, "SELECT score FROM t WHERE id = 1");
    let expect: Vec<Row> = vec![vec!["9.5".into()]];
    assert_eq!(r, expect);
    exec(&db, &mut s, "DELETE FROM t WHERE id = 2");
    let r = exec(&db, &mut s, "SELECT count(*) FROM t");
    assert_eq!(r[0][0], "2");
}

#[test]
fn smoke_branch_and_merge() {
    let db = open_mem();
    let mut s = db.new_session();
    exec(&db, &mut s, "CREATE TABLE kv (k BIGINT PRIMARY KEY, val TEXT)");
    exec(&db, &mut s, "INSERT INTO kv VALUES (1, 'base')");
    // 分支
    exec(&db, &mut s, "CREATE BRANCH agent42 FROM main");
    exec(&db, &mut s, "USE BRANCH agent42");
    exec(&db, &mut s, "INSERT INTO kv VALUES (2, 'from-agent')");
    // main 不可见
    exec(&db, &mut s, "USE BRANCH main");
    let r = exec(&db, &mut s, "SELECT count(*) FROM kv");
    assert_eq!(r[0][0], "1");
    // agent42 可见
    exec(&db, &mut s, "USE BRANCH agent42");
    let r = exec(&db, &mut s, "SELECT count(*) FROM kv");
    assert_eq!(r[0][0], "2");
    // 合并到 main
    exec(&db, &mut s, "USE BRANCH main");
    exec(&db, &mut s, "MERGE BRANCH agent42 INTO main");
    let r = exec(&db, &mut s, "SELECT count(*) FROM kv");
    assert_eq!(r[0][0], "2");
    let r = exec(&db, &mut s, "SELECT val FROM kv WHERE k = 2");
    let expect: Vec<Row> = vec![vec!["from-agent".into()]];
    assert_eq!(r, expect);
}

#[test]
fn smoke_durability_and_recovery() {
    let dir = std::env::temp_dir().join(format!("dendro-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Database::open(DbOptions {
            store: StoreConfig::LocalDir(dir.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut s = db.new_session();
        exec(&db, &mut s, "CREATE TABLE d (id BIGINT PRIMARY KEY, tag TEXT)");
        for i in 0..100 {
            s.exec(&format!("INSERT INTO d VALUES ({i}, 'v{i}')")).unwrap();
        }
        // 显式 checkpoint 落树
        exec(&db, &mut s, "CHECKPOINT");
        // checkpoint 后再写（仅 WAL）
        s.exec("INSERT INTO d VALUES (999, 'tail')").unwrap();
    }
    // 重新打开（恢复）
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir.clone()),
        ..Default::default()
    })
    .unwrap();
    let mut s = db.new_session();
    let r = exec(&db, &mut s, "SELECT count(*) FROM d");
    assert_eq!(r[0][0], "101", "checkpoint 数据 + WAL 尾部都应恢复");
    let r = exec(&db, &mut s, "SELECT tag FROM d WHERE id = 999");
    let expect: Vec<Row> = vec![vec!["tail".into()]];
    assert_eq!(r, expect);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn smoke_prepared() {
    let db = open_mem();
    let mut s = db.new_session();
    exec(&db, &mut s, "CREATE TABLE p (id BIGINT PRIMARY KEY, v TEXT)");
    exec(&db, &mut s, "INSERT INTO p VALUES (1, 'x'), (2, 'y')");
    let meta = s.prepare("q", "SELECT v FROM p WHERE id = $1", &[]).unwrap();
    assert_eq!(meta.param_types.len(), 1);
    let out = s.exec_prepared("q", &[SqlValue::Int64(2)]).unwrap();
    let rs = match out {
        dendro_core::types::Output::Rows(rs) => rs,
        _ => panic!("expected rows"),
    };
    assert_eq!(rs.text_rows(), vec![vec![Some("y".into())]]);
}

#[test]
fn smoke_conflict_merge() {
    let db = open_mem();
    let mut s = db.new_session();
    exec(&db, &mut s, "CREATE TABLE c (k BIGINT PRIMARY KEY, v TEXT)");
    exec(&db, &mut s, "INSERT INTO c VALUES (1, 'base')");
    exec(&db, &mut s, "CREATE BRANCH b2 FROM main");
    // main 改 k=1 -> M
    exec(&db, &mut s, "UPDATE c SET v = 'M' WHERE k = 1");
    exec(&db, &mut s, "CHECKPOINT");
    // b2 改 k=1 -> B
    exec(&db, &mut s, "USE BRANCH b2");
    exec(&db, &mut s, "UPDATE c SET v = 'B' WHERE k = 1");
    exec(&db, &mut s, "CHECKPOINT");
    // 合并应报冲突
    exec(&db, &mut s, "USE BRANCH main");
    let err = s.exec("MERGE BRANCH b2 INTO main").unwrap_err();
    assert_eq!(err.state, "40001");
}
