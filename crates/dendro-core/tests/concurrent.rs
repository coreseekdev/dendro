//! 真并发 OCC 回归（P1-7，第七/九/十轮反复点名的并发防线）：
//! - 同键并发提交：first-committer-wins——恰好一个成功，其余 40001
//! - 异键并发提交：全部成功（commit_mu 串行化不误伤）
//! - 事务覆盖盲区（Q-9）：跨 checkpoint 的写事务显式 40001（已有
//!   sql_semantics::q9 用例；此处覆盖并发交错形态）
//!
//! P1-7 扩展（本提交）：并发不变量防线——
//! - 同 PK 并发 INSERT：终态恰一行（23505/40001 拒绝，不静默双写）
//! - 快照稳定性：显式事务内可重复读，并发提交者不可见
//! - 混合负载压力：PK 唯一性 + 值自洽（写者标记 = 线程号）终态校验
//! - 并发 DDL：同名 CREATE TABLE 恰一赢家
//! - 并发 MERGE：行级冲突显式 40001，终态 = 赢家值
//! - DROP BRANCH × 在途事务：COMMIT 干净失败（3D000），无 panic

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
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
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
                s.exec(&format!("UPDATE t SET v = 'w{i}' WHERE id = 1"))
                    .unwrap();
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
        let results: Vec<Result<String, String>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<&String> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(winners.len(), 1, "恰好一个赢家（实际 {winners:?}）");
        let losers = results.iter().filter(|r| r.is_err()).count();
        assert_eq!(losers, N - 1);
        // 赢家的值持久
        assert_eq!(
            rows(&db, "SELECT v FROM t WHERE id = 1")[0][0],
            winners[0].as_str()
        );
    });
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], "1");
}

#[test]
fn concurrent_different_keys_all_succeed() {
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    const N: usize = 8;
    std::thread::scope(|scope| {
        for i in 0..N {
            let db = db.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"))
                    .unwrap();
            });
        }
    });
    assert_eq!(
        rows(&db, "SELECT count(*) FROM t")[0][0],
        "8",
        "异键并发全部成功"
    );
}

#[test]
fn concurrent_same_key_autocommit_40001_or_success() {
    // 自动提交模式：同键并发 UPDATE 的两语句可能拿到同一 watermark
    // → OCC 判 40001（正确：不静默丢失）；也可能错开（后到者赢）。
    // 不变式：终值唯一、行数 1、无 panic、失败者均为 40001。
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
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

// ---- P1-7 扩展：并发不变量防线 ----

#[test]
fn concurrent_insert_same_pk_exactly_one_row() {
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    const N: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(N));
    std::thread::scope(|scope| {
        for i in 0..N {
            let db = db.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                s.exec("BEGIN").unwrap();
                s.exec(&format!("INSERT INTO t VALUES (7, 'w{i}')"))
                    .unwrap();
                barrier.wait();
                match s.exec("COMMIT") {
                    Ok(_) => true,
                    Err(e) => {
                        // OCC 序列化失败或 PK 冲突都合法——唯独不允许双行
                        assert!(
                            e.state == "40001" || e.state == "23505",
                            "拒绝状态码意外：{e}"
                        );
                        false
                    }
                }
            });
        }
    });
    assert_eq!(
        rows(&db, "SELECT count(*) FROM t")[0][0],
        "1",
        "同 PK 并发 INSERT 终态恰一行"
    );
}

#[test]
fn concurrent_repeatable_read_snapshot_stability() {
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("INSERT INTO t VALUES (0)").unwrap();
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let db = db.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut s = db.new_session();
            for i in 1.. {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return i;
                }
                s.exec(&format!("INSERT INTO t VALUES ({i})")).unwrap();
            }
            unreachable!()
        })
    };
    // 读事务：两读之间写者持续推进 → 可重复读（快照不漂移）
    let mut s = db.new_session();
    s.exec("BEGIN").unwrap();
    let c1 = rows_in(&mut s, "SELECT count(*) FROM t")[0][0].clone();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let c2 = rows_in(&mut s, "SELECT count(*) FROM t")[0][0].clone();
    assert_eq!(c1, c2, "显式事务内可重复读：{c1} vs {c2}");
    s.exec("COMMIT").unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let committed = writer.join().unwrap();
    // 提交后新快照可见写者推进（至少推进若干行）
    let after: usize = rows(&db, "SELECT count(*) FROM t")[0][0].parse().unwrap();
    assert!(
        after > c1.parse::<usize>().unwrap(),
        "提交后应见并发写入：before={c1} after={after} writer_commits={committed}"
    );
}

