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

#[test]
fn q9_txn_spanning_checkpoint_rejected_not_silent() {
    // 第八轮 R8-4/Q-9：显式事务的冲突检测盲区——事务与提交之间隔着一次
    // checkpoint（memtx 历史被截断）时，此前的 40001 退化为静默
    // last-writer-wins（丢失更新无报错）。现定案：跨越 checkpoint 的显式
    // 事务提交显式 40001，客户端重试即获得完整视图。
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a')").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    let mut s = db.new_session();
    s.exec("BEGIN").unwrap();
    s.exec("UPDATE t SET v = 'mine' WHERE id = 1").unwrap();
    // 并发提交 + checkpoint（截断 memtx 历史 → 冲突检测盲区）
    {
        let mut s2 = db.new_session();
        s2.exec("UPDATE t SET v = 'theirs' WHERE id = 1").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    let e = match s.exec("COMMIT") {
        Ok(_) => panic!("跨越 checkpoint 的事务必须显式拒绝，而非静默丢失更新"),
        Err(e) => e,
    };
    assert_eq!(e.state, "40001", "{e}");
    // 重试（新事务）获得完整视图：对方的更新在，我的更新按新视图生效
    s.exec("BEGIN").unwrap();
    s.exec("UPDATE t SET v = 'mine-retry' WHERE id = 1").unwrap();
    s.exec("COMMIT").unwrap();
    let mut s2 = db.new_session();
    match &s2.exec("SELECT v FROM t WHERE id = 1").unwrap()[0] {
        dendro_core::Output::Rows(rs) => assert_eq!(rs.text_rows()[0][0].as_deref(), Some("mine-retry")),
        _ => panic!(),
    }
}

#[test]
fn q11_use_branch_inside_txn_rejected() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("CREATE BRANCH b2 FROM main").unwrap();
    s.exec("BEGIN").unwrap();
    let e = match s.exec("USE BRANCH b2") {
        Ok(_) => panic!("事务内 USE BRANCH 应被拒"),
        Err(e) => e,
    };
    assert_eq!(e.state, "25001", "{e}");
    s.exec("COMMIT").unwrap();
}

#[test]
fn r9_1_frozen_reads_survive_checkpoint() {
    // 第九轮 R9-1（P0）：BEGIN 前提交的行若在 BEGIN 后才首次被物化
    // （checkpoint 截断 memtx 版本），冻结读会把该行静默抹掉（count 1→0）。
    // 截断水位现尊重最老活跃快照——只读显式事务跨 checkpoint 稳定。
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a')").unwrap();
    }
    let mut s = db.new_session();
    s.exec("BEGIN").unwrap();
    let q = |s: &mut dendro_core::Session| -> String {
        match &s.exec("SELECT count(*) FROM t").unwrap()[0] {
            dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
            _ => panic!(),
        }
    };
    assert_eq!(q(&mut s), "1");
    // 并发：另一事务写入新行 + checkpoint（触发 memtx 截断）
    {
        let mut s2 = db.new_session();
        s2.exec("INSERT INTO t VALUES (2, 'b')").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    assert_eq!(q(&mut s), "1", "冻结读被 checkpoint 截断击穿（行静默消失）");
    // 新会话看到 2 行（物化后可见）
    {
        let mut s2 = db.new_session();
        match &s2.exec("SELECT count(*) FROM t").unwrap()[0] {
            dendro_core::Output::Rows(rs) => assert_eq!(rs.text_rows()[0][0].as_deref(), Some("2")),
            _ => panic!(),
        }
    }
    // 事务继续：仍恒 1，COMMIT 后新快照可见 2
    assert_eq!(q(&mut s), "1");
    s.exec("COMMIT").unwrap();
    assert_eq!(q(&mut s), "2");
}

#[test]
fn r9_2_checkpoint_and_branch_ddl_rejected_inside_txn() {
    // 第九轮 R9-2：事务内 CHECKPOINT / CREATE BRANCH 会在提交时刻自伤
    // （checkpoint 推进 covered_min → 本事务提交撞 Q-9 的 40001）——
    // 与隔离语义冲突的语句在 BEGIN 后直接拒绝（25001）。
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    // PG aborted 语义：事务内首个错误后 25P02 接管——逐事务验证每条语句
    for sql in ["CHECKPOINT", "CREATE BRANCH bx FROM main", "DROP BRANCH bx", "MERGE BRANCH bx INTO main"] {
        s.exec("BEGIN").unwrap();
        let e = match s.exec(sql) {
            Ok(_) => panic!("事务内不应允许：{sql}"),
            Err(e) => e,
        };
        assert_eq!(e.state, "25001", "{sql}: {e}");
        s.exec("ROLLBACK").unwrap();
    }
}

