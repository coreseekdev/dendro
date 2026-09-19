//! embed pk 直读快路径（find_by_pk）：与 SQL 点查同语义的差分锁定
use dendro_core::embed::Connection;
use dendro_core::types::SqlValue;
use dendro_core::{DbOptions, StoreConfig};

fn conn(tag: &str) -> Connection {
    let dir = std::env::temp_dir().join(format!("dendro-fpk-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Connection::open_with(&dir, DbOptions::embedded(StoreConfig::Memory)).unwrap()
}

#[test]
fn find_by_pk_matches_sql_point_query() {
    let mut c = conn("diff");
    c.execute("CREATE TABLE f (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)")
        .unwrap();
    for ch in 0..5 {
        let vals: Vec<String> = (0..500)
            .map(|i| {
                let id = ch * 500 + i + 1;
                format!("({id}, {}, 't{}')", id % 9, id % 5)
            })
            .collect();
        c.execute(&format!("INSERT INTO f VALUES {}", vals.join(",")))
            .unwrap();
    }
    // 三形态逐一差分：memtx 驻留 / checkpoint 树驻留 / 树+overlay 尾巴
    for phase in ["memtx", "tree", "overlay"] {
        if phase == "tree" {
            c.execute("CHECKPOINT").unwrap();
        }
        if phase == "overlay" {
            c.execute("INSERT INTO f VALUES (999999, 42, 'tail')")
                .unwrap();
            c.execute("DELETE FROM f WHERE id = 1").unwrap();
        }
        for id in [1i64, 2, 500, 1234, 2500, 999999, 888888] {
            let fast = c.find_by_pk("f", id).unwrap();
            let sql = c
                .query(&format!("SELECT * FROM f WHERE id = {id}"))
                .unwrap();
            let sql_row = sql.rows.first().cloned();
            match (&fast, &sql_row) {
                (Some(f), Some(r)) => assert_eq!(f, r, "phase={phase} id={id}"),
                (None, None) => {}
                other => panic!("phase={phase} id={id}: 形态分歧 {other:?}"),
            }
        }
    }
}

#[test]
fn find_by_pk_semantics() {
    let mut c = conn("sem");
    c.execute("CREATE TABLE s (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("INSERT INTO s VALUES (1, 10), (2, 20)").unwrap();
    // 事务内读自身写
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO s VALUES (3, 30)").unwrap();
    c.execute("DELETE FROM s WHERE id = 1").unwrap();
    assert!(matches!(
        c.find_by_pk("s", 3).unwrap().unwrap()[1],
        SqlValue::Int64(30)
    ));
    assert!(c.find_by_pk("s", 1).unwrap().is_none(), "事务内删除不可见");
    c.execute("COMMIT").unwrap();
    assert!(c.find_by_pk("s", 1).unwrap().is_none(), "提交后仍不可见");
    // 树驻留（checkpoint 后 memtx 空）
    c.execute("CHECKPOINT").unwrap();
    assert!(matches!(
        c.find_by_pk("s", 2).unwrap().unwrap()[1],
        SqlValue::Int64(20)
    ));
    // 墓碑不复活
    c.execute("DELETE FROM s WHERE id = 2").unwrap();
    assert!(
        c.find_by_pk("s", 2).unwrap().is_none(),
        "删除后（memtx 墓碑期）"
    );
}
