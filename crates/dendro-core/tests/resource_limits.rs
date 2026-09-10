//! 资源上界回归（S-3）：分支数 / 单事务字节 / 连接数守卫。
//! 配置旋钮 0 = 不限；超限错误码 54000 / 53300；失败无副作用。

use dendro_core::{Database, DbOptions};

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
    let g = dendro_core::engine::ConnGuard::new(2);
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
    let g0 = dendro_core::engine::ConnGuard::new(0);
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
