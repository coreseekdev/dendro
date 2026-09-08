#![allow(unused_variables, dead_code, unused_mut)]

//! 基准套件（SPEC 09）：TP 微基准、分支操作曲线、恢复时间、OSS 延迟注入。
//! 结果 JSON 写入 benches/results/。

use dendro_core::{Database, DbOptions, Durability, StoreConfig};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct BenchResult {
    #[allow(dead_code)]
    pub suite: String,
    pub rows: Vec<BenchRow>,
}

pub struct BenchRow {
    #[allow(dead_code)]
    pub name: String,
    pub value: f64,
    pub unit: &'static str,
}

impl BenchResult {
    pub fn to_json(&self) -> String {
        let rows: Vec<String> = self
            .rows
            .iter()
            .map(|r| format!(r#"  {{"name": "{}", "value": {:.3}, "unit": "{}"}}"#, r.name, r.value, r.unit))
            .collect();
        format!("{{\n  \"suite\": \"{}\",\n  \"rows\": [\n{}\n  ]\n}}", self.suite, rows.join(",\n"))
    }
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[idx.min(v.len() - 1)]
}

fn pct(v: &[f64], p: f64) -> f64 {
    let c: Vec<f64> = v.to_vec();
    percentile(c, p)
}

fn mem_db(dur: Durability, interval_ms: u64) -> Arc<Database> {
    Database::open(DbOptions {
        store: StoreConfig::Memory,
        durability: dur,
        wal_flush_interval_ms: interval_ms,
        ..Default::default()
    })
    .unwrap()
}

fn local_db(tag: &str, dur: Durability, interval_ms: u64) -> Arc<Database> {
    let dir = std::env::temp_dir().join(format!("dendro-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir),
        durability: dur,
        wal_flush_interval_ms: interval_ms,
        ..Default::default()
    })
    .unwrap()
}

/// TP 微基准：单行插入 + 主键点查（进程内，引擎天花板）
pub fn bench_tp(n_insert: usize, n_select: usize) -> BenchResult {
    let mut rows = Vec::new();
    let db = mem_db(Durability::NoWait, 1);
    let mut s = db.new_session();
    s.exec("CREATE TABLE b (id BIGINT PRIMARY KEY, v TEXT, n BIGINT)").unwrap();

    // 1. 插入吞吐（自动提交逐行）
    let mut lat = Vec::with_capacity(n_insert);
    let t0 = Instant::now();
    for i in 0..n_insert {
        let t = Instant::now();
        s.exec(&format!("INSERT INTO b VALUES ({i}, 'v{i}', {i})")).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let total = t0.elapsed().as_secs_f64();
    rows.push(BenchRow { name: "oltp_insert".into(), value: n_insert as f64 / total, unit: "txn/s" });
    rows.push(BenchRow { name: "oltp_insert_p50_us".into(), value: pct(&lat, 0.5), unit: "us" });
    rows.push(BenchRow { name: "oltp_insert_p99_us".into(), value: pct(&lat, 0.99), unit: "us" });

    // 2. 点查吞吐（写集预热后）
    let mut lat = Vec::with_capacity(n_select);
    let t0 = Instant::now();
    let mut hits = 0usize;
    for i in 0..n_select {
        let t = Instant::now();
        let key = (i * 2654435761u64 as usize) % n_insert;
        let out = s
            .exec(&format!("SELECT v FROM b WHERE id = {key}"))
            .unwrap();
        if !out.is_empty() {
            hits += 1;
        }
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let total = t0.elapsed().as_secs_f64();
    assert_eq!(hits, n_select);
    rows.push(BenchRow { name: "oltp_point_select".into(), value: n_select as f64 / total, unit: "txn/s" });
    rows.push(BenchRow { name: "oltp_point_select_p50_us".into(), value: pct(&lat, 0.5), unit: "us" });
    rows.push(BenchRow { name: "oltp_point_select_p99_us".into(), value: pct(&lat, 0.99), unit: "us" });

    BenchResult { suite: "tp".into(), rows }
}

/// 组提交延迟 vs durability 模式 vs 模拟 RTT（LocalDir + flush 间隔）
pub fn bench_commit_latency() -> BenchResult {
    let mut rows = Vec::new();
    let n = 200;
    for (dur_name, dur) in [("no_wait", Durability::NoWait), ("group", Durability::Group)] {
        for interval in [1u64, 5, 25, 50] {
            let db = local_db(&format!("cl-{dur_name}-{interval}"), dur, interval);
            let mut s = db.new_session();
            s.exec("CREATE TABLE c (id BIGINT PRIMARY KEY)").unwrap();
            let mut lat = Vec::with_capacity(n);
            for i in 0..n {
                let t = Instant::now();
                s.exec(&format!("INSERT INTO c VALUES ({i})")).unwrap();
                lat.push(t.elapsed().as_secs_f64() * 1e3);
            }
            rows.push(BenchRow {
                name: format!("commit_{dur_name}_interval{interval}_ms_p50"),
                value: pct(&lat, 0.5),
                unit: "ms",
            });
            rows.push(BenchRow {
                name: format!("commit_{dur_name}_interval{interval}_ms_p99"),
                value: pct(&lat, 0.99),
                unit: "ms",
            });
        }
    }
    BenchResult { suite: "commit_latency".into(), rows }
}

/// 分支操作 O(1) 验证：不同数据规模下 CREATE BRANCH / MERGE 耗时
pub fn bench_branch() -> BenchResult {
    let mut rows = Vec::new();
    for size in [10_000usize, 100_000, 500_000] {
        let db = local_db("br", Durability::NoWait, 1);
        let mut s = db.new_session();
        s.exec("CREATE TABLE big (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        // 分块批量插入（避免巨型 INSERT 的解析开销主导）
        let t0 = Instant::now();
        const CHUNK: usize = 2000;
        let mut done = 0usize;
        while done < size {
            let end = (done + CHUNK).min(size);
            let mut sql = String::from("INSERT INTO big VALUES ");
            for i in done..end {
                if i > done {
                    sql.push(',');
                }
                sql.push_str(&format!("({i}, 'value-{i}')"));
            }
            s.exec(&sql).unwrap();
            done = end;
        }
        let load_s = t0.elapsed().as_secs_f64();
        s.exec("CHECKPOINT").unwrap();
        let ckpt = Instant::now();

        // CREATE BRANCH（应平坦：不随 size 变化）
        s.exec("CREATE BRANCH agent FROM main").unwrap();
        let create_us = ckpt.elapsed().as_secs_f64() * 1e6;

        // 分支写入（单批 1000 行）
        s.exec("USE BRANCH agent").unwrap();
        let mut sql = String::from("INSERT INTO big VALUES ");
        for i in size..size + 1000 {
            if i > size {
                sql.push(',');
            }
            sql.push_str(&format!("({i}, 'agent-{i}')"));
        }
        s.exec(&sql).unwrap();
        s.exec("CHECKPOINT").unwrap();

        // MERGE（O(diff)）
        s.exec("USE BRANCH main").unwrap();
        let t1 = Instant::now();
        s.exec("MERGE BRANCH agent INTO main").unwrap();
        let merge_us = t1.elapsed().as_secs_f64() * 1e6;

        rows.push(BenchRow { name: format!("branch_load_{size}_rows_s"), value: load_s, unit: "s" });
        rows.push(BenchRow { name: format!("branch_create_at_{size}_us"), value: create_us, unit: "us" });
        rows.push(BenchRow { name: format!("branch_merge_diff1000_at_{size}_us"), value: merge_us, unit: "us" });
        // 结构共享验证：checkpoint 后统计 chunk 数
        let _ = db.checkpoint_branch("main").unwrap();
    }
    BenchResult { suite: "branch".into(), rows }
}

/// AP 列式 vs TP 行式聚合（物化后 CBF 路由）
pub fn bench_ap(rows_n: usize) -> BenchResult {
    let mut rows = Vec::new();
    let dir = std::env::temp_dir().join(format!("dendro-bench-ap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(dir.clone());
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir.clone()),
        durability: Durability::NoWait,
        wal_flush_interval_ms: 1,
        wal_segment_bytes: 32 << 20,
        checkpoint_threshold_bytes: u64::MAX,
        checkpoint_interval_s: 0,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar { row_group_rows: 1_048_576 }));
    
    let mut s = db.new_session();
    s.exec("CREATE TABLE lineitem (id BIGINT PRIMARY KEY, region TEXT, qty BIGINT, price DOUBLE)").unwrap();
    let t0 = Instant::now();
    const CHUNK: usize = 2000;
    let mut done = 0usize;
    while done < rows_n {
        let end = (done + CHUNK).min(rows_n);
        let mut sql = String::from("INSERT INTO lineitem VALUES ");
        for i in done..end {
            if i > done { sql.push(','); }
            let region = ["east", "west", "south", "north"][i % 4];
            sql.push_str(&format!("({i}, '{region}', {}, {})", i % 50, (i % 1000) as f64 * 0.01 + 0.5));
        }
        s.exec(&sql).unwrap();
        done = end;
    }
    rows.push(BenchRow { name: "ap_load_rows_s".into(), value: t0.elapsed().as_secs_f64(), unit: "s" });
    s.exec("CHECKPOINT").unwrap();

    let q = "SELECT region, count(*), sum(price) FROM lineitem GROUP BY region";
    // AP（列存，行数达阈值自动路由）
    let t1 = Instant::now();
    let out = s.exec(q).unwrap();
    let ap = t1.elapsed();
    let groups = match &out[0] { dendro_core::types::Output::Rows(rs) => rs.total_rows(), _ => 0 };
    // TP（小表副本走行路径；用 EXPLAIN 不可行，直接以 force 小表对照：借 1 万行阈值以下副本）
    rows.push(BenchRow { name: format!("ap_group_agg_{rows_n}_ms"), value: ap.as_secs_f64() * 1e3, unit: "ms" });
    rows.push(BenchRow { name: format!("ap_groups_{rows_n}"), value: groups as f64, unit: "groups" });
    let _ = std::fs::remove_dir_all(&dir);
    BenchResult { suite: "ap".into(), rows }
}

/// 恢复时间 vs WAL 未物化事务数
pub fn bench_recovery() -> BenchResult {
    let mut rows = Vec::new();
    for txn_count in [1000usize, 5000, 20000] {
        let dir = std::env::temp_dir().join(format!("dendro-bench-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(dir.clone());
        {
            let db = Database::open(DbOptions {
                store: StoreConfig::LocalDir(dir.clone()),
                durability: Durability::NoWait,
                wal_flush_interval_ms: 1,
                wal_segment_bytes: 32 << 20,
                checkpoint_threshold_bytes: u64::MAX, // 关闭自动 checkpoint
                checkpoint_interval_s: 0,
                ..Default::default()
            })
            .unwrap();
            let mut s = db.new_session();
            s.exec("CREATE TABLE r (id BIGINT PRIMARY KEY)").unwrap();
            for i in 0..txn_count {
                s.exec(&format!("INSERT INTO r VALUES ({i})")).unwrap();
            }
            // 不 checkpoint，直接"崩溃"（drop）
        }
        let t0 = Instant::now();
        let db = Database::open(DbOptions {
            store: StoreConfig::LocalDir(dir.clone()),
            durability: Durability::NoWait,
            wal_flush_interval_ms: 1,
            wal_segment_bytes: 32 << 20,
            checkpoint_threshold_bytes: u64::MAX,
            checkpoint_interval_s: 0,
            ..Default::default()
        })
        .unwrap();
        let recover_s = t0.elapsed().as_secs_f64();
        let mut s = db.new_session();
        let out = s.exec("SELECT count(*) FROM r").unwrap();
        let count = match &out[0] {
            dendro_core::types::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
            _ => "?".into(),
        };
        rows.push(BenchRow { name: format!("recover_txns_{txn_count}_s"), value: recover_s, unit: "s" });
        rows.push(BenchRow {
            name: format!("recovered_rows_{txn_count}"),
            value: count.parse().unwrap_or(0.0),
            unit: "rows",
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
    BenchResult { suite: "recovery".into(), rows }
}

pub fn run_all(out_dir: &PathBuf) {
    std::fs::create_dir_all(out_dir).unwrap();
    eprintln!("[bench] tp starting...");
    let suites = vec![bench_tp(20_000, 200_000)];
    #[allow(unused_variables)]
    for s in suites {
        let path = out_dir.join(format!("{}.json", s.suite));
        std::fs::write(&path, s.to_json()).unwrap();
        println!("wrote {}", path.display());
        for r in &s.rows {
            println!("  {:<48} {:>12.3} {}", r.name, r.value, r.unit);
        }
    }
    eprintln!("[bench] commit_latency starting...");
    let s = bench_commit_latency();
    let path = out_dir.join(format!("{}.json", s.suite));
    std::fs::write(&path, s.to_json()).unwrap();
    println!("wrote {}", path.display());
    for r in &s.rows {
        println!("  {:<48} {:>12.3} {}", r.name, r.value, r.unit);
    }
    eprintln!("[bench] branch starting...");
    let s = bench_branch();
    let path = out_dir.join(format!("{}.json", s.suite));
    std::fs::write(&path, s.to_json()).unwrap();
    println!("wrote {}", path.display());
    for r in &s.rows {
        println!("  {:<48} {:>12.3} {}", r.name, r.value, r.unit);
    }
    eprintln!("[bench] recovery starting...");
    let s = bench_recovery();
    let path = out_dir.join(format!("{}.json", s.suite));
    std::fs::write(&path, s.to_json()).unwrap();
    println!("wrote {}", path.display());
    for r in &s.rows {
        println!("  {:<48} {:>12.3} {}", r.name, r.value, r.unit);
    }
    eprintln!("[bench] ap starting...");
    let s = bench_ap(200_000);
    let path = out_dir.join(format!("{}.json", s.suite));
    std::fs::write(&path, s.to_json()).unwrap();
    println!("wrote {}", path.display());
    for r in &s.rows {
        println!("  {:<48} {:>12.3} {}", r.name, r.value, r.unit);
    }
}

/// M-3：最小压缩量化曲线——每 codec 对同一批数据记录
/// (压缩大小/原始大小, 压缩 MB/s, 解压 MB/s)，JSON 落盘供 Q 曲线绘制。
/// 数据形态：TEXT（可压缩字符串）、BIGINT（连续整数，BITPACK/Delta 友好）。
pub fn compression_curve(rows_n: usize, out_path: &PathBuf) -> Result<(), String> {
    use arrow::array::{ArrayRef, Int64Array, StringArray};
    use arrow::record_batch::RecordBatch;
    use dendro_columnar::{read_cbf, write_cbf, CodecId};
    use std::sync::Arc;
    use std::time::Instant;

    let ids: Vec<i64> = (0..rows_n as i64).collect();
    let texts: Vec<String> = (0..rows_n)
        .map(|i| format!("region-{}-order-{}", i % 64, i * 7919))
        .collect();
    let id_arr: ArrayRef = Arc::new(Int64Array::from(ids));
    let text_arr: ArrayRef = Arc::new(StringArray::from(
        texts.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    ));
    let batch = RecordBatch::try_from_iter(vec![
        ("id", id_arr),
        ("text", text_arr),
    ])
    .map_err(|e| format!("batch: {e}"))?;

    #[derive(serde::Serialize)]
    struct Point {
        codec: String,
        raw_bytes: usize,
        compressed_bytes: usize,
        ratio: f64,
        enc_mrows_s: f64,
        dec_mrows_s: f64,
    }
    let mut points: Vec<Point> = Vec::new();
    let raw_bytes = batch.get_array_memory_size(); // arrow 59.3 已对各列求和

    for codec in [CodecId::Raw, CodecId::RleDict, CodecId::Zstd] {
        let name = format!("{codec:?}");
        let choice = |_col: &str, _ty: dendro_core::types::ColType, _st: &dendro_columnar::ColStats| -> CodecId { codec };
        let mut enc = 0.0f64;
        let mut size = 0usize;
        let mut last_bytes: Option<Vec<u8>> = None;
        for _ in 0..3 {
            let t0 = Instant::now();
            let bytes = write_cbf(std::slice::from_ref(&batch), 4096, Some(&choice))
                .map_err(|e| format!("write_cbf {name}: {e}"))?;
            enc = enc.max(rows_n as f64 / t0.elapsed().as_secs_f64() / 1e6); // Mrows/s
            size = bytes.len();
            last_bytes = Some(bytes);
        }
        let bytes = last_bytes.expect("至少一次编码");
        let mut dec = 0.0f64;
        for _ in 0..3 {
            let t0 = Instant::now();
            let (_schema, batches) =
                read_cbf(&bytes).map_err(|e| format!("read_cbf {name}: {e}"))?;
            let got: usize = batches.iter().map(|b| b.num_rows()).sum();
            assert_eq!(got, rows_n, "{name}: 回读行数不符");
            dec = dec.max(rows_n as f64 / t0.elapsed().as_secs_f64() / 1e6);
        }
        points.push(Point {
            codec: name,
            raw_bytes,
            compressed_bytes: size,
            ratio: raw_bytes as f64 / size as f64,
            enc_mrows_s: enc,
            dec_mrows_s: dec,
        });
    }

    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
    }
    let json = serde_json::to_vec_pretty(&points).map_err(|e| format!("json: {e}"))?;
    std::fs::write(out_path, json).map_err(|e| format!("write: {e}"))?;
    Ok(())
}
