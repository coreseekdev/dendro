//! RESP（Redis 序列化协议）wire：把 KV 层对外暴露给 Redis 生态客户端。
//!
//! 命令：PING / ECHO / GET / SET / DEL / EXISTS / DBSIZE /
//!       BRANCH <name>（切换键空间）/ BRANCHES（列出分支）/
//!       MULTI → EXEC / DISCARD（事务 = KV 显式事务，OCC 冲突 → ERR）。
//! 键空间 = 当前分支；写经 OCC + WAL，与 SQL/PG/MySQL 完全一致。

use dendro_core::kv::Kv;
use dendro_core::{Database, SqlValue};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct KvRespConfig {
    pub default_branch: String,
}

enum Resp {
    Array(Vec<Resp>),
    Bulk(Vec<u8>),
    Simple(String),
    Err(String),
    Int(i64),
    Nil,
}

fn read_resp(r: &mut impl BufRead) -> std::io::Result<Option<Resp>> {
    let mut line = Vec::new();
    if r.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    let head = String::from_utf8_lossy(&line).trim_end().to_string();
    if head.is_empty() {
        return Ok(None);
    }
    let (ty, rest) = (line[0], head[1..].to_string());
    match ty {
        b'*' => {
            let n: usize = rest.parse().unwrap_or(0);
            let mut items = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                match read_resp(r)? {
                    Some(x) => items.push(x),
                    None => return Ok(None),
                }
            }
            Ok(Some(Resp::Array(items)))
        }
        b'$' => {
            let n: usize = rest.parse().unwrap_or(0);
            if n == 0 {
                let mut crlf = [0u8; 2];
                r.read_exact(&mut crlf)?;
                return Ok(Some(Resp::Bulk(Vec::new())));
            }
            let mut buf = vec![0u8; n];
            r.read_exact(&mut buf)?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf)?;
            Ok(Some(Resp::Bulk(buf)))
        }
        b'+' => Ok(Some(Resp::Simple(rest))),
        b'-' => Ok(Some(Resp::Err(rest))),
        b':' => Ok(Some(Resp::Int(rest.parse().unwrap_or(0)))),
        _ => Ok(Some(Resp::Bulk(line))),
    }
}

fn enc_bulk(b: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n", b.len()).into_bytes();
    out.extend_from_slice(b);
    out.extend_from_slice(b"\r\n");
    out
}

fn enc_simple(s: &str) -> Vec<u8> {
    format!("+{}\r\n", s).into_bytes()
}

fn enc_err(s: &str) -> Vec<u8> {
    format!("-ERR {}\r\n", s.replace(['\r', '\n'], " ")).into_bytes()
}

fn enc_int(i: i64) -> Vec<u8> {
    format!(":{i}\r\n").into_bytes()
}

fn enc_nil() -> Vec<u8> {
    b"$-1\r\n".to_vec()
}

fn enc_array(arr: &[Vec<u8>]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", arr.len()).into_bytes();
    for a in arr {
        out.extend(enc_bulk(a));
    }
    out
}

#[derive(Default)]
struct ConnState {
    in_multi: bool,
    queued: Vec<(Vec<u8>, Vec<u8>)>,   // SET 排队
    queued_dels: Vec<Vec<u8>>,
}

/// 阻塞监听（server 装配用）
pub fn serve(addr: &str, db: Arc<Database>, default_branch: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    tracing::info!(%addr, "dendro kv-resp: listening");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let db = Arc::clone(&db);
                let def = default_branch.to_string();
                std::thread::spawn(move || {
                    let _ = stream.set_nodelay(true);
                    let _ = serve_conn(stream, db, &def);
                });
            }
            Err(e) => tracing::warn!(error = %e, "kv-resp accept failed"),
        }
    }
    Ok(())
}

