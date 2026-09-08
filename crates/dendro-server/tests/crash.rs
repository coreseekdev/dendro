//! P1-8：真 SIGKILL 崩溃恢复（进程级）——替代 drop(db) 模拟的最后一块。
//! 流程：spawn 真实 dendro serve → 经 PG 协议写入（一部分 checkpoint、
//! 一部分仅 WAL）→ SIGKILL 子进程 → 重启 → 全部数据可见。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 持有子进程；Drop 时 kill+wait（失败路径不泄漏进程）
struct CrashServer {
    child: Child,
}

impl Drop for CrashServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(data: &std::path::Path, pg: u16) -> CrashServer {
    let child = Command::new(env!("CARGO_BIN_EXE_dendro"))
        .args([
            "serve",
            "--data",
            data.to_str().unwrap(),
            "--host",
            "127.0.0.1",
            "--pg-port",
            &pg.to_string(),
            "--mysql-port",
            "0",
            "--kv-port",
            "0",
            "--metrics-port",
            "0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dendro serve");
    CrashServer { child }
}

fn wait_ready(pg: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", pg)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 简易 PG 线协议客户端（仅本测试所需的 QUERY/密码免认证路径）
struct PgSync {
    stream: TcpStream,
}

impl PgSync {
    fn connect(port: u16) -> Self {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // StartupMessage: len + 196608 + "user\0postgres\0\0"
        let params = b"user\0postgres\0\0";
        let len = 4 + 4 + params.len();
        let mut msg = Vec::new();
        msg.extend_from_slice(&(len as u32).to_be_bytes());
        msg.extend_from_slice(&196608u32.to_be_bytes());
        msg.extend_from_slice(params);
        s.write_all(&msg).unwrap();
        Self::until_ready_for_query(&mut s);
        Self { stream: s }
    }

    /// 读包直到 ReadyForQuery('Z')
    fn until_ready_for_query(s: &mut TcpStream) {
        loop {
            let mut tag = [0u8; 1];
            if s.read_exact(&mut tag).is_err() {
                return;
            }
            let mut len = [0u8; 4];
            s.read_exact(&mut len).unwrap();
            let n = u32::from_be_bytes(len) as usize - 4;
            let mut body = vec![0u8; n];
            s.read_exact(&mut body).unwrap();
            if tag[0] == b'Z' {
                return;
            }
        }
    }

    fn exec(&mut self, sql: &str) {
        let mut msg = Vec::new();
        msg.push(b'Q');
        let body = sql.as_bytes();
        msg.extend_from_slice(&((body.len() + 4 + 1) as u32).to_be_bytes());
        msg.extend_from_slice(body);
        msg.push(0);
        self.stream.write_all(&msg).unwrap();
        Self::until_ready_for_query(&mut self.stream);
    }

    /// 执行查询并返回最后一个 DataRow 的首个整型列值（无行则 None）
    fn query_count(&mut self, sql: &str) -> Option<i64> {
        let mut msg = Vec::new();
        msg.push(b'Q');
        let body = sql.as_bytes();
        msg.extend_from_slice(&((body.len() + 4 + 1) as u32).to_be_bytes());
        msg.extend_from_slice(body);
        msg.push(0);
        self.stream.write_all(&msg).unwrap();
        let mut last_row_val = None;
        loop {
            let mut tag = [0u8; 1];
            self.stream.read_exact(&mut tag).unwrap();
            let mut len = [0u8; 4];
            self.stream.read_exact(&mut len).unwrap();
            let n = u32::from_be_bytes(len) as usize - 4;
            let mut body = vec![0u8; n];
            self.stream.read_exact(&mut body).unwrap();
            match tag[0] {
                b'D' => {
                    // DataRow（text 格式）：nfields(2B) + collen(4B) + ASCII 值
                    if body.len() >= 6 {
                        let clen = u32::from_be_bytes(body[2..6].try_into().unwrap()) as usize;
                        if body.len() >= 6 + clen {
                            last_row_val = std::str::from_utf8(&body[6..6 + clen])
                                .ok()
                                .and_then(|s| s.parse::<i64>().ok());
                        }
                    }
                }
                b'E' => panic!("ErrorResponse: {}", String::from_utf8_lossy(&body)),
                b'Z' => break, // ReadyForQuery
                other => {
                    if std::env::var("DENDRO_PGW_DEBUG").is_ok() {
                        eprintln!("[pg] tag={:?} body={:?}", other, String::from_utf8_lossy(&body));
                    }
                }
            }
        }
        last_row_val
    }

    fn count_t(&mut self) -> i64 {
        self.query_count("SELECT count(*) FROM t").expect("no DataRow for count")
    }
}

#[test]
fn sigkill_crash_recovery_via_pg() {
    let data = std::env::temp_dir().join(format!("dendro-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let pg_port = free_port();
    let mut child = start_server(&data, pg_port);
    assert!(wait_ready(pg_port), "serve 未就绪");

    // 经 PG 协议写入：一半 checkpoint（物化进树）、一半仅 WAL
    {
        let mut c = PgSync::connect(pg_port);
        c.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)");
        c.exec("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");
        c.exec("CHECKPOINT");
        c.exec("INSERT INTO t VALUES (4, 'd'), (5, 'e')"); // 仅 WAL
    }

    // **SIGKILL**：无任何清理（Group 持久级下已 ack 者必已 durable）
    child.child.kill().unwrap();
    let _ = child.child.wait();

    // 重启（新端口）：崩溃前 ack 的全部数据必须可见
    let pg2 = free_port();
    let child2 = start_server(&data, pg2);
    assert!(wait_ready(pg2), "重启 serve 未就绪");
    let mut c = PgSync::connect(pg2);
    assert_eq!(c.count_t(), 5, "SIGKILL 后 ack 数据丢失");

    // 崩溃后可继续写入
    c.exec("INSERT INTO t VALUES (6, 'f')");
    assert_eq!(c.count_t(), 6);

    drop(child2); // Drop：kill+wait
    let _ = std::fs::remove_dir_all(&data);
}
