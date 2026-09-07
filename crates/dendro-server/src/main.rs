#![allow(clippy::all)]
//! dendro 服务器入口：装配引擎 + PG/MySQL 双协议监听 + 基准。

mod bench;
pub mod kv_resp;

use clap::{Parser as ClapParser, Subcommand};
use dendro_core::{Database, DbOptions, Durability, StoreConfig};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapParser)]
#[command(name = "dendro", version, about = "只写 · 分支化 · 云原生 SQL 数据库")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动数据库服务器（PG + MySQL 协议）
    Serve {
        /// 数据目录（本地对象存储根；与 --s3-endpoint 二选一）
        #[arg(long, default_value = "/tmp/dendro-data")]
        data: PathBuf,
        /// S3 兼容端点（设置后存储主体落在对象存储，--data 忽略）
        #[arg(long)]
        s3_endpoint: Option<String>,
        /// S3 桶名（需已存在）
        #[arg(long, default_value = "dendro")]
        s3_bucket: String,
        #[arg(long, default_value = "minioadmin")]
        s3_access_key: String,
        #[arg(long, default_value = "minioadmin")]
        s3_secret_key: String,
        #[arg(long, default_value = "us-east-1")]
        s3_region: String,
        /// 读路径缓存目录
        #[arg(long, default_value = "/tmp/dendro-cache")]
        cache_dir: PathBuf,
        /// 读路径缓存字节预算
        #[arg(long, default_value_t = 1 << 30)]
        cache_bytes: u64,
        /// RESP(KV) 监听端口（0 = 关闭）
        #[arg(long, default_value_t = 0)]
        kv_port: u16,
        /// 运维 HTTP 端口（/readyz /metrics；0 = 关闭）
        #[arg(long, default_value_t = 9469)]
        metrics_port: u16,
        /// 只读模式（读副本）：不领写者租约，拒绝一切写
        #[arg(long, default_value_t = false)]
        read_only: bool,
        /// GC 保留窗口毫秒（墓碑对象登记后至少保留时长；<0 禁用）
        #[arg(long, default_value_t = 24 * 3600 * 1000)]
        gc_retention_ms: i64,
        /// PG 监听端口（0 = 关闭）
        #[arg(long, default_value_t = 5432)]
        pg_port: u16,
        /// MySQL 监听端口（0 = 关闭）
        #[arg(long, default_value_t = 13306)]
        mysql_port: u16,
        /// 监听地址
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// MySQL 口令（native password）；不设 = 任意口令放行
        #[arg(long)]
        password: Option<String>,
        /// WAL 组提交间隔 ms
        #[arg(long, default_value_t = 50)]
        wal_interval_ms: u64,
        /// durability: no_wait | group | always
        #[arg(long, default_value = "group")]
        durability: String,
        /// checkpoint 触发阈值（字节）
        #[arg(long, default_value_t = 16777216)]
        checkpoint_bytes: u64,
    },
    /// 运行基准套件，结果写 JSON
    Bench {
        /// 结果输出目录
        #[arg(long, default_value = "benches/results")]
        out: PathBuf,
    },
    /// 一致性点物理备份（append-only：数据先拷、manifest 最后拷）
    Backup {
        /// 源数据目录（本地对象存储根）
        #[arg(long)]
        data: PathBuf,
        /// 备份输出目录
        #[arg(long)]
        out: PathBuf,
    },
    /// 引擎自检（内存库跑一组 SQL 并打印）
    Smoke {
        /// SQL（分号分隔）
        #[arg(default_value = "SELECT 1+1 AS two")]
        sql: String,
    },
}

