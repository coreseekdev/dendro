//! dendro × SQLite TP 对比基线（双维判定：磁盘不弱于 / MemTx 远超）。
//!
//! 公平口径：
//! - SQLite：`prepare_cached` + 参数绑定（其最佳实践）；磁盘臂
//!   WAL + synchronous=NORMAL（TP 常规配置）
//! - dendro：SQL 文本经形状缓存（自动 prepare——我们的等价机制）
//! - 负载：1M 行 (id BIGINT PK, v BIGINT, tag TEXT)；点查/范围100/
//!   随机 UPDATE / 批量 INSERT 四场景，median of 5×1s

use crate::bench::{BenchResult, BenchRow};
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use rusqlite::Connection as SqConn;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn bench<F: FnMut(u64)>(mut f: F) -> f64 {
    for i in 0..2000u64 {
        f(i);
    }
    let mut rates = Vec::with_capacity(5);
    for _ in 0..5 {
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed() < Duration::from_secs(1) {
            f(n);
            n += 1;
        }
        rates.push(n as f64 / t.elapsed().as_secs_f64());
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    rates[2]
}

const DDL: &str = "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)";

pub fn bench_sqlite_cmp(
    data_dir: &PathBuf,
    rows: u64,
    out: &PathBuf,
) -> BenchResult {
    let mut ro: Vec<BenchRow> = Vec::new();
    fn push(ro: &mut Vec<BenchRow>, k: &str, m: &str, v: f64) {
        ro.push(BenchRow {
            name: format!("{k}.{m}"),
            value: v,
            unit: "ops/s".into(),
        });
    }

    // ================= SQLite =================
    std::fs::create_dir_all(data_dir).unwrap();
    for (mode, mem) in [("disk", false), ("mem", true)] {
        let path = if mem {
            ":memory:".to_string()
        } else {
            format!("{}/cmp.sqlite", data_dir.display())
        };
        let mut sq = SqConn::open(&path).unwrap();
        if !mem {
            sq.pragma_update(None, "journal_mode", "WAL").unwrap();
            sq.pragma_update(None, "synchronous", "NORMAL").unwrap();
        }
        sq.execute(DDL, []).unwrap();
        let t0 = Instant::now();
        let tx = sq.transaction().unwrap();
        {
            let mut ins = tx
                .prepare("INSERT INTO t VALUES (?, ?, ?)")
                .unwrap();
            for ch in 0..(rows / 10_000) {
                for i in 0..10_000u64 {
                    let id = ch * 10_000 + i + 1;
                    ins.execute(rusqlite::params![
                        id as i64,
                        (id % 1000) as i64,
                        format!("tag{}", id % 97)
                    ])
                    .unwrap();
                }
            }
        }
        tx.commit().unwrap();
        push(&mut ro, "sqlite.load", mode, t0.elapsed().as_secs_f64());

        // 点查（prepare_cached + 绑定——最佳实践）
        let p = bench(|i| {
            let id = ((i * 7919) % rows + 1) as i64;
            let mut st = sq
                .prepare_cached("SELECT v, tag FROM t WHERE id = ?")
                .unwrap();
            let _ = st.query_row([id], |_r| Ok(()));
        });
        push(&mut ro, "sqlite.point", mode, p);
        let r = bench(|i| {
            let lo = ((i * 7919) % (rows - 100_000).max(1) + 1) as i64;
            let mut st = sq
                .prepare_cached("SELECT id, v FROM t WHERE id >= ? AND id < ?")
                .unwrap();
            let mut rows = st.query([lo, lo + 100]).unwrap();
            while rows.next().unwrap().is_some() {}
        });
        push(&mut ro, "sqlite.range100", mode, r);
        let w = bench(|i| {
            let id = ((i * 104729) % rows + 1) as i64;
            sq.execute("UPDATE t SET v = v + 1 WHERE id = ?", [id])
                .unwrap();
        });
        push(&mut ro, "sqlite.update", mode, w);
        let ins = bench(|i| {
            let id = rows + (i % 50_000) + 1;
            sq.execute(
                "INSERT OR REPLACE INTO t VALUES (?, 1, 'z')",
                [id as i64],
            )
            .unwrap();
        });
        push(&mut ro, "sqlite.insert", mode, ins);
    }

    // ================= dendro =================
    for (mode, mem, ckpt) in [("disk", false, true), ("mem", true, false)] {
        let dir = data_dir.join(format!("dendro-{mode}"));
        let _ = std::fs::remove_dir_all(&dir);
        let store = if mem {
            StoreConfig::Memory
        } else {
            StoreConfig::LocalDir(dir.clone())
        };
        let mut c = Connection::open_with(&dir, DbOptions::embedded(store)).unwrap();
        c.execute(DDL).unwrap();
        let t0 = Instant::now();
        for ch in 0..(rows / 10_000) {
            let vals: Vec<String> = (0..10_000)
                .map(|i| {
                    let id = ch * 10_000 + i + 1;
                    format!("({id}, {}, 'tag{}')", id % 1000, id % 97)
                })
                .collect();
            c.execute(&format!("INSERT INTO t VALUES {}", vals.join(",")))
                .unwrap();
        }
        if ckpt {
            c.execute("CHECKPOINT").unwrap(); // 树驻留 = 磁盘底座
        }
        push(&mut ro, "dendro.load", mode, t0.elapsed().as_secs_f64());

        let p = bench(|i| {
            let id = ((i * 7919) % rows + 1) as i64;
            let _ = c.query(&format!("SELECT v, tag FROM t WHERE id = {id}"));
        });
        push(&mut ro, "dendro.point", mode, p);
        let r = bench(|i| {
            let lo = ((i * 7919) % (rows - 100_000).max(1) + 1) as i64;
            let _ = c.query(&format!(
                "SELECT id, v FROM t WHERE id >= {lo} AND id < {}",
                lo + 100
            ));
        });
        push(&mut ro, "dendro.range100", mode, r);
        let w = bench(|i| {
            let id = ((i * 104729) % rows + 1) as i64;
            let _ = c.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {id}"));
        });
        push(&mut ro, "dendro.update", mode, w);
        let ins = bench(|i| {
            let id = rows + (i % 50_000) + 1;
            let _ = c.execute(&format!("INSERT OR REPLACE INTO t VALUES ({id}, 1, 'z')"));
        });
        push(&mut ro, "dendro.insert", mode, ins);
    }

    // 判定行（先取值后推入——避免借用交叉）
    let ratios: Vec<(String, String, f64)> = {
        let get = |e: &str, m: &str, k: &str| {
            ro.iter()
                .find(|r| r.name == format!("{e}.{k}.{m}"))
                .map(|r| r.value)
                .unwrap_or(0.0)
        };
        [("point", "disk"), ("range100", "disk"), ("update", "disk"), ("insert", "disk"), ("point", "mem"), ("update", "mem"), ("insert", "mem")]
            .into_iter()
            .map(|(k, m)| {
                let d = get("dendro", m, k);
                let s = get("sqlite", m, k);
                (format!("ratio.{k}"), m.to_string(), if s > 0.0 { d / s } else { 0.0 })
            })
            .collect()
    };
    for (name, m, ratio) in ratios {
        push(&mut ro, &name, &m, ratio);
    }

    let r = BenchResult {
        suite: "sqlite-cmp".into(),
        rows: ro,
    };
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(out, r.to_json()).unwrap();
    r
}