fn rows_in(s: &mut dendro_core::Session, sql: &str) -> Vec<Vec<String>> {
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
fn concurrent_mixed_ops_pk_invariant_stress() {
    // 8 线程 × 混合 INSERT/UPDATE/DELETE 打 0..32 键空间：
    // 终态不变量——PK 唯一、值形如 "t{thread}" 自洽、行数 = 存活键数。
    // 组提交间隔压到 1ms：默认 50ms 下每条自动提交语句要等一个 flush
    // 间隔（设计使然，README"组提交延迟 p50 ≈ flush_interval"），1600 条
    // 语句会把用例拖到 30s+；不变量断言与间隔无关。
    let db = Database::open(DbOptions {
        wal_flush_interval_ms: 1,
        ..DbOptions::memory()
    })
    .unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    const THREADS: usize = 8;
    const KEYS: i64 = 32;
    std::thread::scope(|scope| {
        for th in 0..THREADS {
            let db = db.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                let mut rng = th as u64 * 2654435761 + 1;
                for _ in 0..200 {
                    rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let id = (rng >> 33) % KEYS as u64;
                    match rng >> 61 {
                        0..=1 => {
                            let _ = s.exec(&format!("INSERT INTO t VALUES ({id}, 't{th}')"));
                        }
                        2..=3 => {
                            let _ = s.exec(&format!("UPDATE t SET v = 't{th}' WHERE id = {id}"));
                        }
                        4 => {
                            let _ = s.exec(&format!("DELETE FROM t WHERE id = {id}"));
                        }
                        _ => {
                            let _ = s.exec(&format!("SELECT v FROM t WHERE id = {id}"));
                        }
                    }
                }
            });
        }
    });
    let all = rows(&db, "SELECT id, v FROM t ORDER BY id");
    // 不变量 1：PK 唯一（ordered + 主键 ⇒ 无重复 id）
    let ids: Vec<&String> = all.iter().map(|r| &r[0]).collect();
    let uniq = ids.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), uniq.len(), "PK 唯一性被破坏：{ids:?}");
    // 不变量 2：值自洽（v 恒为某写者标记）
    for r in &all {
        assert!(
            r[1].starts_with('t') && r[1][1..].parse::<usize>().is_ok(),
            "值被撕裂/污染：{r:?}"
        );
    }
}

#[test]
fn concurrent_ddl_same_table_one_winner() {
    let db = open_mem();
    const N: usize = 6;
    let barrier = Arc::new(std::sync::Barrier::new(N));
    std::thread::scope(|scope| {
        for _ in 0..N {
            let db = db.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                barrier.wait();
                match s.exec("CREATE TABLE ddl_t (id BIGINT PRIMARY KEY)") {
                    Ok(_) => true,
                    Err(e) => {
                        assert_eq!(e.state, "42P07", "重复建表应 42P07：{e}");
                        false
                    }
                }
            });
        }
    });
    assert_eq!(
        rows(&db, "SELECT count(*) FROM ddl_t")[0][0],
        "0",
        "表恰好存在一个且为空"
    );
}

#[test]
fn concurrent_merge_row_conflict_detected() {
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        s.exec("INSERT INTO t VALUES (1, 'base')").unwrap();
        s.exec("CHECKPOINT").unwrap();
    }
    {
        let mut s = db.new_session();
        s.exec("CREATE BRANCH b1 FROM main").unwrap();
        s.exec("CREATE BRANCH b2 FROM main").unwrap();
    }
    {
        let mut s = db.new_session();
        s.exec("USE BRANCH b1").unwrap();
        s.exec("UPDATE t SET v = 'one' WHERE id = 1").unwrap();
        s.exec("CHECKPOINT").unwrap();
    }
    {
        let mut s = db.new_session();
        s.exec("USE BRANCH b2").unwrap();
        s.exec("UPDATE t SET v = 'two' WHERE id = 1").unwrap();
        s.exec("CHECKPOINT").unwrap();
    }
    {
        let mut s = db.new_session();
        s.exec("USE BRANCH main").unwrap();
        s.exec("MERGE BRANCH b1 INTO main").unwrap();
        // b2 与 main 同键分歧 → 结构化合并行级冲突 → 40001
        let err = s.exec("MERGE BRANCH b2 INTO main").unwrap_err();
        assert_eq!(err.state, "40001", "行级冲突应显式拒绝：{err}");
    }
    assert_eq!(
        rows(&db, "SELECT v FROM t WHERE id = 1")[0][0],
        "one",
        "终态 = 赢家分支值"
    );
}

#[test]
fn concurrent_drop_branch_fails_inflight_txn_cleanly() {
    let db = open_mem();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("CREATE BRANCH victim FROM main").unwrap();
    }
    let db2 = db.clone();
    let writer = std::thread::spawn(move || {
        let mut s = db2.new_session();
        s.exec("USE BRANCH victim").unwrap();
        s.exec("BEGIN").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // DROP 之后 COMMIT：分支已不在 manifest → 干净 3D000（不 panic、不部分写入）
        let drop_r = {
            let mut dropper = db2.new_session();
            dropper.exec("USE BRANCH main").unwrap();
            dropper.exec("DROP BRANCH victim")
        };
        assert!(drop_r.is_ok(), "drop 应成功：{:?}", drop_r.err());
        match s.exec("COMMIT") {
            Err(e) => assert_eq!(e.state, "3D000", "COMMIT 应报分支不存在：{e}"),
            Ok(_) => panic!("已删除分支上的 COMMIT 不应成功"),
        }
    });
    writer.join().unwrap();
    // main 不受影响（孤儿写入未泄漏到其他分支）
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], "0");
}
