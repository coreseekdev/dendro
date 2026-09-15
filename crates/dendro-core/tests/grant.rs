//! S-4（方向4）：GRANT/REVOKE 与权限门——表级 ACL 全语义矩阵。
//! 缺省 user = 超户 dendro（全放行，既有行为不变）；非超户按 owner/ACL 判定。

use dendro_core::embed::Connection;

fn setup() -> Connection {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    c
}

fn denied(err: &dendro_core::error::SqlError, table: &str) -> bool {
    err.state == "42501" && err.message.contains(table)
}

#[test]
fn superuser_default_unaffected() {
    let mut c = setup();
    // 缺省 user = dendro 超户：读写全通（既有行为不变的回归锚）
    assert_eq!(c.query("SELECT count(*) FROM t").unwrap().row_count(), 1);
    c.execute("INSERT INTO t VALUES (3, 30)").unwrap();
    c.execute("UPDATE t SET v = 99 WHERE id = 1").unwrap();
    c.execute("DELETE FROM t WHERE id = 2").unwrap();
    c.execute("GRANT SELECT ON t TO alice").unwrap();
    c.execute("REVOKE SELECT ON t FROM alice").unwrap();
}

#[test]
fn non_superuser_denied_without_grant() {
    let mut c = setup();
    c.set_user("bob");
    for (sql, table) in [
        ("SELECT * FROM t", "t"),
        ("INSERT INTO t VALUES (9, 9)", "t"),
        ("UPDATE t SET v = 1 WHERE id = 1", "t"),
        ("DELETE FROM t WHERE id = 1", "t"),
        ("TRUNCATE t", "t"),
    ] {
        let e = c.execute(sql).err().unwrap_or_else(|| panic!("应拒绝：{sql}"));
        assert!(denied(&e, table), "{sql} → {e:?}");
    }
    // 表不存在 → 执行路径原生报错（42P01 口径），非权限门
    let e = c.execute("SELECT * FROM nope").unwrap_err();
    assert_ne!(e.state, "42501", "{e:?}");
}

