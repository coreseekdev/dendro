//! v2c-1 差分测试（ADR-5）：强制派发——同一 SQL 走不同数据源，结果集
//! 必须等价（tests/common §1.1 结果等价正式定义：多重集模式）。
//! force_source 仅调试/测试构建可 SET（本测试在 debug 下运行）。

mod common;

use common::{assert_rows_equiv, canonicalize, Canonical};
use dendro_core::types::{Output, SqlValue};
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn rows_of(outs: &[Output]) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match canonicalize(outs) {
        Canonical::Rows { cols, rows, .. } => (cols, rows),
        Canonical::Commands(c) => panic!("期望 Rows 输出：{c:?}"),
    }
}

fn fixture() -> Arc<Database> {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    // 列存引擎必须显式接线（core 不静态依赖 columnar；不设置则 AP 路径
    // 静默不可用——ap_tp_differential 曾因此空转，见该文件修复注记）
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    // 12k 行（≥1 万触发列存物化），分批插入
    for chunk in 0..12 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {id})",)
            })
            .collect();
        s.exec(&format!("INSERT INTO t VALUES {}", vals.join(",")))
            .unwrap();
    }
    db.checkpoint_branch("main").unwrap(); // 物化列存段
    s.exec("INSERT INTO t VALUES (999999, 7)").unwrap(); // overlay 尾巴
    s.exec("DELETE FROM t WHERE id = 5").unwrap(); // overlay 删除
    db
}

fn run(db: &Arc<Database>, force: &str, sql: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    let mut s = db.new_session();
    s.exec(&format!("SET dendro.force_source = '{force}'"))
        .unwrap();
    let outs = s.exec(sql).unwrap();
    rows_of(&outs)
}

fn diff(db: &Arc<Database>, sql: &str, paths: &[&str]) {
    let mut ref_: Option<(Vec<String>, Vec<Vec<SqlValue>>)> = None;
    for p in paths {
        let r = run(db, p, sql);
        match &ref_ {
            None => ref_ = Some(r),
            Some(base) => {
                assert_rows_equiv(
                    &format!("`{sql}` force={p} vs 基线"),
                    &base.0,
                    &base.1,
                    &r.0,
                    &r.1,
                    false,
                );
            }
        }
    }
}

#[test]
fn point_query_all_paths_equivalent() {
    let db = fixture();
    diff(
        &db,
        "SELECT v FROM t WHERE id = 500",
        &["auto", "delta", "fallback"],
    );
}

#[test]
fn range_query_main_vs_fallback() {
    let db = fixture();
    diff(
        &db,
        "SELECT id, v FROM t WHERE id > 11995",
        &["auto", "main", "fallback"],
    );
}

#[test]
fn full_scan_main_vs_fallback_with_overlay() {
    let db = fixture();
    // 全表（含 overlay 尾巴 999999 与删除 5）——Main+Delta 归并 vs 行路径
    diff(&db, "SELECT id, v FROM t", &["auto", "main", "fallback"]);
    // 覆盖两处 overlay 特征值
    let (_, rows) = run(&db, "main", "SELECT v FROM t WHERE id = 999999");
    assert_eq!(rows.len(), 1, "overlay 插入行必须可见：{rows:?}");
    let (_, rows2) = run(&db, "main", "SELECT v FROM t WHERE id = 5");
    assert_eq!(rows2.len(), 0, "overlay 删除行必须不可见");
}

#[test]
fn forced_structural_impossibilities_error() {
    let db = fixture();
    // 无版本子句强制 prolly → 报错（不静默回落）
    let mut s = db.new_session();
    s.exec("SET dendro.force_source = 'prolly'").unwrap();
    assert!(s.exec("SELECT v FROM t WHERE id = 1").is_err());
    // 无段表强制 main → 报错
    let mut s2 = db.new_session();
    s2.exec("CREATE TABLE small (id BIGINT PRIMARY KEY)")
        .unwrap();
    s2.exec("SET dendro.force_source = 'main'").unwrap();
    assert!(s2.exec("SELECT * FROM small").is_err());
    // 无 pk 谓词强制 delta → 报错
    let mut s3 = db.new_session();
    s3.exec("SET dendro.force_source = 'delta'").unwrap();
    assert!(s3.exec("SELECT * FROM t WHERE v > 5").is_err());
    // auto 恢复
    let mut s4 = db.new_session();
    s4.exec("SET dendro.force_source = 'auto'").unwrap();
    assert!(s4.exec("SELECT v FROM t WHERE v > 5").is_ok());
}

