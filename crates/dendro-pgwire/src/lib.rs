//! dendro-pgwire — PostgreSQL wire 协议 v3 服务端（SPEC 06 §2）。
//!
//! 传输层按 SPEC 10 §7 的推荐方案：`std::net` + `std::thread`，不走 tokio。
//! 协议状态机的核心入口是 [`handle_connection`]，对任意 `Read + Write`
//! 双向流工作——单测可直接用 `UnixStream::pair` / TcpStream 驱动。
//!
//! ```text
//! serve(addr, Arc<Database>)     阻塞监听，每连接 std::thread::spawn
//!   └─ handle_connection(stream, Box<dyn WireSession>, PgConfig)
//!        ├─ startup.rs    startup + 认证 + ParameterStatus/BackendKeyData/RFQ
//!        ├─ simple_query.rs   'Q'
//!        └─ extended.rs       P/B/D/E/C/H/S（出错跳过直到 Sync）
//! ```
//!
//! 协议层只做「字节 ↔ 内部 AST/结果集」，无业务逻辑（SPEC 06 §0）。

#![deny(unsafe_code)]

#![allow(clippy::all)]
pub mod codec;
pub mod error;
pub mod extended;
pub mod param;
mod parse;
pub mod simple_query;
pub mod startup;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use dendro_core::engine::Database;
use dendro_core::engine::WireSession;

use crate::codec::{FeMessage, PgStream};

/// 连接级配置（SPEC 10 §8：认证 trust / cleartext）
#[derive(Debug, Clone, Default)]
pub struct PgConfig {
    /// Some → AuthenticationCleartextPassword 流程；None → trust
    pub password: Option<String>,
}

static NEXT_BACKEND_PID: AtomicI32 = AtomicI32::new(1);
static SECRET_COUNTER: AtomicU64 = AtomicU64::new(0);

/// BackendKeyData pid：进程内自增
pub(crate) fn next_backend_pid() -> i32 {
    NEXT_BACKEND_PID.fetch_add(1, Ordering::Relaxed)
}

/// BackendKeyData secret：无 rand 依赖，用时间 + 计数器的 splitmix64 终结器
pub(crate) fn next_backend_secret() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut x = nanos.wrapping_add(
        SECRET_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15),
    );
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (x ^ (x >> 31)) as u32
}

/// 阻塞监听 `addr`，每连接派生一个线程执行 [`handle_connection`]。
///
/// 单个连接的错误只影响该连接（记日志后丢弃）；accept 错误记日志继续。
pub fn serve(addr: SocketAddr, db: Arc<Database>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    tracing::info!(%addr, "dendro-pgwire: listening");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let db = Arc::clone(&db);
                thread::spawn(move || {
                    let _ = stream.set_nodelay(true);
                    // Session 单线程使用（SPEC 10 §7）；move 进连接线程
                    let sess: Box<dyn WireSession> = Box::new(db.new_session());
                    if let Err(e) = handle_connection(stream, sess, PgConfig::default()) {
                        tracing::debug!(error = %e, "dendro-pgwire: connection ended");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "dendro-pgwire: accept failed"),
        }
    }
    Ok(())
}

/// 协议状态机入口：startup/认证 → 消息循环（'Q'/扩展协议/X）。
///
/// 返回 `Ok(())` 表示客户端正常结束（EOF 或 Terminate）；协议违规/
/// IO 错误以 `Err` 返回（致命 ErrorResponse 已发出后才返回 Err 的场景
/// 除外——协议错误在发送 FATAL 后断连）。
pub fn handle_connection<T: Read + Write>(
    stream: T,
    mut sess: Box<dyn WireSession>,
    cfg: PgConfig,
) -> io::Result<()> {
    let mut pg = PgStream::new(stream);

    // startup + 认证（SPEC 06 §2.1）；None = 连接应关闭（已发错误或对端断开）
    let Some(_params) = startup::handshake(&mut pg, &cfg)? else {
        return Ok(());
    };

    let mut ext = extended::ExtendedState::default();
    loop {
        let Some(msg) = pg.read_message()? else {
            break; // 客户端干净断开
        };
        match msg {
            FeMessage::Query(sql) => {
                simple_query::handle_query(&mut pg, sess.as_mut(), &sql)?;
                // 简单查询完成 = 扩展协议错误跳过态结束
                ext.in_error = false;
            }
            FeMessage::Terminate => break,
            msg => match extended::handle_message(&mut pg, sess.as_mut(), &mut ext, msg)? {
                extended::Flow::Continue => {}
                extended::Flow::Close => break,
            },
        }
    }
    // 尽力冲刷残留输出（如 Terminate 之前未 Sync 的缓冲）
    let _ = pg.flush();
    Ok(())
}
