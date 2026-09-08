//! Q-14：AP 列存路径（≥1 万行）的显式事务语义——读自己的写 + 冻结根。
//!
//! ⚠ 必须在 dendro-server 侧测试：需要 set_columnar（列存依赖）。
//! 第十三轮 R13-1 教训：此测试曾放在 core 侧且未 set_columnar——
//! `columnar()` 为 None → AP 路径在 scan.rs 短路 → 全程走行路径的
//! 空转测试（评审探针：还原 Q-14 修复仍绿）。

use dendro_columnar::integrate::CbfColumnar;
use dendro_core::versioned::Versioned;
use dendro_core::{Database, DbOptions};
use std::sync::Arc;

#[test]
fn q14_ap_path_reads_own_writes_and_frozen() {
    let db = Database::open(DbOptions::memory()).unwrap();
    // **接线列存**（R13-1：缺这行 = 空转测试）
    db.set_columnar(Arc::new(CbfColumnar { row_group_rows: 1_048_576 }));
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE big (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        let values: Vec<String> = (1..=12_000).map(|i| format!("({i}, 'v{i}')")).collect();
        s.exec(&format!("INSERT INTO big VALUES {}", values.join(", "))).unwrap();
        db.checkpoint_branch("main").unwrap(); // 物化进 CBF
    }
    // **断言 AP 门真的满足**：columnar 已装配 + col_rows ≥ 1 万
    {
        let b = db.branch("main").unwrap();
        let head = b.head.load_full();
        let catalog = Versioned::new(db.node_store().clone());
        let entry = catalog
            .catalog_lookup(head.as_ref().as_ref().map(|c| c.root).as_ref(), "big")
            .unwrap()
            .expect("table big in catalog");
        assert!(entry.col_rows >= 10_000, "col_rows={}，AP 门未达标", entry.col_rows);
    }
    let mut s = db.new_session();
    s.exec("BEGIN").unwrap();
    s.exec("INSERT INTO big VALUES (20000, 'new')").unwrap();
    s.exec("UPDATE big SET v = 'upd' WHERE id = 5").unwrap();
    s.exec("DELETE FROM big WHERE id = 6").unwrap();
    let q = |s: &mut dendro_core::Session, sql: &str| -> String {
        match &s.exec(sql).unwrap()[0] {
            dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
            _ => panic!(),
        }
    };
    // 读自己的写（AP 归并三层：CBF → memtx overlay → 会话事务写）
    assert_eq!(q(&mut s, "SELECT count(*) FROM big"), "12000", "12000 - 1 删 + 1 插");
    assert_eq!(q(&mut s, "SELECT v FROM big WHERE id = 20000"), "new", "事务内 INSERT 经 AP 可见");
    assert_eq!(q(&mut s, "SELECT v FROM big WHERE id = 5"), "upd", "事务内 UPDATE 经 AP 归并");
    let n6 = match &s.exec("SELECT count(*) FROM big WHERE id = 6").unwrap()[0] {
        dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    };
    assert_eq!(n6, "0", "事务内删除的行经 AP 归并不可见");
    // **非 pk 断言**（第十四轮 R14-1 配方）：pk 等值断言被 pk 下推截走、走
    // 行路径的 R8-1 读己写，到不了 AP——非 pk 列扫描强制走 CBF 归并：
    // ① 事务内 UPDATE 的新值经 v 列（非 pk）可见；② 被删行的**旧值**经
    // v 列不可见。缺失层③时两条分别退化为 0 行 / 1 行，测试必红。
    let n_upd = match &s.exec("SELECT count(*) FROM big WHERE v = 'upd'").unwrap()[0] {
        dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    };
    assert_eq!(n_upd, "1", "事务内 UPDATE 的新值必须经 AP 归并可见");
    let n_old = match &s.exec("SELECT count(*) FROM big WHERE v = 'v6'").unwrap()[0] {
        dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    };
    assert_eq!(n_old, "0", "被删行的旧值必须经 AP 归并消失");
    // 冻结：并发提交 + checkpoint 不翻转事务内可见性
    {
        let mut s2 = db.new_session();
        s2.exec("INSERT INTO big VALUES (30000, 'z')").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    assert_eq!(q(&mut s, "SELECT count(*) FROM big"), "12000", "冻结读被并发 checkpoint 翻转");
    s.exec("COMMIT").unwrap();
    assert_eq!(q(&mut s, "SELECT count(*) FROM big"), "12001", "COMMIT 后新快照包含事务写入");
    assert_eq!(q(&mut s, "SELECT v FROM big WHERE id = 30000"), "z", "并发提交的行 COMMIT 后可见");
}

#[test]
fn limit_pushdown_stops_scan_early() {
    // Q-1：无 ORDER BY 的 LIMIT 下推——扫描在 cap 行后终止。
    // 行为正确性（LIMIT 100 仍返回恰 100 行、id 集正确）在此回归；
    // 内存收益由 BTreeMap 迭代顺序确定性保证（前 cap 个键），且
    // table_scan 的行解码循环带早停 break。
    let db = Database::open(DbOptions::memory()).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        let values: Vec<String> = (1..=5000).map(|i| format!("({i}, 'v{i}')")).collect();
        s.exec(&format!("INSERT INTO t VALUES {}", values.join(", "))).unwrap();
    }
    let mut s = db.new_session();
    let o = s.exec("SELECT id FROM t LIMIT 100").unwrap();
    match &o[0] {
        dendro_core::Output::Rows(rs) => {
            let rows = rs.text_rows();
            assert_eq!(rows.len(), 100, "LIMIT 100 应恰 100 行");
            // BTreeMap 键序 = 编码键序，前 100 个即 id 1..=100（大端序下
            // 1..=99 先于 100..，逐一校验首行与末行）
            assert_eq!(rows[0][0].as_deref(), Some("1"));
            assert_eq!(rows[99][0].as_deref().and_then(|s| s.parse::<i64>().ok()), Some(100));
        }
        _ => panic!(),
    }
    // LIMIT + OFFSET 组合
    let o = s.exec("SELECT id FROM t LIMIT 5 OFFSET 10").unwrap();
    match &o[0] {
        dendro_core::Output::Rows(rs) => {
            let rows = rs.text_rows();
            assert_eq!(rows.len(), 5);
            assert_eq!(rows[0][0].as_deref(), Some("11"));
        }
        _ => panic!(),
    }
}
