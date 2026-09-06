//! dendro 服务器入口：装配引擎 + PG/MySQL 双协议监听 + 基准。

mod bench;

use clap::{Parser as ClapParser, Subcommand};
use dendro_core::{Database, DbOptions, Durability, StoreConfig};
use std::path::PathBuf;

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
        /// 数据目录（对象存储根）
        #[arg(long, default_value = "/tmp/dendro-data")]
        data: PathBuf,
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
    /// 引擎自检（内存库跑一组 SQL 并打印）
    Smoke {
        /// SQL（分号分隔）
        #[arg(default_value = "SELECT 1+1 AS two")]
        sql: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            data,
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
            let opts = DbOptions {
                store: StoreConfig::LocalDir(data.clone()),
                wal_flush_interval_ms: wal_interval_ms,
                wal_segment_bytes: 32 << 20,
                durability: dur,
                checkpoint_threshold_bytes: checkpoint_bytes,
                checkpoint_interval_s: 30,
            };
            let db = Database::open(opts).unwrap_or_else(|e| panic!("open {}: {e}", data.display()));
            eprintln!("dendro opened at {}", data.display());
            let my_handle = if mysql_port > 0 {
                let db_my = db.clone();
                let my_addr = format!("{host}:{mysql_port}");
                let cfg = dendro_mywire::MyConfig { password, ..Default::default() };
                Some(
                    std::thread::Builder::new()
                        .name("my-listener".into())
                        .spawn(move || {
                            dendro_mywire::serve(&my_addr, db_my, cfg).expect("mysql listener")
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
                Some(
                    std::thread::Builder::new()
                        .name("pg-listener".into())
                        .spawn(move || dendro_pgwire::serve(pg_sock, db_pg).expect("pg listener"))
                        .unwrap(),
                )
            } else {
                None
            };
            println!(
                "dendro ready: pg={host}:{pg_port} mysql={host}:{mysql_port} data={}",
                data.display()
            );
            if let Some(h) = pg_handle {
                h.join().unwrap();
            }
            if let Some(h) = my_handle {
                h.join().unwrap();
            }
        }
        Cmd::Bench { out } => {
            bench::run_all(&out);
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
