//! time travel 集成测试（P1-10 的非确定性面——依赖墙钟/哈希，不进 slt 语料）：
//! - 哈希精读：AS OF '<commit_hash>' 原样读该提交的物化树
//! - 中间时刻：两次 CHECKPOINT 之间的时间戳 → 解析到**较早**提交
//! - 跨 fork 回溯：分支第一父链穿越 fork 点，可读源分支创建前的历史
//! - 错误路径：哈希不存在（退化为时间戳解析失败）→ 22023

use dendro_core::{Database, DbOptions, Output};
use std::sync::Arc;

fn q(db: &Arc<Database>, sql: &str) -> Vec<Vec<String>> {
    let mut s = db.new_session();
    let outs = s.exec(sql).unwrap();
    let mut result = Vec::new();
    for o in &outs {
        if let Output::Rows(rs) = o {
            for row in rs.text_rows() {
                result.push(
                    row.iter()
                        .map(|c| c.clone().unwrap_or_else(|| "NULL".into()))
                        .collect(),
                );
            }
        }
    }
    result
}

/// 取分支第一父链上第 k 个提交哈希：0 = HEAD
fn commit_at(db: &Arc<Database>, branch: &str, k: usize) -> String {
    let rows = q(
        db,
        &format!("SELECT commit FROM cambium.commit_log('{branch}')"),
    );
    rows.get(k)
        .unwrap_or_else(|| panic!("commit_log[{k}] 缺失：{rows:?}"))[0]
        .clone()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 三次 checkpoint 的历史：v: c0 → c1 → c2（HEAD）。
/// 返回 (提交1哈希, 提交1与提交2之间的墙钟时刻)——后者必解析到提交 1。
fn setup_history(db: &Arc<Database>) -> (String, i64) {
    // HEAD 哈希在每次 CHECKPOINT 后立即采样（不依赖链内下标——catalog 提交
    // 与 CREATE BRANCH 的隐式 checkpoint 都会占链位）
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    s.exec("INSERT INTO t VALUES (1, 'c0')").unwrap();
    s.exec("CHECKPOINT").unwrap();
    s.exec("UPDATE t SET v = 'c1' WHERE id = 1").unwrap();
    s.exec("CHECKPOINT").unwrap();
    let h_c1 = commit_at(db, "main", 0); // HEAD = c1 提交
    let between = now_ms(); // ≥ c1 提交的 ts，< c2 提交的 ts
    s.exec("UPDATE t SET v = 'c2' WHERE id = 1").unwrap();
    s.exec("CHECKPOINT").unwrap();
    let _h_c2 = commit_at(db, "main", 0);
    (h_c1, between)
}

#[test]
fn as_of_hash_reads_exact_commit() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let (h_c1, _) = setup_history(&db);
    // 采样于 c1 CHECKPOINT 之后的 HEAD 哈希 → 内容恰为 c1
    let v = q(
        &db,
        &format!("SELECT v FROM t FOR SYSTEM_TIME AS OF '{h_c1}'"),
    );
    assert_eq!(v, vec![vec!["c1".to_string()]], "哈希精读该提交物化树");
    // 当前 HEAD → 最新
    let h_head = commit_at(&db, "main", 0);
    assert_eq!(
        q(
            &db,
            &format!("SELECT v FROM t FOR SYSTEM_TIME AS OF '{h_head}'")
        ),
        vec![vec!["c2".to_string()]]
    );
}

#[test]
fn as_of_timestamp_between_commits_resolves_earlier() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let (h1, between) = setup_history(&db);
    // 提交 1 与提交 2 之间的墙钟时刻 → 解析到较早的提交 1
    let v = q(
        &db,
        &format!("SELECT v FROM t FOR SYSTEM_TIME AS OF {between}"),
    );
    assert_eq!(v, vec![vec!["c1".to_string()]]);
    // 与哈希解析等价：between 的解析目标就是 h1
    assert_eq!(
        q(
            &db,
            &format!("SELECT v FROM t FOR SYSTEM_TIME AS OF '{h1}'")
        ),
        v
    );
}

#[test]
fn as_of_crosses_fork_boundary() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let setup_hashes = setup_history(&db);
    {
        let mut s = db.new_session();
        s.exec("CREATE BRANCH dev FROM main").unwrap();
    }
    // dev 的第一父链继承 main 全部历史
    let dev_head = commit_at(&db, "dev", 0);
    assert_eq!(
        q(
            &db,
            &format!("SELECT v FROM t FOR SYSTEM_TIME AS OF '{dev_head}'")
        ),
        vec![vec!["c2".to_string()]],
        "dev 首个提交即 fork 点（源 HEAD）"
    );
    // dev 上往前回溯应到达 main 的历史（链跨 fork 边界）
    let (main_c1, _) = setup_hashes;
    assert_eq!(
        q(
            &db,
            &format!("USE BRANCH dev; SELECT v FROM t FOR SYSTEM_TIME AS OF '{main_c1}'")
        ),
        vec![vec!["c1".to_string()]]
    );
}

#[test]
fn as_of_errors_and_dialect_fallback() {
    let db = Database::open(DbOptions::memory()).unwrap();
    setup_history(&db);
    let mut s = db.new_session();
    // base32 合法但 CAS 无此对象 → 退化为时间戳解析 → 失败 22023
    //（base32 字母表无 0/1/8/9？有——'0'合法字符集见 Hash::from_base32；
    //  用明显非法的哈希保证 CAS miss + 非 数字）
    let err = s
        .exec("SELECT * FROM t FOR SYSTEM_TIME AS OF 'zzzzzzzzzzzzzzzz'")
        .unwrap_err();
    assert_eq!(err.state, "22023", "{err}");
    // PG 方言本身不解析该子句；重解析兜底后才可用——上面的成功路径
    // 已隐式验证。此处验证不含子句的普通查询不受影响：
    s.exec("SELECT * FROM t").unwrap();
    // PG 语义：双引号 = 标识符（非字符串）→ AS OF 收到 Identifier → 0A000
    let err = s
        .exec("SELECT v FROM t FOR SYSTEM_TIME AS OF \"9999999999999\"")
        .unwrap_err();
    assert_eq!(err.state, "0A000", "{err}");
}

#[test]
fn as_of_time_travel_is_read_only_and_stable() {
    let db = Database::open(DbOptions::memory()).unwrap();
    setup_history(&db);
    let h1 = commit_at(&db, "main", 1);
    let sql = format!("SELECT v FROM t FOR SYSTEM_TIME AS OF '{h1}'");
    // 历史树不可变：重复读结果一致；当前分支继续写不影响
    let first = q(&db, &sql);
    {
        let mut s = db.new_session();
        s.exec("UPDATE t SET v = 'later' WHERE id = 1").unwrap();
        s.exec("CHECKPOINT").unwrap();
    }
    assert_eq!(q(&db, &sql), first);
    assert_eq!(first, vec![vec!["c1".to_string()]]);
    // JOIN 两侧独立 time travel（同为历史提交）
    assert_eq!(
        q(
            &db,
            &format!(
                "SELECT a.v, b.v FROM t FOR SYSTEM_TIME AS OF '{h1}' a \
                 JOIN t FOR SYSTEM_TIME AS OF '{h1}' b ON a.id = b.id"
            )
        ),
        vec![vec!["c1".to_string(), "c1".to_string()]]
    );
}
