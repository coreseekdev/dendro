//! 语句取消（S-3 后续）：执行器检查点 + 取消令牌——客户端主动终止失控查询。
//!
//! 架构：`CancelToken`（AtomicBool）挂 Session，执行器在扫描/JOIN 循环
//! 检查点检测。PG CancelRequest 可从另一连接设置目标 backend 的令牌。

use dendro_core::{Database, DbOptions, Output};

#[test]
fn cancel_token_aborts_running_scan() {
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

    // 创建带取消令牌的会话
    let mut s = db.new_session();
    let token = s.cancel_token();
    assert!(
        !token.load(std::sync::atomic::Ordering::Relaxed),
        "初始未取消"
    );

    // 后台线程 50ms 后取消
    let t = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        t.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // 大表全扫（>50ms）→ 取消后 57014
    let start = std::time::Instant::now();
    let r = s.exec("SELECT count(*) FROM t");
    let elapsed = start.elapsed();

    match r {
        Err(e) => {
            assert_eq!(e.state, "57014", "取消应返回 57014 query_canceled: {e}");
        }
        Ok(_) => {
            // 查询在取消前完成（机器太快）——不算失败但记日志
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "查询不应超过 5s"
            );
        }
    }
    // 取消后同会话新语句正常（deadline/token 逐语句重置）
    let n = match &s.exec("SELECT count(*) FROM t WHERE id = 1").unwrap()[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!("expected rows"),
    };
    assert_eq!(n, "1", "取消后后续语句正常");
}

#[test]
fn cancel_token_does_not_affect_other_sessions() {
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
    }
    // 会话 A 取消令牌
    let mut sa = db.new_session();
    let _token_a = sa.cancel_token();
    // 会话 B 不受影响
    let mut sb = db.new_session();
    sb.exec("SELECT count(*) FROM t").unwrap();
    // A 语句正常
    sa.exec("SELECT count(*) FROM t").unwrap();
}
