//! ClickBench 基准 harness（性能基线计划 §3——A 类任务的前置门）。
//!
//! 数据：官方 hits.csv.gz（datasets.clickhouse.com，1 亿行 / 105 列）。
//!
//! 装载：COPY FROM（绕行值 parse 的官方路径；样本截取/全量编号在
//! harness 侧前置合成主键 rid）。schema = PG 口径 105 列的类型适配版：
//! 数值 → BIGINT；TEXT/DATE/TIMESTAMP → TEXT（ISO 字典序比较与时间
//! 序一致，DATE_TRUNC/extract 查询本就跳过）。
//!
//! 查询：官方 43 条 PG 口径的适配子集（跳过 extract/REGEXP_REPLACE/
//! DATE_TRUNC 共 4 条；GROUP BY 1 序号改显式列）。计时：warmup 1 +
//! 中位数 of 3。愚弄率留给 bench_pair（本工具是绝对基线不是 A/B）。

use super::{BenchResult, BenchRow};
use dendro_core::{Database, DbOptions, Durability, StoreConfig};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// (列名, 是否数值)。首列 = 合成行号主键（dendro 表必须有 PK 才能
/// 检查点/列存物化；hits 官方 schema 无 PK 且 WatchID 不唯一——
/// ClickBench 各引擎移植的常见适配位）。其余 105 列序 = 官方口径
const COLS: &[(&str, bool)] = &[
    ("rid", true),
    ("WatchID", true),
    ("JavaEnable", true),
    ("Title", false),
    ("GoodEvent", true),
    ("EventTime", false),
    ("EventDate", false),
    ("CounterID", true),
    ("ClientIP", true),
    ("RegionID", true),
    ("UserID", true),
    ("CounterClass", true),
    ("OS", true),
    ("UserAgent", true),
    ("URL", false),
    ("Referer", false),
    ("IsRefresh", true),
    ("RefererCategoryID", true),
    ("RefererRegionID", true),
    ("URLCategoryID", true),
    ("URLRegionID", true),
    ("ResolutionWidth", true),
    ("ResolutionHeight", true),
    ("ResolutionDepth", true),
    ("FlashMajor", true),
    ("FlashMinor", true),
    ("FlashMinor2", false),
    ("NetMajor", true),
    ("NetMinor", true),
    ("UserAgentMajor", true),
    ("UserAgentMinor", false),
    ("CookieEnable", true),
    ("JavascriptEnable", true),
    ("IsMobile", true),
    ("MobilePhone", true),
    ("MobilePhoneModel", false),
    ("Params", false),
    ("IPNetworkID", true),
    ("TraficSourceID", true),
    ("SearchEngineID", true),
    ("SearchPhrase", false),
    ("AdvEngineID", true),
    ("IsArtifical", true),
    ("WindowClientWidth", true),
    ("WindowClientHeight", true),
    ("ClientTimeZone", true),
    ("ClientEventTime", false),
    ("SilverlightVersion1", true),
    ("SilverlightVersion2", true),
    ("SilverlightVersion3", true),
    ("SilverlightVersion4", true),
    ("PageCharset", false),
    ("CodeVersion", true),
    ("IsLink", true),
    ("IsDownload", true),
    ("IsNotBounce", true),
    ("FUniqID", true),
    ("OriginalURL", false),
    ("HID", true),
    ("IsOldCounter", true),
    ("IsEvent", true),
    ("IsParameter", true),
    ("DontCountHits", true),
    ("WithHash", true),
    ("HitColor", false),
    ("LocalEventTime", false),
    ("Age", true),
    ("Sex", true),
    ("Income", true),
    ("Interests", true),
    ("Robotness", true),
    ("RemoteIP", true),
    ("WindowName", true),
    ("OpenerName", true),
    ("HistoryLength", true),
    ("BrowserLanguage", false),
    ("BrowserCountry", false),
    ("SocialNetwork", false),
    ("SocialAction", false),
    ("HTTPError", true),
    ("SendTiming", true),
    ("DNSTiming", true),
    ("ConnectTiming", true),
    ("ResponseStartTiming", true),
    ("ResponseEndTiming", true),
    ("FetchTiming", true),
    ("SocialSourceNetworkID", true),
    ("SocialSourcePage", false),
    ("ParamPrice", true),
    ("ParamOrderID", false),
    ("ParamCurrency", false),
    ("ParamCurrencyID", true),
    ("OpenstatServiceName", false),
    ("OpenstatCampaignID", false),
    ("OpenstatAdID", false),
    ("OpenstatSourceID", false),
    ("UTMSource", false),
    ("UTMMedium", false),
    ("UTMCampaign", false),
    ("UTMContent", false),
    ("UTMTerm", false),
    ("FromTag", false),
    ("HasGCLID", true),
    ("RefererHash", true),
    ("URLHash", true),
    ("CLID", true),
];