#[test]
fn r10_show_branches_allowed_and_snapshot_lifecycle() {
    // 第十一轮收口①：SHOW BRANCHES 只读放行（第十轮声称已做实际未落地）
    // + R10-1/R10-3 的 **SQL 侧常驻回归**（此前证据列引用的是 KV 侧测试）：
    // COMMIT/ROLLBACK 注销 + 同 watermark 引用计数。
    use std::collections::BTreeMap;
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("CREATE BRANCH b2 FROM main").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
    }
    let mut s = db.new_session();
    s.exec("BEGIN").unwrap();
    // SHOW 放行
    match &s.exec("SHOW BRANCHES").unwrap()[0] {
        dendro_core::Output::Rows(_) => {}
        _ => panic!("expected rows"),
    }
    // COMMIT 注销
    s.exec("COMMIT").unwrap();
    let b = db.branch("main").unwrap();
    assert!(b.active_snaps.lock().is_empty(), "COMMIT 后必须注销（此前永久跳过截断）");

    // 引用计数：同一 watermark 两个事务，先结束者不摘除后者的保护
    let mut s1 = db.new_session();
    s1.exec("BEGIN").unwrap();
    let mut s2 = db.new_session();
    s2.exec("BEGIN").unwrap();
    {
        let snaps: &BTreeMap<u64, usize> = &b.active_snaps.lock();
        let slot = snaps.values().sum::<usize>();
        assert!(slot >= 2, "同 watermark 双事务应各自计槽（实际 {slot}）");
    }
    s1.exec("ROLLBACK").unwrap(); // 只减自己的槽
    {
        let snaps: &BTreeMap<u64, usize> = &b.active_snaps.lock();
        let slot = snaps.values().sum::<usize>();
        assert!(slot >= 1, "先结束者不得连带摘除他人保护（实际 {slot}）");
    }
    s2.exec("ROLLBACK").unwrap();
    assert!(b.active_snaps.lock().is_empty(), "全部结束后注册表清空");
}


#[test]
fn q1b_cursor_declare_fetch_close() {
    // Q-1b：游标 v1（INSENSITIVE/READ ONLY/会话级）
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    for i in 1..=5 {
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')")).unwrap();
    }
    s.exec("DECLARE c CURSOR FOR SELECT id, v FROM t ORDER BY id").unwrap();
    // 分批 FETCH：3 + 2
    let o = s.exec("FETCH 3 FROM c").unwrap();
    match &o[0] {
        dendro_core::Output::Rows(rs) => {
            let rows = rs.text_rows();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0].as_deref(), Some("1"));
            assert_eq!(rows[2][1].as_deref(), Some("v3"));
        }
        _ => panic!(),
    }
    let o = s.exec("FETCH 3 FROM c").unwrap();
    match &o[0] {
        dendro_core::Output::Rows(rs) => {
            let rows = rs.text_rows();
            assert_eq!(rows.len(), 2, "第二次 FETCH 只剩 2 行");
            assert_eq!(rows[0][0].as_deref(), Some("4"));
        }
        _ => panic!(),
    }
    // FETCH ALL：耗尽后为空
    let o = s.exec("FETCH ALL FROM c").unwrap();
    match &o[0] {
        dendro_core::Output::Rows(rs) => assert_eq!(rs.total_rows(), 0),
        _ => panic!(),
    }
    s.exec("CLOSE c").unwrap();
    // CLOSE 后 FETCH → 34000
    let e = match s.exec("FETCH 1 FROM c") {
        Ok(_) => panic!("已关闭游标应不可 FETCH"),
        Err(e) => e,
    };
    assert_eq!(e.state, "34000", "{e}");
    // 未声明游标 FETCH → 34000
    let e = match s.exec("FETCH 1 FROM nosuch") {
        Ok(_) => panic!("未声明游标应不可 FETCH"),
        Err(e) => e,
    };
    assert_eq!(e.state, "34000");
}

#[test]
fn q1b_cursor_is_insensitive_snapshot() {
    // INSENSITIVE：DECLARE 后他人提交的新行不可见（物化语义，文档口径）
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    s.exec("DECLARE c CURSOR FOR SELECT count(*) FROM t").unwrap();
    {
        let mut s2 = db.new_session();
        s2.exec("INSERT INTO t VALUES (2)").unwrap();
        s2.exec("INSERT INTO t VALUES (3)").unwrap();
    }
    // FETCH 时计数仍为 DECLARE 时点（物化）
    match &s.exec("FETCH 1 FROM c").unwrap()[0] {
        dendro_core::Output::Rows(rs) => assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1"), "INSENSITIVE 游标不得看见 DECLARE 后的新行"),
        _ => panic!(),
    }
}

