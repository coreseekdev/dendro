// 端到端：SQL 自省表 + memtx 计量 + 采样器
use dendro_core::embed::Connection;
#[test]
fn memory_introspection_sql_surface() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    for i in 0..500 {
        c.execute(&format!(
            "INSERT INTO t VALUES ({i}, 'xxx{}')",
            "y".repeat(100)
        ))
        .unwrap();
    }
    // 未 checkpoint：memtx.pending meter > 0
    let r = c
        .query("SELECT bytes FROM cambium.memory_usage WHERE name = 'memtx.pending'")
        .unwrap();
    let b = match &r.rows[0][0] {
        dendro_core::types::SqlValue::Int64(v) => *v,
        o => panic!("{o:?}"),
    };
    assert!(b > 40_000, "memtx pending 应计入 500×~100B：{b}");
    // checkpoint 后归零；entries census 仍在（段化前的键数——表已空则 0，
    // 但 INSERT 期间 census 必须能读到 >0——上面查询前已验证）
    c.execute("CHECKPOINT").unwrap();
    let r = c
        .query("SELECT bytes FROM cambium.memory_usage WHERE name = 'memtx.pending'")
        .unwrap();
    let b = match &r.rows[0][0] {
        dendro_core::types::SqlValue::Int64(v) => *v,
        o => panic!("{o:?}"),
    };
    assert_eq!(b, 0, "checkpoint 后 pending 归零：{b}");
    // entries census（写后即查——memtx 键数）
    let mut c2 = Connection::memory().unwrap();
    c2.execute("CREATE TABLE e (id BIGINT PRIMARY KEY)")
        .unwrap();
    c2.execute("INSERT INTO e VALUES (1),(2),(3)").unwrap();
    let r = c2
        .query("SELECT items FROM cambium.memory_usage WHERE name = 'memtx.entries'")
        .unwrap();
    let n = match &r.rows[0][0] {
        dendro_core::types::SqlValue::Int64(v) => *v,
        o => panic!("{o:?}"),
    };
    assert_eq!(n, 3, "memtx.entries census = 键数：{n}");
    // 包络行存在
    let r = c
        .query("SELECT count(*) FROM cambium.memory_usage WHERE kind = 'envelope'")
        .unwrap();
    assert!(
        format!("{:?}", r.rows).contains("3"),
        "rss/hwm/unattributed 三行"
    );
    // 缓存 census：查询后 plan 缓存有条目
    let r = c
        .query("SELECT items FROM cambium.memory_usage WHERE name = 'cache.plan_entries'")
        .unwrap();
    let n = match &r.rows[0][0] {
        dendro_core::types::SqlValue::Int64(v) => *v,
        o => panic!("{o:?}"),
    };
    assert!(n > 0, "查询后 plan 缓存 census > 0：{n}");
    // embed JSON 面
    let j = c.memory_snapshot_json();
    assert!(j.contains("\"unattributed\":"), "{j}");
    assert!(j.contains("cache.plan_entries"), "{j}");
}
