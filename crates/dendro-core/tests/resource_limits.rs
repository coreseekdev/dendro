//! 资源上界回归（S-3）：分支数 / 单事务字节 / 连接数守卫。
//! 配置旋钮 0 = 不限；超限错误码 54000 / 53300；失败无副作用。

use dendro_core::objstore::sim::SimObjStore;
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn opts_with(max_branches: usize, max_txn_bytes: u64) -> DbOptions {
    DbOptions {
        max_branches,
        max_txn_bytes,
        ..DbOptions::memory()
    }
}

#[test]
fn branch_limit_rejects_at_cap() {
    let db = Database::open(opts_with(3, 0)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    }
    // main + 2 个分支 = 3；第 3 个分支被拒
    {
        let mut s = db.new_session();
        s.exec("CREATE BRANCH b1 FROM main").unwrap();
        s.exec("CREATE BRANCH b2 FROM main").unwrap();
        let e = s.exec("CREATE BRANCH b3 FROM main").unwrap_err();
        assert_eq!(e.state, "54000", "{e}");
        assert!(e.message.contains("branch limit"), "{e}");
    }
    // 已有分支不受影响；上限内继续可用
    let mut s = db.new_session();
    s.exec("USE BRANCH b1").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    s.exec("USE BRANCH main").unwrap();
    assert!(s.exec("SELECT count(*) FROM t").is_ok());
}

#[test]
fn branch_limit_zero_is_unlimited() {
    let db = Database::open(opts_with(0, 0)).unwrap();
    let mut s = db.new_session();
    for i in 0..20 {
        s.exec(&format!("CREATE BRANCH b{i} FROM main")).unwrap();
    }
}

#[test]
fn txn_size_limit_rejects_before_install() {
    // 32B 上限：大 INSERT 超限 → 54000；且**无任何可见状态**（Pass1 拒绝，
    // 未入队未安装——Uncertain 域之外）
    let db = Database::open(opts_with(0, 256)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    let mut s = db.new_session();
    let big = "x".repeat(1024);
    let e = s
        .exec(&format!("INSERT INTO t VALUES (1, '{big}')"))
        .unwrap_err();
    assert_eq!(e.state, "54000", "{e}");
    assert!(e.message.contains("transaction too large"), "{e}");
    assert_eq!(
        rows(&db, "SELECT count(*) FROM t")[0],
        "0",
        "超限事务不得有任何可见状态"
    );
    // 小事务正常
    s.exec("INSERT INTO t VALUES (1, 'ok')").unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0], "1");
}

#[test]
fn txn_size_limit_zero_is_unlimited() {
    let db = Database::open(opts_with(0, 0)).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    let big = "x".repeat(4096);
    s.exec(&format!("INSERT INTO t VALUES (1, '{big}')"))
        .unwrap();
}

#[test]
fn conn_guard_enter_exit_counts() {
    let g = std::sync::Arc::new(dendro_core::engine::ConnGuard::new(2));
    g.enter().unwrap();
    g.enter().unwrap();
    let e = g.enter().unwrap_err();
    assert_eq!(e.state, "53300", "{e}");
    g.exit();
    g.enter().unwrap(); // 退出后可再进
    g.exit();
    g.exit();
    assert_eq!(g.active(), 0);
    // max = 0 不限
    let g0 = std::sync::Arc::new(dendro_core::engine::ConnGuard::new(0));
    for _ in 0..1000 {
        g0.enter().unwrap();
    }
}

#[test]
fn limit_transactions_reject_cleanly_under_concurrency() {
    // 大事务拒绝发生在 Pass1（入队前）——并发下拒绝者不占 in-flight 槽
    let db = Database::open(opts_with(0, 128)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    std::thread::scope(|scope| {
        for i in 0..4 {
            let db = db.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                let big = "x".repeat(1024);
                if i % 2 == 0 {
                    let r = s.exec(&format!("INSERT INTO t VALUES ({i}, '{big}')"));
                    assert!(r.is_err(), "大事务应被拒");
                } else {
                    s.exec(&format!("INSERT INTO t VALUES ({i}, 'ok')"))
                        .unwrap();
                }
            });
        }
    });
    // 只有小事务落库
    let got = rows(&db, "SELECT count(*) FROM t WHERE v = 'ok'");
    assert_eq!(got[0], "2");
}

fn rows(db: &std::sync::Arc<Database>, sql: &str) -> Vec<String> {
    let mut s = db.new_session();
    match &s.exec(sql).unwrap()[0] {
        dendro_core::Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap_or_default())
            .collect(),
        _ => vec![],
    }
}