#[test]
fn r18_3_declare_with_multiple_spaces() {
    // 第十八轮 R18-3：连续空白下 DECLARE 的 query 提取不错位
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    s.exec("DECLARE   c   CURSOR FOR   SELECT count(*) FROM t").unwrap();
    let o = s.exec("FETCH 1 FROM c").unwrap();
    match &o[0] {
        dendro_core::Output::Rows(rs) => assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1")),
        _ => panic!(),
    }
}

#[test]
fn q10_ddl_inside_explicit_txn_rejected() {
    // Q-10（保守口径）：catalog 写不经事务写集——立即生效且 ROLLBACK 不可
    // 撤销（第十八轮 R18-2 实证可见性漂移）。显式事务内拒绝（25001）。
    let db = Database::open(DbOptions::memory()).unwrap();
    // PG aborted 语义：事务内首个错误后 25P02 接管——逐事务验证每条 DDL
    let mut n = 0;
    for sql in ["CREATE TABLE x (id BIGINT PRIMARY KEY)", "DROP TABLE t", "ALTER TABLE t ADD COLUMN w TEXT"] {
        n += 1;
        let mut s = db.new_session();
        s.exec(&format!("CREATE TABLE keep{n} (id BIGINT PRIMARY KEY)")).unwrap();
        s.exec("BEGIN").unwrap();
        let e = match s.exec(sql) {
            Ok(_) => panic!("事务内不应允许：{sql}"),
            Err(e) => e,
        };
        assert_eq!(e.state, "25001", "{sql}: {e}");
        s.exec("ROLLBACK").unwrap();
    }
    // 事务内 DML 仍允许
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("BEGIN").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    s.exec("COMMIT").unwrap();
    assert_eq!(
        {
            let o = s.exec("SELECT count(*) FROM t").unwrap();
            match &o[0] {
                dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
                _ => panic!(),
            }
        },
        "1"
    );
}

#[test]
fn q10_truncate_inside_explicit_txn_rejected() {
    // 第二十轮 R20-1：truncate_impl 同样走 catalog_commit——事务内必须拒绝
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    s.exec("BEGIN").unwrap();
    let e = match s.exec("TRUNCATE TABLE t") {
        Ok(_) => panic!("事务内 TRUNCATE 应被拒"),
        Err(e) => e,
    };
    assert_eq!(e.state, "25001", "{e}");
    s.exec("ROLLBACK").unwrap();
}

#[test]
fn q1_cursor_count_limit_255() {
    // Q-1 上界护栏：每会话最多 255 个游标，超限 53310；CLOSE 后可复用槽位
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    s.exec("INSERT INTO t VALUES (1)").unwrap();
    for i in 0..255 {
        s.exec(&format!("DECLARE c{i} CURSOR FOR SELECT id FROM t")).unwrap();
    }
    let e = match s.exec("DECLARE c255 CURSOR FOR SELECT id FROM t") {
        Ok(_) => panic!("第 256 个游标应被拒"),
        Err(e) => e,
    };
    assert_eq!(e.state, "53310", "{e}");
    // 关闭一个后槽位释放
    s.exec("CLOSE c0").unwrap();
    s.exec("DECLARE c255 CURSOR FOR SELECT id FROM t").unwrap();
}

#[test]
fn q16_in_txn_duplicate_insert_rejected() {
    // Q-16：显式事务内同一键两次 INSERT → 23505（此前静默覆盖）
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    s.exec("BEGIN").unwrap();
    s.exec("INSERT INTO t VALUES (1, 'first')").unwrap();
    let e = match s.exec("INSERT INTO t VALUES (1, 'second')") {
        Ok(_) => panic!("事务内重复键应被拒"),
        Err(e) => e,
    };
    assert_eq!(e.state, "23505", "{e}");
    // aborted 后 ROLLBACK
    s.exec("ROLLBACK").unwrap();
    match &s.exec("SELECT count(*) FROM t").unwrap()[0] {
        dendro_core::Output::Rows(rs) => assert_eq!(rs.text_rows()[0][0].as_deref(), Some("0"), "事务回滚后无数据"),
        _ => panic!(),
    }
}