#[test]
fn explain_shows_dispatch() {
    let db = fixture();
    let mut s = db.new_session();
    let outs = s.exec("EXPLAIN SELECT v FROM t WHERE id = 500").unwrap();
    let (cols, rows) = rows_of(&outs);
    assert_eq!(cols, vec!["QUERY PLAN"]);
    // L4 统一：dendro.ir v1 计划方言（派发标签退役——可观测面由
    // EXPLAIN ANALYZE 的 est/actual 与 force 轴差分承接）
    let joined = format!("{rows:?}");
    assert!(joined.contains("dendro.ir v1"), "{joined}");
    assert!(joined.contains("table "), "{joined}");
    assert!(joined.contains("pred "), "{joined}");
    // 强制轴：计划文本与 auto 一致（force 是物理执行轴，不进逻辑计划）
    let mut s2 = db.new_session();
    s2.exec("SET dendro.force_source = 'main'").unwrap();
    let outs2 = s2.exec("EXPLAIN SELECT v FROM t WHERE id = 500").unwrap();
    let j2 = format!("{:?}", rows_of(&outs2).1);
    assert_eq!(j2, joined, "force_source 不改变逻辑计划输出");
}

/// AS OF（HistoryScan 臂）：auto 与 forced prolly 在带版本子句查询上等价。
/// 取最近合法时间点（夹具建好后 1s 前——落后于最后提交 ⇒ 完整可见集）。
/// 早于一切的点两条路径同错（22023），不走 diff。
#[test]
fn as_of_history_arm_equivalent() {
    let db = fixture();
    // 现时刻：必然 ≥ 最新快照点（at-or-before 闭区间），返回完整可见集
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    diff(
        &db,
        &format!("SELECT id FROM t FOR SYSTEM_TIME AS OF {ts}"),
        &["auto", "prolly"],
    );
}

/// v2c-2 归并源新保证：① AP 路径输出**恒 pk 有序**（此前仅 overlay 非空
/// 分支有序——段乱序输出是路径相关序的来源之一，现已收敛）；
/// ② pushdown_limit 早停两分支同口径。
#[test]
fn merge_source_ordered_output_without_overlay() {
    use dendro_core::types::Output;
    let db = {
        let db = Database::open(DbOptions {
            store: StoreConfig::Memory,
            ..Default::default()
        })
        .unwrap();
        db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
            row_group_rows: 4096,
        }));
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
            .unwrap();
        for chunk in 0..12 {
            // 交错块序写入（块内递增、块间倒序）——若段收集保序错误会被放大
            let vals: Vec<String> = (0..1000)
                .rev()
                .map(|i| {
                    let id = (11 - chunk) * 1000 + i + 1;
                    format!("({id}, {id})")
                })
                .collect();
            s.exec(&format!("INSERT INTO t VALUES {}", vals.join(",")))
                .unwrap();
        }
        db.checkpoint_branch("main").unwrap();
        db
    };
    let mut s = db.new_session();
    s.exec("SET dendro.force_source = 'main'").unwrap();
    let outs = s.exec("SELECT id FROM t").unwrap();
    let ids: Vec<i64> = match &outs[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].as_deref().unwrap_or_default().parse().unwrap())
            .collect(),
        _ => panic!(),
    };
    assert_eq!(ids.len(), 12_000);
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "无 overlay 时 AP 输出必须恒 pk 有序（归并源）");
    // cap 同口径：LIMIT + ORDER BY 顶截断正确
    let outs2 = s
        .exec("SELECT id FROM t WHERE v > 500 ORDER BY id DESC LIMIT 3")
        .unwrap();
    let top: Vec<i64> = match &outs2[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].as_deref().unwrap_or_default().parse().unwrap())
            .collect(),
        _ => panic!(),
    };
    assert_eq!(top.len(), 3);
    assert_eq!(top[0], 12_000, "DESC 顶三：{top:?}");
}