/// 官方 43 条（PG 口径）的适配子集。(idx, sql, skip_reason)
fn queries() -> Vec<(usize, String, Option<&'static str>)> {
    let q: Vec<&str> = vec![
        "SELECT COUNT(*) FROM hits",
        "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0",
        "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits",
        "SELECT AVG(UserID) FROM hits",
        "SELECT COUNT(DISTINCT UserID) FROM hits",
        "SELECT COUNT(DISTINCT SearchPhrase) FROM hits",
        "SELECT MIN(EventDate), MAX(EventDate) FROM hits",
        "SELECT AdvEngineID, COUNT(*) FROM hits WHERE AdvEngineID <> 0 GROUP BY AdvEngineID ORDER BY COUNT(*) DESC",
        "SELECT RegionID, COUNT(DISTINCT UserID) AS u FROM hits GROUP BY RegionID ORDER BY u DESC LIMIT 10",
        "SELECT RegionID, SUM(AdvEngineID), COUNT(*) AS c, AVG(ResolutionWidth), COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10",
        "SELECT MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhoneModel ORDER BY u DESC LIMIT 10",
        "SELECT MobilePhone, MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhone, MobilePhoneModel ORDER BY u DESC LIMIT 10",
        "SELECT SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
        "SELECT SearchPhrase, COUNT(DISTINCT UserID) AS u FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY u DESC LIMIT 10",
        "SELECT SearchEngineID, SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, SearchPhrase ORDER BY c DESC LIMIT 10",
        "SELECT UserID, COUNT(*) FROM hits GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10",
        "SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10",
        "SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase LIMIT 10",
        "SELECT UserID, extract(minute FROM EventTime) AS m, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, m, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10",
        "SELECT UserID FROM hits WHERE UserID = 435090932899640449",
        "SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'",
        "SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits WHERE URL LIKE '%google%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
        "SELECT SearchPhrase, MIN(URL), MIN(Title), COUNT(*) AS c, COUNT(DISTINCT UserID) FROM hits WHERE Title LIKE '%Google%' AND URL NOT LIKE '%.google.%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
        "SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10",
        "SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10",
        "SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY SearchPhrase LIMIT 10",
        "SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime, SearchPhrase LIMIT 10",
        "SELECT CounterID, AVG(length(URL)) AS l, COUNT(*) AS c FROM hits WHERE URL <> '' GROUP BY CounterID HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25",
        "SELECT REGEXP_REPLACE(Referer, '^https?://(?:www\\.)?([^/]+)/.*$', '\\1') AS k, AVG(length(Referer)) AS l, COUNT(*) AS c, MIN(Referer) FROM hits WHERE Referer <> '' GROUP BY k HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25",
        "SELECT SUM(ResolutionWidth), SUM(ResolutionWidth + 1), SUM(ResolutionWidth + 2), SUM(ResolutionWidth + 3), SUM(ResolutionWidth + 4), SUM(ResolutionWidth + 5), SUM(ResolutionWidth + 6), SUM(ResolutionWidth + 7), SUM(ResolutionWidth + 8), SUM(ResolutionWidth + 9), SUM(ResolutionWidth + 10), SUM(ResolutionWidth + 11), SUM(ResolutionWidth + 12), SUM(ResolutionWidth + 13), SUM(ResolutionWidth + 14), SUM(ResolutionWidth + 15), SUM(ResolutionWidth + 16), SUM(ResolutionWidth + 17), SUM(ResolutionWidth + 18), SUM(ResolutionWidth + 19), SUM(ResolutionWidth + 20), SUM(ResolutionWidth + 21), SUM(ResolutionWidth + 22), SUM(ResolutionWidth + 23), SUM(ResolutionWidth + 24), SUM(ResolutionWidth + 25), SUM(ResolutionWidth + 26), SUM(ResolutionWidth + 27), SUM(ResolutionWidth + 28), SUM(ResolutionWidth + 29), SUM(ResolutionWidth + 30), SUM(ResolutionWidth + 31), SUM(ResolutionWidth + 32), SUM(ResolutionWidth + 33), SUM(ResolutionWidth + 34), SUM(ResolutionWidth + 35), SUM(ResolutionWidth + 36), SUM(ResolutionWidth + 37), SUM(ResolutionWidth + 38), SUM(ResolutionWidth + 39), SUM(ResolutionWidth + 40), SUM(ResolutionWidth + 41), SUM(ResolutionWidth + 42), SUM(ResolutionWidth + 43), SUM(ResolutionWidth + 44), SUM(ResolutionWidth + 45), SUM(ResolutionWidth + 46), SUM(ResolutionWidth + 47), SUM(ResolutionWidth + 48), SUM(ResolutionWidth + 49), SUM(ResolutionWidth + 50), SUM(ResolutionWidth + 51), SUM(ResolutionWidth + 52), SUM(ResolutionWidth + 53), SUM(ResolutionWidth + 54), SUM(ResolutionWidth + 55), SUM(ResolutionWidth + 56), SUM(ResolutionWidth + 57), SUM(ResolutionWidth + 58), SUM(ResolutionWidth + 59), SUM(ResolutionWidth + 60), SUM(ResolutionWidth + 61), SUM(ResolutionWidth + 62), SUM(ResolutionWidth + 63), SUM(ResolutionWidth + 64), SUM(ResolutionWidth + 65), SUM(ResolutionWidth + 66), SUM(ResolutionWidth + 67), SUM(ResolutionWidth + 68), SUM(ResolutionWidth + 69), SUM(ResolutionWidth + 70), SUM(ResolutionWidth + 71), SUM(ResolutionWidth + 72), SUM(ResolutionWidth + 73), SUM(ResolutionWidth + 74), SUM(ResolutionWidth + 75), SUM(ResolutionWidth + 76), SUM(ResolutionWidth + 77), SUM(ResolutionWidth + 78), SUM(ResolutionWidth + 79), SUM(ResolutionWidth + 80), SUM(ResolutionWidth + 81), SUM(ResolutionWidth + 82), SUM(ResolutionWidth + 83), SUM(ResolutionWidth + 84), SUM(ResolutionWidth + 85), SUM(ResolutionWidth + 86), SUM(ResolutionWidth + 87), SUM(ResolutionWidth + 88), SUM(ResolutionWidth + 89) FROM hits",
        "SELECT SearchEngineID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, ClientIP ORDER BY c DESC LIMIT 10",
        "SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10",
        "SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10",
        "SELECT URL, COUNT(*) AS c FROM hits GROUP BY URL ORDER BY c DESC LIMIT 10",
        "SELECT 1, URL, COUNT(*) AS c FROM hits GROUP BY 1, URL ORDER BY c DESC LIMIT 10",
        "SELECT ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3, COUNT(*) AS c FROM hits GROUP BY ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3 ORDER BY c DESC LIMIT 10",
        "SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 AND IsRefresh = 0 AND URL <> '' GROUP BY URL ORDER BY PageViews DESC LIMIT 10",
        "SELECT Title, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 AND IsRefresh = 0 AND Title <> '' GROUP BY Title ORDER BY PageViews DESC LIMIT 10",
        "SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 AND IsLink <> 0 AND IsDownload = 0 GROUP BY URL ORDER BY PageViews DESC LIMIT 10 OFFSET 1000",
        "SELECT TraficSourceID, SearchEngineID, AdvEngineID, CASE WHEN (SearchEngineID = 0 AND AdvEngineID = 0) THEN Referer ELSE '' END AS Src, URL AS Dst, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 GROUP BY TraficSourceID, SearchEngineID, AdvEngineID, Src, Dst ORDER BY PageViews DESC LIMIT 10 OFFSET 1000",
        "SELECT URLHash, EventDate, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 AND TraficSourceID IN (-1, 6) AND RefererHash = 3594120000172545465 GROUP BY URLHash, EventDate ORDER BY PageViews DESC LIMIT 10 OFFSET 100",
        "SELECT WindowClientWidth, WindowClientHeight, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 AND DontCountHits = 0 AND URLHash = 2868770270353813622 GROUP BY WindowClientWidth, WindowClientHeight ORDER BY PageViews DESC LIMIT 10 OFFSET 10000",
        "SELECT DATE_TRUNC('minute', EventTime) AS M, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-14' AND EventDate <= '2013-07-15' AND IsRefresh = 0 AND DontCountHits = 0 GROUP BY DATE_TRUNC('minute', EventTime) ORDER BY DATE_TRUNC('minute', EventTime) LIMIT 10 OFFSET 1000",
    ];
    q.into_iter()
        .enumerate()
        .map(|(i, sql)| {
            let n = i + 1;
            // 跳过清单（4 条）：extract / REGEXP_REPLACE×2 / DATE_TRUNC
            let skip = match n {
                19 => Some("extract(minute FROM ...) 未实现"),
                28 | 29 => Some("REGEXP_REPLACE 未实现"),
                43 => Some("DATE_TRUNC 未实现"),
                _ => None,
            };
            // 适配（执行而非跳过）：GROUP BY 序号 → 显式列
            let sql = if n == 33 {
                sql.replace("GROUP BY 1, URL", "GROUP BY URL")
            } else {
                sql.to_string()
            };
            (n, sql, skip)
        })
        .collect()
}