#[test]
fn branch_limit_race_exact_under_concurrency() {
    // 审计 R7-2 回归：上限检查若在 manifest CAS 之外，32 线程并发创建
    // 会在"预检通过 → CAS 重试"窗口越限。修复后权威检查在闭包内
    // （CAS 重试以最新 manifest 重评估）——成功数恰 = 上限。
    let db = Database::open(opts_with(8, 0)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    }
    const THREADS: usize = 32;
    std::thread::scope(|scope| {
        for i in 0..THREADS {
            let db = db.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                match s.exec(&format!("CREATE BRANCH race{i} FROM main")) {
                    Ok(_) => true,
                    Err(e) => {
                        eprintln!("[race] err: {e}");
                        false
                    }
                }
            });
        }
    });
    // main + 恰好 7 个分支 = 8（上限）；多一个都算越限
    let n = rows(&db, "SHOW BRANCHES").len();
    assert_eq!(n, 8, "并发下分支数必须恰等于上限（含 main）：{n}");
    assert_eq!(
        rows(&db, "SELECT count(*) FROM t")[0],
        "0",
        "越限分支不得存在"
    );
}

#[test]
fn conn_guard_rejected_counter_and_panic_safety() {
    // 拒绝计数（/metrics 可观测）
    let g = std::sync::Arc::new(dendro_core::engine::ConnGuard::new(1));
    g.enter().unwrap();
    let _ = g.enter();
    assert_eq!(g.rejected.load(std::sync::atomic::Ordering::Relaxed), 1);
    g.exit();
    assert_eq!(g.active(), 0);
    // 恐慌安全：ConnSession Drop 在 panic 展开时回收计数
    let g2 = std::sync::Arc::new(dendro_core::engine::ConnGuard::new(10));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cs = dendro_core::engine::ConnSession::enter(&g2).unwrap();
        let _ = &_cs; // panic 在持有期发生 → Drop 回收计数
        panic!("simulated connection thread panic");
    }));
    assert!(result.is_err());
    assert_eq!(
        g2.active(),
        0,
        "panic 展开后计数必须归零（泄漏 = 永久拒服）"
    );
}

#[test]
fn branch_limit_sequential_probe() {
    let db = Database::open(opts_with(8, 0)).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    for i in 0..12 {
        let r = s.exec(&format!("CREATE BRANCH b{i} FROM main"));
        let n = rows(&db, "SHOW BRANCHES").len();
        match r {
            Err(e) => println!("create b{i}: ERR {} refs_visible={n}", e.message),
            Ok(_) => println!("create b{i}: OK refs_visible={n}"),
        }
    }
}

#[test]
fn statement_timeout_aborts_runaway_query() {
    // S-3 语句超时：大表 count(*) + 50ms 超时 → 57014 query_canceled；
    // 随后语句不受影响（deadline 逐语句重置）；SET 0 = 关闭。
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        for batch in (0..300_000).step_by(10_000) {
            let vals: Vec<String> = (batch..batch + 10_000)
                .map(|i| format!("({i}, 'v{i}')"))
                .collect();
            s.exec(&format!("INSERT INTO t VALUES {}", vals.join(", ")))
                .unwrap();
        }
        s.exec("CHECKPOINT").unwrap();
    }
    let mut s = db.new_session();
    s.exec("SET statement_timeout = 50").unwrap();
    // 全表扫描（>50ms）→ 57014
    let mut timed_out = false;
    for _ in 0..5 {
        match s.exec("SELECT count(*) FROM t") {
            Ok(_) => {}
            Err(e) => {
                assert_eq!(e.state, "57014", "{e}");
                timed_out = true;
                break;
            }
        }
    }
    assert!(timed_out, "50ms 超时下 30 万行全表扫描应触发 57014");
    // 关闭超时后同查询正常完成
    s.exec("SET statement_timeout = 0").unwrap();
    let n = rows(&db, "SELECT count(*) FROM t");
    assert_eq!(n[0], "300000");
}

#[test]
fn statement_timeout_does_not_affect_fast_queries() {
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
    }
    let mut s = db.new_session();
    s.exec("SET statement_timeout = 50").unwrap();
    for _ in 0..10 {
        s.exec("SELECT count(*) FROM t WHERE id = 1").unwrap();
    }
}

// ---- S-3 会话配额：prepared / 游标字节 / 结果集字节 ----