/// v2c-4 流式归并源（exec::source）：多段覆盖 + col_deletes 重插 +
/// 显式事务写（Q-14）的全语义矩阵，Main vs 行路径逐项等价。
/// 段拓扑：checkpoint① 12k 行 → UPDATE/INSERT/DELETE → checkpoint②
/// （段2 含同键新版本 + 新行；col_deletes 增 50 键）→ overlay 尾巴
/// （含对 col_deletes 键的重插——段源抑制不波及尾巴）→ 未提交事务写。
#[test]
fn streaming_source_multi_segment_overlay_txn_matrix() {
    let db = {
        let db = Database::open(DbOptions {
            store: StoreConfig::Memory,
            ..Default::default()
        })
        .unwrap();
        db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
            row_group_rows: 4096,
        }));
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
            .unwrap();
        for chunk in 0..12 {
            let vals: Vec<String> = (0..1000)
                .map(|i| {
                    let id = chunk * 1000 + i + 1;
                    format!("({id}, {id})")
                })
                .collect();
            s.exec(&format!("INSERT INTO t VALUES {}", vals.join(",")))
                .unwrap();
        }
        db.checkpoint_branch("main").unwrap(); // 段1：12k
                                               // 增量：更新段1内 100 行（同键新版本）、插入 1k 新行、删除 50 行
        s.exec("UPDATE t SET v = v + 100000 WHERE id <= 100")
            .unwrap();
        let ins: Vec<String> = (0..1000)
            .map(|i| format!("({}, {})", 20000 + i, 20000 + i))
            .collect();
        s.exec(&format!("INSERT INTO t VALUES {}", ins.join(",")))
            .unwrap();
        s.exec("DELETE FROM t WHERE id BETWEEN 5000 AND 5049")
            .unwrap();
        db.checkpoint_branch("main").unwrap(); // 段2 + col_deletes
                                               // overlay 尾巴：重插一个 col_deletes 键（段源抑制不波及尾巴）+
                                               // 常规更新 + 墓碑
        s.exec("INSERT INTO t VALUES (5005, 777777)").unwrap();
        s.exec("UPDATE t SET v = v + 200000 WHERE id = 10001")
            .unwrap();
        s.exec("DELETE FROM t WHERE id = 10002").unwrap();
        // 显式事务写（未提交；Q-14——AP 路径读事务的写）。
        // 断言必须在事务存活期内做（session drop = 回滚）
        s.exec("BEGIN").unwrap();
        s.exec("UPDATE t SET v = v + 300000 WHERE id = 10003")
            .unwrap();
        s.exec("DELETE FROM t WHERE id = 10004").unwrap();
        s.exec("SET dendro.force_source = 'main'").unwrap();
        let r = s.exec("SELECT v FROM t WHERE id = 10003").unwrap();
        let v = match &r[0] {
            Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
            _ => panic!(),
        };
        assert_eq!(v, "310003", "事务写必须在 AP 路径可见（Q-14）");
        let r2 = s.exec("SELECT v FROM t WHERE id = 10004").unwrap();
        assert!(
            matches!(&r2[0], Output::Rows(rs) if rs.text_rows().is_empty()),
            "事务删除必须在 AP 路径可见（Q-14）"
        );
        s.exec("SET dendro.force_source = 'auto'").unwrap();
        s.exec("ROLLBACK").unwrap(); // 回滚——后续差分不带事务写
        db
    };
    // 全表等价（13k - 50 + 1 行级别；两路径行集必须一致）
    diff(&db, "SELECT id, v FROM t", &["auto", "main", "fallback"]);
    // 特征值逐项断言（main 臂）
    let q = |sql: &str| -> Vec<i64> {
        let (_, rows) = run(&db, "main", sql);
        rows.into_iter()
            .map(|r| match r[1] {
                SqlValue::Int64(v) => v,
                SqlValue::Int32(v) => v as i64,
                _ => panic!("{r:?}"),
            })
            .collect()
    };
    // 段2 覆盖段1（id=1 两段都有 → 新版本 +100000）
    assert_eq!(q("SELECT id, v FROM t WHERE id = 1"), vec![100001]);
    // col_deletes 抑制段源，但 overlay 重插的 5005 可见（值 777777）
    assert_eq!(q("SELECT id, v FROM t WHERE id = 5005"), vec![777777]);
    // 删除区间外相邻键正常（5050；区间内 5004 不可见）
    assert_eq!(q("SELECT id, v FROM t WHERE id = 5050"), vec![5050]);
    assert_eq!(
        run(&db, "main", "SELECT id FROM t WHERE id = 5004").1.len(),
        0
    );
    // overlay 更新 + 墓碑
    assert_eq!(q("SELECT id, v FROM t WHERE id = 10001"), vec![210001]);
    assert_eq!(
        run(&db, "main", "SELECT id FROM t WHERE id = 10002")
            .1
            .len(),
        0
    );
    // 事务已回滚（fixture 尾部 ROLLBACK）——写不可见
    assert_eq!(q("SELECT id, v FROM t WHERE id = 10003"), vec![10003]);
    assert_eq!(q("SELECT id, v FROM t WHERE id = 10004"), vec![10004]);
    // pk 范围 + LIMIT 早停同口径
    diff(
        &db,
        "SELECT id FROM t WHERE id > 19999 ORDER BY id LIMIT 5",
        &["main", "fallback"],
    );
    // 行数等价（两路径 count 一致）
    diff(&db, "SELECT count(*) FROM t", &["main", "fallback"]);
}
