#![allow(clippy::all)]
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