fn main() {
    // 日志初始化（第八轮 R8-5：此前 17 处 tracing 全部无 subscriber——毒化/
    // WAL 失败/checkpoint 失败等关键事件生产环境全部静默）
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            data,
            kv_port,
            metrics_port,
            read_only,
            gc_retention_ms,
            s3_endpoint,
            s3_bucket,
            s3_access_key,
            s3_secret_key,
            s3_region,
            cache_dir: _,
            cache_bytes,
            pg_port,
            mysql_port,
            host,
            password,
            wal_interval_ms,
            durability,
            checkpoint_bytes,
        } => {
            let dur = match durability.as_str() {
                "no_wait" => Durability::NoWait,
                "always" => Durability::Always,
                _ => Durability::Group,
            };
            let store = if let Some(ep) = &s3_endpoint {
                StoreConfig::S3(dendro_core::objstore::s3::S3Config {
                    endpoint: ep.clone(),
                    bucket: s3_bucket.clone(),
                    access_key: s3_access_key.clone(),
                    secret_key: s3_secret_key.clone(),
                    region: s3_region.clone(),
                    ..Default::default()
                })
            } else {
                StoreConfig::LocalDir(data.clone())
            };
            let opts = DbOptions {
                store,
                cache_budget_bytes: cache_bytes,
                wal_flush_interval_ms: wal_interval_ms,
                wal_segment_bytes: 32 << 20,
                durability: dur,
                checkpoint_threshold_bytes: checkpoint_bytes,
                checkpoint_interval_s: 30,
                lease_ttl_ms: 30_000,
                read_only,
            gc_retention_ms,
            };
            let db = Database::open(opts).unwrap_or_else(|e| panic!("open {}: {e}", data.display()));
            db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
                row_group_rows: 1_048_576,
            }));
            
            eprintln!("dendro opened at {}", data.display());
            // **前置 bind**（第九轮 R9-4）：全部监听器先绑定成功才打印 ready
            // 并 spawn——此前 bind 在各自线程内发生，pg 端口冲突时照常打印
            // ready 且进程永不退出
            let mut listeners: Vec<(&str, std::net::TcpListener)> = Vec::new();
            for (name, port) in [("metrics", metrics_port), ("pg", pg_port), ("mysql", mysql_port), ("kv", kv_port)] {
                if port == 0 {
                    continue;
                }
                match std::net::TcpListener::bind(format!("{host}:{port}")) {
                    Ok(l) => listeners.push((name, l)),
                    Err(e) => {
                        eprintln!("dendro: {name} listen {host}:{port} failed: {e}");
                        std::process::exit(1);
                    }
                }
            }

            let metrics_handle = if metrics_port > 0 {
                let db_m = db.clone();
                let (_, l) = listeners.remove(0);
                Some(
                    std::thread::Builder::new()
                        .name("metrics-listener".into())
                        .spawn(move || dendro_server::metrics::serve_listener(l, db_m).expect("metrics listener"))
                        .unwrap(),
                )
            } else {
                None
            };
            let kv_handle = if kv_port > 0 {
                let db_kv = db.clone();
                let kv_addr = format!("{host}:{kv_port}");
                let (_, l) = listeners.remove(0);
                Some(
                    std::thread::Builder::new()
                        .name("kv-resp-listener".into())
                        .spawn(move || dendro_server::kv_resp::serve_listener(l, db_kv, "main").expect("kv listener"))
                        .unwrap(),
                )
            } else {
                None
            };
            let my_handle = if mysql_port > 0 {
                let db_my = db.clone();
                let cfg = dendro_mywire::MyConfig { password: password.clone(), ..Default::default() };
                let (_, l) = listeners.remove(0);
                Some(
                    std::thread::Builder::new()
                        .name("my-listener".into())
                        .spawn(move || {
                            let factory: dendro_mywire::SessionFactory = {
                                let db = db_my.clone();
                                std::sync::Arc::new(move || -> Box<dyn dendro_core::WireSession> {
                                    Box::new(db.new_session())
                                })
                            };
                            dendro_mywire::serve_listener(l, cfg, factory).expect("mysql listener")
                        })
                        .unwrap(),
                )
            } else {
                None
            };
            let pg_handle = if pg_port > 0 {
                let db_pg = db.clone();
                let pg_sock: std::net::SocketAddr =
                    format!("{host}:{pg_port}").parse().expect("pg addr");
                let (_, l) = listeners.remove(0);
                Some(
                    std::thread::Builder::new()
                        .name("pg-listener".into())
                        .spawn(move || {
                            let cfg = dendro_pgwire::PgConfig { password: password.clone() };
                            dendro_pgwire::serve_listener(l, db_pg, cfg).expect("pg listener")
                        })
                        .unwrap(),
                )
            } else {
                None
            };
            println!(
                "dendro ready: pg={host}:{pg_port} mysql={host}:{mysql_port} metrics={host}:{metrics_port} backend={} data={}",
                if s3_endpoint.is_some() { "s3" } else { "local" },
                data.display()
            );
            // 任一监听器失败（bind 冲突等）都会使 join 返回 Err → 进程非零
            // 退出（第八轮 R8-6：此前 pg bind 失败时照常打印 ready 且永不退出）
            let mut handles: Vec<(&str, std::thread::JoinHandle<()>)> = Vec::new();
            if let Some(h) = metrics_handle { handles.push(("metrics", h)); }
            if let Some(h) = pg_handle { handles.push(("pg", h)); }
            if let Some(h) = my_handle { handles.push(("mysql", h)); }
            if let Some(h) = kv_handle { handles.push(("kv", h)); }
            for (name, h) in handles {
                if h.join().is_err() {
                    eprintln!("dendro: listener '{name}' failed — exiting");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Backup { data, out } => {
            match dendro_server::backup::backup_dir(&data, &out) {
                Ok((objects, bytes)) => {
                    println!("backup complete: {objects} objects, {bytes} bytes -> {}", out.display());
                }
                Err(e) => {
                    eprintln!("backup failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Bench { out } => {
            dendro_server::bench::run_all(&out);
        }
        Cmd::Smoke { sql } => {
            let db = Database::open(DbOptions::memory()).unwrap();
            let mut s = db.new_session();
            for stmt in sql.split(';').filter(|p| !p.trim().is_empty()) {
                let outs = s.exec(stmt).expect("exec");
                for o in outs {
                    match o {
                        dendro_core::types::Output::Command { tag, affected } => {
                            println!("{tag} ({affected})");
                        }
                        dendro_core::types::Output::Rows(rs) => {
                            let names: Vec<&str> =
                                rs.columns.iter().map(|c| c.name.as_str()).collect();
                            println!("| {}", names.join(" | "));
                            for r in rs.text_rows() {
                                let cells: Vec<String> = r
                                    .into_iter()
                                    .map(|c| c.unwrap_or_else(|| "NULL".into()))
                                    .collect();
                                println!("| {}", cells.join(" | "));
                            }
                        }
                    }
                }
            }
        }
    }
}