/// 引号感知 CSV 行解析（", "" 转义；无跨行字段——hits 数据保证）
fn parse_csv_line(line: &str, out: &mut Vec<String>) {
    out.clear();
    let b: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    let mut field = String::new();
    while i < b.len() {
        if b[i] == '"' {
            i += 1;
            while i < b.len() {
                if b[i] == '"' {
                    if i + 1 < b.len() && b[i + 1] == '"' {
                        field.push('"');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    field.push(b[i]);
                    i += 1;
                }
            }
        } else if b[i] == ',' {
            out.push(std::mem::take(&mut field));
            i += 1;
        } else {
            field.push(b[i]);
            i += 1;
        }
    }
    out.push(field);
}

/// SQL 字面量转义（' 加倍）
fn sql_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 跑 ClickBench：装载（COPY FROM，.gz 自动解压；rows_limit>0 先截样）
/// → 检查点/ANALYZE → 查询计时。ckpt_every 在 COPY 路径下不生效
///（单语句装载；保留参数兼容旧签名）
///
/// max_rss_mb > 0 时启用内存上限（装载逐段/查询逐条间检查）：超限把
/// 剩余步骤标记 skipped 写出**已完成部分**的 JSON 后优雅退出——宁可
/// 部分结果也不让 TableView 物化把整机压垮（swap 打满殃及他进程）。
/// 注意它是"步间"防线：单条查询内部的失控仍需外层 memguard 硬杀。
pub fn bench_clickbench(
    csv: &Path,
    data_dir: &Path,
    rows_limit: usize, // 0 = 全量
    chunk_rows: usize,
    ckpt_every: usize, // 每 N 行 CHECKPOINT（界内存tx/WAL）；0 = 不做
    max_rss_mb: u64,   // 0 = 不设限；>0 = 超限优雅截断（mb）
    out: &PathBuf,
) -> BenchResult {
    let rss_over_cap = |gb_cap: f64| -> Option<f64> {
        if gb_cap <= 0.0 {
            return None;
        }
        dendro_core::engine::proc_rss_bytes()
            .map(|b| b as f64 / (1 << 30) as f64) // GiB
            .filter(|gb| *gb > gb_cap)
    };
    let write_out = |rows: Vec<BenchRow>| -> BenchResult {
        let r = BenchResult {
            suite: "clickbench".into(),
            rows,
        };
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(out, r.to_json()).unwrap();
        r
    };
    let gb_cap = max_rss_mb as f64 / 1024.0;
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(data_dir.to_path_buf()),
        durability: Durability::Group,
        // 装载基准：关后台自动 checkpoint（30s/16MB 轮询与 bulk COPY
        // 并发曾引发 CAS chunk 丢失 58030——并发缺陷另行立项；此处
        // 物化点由段间 CHECKPOINT 显式控制，单线程无竞态）
        checkpoint_interval_s: 0,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 1_048_576,
    }));
    let mut s = db.new_session();
    let mut rows_out: Vec<BenchRow> = Vec::new();
    let total_rows: u64;

    // 建表（幂等：存在则 DROP 重建——基线可重跑）
    let ddl = COLS
        .iter()
        .enumerate()
        .map(|(i, (n, num))| {
            let ty = if *num { "BIGINT" } else { "TEXT" };
            if i == 0 {
                format!("{n} {ty} PRIMARY KEY")
            } else {
                format!("{n} {ty}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let _ = s.exec("DROP TABLE hits");
    s.exec(&format!("CREATE TABLE hits ({ddl})")).unwrap();

    // ---- 装载（分段 COPY + 中间 CHECKPOINT——内存治理）：
    // 单条 COPY 的 pending 全量积压到装载末 checkpoint，
    // materialize_delta 全量 decode_row = O(全表×行宽) 峰值
    //（1M 行实测 RSS 12G）。分段后每次 delta=段，memtx 截断回
    // O(段)，峰值 ≈ 段 + 编号缓冲。样本与全量统一路径 ----
    let t0 = Instant::now();
    const SEG_ROWS: u64 = 200_000; // ≈240MB/段
    let mut seg_paths: Vec<PathBuf> = Vec::new();
    {
        let f = std::fs::File::open(csv).unwrap();
        let mut raw: Box<dyn std::io::BufRead> =
            if csv.extension().and_then(|e| e.to_str()) == Some("gz") {
                Box::new(std::io::BufReader::new(flate2::read::GzDecoder::new(f)))
            } else {
                Box::new(std::io::BufReader::new(f))
            };
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut n: u64 = 0;
        let limit = if rows_limit > 0 {
            rows_limit as u64
        } else {
            u64::MAX
        };
        let seg_path =
            |idx: u64| -> PathBuf { data_dir.with_extension(format!("cbseg{idx:05}.csv")) };
        let mut w =
            std::io::BufWriter::with_capacity(4 << 20, std::fs::File::create(seg_path(0)).unwrap());
        seg_paths.push(seg_path(0));
        while n < limit {
            buf.clear();
            if raw.read_until(b'\n', &mut buf).unwrap() == 0 {
                break;
            }
            while dendro_core::sql::in_open_quote(&buf) {
                let mut cont = Vec::new();
                if raw.read_until(b'\n', &mut cont).unwrap() == 0 {
                    break;
                }
                buf.extend_from_slice(&cont);
            }
            write!(w, "{n},").unwrap();
            w.write_all(&buf).unwrap();
            n += 1;
            if n.is_multiple_of(5_000_000) {
                eprintln!("[clickbench] numbered {n} rows");
            }
            if n.is_multiple_of(SEG_ROWS) {
                w.flush().unwrap();
                let next = seg_path(n / SEG_ROWS);
                w = std::io::BufWriter::with_capacity(
                    4 << 20,
                    std::fs::File::create(&next).unwrap(),
                );
                seg_paths.push(next);
            }
        }
        w.flush().unwrap();
    }
    // 逐段 COPY + 中间 CHECKPOINT（末段后的 checkpoint 由下方
    // 物化段统一执行）；段文件即用即删
    let mut loaded = 0u64;
    for (i, seg) in seg_paths.iter().enumerate() {
        let out_copy = s
            .exec(&format!("COPY hits FROM '{}'", seg.display()))
            .unwrap();
        if let dendro_core::types::Output::Command { affected, .. } = &out_copy[0] {
            loaded += *affected;
        }
        if i + 1 < seg_paths.len() {
            s.exec("CHECKPOINT").unwrap();
        }
        let _ = std::fs::remove_file(seg);
        if let Some(gb) = rss_over_cap(gb_cap) {
            rows_out.push(BenchRow {
                name: format!("memcap[load rss {gb:.1}gb > {gb_cap:.0}gb]"),
                value: 0.0,
                unit: "memcap",
            });
            eprintln!("[clickbench] MEMCAP load: {gb:.1}gb > {gb_cap:.0}gb @ seg {i}");
            return write_out(std::mem::take(&mut rows_out));
        }
    }
    let load_s = t0.elapsed().as_secs_f64();
    let total_rows = loaded;
    rows_out.push(BenchRow {
        name: "load_seconds".into(),
        value: load_s,
        unit: "s",
    });
    rows_out.push(BenchRow {
        name: "load_rows_per_s".into(),
        value: loaded as f64 / load_s,
        unit: "rows/s",
    });
    rows_out.push(BenchRow {
        name: "total_rows".into(),
        value: total_rows as f64,
        unit: "rows",
    });

    // ---- 物化 + 统计 ----
    let t1 = Instant::now();
    s.exec("CHECKPOINT").unwrap();
    rows_out.push(BenchRow {
        name: "checkpoint_seconds".into(),
        value: t1.elapsed().as_secs_f64(),
        unit: "s",
    });
    let t2 = Instant::now();
    let _ = s.exec("ANALYZE hits");
    rows_out.push(BenchRow {
        name: "analyze_seconds".into(),
        value: t2.elapsed().as_secs_f64(),
        unit: "s",
    });

    // ---- 查询：warmup 1 + 中位数 of 3 ----
    for (qi, sql, skip) in queries() {
        // 内存上限（步间防线）：上一条查询（或 warmup 后）RSS 仍超
        // → 本条及剩余全部标记 memcap skip，写出部分结果优雅退出
        if let Some(gb) = rss_over_cap(gb_cap) {
            rows_out.push(BenchRow {
                name: format!("q{qi:02}.skipped[memcap rss {gb:.1}gb > {gb_cap:.0}gb]"),
                value: 0.0,
                unit: "skip",
            });
            eprintln!("[clickbench] MEMCAP queries: {gb:.1}gb > {gb_cap:.0}gb @ q{qi:02}");
            // 剩余查询统一标注（含天然 skip 的原样保留语义）
            let all = queries();
            for (rqi, _, rskip) in all.iter().skip_while(|(x, _, _)| *x != qi).skip(1) {
                let reason = rskip
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "memcap".into());
                rows_out.push(BenchRow {
                    name: format!("q{rqi:02}.skipped[{reason}]"),
                    value: 0.0,
                    unit: "skip",
                });
            }
            return write_out(std::mem::take(&mut rows_out));
        }
        if let Some(reason) = skip {
            rows_out.push(BenchRow {
                name: format!("q{qi:02}.skipped[{reason}]"),
                value: 0.0,
                unit: "skip",
            });
            continue;
        }
        // warmup（不计时）+ 结果行数校验留痕
        let w = s.exec(&sql);
        let rowcount = match &w {
            Ok(outs) => outs
                .iter()
                .map(|o| match o {
                    dendro_core::types::Output::Rows(rs) => rs.text_rows().len(),
                    _ => 1,
                })
                .sum::<usize>(),
            Err(e) => {
                rows_out.push(BenchRow {
                    name: format!("q{qi:02}.error[{}]", e.state),
                    value: 0.0,
                    unit: "err",
                });
                eprintln!("[clickbench] q{qi:02} ERROR: {e}");
                let _ = write_out(rows_out.clone());
                continue;
            }
        };
        let mut times = Vec::with_capacity(3);
        for _ in 0..3 {
            let t = Instant::now();
            let _ = std::hint::black_box(s.exec(&sql));
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        rows_out.push(BenchRow {
            name: format!("q{qi:02}.median_ms[n={rowcount}]"),
            value: times[1],
            unit: "ms",
        });
        // 逐查询打点：即使外层 memguard 硬杀，tee 日志也留有已完成
        // 查询的耗时痕迹（部分基线可从日志恢复）
        let rss_now = dendro_core::engine::proc_rss_bytes().unwrap_or(0) as f64 / (1 << 30) as f64;
        eprintln!(
            "[clickbench] q{qi:02} done {} ms (rss {rss_now:.1}gb)",
            times[1]
        );
    }
    write_out(rows_out)
}

fn panic_harness(msg: &str) -> BenchResult {
    panic!("clickbench harness: {msg}");
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quoted_parse() {
        let mut out = Vec::new();
        parse_csv_line(r#"a,"b,c","say ""hi""",,123"#, &mut out);
        assert_eq!(out, vec!["a", "b,c", "say \"hi\"", "", "123"]);
    }

    #[test]
    fn query_skip_accounting() {
        let qs = queries();
        assert_eq!(qs.len(), 43);
        let skipped = qs.iter().filter(|(_, _, s)| s.is_some()).count();
        assert_eq!(skipped, 4, "extract/regexp×2/date_trunc");
    }
}
