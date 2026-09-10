//! sqllogictest runner：进程内驱动 dendro 引擎（SPEC 07 §5 测试基线）。
//!
//! 用法：`slt run <文件或目录>...`

use dendro_core::{Database, DbOptions, Output};
use sqllogictest::{DBOutput, DefaultColumnType, Runner, DB};
use std::path::PathBuf;
use std::sync::Mutex;

struct SltDb {
    sess: Mutex<dendro_core::Session>,
}

impl DB for SltDb {
    type Error = dendro_core::SqlError;
    type ColumnType = DefaultColumnType;

    fn run(&mut self, sql: &str) -> Result<DBOutput<Self::ColumnType>, Self::Error> {
        let sql = sql.trim();
        if sql.is_empty() {
            return Ok(DBOutput::StatementComplete(0));
        }
        let outs = {
            let mut sess = self.sess.lock().unwrap();
            sess.exec(sql)?
        };
        let mut last_statement = 0usize;
        let mut rows_out: Vec<Vec<String>> = Vec::new();
        for o in &outs {
            match o {
                Output::Command { affected, .. } => last_statement = *affected as usize,
                Output::Rows(rs) => {
                    for r in rs.text_rows() {
                        rows_out.push(
                            r.into_iter()
                                .map(|c| c.unwrap_or_else(|| "NULL".into()))
                                .collect(),
                        );
                    }
                }
            }
        }
        if rows_out.is_empty() {
            Ok(DBOutput::StatementComplete(last_statement as u64))
        } else {
            let types = rows_out
                .first()
                .map(|r| vec![DefaultColumnType::Text; r.len()])
                .unwrap_or_default();
            Ok(DBOutput::Rows {
                types,
                rows: rows_out,
            })
        }
    }

    fn shutdown(&mut self) {}
}

async fn run_files(paths: Vec<PathBuf>) -> (usize, usize, Vec<String>) {
    let mut passed = 0;
    let mut failed = 0;
    let mut failures = Vec::new();
    for p in paths {
        // 每个文件独立内存库（slt 语料自建表）
        let db = Database::open(DbOptions {
            checkpoint_interval_s: 0, // Q-17：禁用自动 checkpoint（避免不确定的截断时序）
            ..DbOptions::memory()
        })
        .unwrap();
        let db2 = db.clone();
        let mut runner = Runner::new(move || {
            let db3 = db2.clone();
            async move {
                Ok(SltDb {
                    sess: Mutex::new(db3.new_session()),
                })
            }
        });
        match runner.run_file_async(&p).await {
            Ok(_) => passed += 1,
            Err(e) => {
                failed += 1;
                failures.push(format!("{}: {}", p.display(), e));
            }
        }
    }
    (passed, failed, failures)
}

fn collect(path: &PathBuf, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.extension().map(|e| e == "slt").unwrap_or(false) {
        out.push(path.clone());
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: slt run <path>...");
        std::process::exit(2);
    }
    if args[1] != "run" {
        eprintln!("unknown command: {}", args[1]);
        std::process::exit(2);
    }
    let mut files = Vec::new();
    for p in &args[2..] {
        collect(&PathBuf::from(p), &mut files);
    }
    if files.is_empty() {
        eprintln!("no .slt files found");
        std::process::exit(2);
    }
    let (ok, bad, failures) = run_files(files).await;
    println!("files passed: {ok}, failed: {bad}");
    for f in &failures {
        println!("FAIL {f}");
    }
    std::process::exit(if bad > 0 { 1 } else { 0 });
}
