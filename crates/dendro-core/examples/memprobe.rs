// 内存剖析探针（每查询独立进程跑，取干净峰值）：argv[1] = 查询编号
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;
fn rss() -> f64 {
    dendro_core::engine::proc_rss_bytes().unwrap_or(0) as f64 / (1u64 << 30) as f64
}
fn hwm() -> f64 {
    let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in st.lines() {
        if let Some(v) = l.strip_prefix("VmHWM:") {
            let kb: f64 = v
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0.0);
            return kb / 1_048_576.0;
        }
    }
    0.0
}
fn main() {
    println!("(start rss {:.2}GB)", rss());
    let args: Vec<String> = std::env::args().collect();
    let which: usize = args.get(1).unwrap_or(&"0".into()).parse().unwrap_or(0);
    let force = args.get(2).map(|s| s == "force").unwrap_or(false);
    let qs: Vec<(&str, &str)> = vec![
        ("q_star",  "SELECT * FROM hits"),
        ("q01_cnt", "SELECT COUNT(*) FROM hits"),
        ("q02_w",   "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0"),
        ("q09_grp", "SELECT SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10"),
        ("q24_like","SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10"),
        ("proj_1col", "SELECT AdvEngineID FROM hits"),
        ("proj_2col", "SELECT WatchID, AdvEngineID FROM hits"),
        ("cnt_where_pkl", "SELECT COUNT(*) FROM hits WHERE WatchID > 0"),
        ("q21_multiagg", "SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits WHERE URL LIKE '%google%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10"),
    ];
    let (name, sql) = qs[which.min(qs.len() - 1)];
    let seq = std::env::var("PROBE_SEQ").is_ok();
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(std::path::PathBuf::from(std::env::var("PROBE_DB").unwrap_or_else(|_| "/home/nzinfo/cb/dbagg".into()))),
        durability: dendro_core::Durability::NoWait,
        checkpoint_interval_s: 0,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 1_048_576,
    }));
    let mut s = db.new_session();
    if force {
        s.exec("SET dendro.force_agg = 'pipeline'").unwrap();
    }
    if seq {
        // 复现基准全序列：ANALYZE → q01..q20（bench 同会话状态交互）
        let _ = std::env::var("PROBE_ANALYZE").map(|_| {
            eprintln!("(analyze...)");
            s.exec("ANALYZE hits").unwrap();
        });
        let prefix = std::fs::read_to_string(std::env::var("PROBE_PREFIX").unwrap_or("/tmp/prefix_q20.sql".into())).unwrap_or_default();
        for (i, q) in prefix.lines().filter(|l| !l.is_empty()).enumerate() {
            match s.exec(q) {
                Ok(_) => {}
                Err(e) => eprintln!("(prefix q{:02} err: {e})", i + 1),
            }
        }
        eprintln!("(q01..q20 prefix done)");
    }
    let r0 = rss();
    let t = std::time::Instant::now();
    let out = match s.exec(sql) {
        Ok(o) => o,
        Err(e) => { println!("{name}: ERROR(run1) {e}"); return; }
    };
    for run in 2..=4u32 {
        if let Err(e) = s.exec(sql) {
            println!("{name}: ERROR(run{run}) {e}");
            return;
        }
    }
    let el = t.elapsed().as_secs_f64();
    let rows = out
        .iter()
        .map(|o| match o {
            dendro_core::types::Output::Rows(r) => r.text_rows().len(),
            _ => 1,
        })
        .sum::<usize>();
    let r1 = rss();
    let hw = hwm();
    println!(
        "{name}: end_rss {r1:.2}GB peak(VmHWM) {hw:.2}GB {el:.2}s rows_out={rows} sql={sql:.60}"
    );
}
