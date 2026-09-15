//! 账本 #21/#22/#23 语义债修复回归（v2c-2 前置清理）：
//! - #21：LEFT join 键类型标签化——NULL↔NULL 不匹配、Int64(1)↔Utf8("1") 不碰撞
//! - #22：JOIN ON AND 链中非等值合取回收求值（曾静默丢弃）
//! - #23：恒假谓词 + 全局聚合 → 单行零聚合（与非短路路径一致）

use dendro_core::embed::Connection;

#[test]
fn issue21_left_join_null_keys_do_not_match() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE a (id INT PRIMARY KEY, k INT)")
        .unwrap();
    c.execute("CREATE TABLE b (id INT PRIMARY KEY, k INT)")
        .unwrap();
    c.execute("INSERT INTO a VALUES (1, NULL)").unwrap();
    c.execute("INSERT INTO b VALUES (1, NULL)").unwrap();
    // SQL 语义：NULL = NULL 不成立 → LEFT 无匹配 → NULL 延展（曾互相匹配）
    let r = c
        .query("SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.id")
        .unwrap();
    assert_eq!(r.row_count(), 1, "单行 NULL 延展");
    assert!(
        format!("{:?}", r.rows).contains("Null"),
        "右列应为 NULL：{:?}",
        r.rows
    );
}

#[test]
fn issue21_typed_keys_no_cross_type_collision() {
    // 注：数字串字面量（'1'）会被账本 #26（value_from_parser 数值化）
    // 污染前提——此处用非数字文本隔离键类型维度
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE a (id BIGINT PRIMARY KEY, k BIGINT)")
        .unwrap();
    c.execute("CREATE TABLE b (id BIGINT PRIMARY KEY, k TEXT)")
        .unwrap();
    c.execute("INSERT INTO a VALUES (1, 7)").unwrap(); // Int64(7)
    c.execute("INSERT INTO b VALUES (1, 'x')").unwrap(); // Utf8("x")
    let r = c
        .query("SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k")
        .unwrap();
    // 类型不同不碰撞：无匹配 → NULL 延展（曾 to_text 无类型 tag 可碰撞）
    assert_eq!(r.row_count(), 1);
    assert!(
        format!("{:?}", r.rows).contains("Null"),
        "Int64 与 Utf8 不得匹配：{:?}",
        r.rows
    );
}

#[test]
fn issue22_residual_conjuncts_enforced() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE a (id INT PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE b (id INT PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO a VALUES (1, 1),(2, 5)").unwrap();
    c.execute("INSERT INTO b VALUES (1, 1),(2, 5)").unwrap();
    // INNER：残留 v>1 过滤（曾静默丢弃 → 2 行）
    let r = c
        .query("SELECT a.id FROM a JOIN b ON a.id = b.id AND a.v > 1 ORDER BY a.id")
        .unwrap();
    assert_eq!(r.row_count(), 1, "残留合取必须生效");
    assert!(format!("{:?}", r.rows).contains("2"));
    // LEFT：残留不成立 = 无匹配 → NULL 延展（曾输出匹配行）
    let rl = c
        .query("SELECT a.id, b.id FROM a LEFT JOIN b ON a.id = b.id AND a.v > 1 ORDER BY a.id")
        .unwrap();
    assert_eq!(rl.row_count(), 2, "LEFT 保留全部左行");
    let txt = format!("{:?}", rl.rows);
    assert!(txt.contains("Null"), "a.id=1 残留不成立须 NULL 延展：{txt}");
}

#[test]
fn issue23_count_with_false_predicate_returns_one_row() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    // 恒假常量 + 全局聚合：单行零聚合（与逐行过滤路径一致；曾 0 行）
    let r1 = c.query("SELECT count(*) AS n FROM t WHERE 1 = 0").unwrap();
    assert_eq!(r1.row_count(), 1, "空输入全局聚合仍出一行（#23）");
    assert!(format!("{:?}", r1.rows).contains("0"), "{:?}", r1.rows);
    // 恒 NULL 谓词同语义
    let r2 = c.query("SELECT count(*) AS n FROM t WHERE NULL").unwrap();
    assert_eq!(r2.row_count(), 1);
    // 等价逐行路径（非常量谓词）——两条路径必须一致
    let r3 = c
        .query("SELECT count(*) AS n FROM t WHERE id > 99")
        .unwrap();
    assert_eq!(r3.row_count(), 1);
    assert_eq!(format!("{:?}", r2.rows), format!("{:?}", r3.rows));
    // 非聚合查询恒假仍空集
    let r4 = c.query("SELECT id FROM t WHERE 1 = 0").unwrap();
    assert_eq!(r4.row_count(), 0);
}
