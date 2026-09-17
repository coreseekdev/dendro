//! bench_pair —— A/B 配对评测纪律（qorl 调研 P0-1 落地）。
//!
//! 协议（对任意优化规则的效果主张统一生效）：
//! 1. **预热**：两侧交替各 WARM 次（缓存/页状态稳定），不计时；
//! 2. **配对交替**：(候选, 基线) 同轮紧邻执行，先手逐轮翻转（防位置
//!    偏置——缓存残余让后手占优的系统性误差）；
//! 3. **每侧中位数**（of ROUNDS），非均值（长尾免疫）；
//! 4. **±5% 平局区**：|ratio−1| ≤ 5% 判 TIE，不算赢；
//! 5. **愚弄率**（fooled rate）：各轮配对方向与中位数方向不一致的
//!    占比——衡量"把慢侧测成快侧"的奖励信号污染，**替代变异系数**
//!    （CV 衡量方差不衡量方向性错误）。
//!
//! 旋钮：`SET dendro.optimize`（候选 = on / 基线 = off）。效果主张
//! 的 PR 附本工具输出（benches/results/pair.json）。

use super::{mem_db, BenchResult, BenchRow};
use dendro_core::Durability;
use std::path::PathBuf;

const WARM: usize = 3;
const ROUNDS: usize = 7;
/// 平局区半宽（qorl 口径 ±5%）
const TIE: f64 = 0.05;

/// 一条查询的配对评测结果
pub struct PairVerdict {
    pub name: String,
    pub cand_ms: f64,
    pub base_ms: f64,
    pub fooled: f64,
}

