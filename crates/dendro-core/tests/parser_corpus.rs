//! P 层解析冒烟（ir-spec B0-a/b/c）：
//! - B0-a：sqlparser-rs 上游 PG/MySQL 方言语料——解析不 panic、**解析幂等**
//!   （两次解析产出恒同 AST Debug 串）、方言覆盖率地板（防 parser 升级
//!   静默回退）；
//! - B0-b：DuckDB 真实负载抽样（PG 方言）——同上；
//! - B0-c：种子变异鲁棒性——对语料做确定性变异（翻转/插入/截断/复制），
//!   parse_only 不得 panic（不可信输入边界，与 WAL 帧 Kani H1 同纪律；
//!   cargo-fuzz nightly 集成是后续升级路径）。
//!
//! 语料 vendor 来源与许可证见各文件头（Apache-2.0 / MIT）。

mod common;

use dendro_core::sql::{parse_only, SqlDialect};

fn load(corpus: &str) -> Vec<(SqlDialect, String)> {
    corpus
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let (d, sql) = l.split_once('\t').expect("corpus 格式：dialect<TAB>sql");
            let sql = sql.replace("\\n", "\n").replace("\\\\", "\\");
            (
                match d {
                    "pg" => SqlDialect::Pg,
                    "mysql" => SqlDialect::MySql,
                    other => panic!("未知方言标签 {other}"),
                },
                sql,
            )
        })
        .collect()
}

fn smoke(name: &str, cases: &[(SqlDialect, String)]) -> (usize, usize) {
    let (mut ok, mut err) = (0, 0);
    for (d, sql) in cases {
        let r1 = parse_only(sql, *d);
        let r2 = parse_only(sql, *d);
        assert_eq!(
            r1,
            r2,
            "{name}: 解析不幂等（同 SQL 两次结果不同）：{}",
            &sql[..sql.len().min(120)]
        );
        match r1 {
            Ok(_) => ok += 1,
            Err(_) => err += 1, // 语法不支持是预期（方言子集），仅计数
        }
    }
    (ok, err)
}

#[test]
fn b0a_sqlparser_pg_corpus() {
    let cases = load(include_str!("parser_corpus/postgres.txt"));
    assert!(!cases.is_empty());
    let (ok, err) = smoke("pg", &cases);
    let cov = ok as f64 / (ok + err) as f64 * 100.0;
    println!(
        "B0-a PG 语料: {} 条，解析通过 {:.1}%（{ok}/{})",
        cases.len(),
        cov,
        ok + err
    );
    // 覆盖率地板：parser 升级/包装层改动的静默回退守卫（实测基线 -3pp）
    assert!(cov >= 62.0, "PG 方言覆盖率跌破地板：{cov:.1}%（基线 67.4）");
}

#[test]
fn b0a_sqlparser_mysql_corpus() {
    let cases = load(include_str!("parser_corpus/mysql.txt"));
    assert!(!cases.is_empty());
    let (ok, err) = smoke("mysql", &cases);
    let cov = ok as f64 / (ok + err) as f64 * 100.0;
    println!("B0-a MySQL 语料: {} 条，解析通过 {:.1}%", cases.len(), cov);
    assert!(
        cov >= 85.0,
        "MySQL 方言覆盖率跌破地板：{cov:.1}%（基线 90.2）"
    );
}

#[test]
fn b0b_duckdb_workload_sample() {
    let cases = load(include_str!("parser_corpus/duckdb_sample.txt"));
    let (ok, err) = smoke("duckdb", &cases);
    let cov = ok as f64 / (ok + err) as f64 * 100.0;
    println!("B0-b DuckDB 抽样: {} 条，解析通过 {:.1}%", cases.len(), cov);
    // DuckDB 超集方言（实测基线 87.0%）
    assert!(cov >= 82.0, "DuckDB 负载覆盖率跌破地板：{cov:.1}%");
}

/// B0-c：种子变异鲁棒性（确定性：xorshift64，种子固定）
#[test]
fn b0c_mutation_no_panic() {
    let cases = load(include_str!("parser_corpus/postgres.txt"));
    let mut seed: u64 = 0x2026_0915_dead_beef;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut mutated = 0usize;
    for (d, sql) in cases.iter().take(600) {
        let bytes = sql.as_bytes();
        if bytes.len() < 4 {
            continue;
        }
        for _ in 0..4 {
            let kind = next() % 4;
            let pos = (next() as usize) % bytes.len();
            let m: String = match kind {
                0 => {
                    // 字节翻转
                    let mut b = bytes.to_vec();
                    b[pos] ^= (next() % 255 + 1) as u8;
                    String::from_utf8_lossy(&b).into_owned()
                }
                1 => {
                    // 插入随机字节
                    let mut b = bytes.to_vec();
                    b.insert(pos, (next() % 128) as u8);
                    String::from_utf8_lossy(&b).into_owned()
                }
                2 => sql[..pos.min(sql.len())].to_string(), // 截断
                _ => format!("{sql}{sql}"),                 // 重复拼接
            };
            let r = std::panic::catch_unwind(|| parse_only(&m, *d));
            assert!(
                r.is_ok(),
                "B0-c 变异输入触发 panic：{}",
                &m[..m.len().min(120)]
            );
            mutated += 1;
        }
    }
    println!("B0-c 变异用例 {mutated} 条，无 panic");
    assert!(mutated > 1000);
}