fn serve_conn(
    mut stream: TcpStream,
    db: Arc<Database>,
    default_branch: &str,
) -> std::io::Result<()> {
    let mut kv = Kv::open(&db, default_branch)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let mut st = ConnState::default();
    let mut r = BufReader::new(stream.try_clone()?);
    loop {
        let Some(msg) = (match read_resp(&mut r) {
            Ok(Some(m)) => Some(Ok(m)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }) else {
            return Ok(());
        };
        let msg = match msg {
            Ok(m) => m,
            Err(e) => return Err(e),
        };
        let args: Vec<Vec<u8>> = match msg {
            Resp::Array(a) => a.into_iter().filter_map(|x| match x {
                Resp::Bulk(b) => Some(b),
                Resp::Simple(s) => Some(s.into_bytes()),
                _ => None,
            }).collect(),
            Resp::Bulk(b) => vec![b],
            _ => continue,
        };
        if args.is_empty() {
            continue;
        }
        let cmd = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
        let reply: Vec<u8> = match (cmd.as_str(), args.len()) {
            ("PING", 1) => enc_simple("PONG"),
            ("PING", 2) => enc_bulk(&args[1]),
            ("ECHO", 2) => enc_bulk(&args[1]),
            ("QUIT", 1) => return Ok(()),
            // BRANCH <name>：存在则切换；不存在则从当前分支创建（git checkout -b 语义）
            ("BRANCH", 2) => {
                let name = String::from_utf8_lossy(&args[1]).to_string();
                let res = if kv.use_branch(&name).is_ok() {
                    Ok(())
                } else {
                    let from = kv.branch().to_string();
                    db.create_branch(&name, &from).and_then(|_| kv.use_branch(&name))
                };
                match res {
                    Ok(()) => enc_simple("OK"),
                    Err(e) => enc_err(&e.to_string()),
                }
            }
            ("BRANCHES", 1) => enc_array(
                &db.manifest().manifest.refs.keys().map(|k| k.clone().into_bytes()).collect::<Vec<_>>(),
            ),
            ("GET", 2) => match kv.get(&args[1]) {
                Ok(Some(v)) => enc_bulk(&v),
                Ok(None) => enc_nil(),
                Err(e) => enc_err(&e.to_string()),
            },
            ("SET", 3) if st.in_multi => {
                st.queued.push((args[1].clone(), args[2].clone()));
                enc_simple("QUEUED")
            }
            ("SET", 3) => match kv.put(&args[1], &args[2]) {
                Ok(()) => enc_simple("OK"),
                Err(e) => enc_err(&e.to_string()),
            },
            ("DEL", n) if n >= 2 && st.in_multi => {
                for k in &args[1..] {
                    st.queued_dels.push(k.clone());
                }
                enc_simple("QUEUED")
            }
            ("DEL", n) if n >= 2 => {
                let mut cnt = 0i64;
                for k in &args[1..] {
                    if kv.get(k).map(|v| v.is_some()).unwrap_or(false) && kv.delete(k).is_ok() {
                        cnt += 1;
                    }
                }
                enc_int(cnt)
            }
            ("EXISTS", 2) => match kv.get(&args[1]) {
                Ok(Some(_)) => enc_int(1),
                Ok(None) => enc_int(0),
                Err(e) => enc_err(&e.to_string()),
            },
            ("DBSIZE", 1) => match kv.scan(None, None) {
                Ok(rows) => enc_int(rows.len() as i64),
                Err(e) => enc_err(&e.to_string()),
            },
            ("MULTI", 1) => {
                st.in_multi = true;
                enc_simple("OK")
            }
            ("EXEC", 1) => {
                st.in_multi = false;
                let out = (|| -> Result<Vec<u8>, String> {
                    kv.begin().map_err(|e| e.to_string())?;
                    for (k, v) in std::mem::take(&mut st.queued) {
                        kv.put(&k, &v).map_err(|e| e.to_string())?;
                    }
                    for k in std::mem::take(&mut st.queued_dels) {
                        kv.delete(&k).map_err(|e| e.to_string())?;
                    }
                    kv.commit().map_err(|e| e.to_string())?;
                    Ok(enc_simple("OK"))
                })();
                match out {
                    Ok(r) => r,
                    Err(e) => {
                        kv.rollback();
                        enc_err(&e)
                    }
                }
            }
            ("DISCARD", 1) => {
                st.in_multi = false;
                st.queued.clear();
                st.queued_dels.clear();
                kv.rollback();
                enc_simple("OK")
            }
            _ => enc_err("unknown command"),
        };
        stream.write_all(&reply)?;
        stream.flush()?;
    }
}

#[allow(dead_code)]
fn _type_check(_: SqlValue) {}