/// 配对样本 → 判定（纯函数，单测锁定口径）
/// 返回 (候选中位数, 基线中位数, 判定标签, 愚弄率)
pub fn pair_stats(cand: &[f64], base: &[f64]) -> (f64, f64, &'static str, f64) {
    let cm = median(cand);
    let bm = median(base);
    let ratio = cm / bm;
    let label = if ratio <= 1.0 - TIE {
        "WIN"
    } else if ratio >= 1.0 + TIE {
        "LOSS"
    } else {
        "TIE"
    };
    // 愚弄率：与中位数方向不一致的轮次占比（TIE 时方向无意义，
    // 记各轮翻转频率——仍是有意义的噪声度量）
    let dir = if cm == bm { 0.0 } else { f64::signum(cm - bm) };
    let fooled = cand
        .iter()
        .zip(base.iter())
        .map(|(c, b)| {
            let d = if c == b { 0.0 } else { f64::signum(c - b) };
            if dir == 0.0 {
                // TIE：任一侧显著偏离（非零方向）都算噪声轮
                f64::from(d != 0.0)
            } else {
                f64::from(d != dir && d != 0.0)
            }
        })
        .sum::<f64>()
        / cand.len() as f64;
    (cm, bm, label, fooled)
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

fn timed_query(s: &mut dendro_core::engine::Session, optimize_on: bool, sql: &str) -> f64 {
    s.exec(if optimize_on {
        "SET dendro.optimize = 'on'"
    } else {
        "SET dendro.optimize = 'off'"
    })
    .unwrap();
    let t = std::time::Instant::now();
    let out = s.exec(sql).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1e3;
    std::hint::black_box(&out);
    ms
}

/// 分析负载上的 optimize on/off 配对评测（固定种子数据 + 5 类
/// 规则敏感查询：范围排序/InList/下推 join/分组 HAVING/半连接）
pub fn bench_pair(n: usize, out: &PathBuf) -> BenchResult {
    let db = mem_db(Durability::NoWait, 1);
    let mut s = db.new_session();
    // 数据：t(主表) + c(维表) + o(事实表)——确定性生成
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, g BIGINT)")
        .unwrap();
    s.exec("CREATE TABLE c (id BIGINT PRIMARY KEY, region TEXT)")
        .unwrap();
    s.exec("CREATE TABLE o (id BIGINT PRIMARY KEY, cid BIGINT, total BIGINT)")
        .unwrap();
    let chunk = 1000;
    for lo in (0..n).step_by(chunk) {
        let hi = (lo + chunk).min(n);
        let vals: Vec<String> = (lo..hi)
            .map(|i| format!("({i}, {}, {})", (i * 7919) % 1_000_000, i % 64))
            .collect();
        s.exec(&format!("INSERT INTO t VALUES {}", vals.join(",")))
            .unwrap();
        let ovals: Vec<String> = (lo..hi)
            .map(|i| format!("({i}, {}, {})", i % 500, (i * 104729) % 100_000))
            .collect();
        s.exec(&format!("INSERT INTO o VALUES {}", ovals.join(",")))
            .unwrap();
    }
    let cvals: Vec<String> = (0..500)
        .map(|i| format!("({i}, '{}')", ["EU", "US", "APAC"][i % 3]))
        .collect();
    s.exec(&format!("INSERT INTO c VALUES {}", cvals.join(",")))
        .unwrap();

    let half = n / 2;
    let inlist: Vec<String> = (0..50).map(|i| format!("{}", i * 1999)).collect();
    let queries: Vec<(&str, String)> = vec![
        (
            "range_sort_limit",
            "SELECT id FROM t WHERE v >= 500000 ORDER BY id DESC LIMIT 200".to_string(),
        ),
        (
            "in_list_rewrite",
            format!("SELECT count(*) FROM t WHERE id IN ({})", inlist.join(",")),
        ),
        (
            "join_pushdown",
            "SELECT count(*) FROM o JOIN c ON o.cid = c.id WHERE c.region = 'EU' AND o.total >= 50000".to_string(),
        ),
        (
            "group_having",
            "SELECT g, count(*) FROM t WHERE v >= 400000 GROUP BY g HAVING count(*) > 2 ORDER BY g LIMIT 50".to_string(),
        ),
        (
            "semi_join",
            "SELECT count(*) FROM o WHERE cid IN (SELECT id FROM c WHERE region = 'EU')".to_string(),
        ),
    ];
    let _ = half;

    let mut rows = Vec::new();
    let mut fooled_all: Vec<f64> = Vec::new();
    for (name, sql) in &queries {
        // 预热：两侧交替各 WARM 次（不计时）
        for w in 0..WARM {
            let first_cand = w % 2 == 0;
            for on in [first_cand, !first_cand] {
                timed_query(&mut s, on, sql);
            }
        }
        // 配对交替：先手逐轮翻转
        let mut cand = Vec::with_capacity(ROUNDS);
        let mut base = Vec::with_capacity(ROUNDS);
        for r in 0..ROUNDS {
            let first_cand = r % 2 == 0;
            if first_cand {
                cand.push(timed_query(&mut s, true, sql));
                base.push(timed_query(&mut s, false, sql));
            } else {
                base.push(timed_query(&mut s, false, sql));
                cand.push(timed_query(&mut s, true, sql));
            }
        }
        let (cm, bm, label, fooled) = pair_stats(&cand, &base);
        fooled_all.push(fooled);
        rows.push(BenchRow {
            name: format!("{name}.cand_ms[{label}]"),
            value: cm,
            unit: "ms",
        });
        rows.push(BenchRow {
            name: format!("{name}.base_ms"),
            value: bm,
            unit: "ms",
        });
        rows.push(BenchRow {
            name: format!("{name}.ratio"),
            value: cm / bm,
            unit: "x",
        });
        rows.push(BenchRow {
            name: format!("{name}.fooled_rate"),
            value: fooled,
            unit: "ratio",
        });
    }
    rows.push(BenchRow {
        name: "global.fooled_rate".into(),
        value: fooled_all.iter().sum::<f64>() / fooled_all.len() as f64,
        unit: "ratio",
    });
    let r = BenchResult {
        suite: "pair".into(),
        rows,
    };
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(out, r.to_json()).unwrap();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 清晰信号：愚弄率 0、判 WIN
    #[test]
    fn stats_clear_win() {
        let cand = [10.0, 10.1, 10.2, 10.0, 10.1];
        let base = [20.0, 19.9, 20.2, 20.1, 20.0];
        let (cm, bm, label, fooled) = pair_stats(&cand, &base);
        assert_eq!(label, "WIN");
        assert_eq!(fooled, 0.0);
        assert!((cm - 10.1).abs() < 1e-9);
        assert!((bm - 20.0).abs() < 1e-9);
    }

    /// 平局区：±5% 内判 TIE
    #[test]
    fn stats_tie_zone() {
        let cand = [100.0; 7];
        let base = [102.0; 7]; // ratio 0.980 ∈ 平局区
        let (_, _, label, _) = pair_stats(&cand, &base);
        assert_eq!(label, "TIE");
    }

    /// 方向翻转污染：5 轮中 2 轮反向 → 愚弄率 0.4（CV 可能仍小——
    /// 这正是愚弄率替代 CV 的理由）
    #[test]
    fn stats_fooled_rate_counts_flips() {
        let cand = [10.0, 21.0, 10.0, 10.0, 21.0];
        let base = [20.0, 20.0, 20.0, 20.0, 20.0];
        let (_, _, label, fooled) = pair_stats(&cand, &base);
        assert_eq!(label, "WIN"); // 中位数 10 vs 20
        assert!((fooled - 0.4).abs() < 1e-9, "{fooled}");
    }
}
