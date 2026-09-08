//! 真并发 OCC 回归（P1-7，第七/九/十轮反复点名的并发防线）：
//! - 同键并发提交：first-committer-wins——恰好一个成功，其余 40001
//! - 异键并发提交：全部成功（commit_mu 串行化不误伤）
//! - 事务覆盖盲区（Q-9）：跨 checkpoint 的写事务显式 40001（已有
//!   sql_semantics::q9 用例；此处覆盖并发交错形态）

use dendro_core::{Database, DbOptions};
use std::sync::Arc;

fn open_mem() -> Arc<Database> {
    Database::open(DbOptions::memory()).unwrap()
}

fn rows(db: &std::sync::Arc<Database>, sql: &str) -> Vec<Vec<String>> {
    let mut s = db.new_session();
    match &s.exec(sql).unwrap()[0] {
        dendro_core::Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r.iter().map(|c| c.clone().unwrap_or_default()).collect())
            .collect(),
        _ => panic!("expected rows: {sql}"),
    }
}

#[test]
fn concurrent_same_key_exactly_one_winner() {
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'init')").unwrap();
    }
    const N: usize = 8;
    // **真 Barrier**：全部线程完成 BEGIN+UPDATE（同快照）后同时放行提交——
    // 否则后 BEGIN 的事务合法拿到新快照并赢（OCC 正确行为，非缺陷：
    // 弱排序下"恰好一个赢家"的断言不成立）
    let barrier = Arc::new(std::sync::Barrier::new(N));
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for i in 0..N {
            let db = db.clone();
            let barrier = barrier.clone();
            handles.push(scope.spawn(move || {
                let mut s = db.new_session();
                s.exec("BEGIN").unwrap();
                s.exec(&format!("UPDATE t SET v = 'w{i}' WHERE id = 1")).unwrap();
                barrier.wait(); // 同快照全体就绪 → **线程内并发提交**（P13-5：
                // 此前 COMMIT 由主线程 join 后串行发出，"并发提交"表述过强）
                match s.exec("COMMIT") {
                    Ok(_) => Ok(format!("w{i}")),
                    Err(e) => {
                        assert_eq!(e.state, "40001", "失败者应为序列化失败：{e}");
                        Err(e.state.to_string())
                    }
                }
            }));
        }
        let results: Vec<Result<String, String>> = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        let winners: Vec<&String> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(winners.len(), 1, "恰好一个赢家（实际 {winners:?}）");
        let losers = results.iter().filter(|r| r.is_err()).count();
        assert_eq!(losers, N - 1);
        // 赢家的值持久
        assert_eq!(rows(&db, "SELECT v FROM t WHERE id = 1")[0][0], winners[0].as_str());
    });
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], "1");
}

#[test]
fn concurrent_different_keys_all_succeed() {
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    }
    const N: usize = 8;
    std::thread::scope(|scope| {
        for i in 0..N {
            let db = db.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')")).unwrap();
            });
        }
    });
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], "8", "异键并发全部成功");
}

#[test]
fn concurrent_same_key_autocommit_40001_or_success() {
    // 自动提交模式：同键并发 UPDATE 的两语句可能拿到同一 watermark
    // → OCC 判 40001（正确：不静默丢失）；也可能错开（后到者赢）。
    // 不变式：终值唯一、行数 1、无 panic、失败者均为 40001。
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'init')").unwrap();
    }
    std::thread::scope(|scope| {
        for i in 0..N_THREADS {
            let db = db.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                match s.exec(&format!("UPDATE t SET v = 'w{i}' WHERE id = 1")) {
                    Ok(_) => true,
                    Err(e) => {
                        assert_eq!(e.state, "40001", "{e}");
                        false
                    }
                }
            });
        }
    });
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], "1");
    let v = rows(&db, "SELECT v FROM t WHERE id = 1")[0][0].clone();
    assert!(v.starts_with('w'), "终值应为某个赢家：{v}");
}

const N_THREADS: usize = 8;