#[test]
fn grant_select_only() {
    let mut c = setup();
    c.execute("GRANT SELECT ON t TO bob").unwrap();
    c.set_user("bob");
    // SELECT 通
    let r = c.query("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.rows.len(), 1);
    // 写仍拒
    let e = c.execute("INSERT INTO t VALUES (9, 9)").unwrap_err();
    assert!(denied(&e, "t"), "{e:?}");
    // JOIN 两表都要 SELECT
    c.set_user("dendro");
    c.execute("CREATE TABLE u (id BIGINT PRIMARY KEY)").unwrap();
    c.set_user("bob");
    let e = c.execute("SELECT * FROM t, u WHERE t.id = u.id").unwrap_err();
    assert!(denied(&e, "u"), "{e:?}");
}

#[test]
fn grant_all_and_revoke() {
    let mut c = setup();
    c.execute("GRANT ALL ON t TO bob").unwrap();
    c.set_user("bob");
    c.execute("INSERT INTO t VALUES (9, 9)").unwrap();
    c.execute("UPDATE t SET v = 1 WHERE id = 1").unwrap();
    c.execute("DELETE FROM t WHERE id = 2").unwrap();
    c.set_user("dendro");
    c.execute("REVOKE ALL ON t FROM bob").unwrap();
    c.set_user("bob");
    let e = c.execute("SELECT * FROM t").unwrap_err();
    assert!(denied(&e, "t"), "{e:?}");
}

#[test]
fn revoke_partial_bits() {
    let mut c = setup();
    c.execute("GRANT ALL ON t TO bob").unwrap();
    c.execute("REVOKE INSERT, DELETE ON t FROM bob").unwrap();
    c.set_user("bob");
    c.execute("SELECT * FROM t").unwrap();
    c.execute("UPDATE t SET v = 5 WHERE id = 1").unwrap();
    assert!(c.execute("INSERT INTO t VALUES (9, 9)").is_err());
    assert!(c.execute("DELETE FROM t WHERE id = 1").is_err());
}

#[test]
fn grant_requires_owner() {
    let mut c = setup();
    // 超户建表 → owner=dendro；bob 不能授权
    c.execute("GRANT SELECT ON t TO bob").unwrap();
    c.set_user("bob");
    let e = c.execute("GRANT SELECT ON t TO carol").unwrap_err();
    assert!(denied(&e, "t"), "{e:?}");
    // 也不能 REVOKE
    let e = c.execute("REVOKE SELECT ON t FROM carol").unwrap_err();
    assert!(denied(&e, "t"), "{e:?}");
}

#[test]
fn creator_owns_new_table() {
    let mut c = setup();
    c.set_user("bob");
    c.execute("CREATE TABLE mine (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO mine VALUES (1)").unwrap();
    c.execute("GRANT SELECT ON mine TO carol").unwrap(); // owner 可授
    c.set_user("carol");
    c.execute("SELECT * FROM mine").unwrap();
    // carol 不能写
    assert!(c.execute("INSERT INTO mine VALUES (2)").is_err());
}

#[test]
fn grant_on_missing_table_errors() {
    let mut c = setup();
    let e = c.execute("GRANT SELECT ON nope TO bob").unwrap_err();
    assert!(e.message.contains("nope") || e.state == "42P01", "{e:?}");
}

#[test]
fn acl_survives_across_connections() {
    // 同一 Database 上新会话（重启模拟）后 ACL 仍生效（catalog 持久）
    let dir = std::env::temp_dir().join(format!("dendro_grant_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut c = Connection::open(&dir).unwrap();
        c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        c.execute("GRANT SELECT ON t TO bob").unwrap();
    }
    {
        let mut c = Connection::open(&dir).unwrap();
        c.set_user("bob");
        c.execute("SELECT count(*) FROM t").unwrap();
        assert!(c.execute("INSERT INTO t VALUES (1)").is_err());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prepared_statement_gated() {
    // prepare 期即拒（防 DESCRIBE/列推断泄漏未授权表结构）；
    // 执行期（exec_prepared → exec_statement）是第二道门
    use dendro_core::{Database, DbOptions, StoreConfig};
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    let mut su = db.new_session();
    su.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    let mut bob = db.new_session();
    bob.user = "bob".into();
    let e = bob.prepare("p1", "SELECT * FROM t", &[]).unwrap_err();
    assert_eq!(e.state, "42501", "{e:?}");
    // 授权后 prepare 通
    su.exec("GRANT SELECT ON t TO bob").unwrap();
    bob.prepare("p1", "SELECT * FROM t", &[]).unwrap();
    // 但未授权写语句 prepare 仍拒
    let e = bob.prepare("p2", "INSERT INTO t VALUES (1)", &[]).unwrap_err();
    assert_eq!(e.state, "42501", "{e:?}");
}

// ---------- 评审修复回归（P0 绕过面 / P1 / P2） ----------

#[test]
fn p0_derived_subquery_bypass_closed() {
    let mut c = setup();
    c.set_user("bob");
    // 派生表（原走查遗漏 → 全量绕过）
    let e = c
        .execute("SELECT * FROM (SELECT * FROM t) x")
        .err()
        .unwrap_or_else(|| panic!("派生表绕过必须关闭"));
    assert!(denied(&e, "t"), "{e:?}");
    // 授权后通
    c.set_user("dendro");
    c.execute("GRANT SELECT ON t TO bob").unwrap();
    c.set_user("bob");
    let r = c.query("SELECT count(*) FROM (SELECT * FROM t) x").unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn p0_view_two_step_chain_closed() {
    let mut c = setup();
    c.execute("CREATE VIEW v AS SELECT * FROM t").unwrap();
    // bob 查视图 → 展开视图体 → t 无权限 → 拒（原两步绕过）
    c.set_user("bob");
    let e = c
        .execute("SELECT * FROM v")
        .err()
        .unwrap_or_else(|| panic!("视图两步链绕过必须关闭"));
    assert!(denied(&e, "t"), "{e:?}");
    // 建视图本身也要走查底层表（原 CREATE VIEW 无检查）
    let e = c
        .execute("CREATE VIEW v2 AS SELECT * FROM t")
        .err()
        .unwrap_or_else(|| panic!("CREATE VIEW 走查缺失"));
    assert!(denied(&e, "t"), "{e:?}");
    // 授权后：查视图 & 建视图都通
    c.set_user("dendro");
    c.execute("GRANT SELECT ON t TO bob").unwrap();
    c.set_user("bob");
    c.execute("SELECT count(*) FROM v").unwrap();
    c.execute("CREATE VIEW v2 AS SELECT v FROM t").unwrap();
}

#[test]
fn p0_drop_alter_owner_gate() {
    let mut c = setup();
    c.set_user("bob");
    // 非属主 DROP / ALTER 他人表 → 拒
    let e = c.execute("DROP TABLE t").unwrap_err();
    assert!(denied(&e, "t"), "{e:?}");
    let e = c.execute("ALTER TABLE t ADD COLUMN x INT").unwrap_err();
    assert!(denied(&e, "t"), "{e:?}");
    // 属主可操作自己的表
    c.execute("CREATE TABLE mine (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("ALTER TABLE mine ADD COLUMN x INT").unwrap();
    c.execute("DROP TABLE mine").unwrap();
    // 非表对象 DROP（视图）非超户拒（v1 无视图属主追踪）
    let e = c.execute("DROP VIEW anything").unwrap_err();
    assert_eq!(e.state, "42501", "{e:?}");
}

#[test]
fn p1_column_level_grant_rejected_not_flattened() {
    let mut c = setup();
    // 列级清单 → 诚实拒绝（原静默展平为全表权限）
    let e = c.execute("GRANT SELECT (v) ON t TO bob").unwrap_err();
    assert!(
        e.message.contains("column-level"),
        "必须显式拒绝：{e:?}"
    );
    let e = c.execute("GRANT UPDATE (v) ON t TO bob").unwrap_err();
    assert!(e.message.contains("column-level"), "{e:?}");
}

#[test]
fn p2_user_name_case_folding() {
    let mut c = setup();
    // PG 标识符折叠：GRANT TO "Alice"（存 alice）→ 连接 "ALICE"/"Alice" 生效
    c.execute("GRANT SELECT ON t TO Alice").unwrap();
    c.set_user("ALICE");
    c.execute("SELECT count(*) FROM t").unwrap();
    c.set_user("Alice");
    c.execute("SELECT count(*) FROM t").unwrap();
    // 反向：未授权的大写变体不误判超户
    c.set_user("DENDRO_EX");
    assert!(c.execute("SELECT * FROM t").is_err());
}

#[test]
fn p2_explain_inner_statement_gated() {
    let mut c = setup();
    c.set_user("bob");
    let e = c
        .execute("EXPLAIN SELECT * FROM t")
        .err()
        .unwrap_or_else(|| panic!("EXPLAIN 内层必须走查"));
    assert!(denied(&e, "t"), "{e:?}");
}
