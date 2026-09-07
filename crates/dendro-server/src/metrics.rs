//! HTTP 运维端点：`/readyz`（k8s 就绪探针）+ `/metrics`（Prometheus 文本格式，
//! 负载自感知 M1/M2——调度器据此刻弹性伸缩）。
//!
//! 暴露的负载信号（每驻留分支）：
//! - `dendro_branch_pending_bytes`   自上次 checkpoint 的累积变更字节——
//!   越高说明 TP 写压越大 / checkpoint 跟不上，是扩容的核心信号
//! - `dendro_branch_watermark`       已安装提交水位（复合时间戳）
//! - `dendro_branch_wal_durable_seq` WAL durable 水位（落后 = flush 压力）
//! - `dendro_branch_lease_ttl_ms`    写者租约剩余毫秒——持续走低至拒写，
//!   说明该写者负载已高到无法维持 commit 路径续期
//!
//! 只读监控：经 `Database::active_branches()`，绝不懒加载分支（无写副作用）。

use dendro_core::engine::Database;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// 阻塞监听（server 装配用）
pub fn serve(addr: &str, db: Arc<Database>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    serve_listener(listener, db)
}

/// 在给定 listener 上服务（测试用：bind 端口 0 后取 local_addr）
pub fn serve_listener(listener: TcpListener, db: Arc<Database>) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "dendro metrics: listening");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let db = Arc::clone(&db);
                std::thread::spawn(move || {
                    let _ = stream.set_nodelay(true);
                    let _ = serve_conn(stream, &db);
                });
            }
            Err(e) => tracing::warn!(error = %e, "metrics accept failed"),
        }
    }
    Ok(())
}

fn serve_conn(stream: TcpStream, db: &Arc<Database>) -> std::io::Result<()> {
    let mut r = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let path = path.split('?').next().unwrap_or("/");
    // 简单逐请求处理（探针/抓取频率低；读完请求头避免粘包残留）
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    match path {
        // readiness（非 liveness）：任一驻留**写者**租约过期 → 503。
        // 过期写者只能返回 40001，继续接流量只会放大错误（评审 §3.4）。
        "/readyz" => {
            let now = now_ms();
            let ready = db.active_branches().iter().all(|b| {
                b.read_only || b.lease.state.lock().lease.expires_at_ms > now
            });
            if ready {
                http(stream, 200, "ok\n")
            } else {
                http(stream, 503, "writer lease expired\n")
            }
        }
        "/metrics" => {
            let body = render_metrics(db);
            http(stream, 200, &body)
        }
        _ => http(stream, 404, "not found\n"),
    }
}

fn http(mut s: TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        _ => "Not Found",
    };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(resp.as_bytes())?;
    s.flush()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn render_metrics(db: &Arc<Database>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# HELP dendro_up process serving requests");
    let _ = writeln!(out, "# TYPE dendro_up gauge");
    let _ = writeln!(out, "dendro_up 1");
    let now = now_ms();
    let mut branches = db.active_branches();
    branches.sort_by(|a, b| a.name.cmp(&b.name));
    // HELP/TYPE 每个指标族只出现一次（Prometheus 文本格式）
    let _ = writeln!(out, "# HELP dendro_branch_pending_bytes un-checkpointed change bytes (scale-out signal)");
    let _ = writeln!(out, "# TYPE dendro_branch_pending_bytes gauge");
    let _ = writeln!(out, "# TYPE dendro_branch_watermark gauge");
    let _ = writeln!(out, "# TYPE dendro_branch_wal_durable_seq gauge");
    let _ = writeln!(out, "# TYPE dendro_branch_lease_epoch gauge");
    let _ = writeln!(out, "# HELP dendro_branch_lease_ttl_ms writer lease time-to-live");
    let _ = writeln!(out, "# TYPE dendro_branch_lease_ttl_ms gauge");
    for b in branches {
        let name = &b.name;
        let watermark = b.snapshot();
        let pending = b.pending_bytes.load(std::sync::atomic::Ordering::Relaxed);
        let durable = b.wal.durable_watermark();
        let (epoch, ttl_ms) = {
            let st = b.lease.state.lock();
            (st.lease.epoch, st.lease.expires_at_ms - now)
        };
        let _ = writeln!(out, "dendro_branch_pending_bytes{{branch=\"{name}\"}} {pending}");
        let _ = writeln!(out, "dendro_branch_watermark{{branch=\"{name}\"}} {watermark}");
        let _ = writeln!(out, "dendro_branch_wal_durable_seq{{branch=\"{name}\"}} {durable}");
        let _ = writeln!(out, "dendro_branch_lease_epoch{{branch=\"{name}\"}} {epoch}");
        if b.read_only {
            // 只读分支无真实租约（占位 expires_at_ms=0）——输出 TTL 会是巨负数，
            // 污染告警面板；以 read_only 标记代替
            let _ = writeln!(out, "dendro_branch_read_only{{branch=\"{name}\"}} 1");
        } else {
            let _ = writeln!(out, "dendro_branch_lease_ttl_ms{{branch=\"{name}\"}} {ttl_ms}");
        }
    }
    out
}