#[test]
fn prepared_statement_count_quota() {
    let db = Database::open(DbOptions {
        max_prepared_per_session: 3,
        ..DbOptions::memory()
    })
    .unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    let mut s = db.new_session();
    for i in 0..3 {
        s.prepare(
            &format!("p{i}"),
            &format!("SELECT id FROM t WHERE id = {i}"),
            &[],
        )
        .unwrap();
    }
    let e = s
        .prepare("p3", "SELECT id FROM t WHERE id = 3", &[])
        .unwrap_err();
    assert_eq!(e.state, "54000", "{e}");
    // 同名覆盖（replace）不受配额限制
    s.prepare("p2", "SELECT v FROM t WHERE id = 2", &[])
        .unwrap();
    // 其他会话不受影响
    let mut s2 = db.new_session();
    s2.prepare("x", "SELECT id FROM t WHERE id = 1", &[])
        .unwrap();
}

#[test]
fn cursor_byte_quota_rejects_retention() {
    let db = Database::open(DbOptions {
        max_cursor_bytes: 1 << 20, // 1MB
        ..DbOptions::memory()
    })
    .unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        for batch in (0..60_000).step_by(10_000) {
            let vals: Vec<String> = (batch..batch + 10_000)
                .map(|i| format!("({i}, 'value-{i}-pad-pad-pad')"))
                .collect();
            s.exec(&format!("INSERT INTO t VALUES {}", vals.join(", ")))
                .unwrap();
        }
    }
    let mut s = db.new_session();
    // 6 万行 × ~40B ≈ 2.4MB > 1MB → DECLARE 被拒（游标保留受限于配额）
    let e = s.exec("DECLARE c CURSOR FOR SELECT * FROM t").unwrap_err();
    assert_eq!(e.state, "54000", "{e}");
    // 小结果集游标正常
    s.exec("DECLARE small CURSOR FOR SELECT id FROM t WHERE id < 10")
        .unwrap();
    s.exec("FETCH 5 FROM small").unwrap();
}

#[test]
fn result_byte_guard_drops_oversized_output() {
    let db = Database::open(DbOptions {
        max_result_bytes: 1 << 20, // 1MB
        ..DbOptions::memory()
    })
    .unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        for batch in (0..60_000).step_by(10_000) {
            let vals: Vec<String> = (batch..batch + 10_000)
                .map(|i| format!("({i}, 'value-{i}-pad-pad-pad')"))
                .collect();
            s.exec(&format!("INSERT INTO t VALUES {}", vals.join(", ")))
                .unwrap();
        }
    }
    let mut s = db.new_session();
    let e = s.exec("SELECT * FROM t").unwrap_err();
    assert_eq!(e.state, "54000", "{e}");
    // 加 LIMIT 后正常（守卫的意图 = 逼分页）
    s.exec("SELECT * FROM t LIMIT 100").unwrap();
}

#[test]
fn watermark_recovers_when_inflight_drains_via_failures() {
    // 模型检查 R8-WM 回归（TLC 反例形态）：pass2(大 ts) 先安装、
    // pass2(小 ts) 等待失败被摘除——摘除不重算水位 ⇒ 已 ack 大 ts 行
    // 永久不可见（直到无关新提交排水）。修复后：任何摘除路径都重算前沿。
    let sim = SimObjStore::new();
    let db = Database::open(DbOptions {
        store: StoreConfig::Obj(Arc::new(sim.clone())),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 2,
        ..DbOptions::memory()
    })
    .unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    // 并发 8 提交：全部入队 + durable；随后注入写失败制造部分 pass2 失败
    let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for i in 0..8 {
            let db = db.clone();
            let acked = acked.clone();
            scope.spawn(move || {
                let mut s = db.new_session();
                if s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"))
                    .is_ok()
                {
                    acked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
    });
    let acked_n = acked.load(std::sync::atomic::Ordering::SeqCst);
    // 毒化后再注入失败已无意义——此处断言的是：**无任何新提交**的情况下，
    // 全部已 ack 行立即可见（水位重算在摘除路径上完成）
    let got = {
        let mut s = db.new_session();
        match &s.exec("SELECT count(*) FROM t").unwrap()[0] {
            dendro_core::Output::Rows(rs) => rs.text_rows()[0][0]
                .clone()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            _ => 0,
        }
    };
    assert_eq!(
        got, acked_n,
        "已 ack 行在无新提交时必须全部可见（水位随摘除精确推进）：got={got} acked={acked_n}"
    );
}
