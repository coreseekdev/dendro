//! dendro-mywire — MySQL 客户端协议 v10 服务端（SPEC 06 §3）。
//!
//! 架构约束（SPEC 10 §7）：`std::net` + `std::thread` 的阻塞线程模型，不用 tokio。
//! 分层（协议层只做「字节 ↔ 内部结果集」，无业务逻辑）：
//! - [`codec`]   —— 帧（3B 长度 + sequence_id）/ lenenc / OK / ERR / EOF 包
//! - [`handshake`] —— v10 greeting、HandshakeResponse41 解析、AuthSwitchRequest
//! - [`auth`]    —— mysql_native_password（SHA1 异或式）校验
//! - [`query`]   —— COM_* 命令分发与 text 结果集编码
//! - [`column_def`] —— ColumnDefinition41 与 ColType → MySQL 类型码映射
//!
//! 能力协商：不宣告 CLIENT_DEPRECATE_EOF，统一走 EOF 包（SPEC 06 §3 简化）；
//! 客户端按服务端宣告的能力回退，均兼容。

pub mod auth;
pub mod codec;
pub mod column_def;
pub mod handshake;
pub mod query;

use dendro_core::engine::{Database, WireSession};
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

/// 服务端配置
#[derive(Clone, Debug)]
pub struct MyConfig {
    /// `None`/空串 = 任意口令放行；`Some(pw)` = 校验 mysql_native_password
    pub password: Option<String>,
    /// 握手中的 server version 字符串
    pub server_version: String,
    /// 读侧单逻辑包上限；超限回 ERR 1153 "08S01" 后断开
    pub max_allowed_packet: usize,
}

impl Default for MyConfig {
    fn default() -> Self {
        Self {
            password: None,
            server_version: "8.0.36-dendro".to_string(),
            max_allowed_packet: codec::DEFAULT_MAX_ALLOWED_PACKET,
        }
    }
}

/// 每连接一个会话的工厂（生产用 [`Database`] 生成；测试注入 mock）
pub type SessionFactory = Arc<dyn Fn() -> Box<dyn WireSession> + Send + Sync>;

/// 核心入口：驱动一条连接的完整生命周期（握手 → 命令循环 → 关闭）。
/// 与具体传输解耦，单测可用 `UnixStream::pair` 注入两端。
pub fn handle_connection<T: Read + Write>(
    stream: T,
    mut sess: Box<dyn WireSession>,
    cfg: MyConfig,
) -> io::Result<()> {
    let mut io = codec::WireIo::new(stream);
    // SPEC 06 §3：greeting(seq 0) → HandshakeResponse41 → 校验 → OK / ERR(断开)
    if !handshake::handshake(&mut io, &cfg)? {
        return Ok(());
    }
    loop {
        match io.read_packet(cfg.max_allowed_packet) {
            Ok(Some(pkt)) => {
                let cont = query::dispatch(&mut io, sess.as_mut(), &pkt.payload, &cfg)?;
                io.flush()?;
                if !cont {
                    return Ok(()); // COM_QUIT / 协议错误
                }
            }
            Ok(None) => return Ok(()), // 对端关闭
            Err(codec::ReadError::TooLarge) => {
                // max_allowed_packet 保护：ERR 1153 后断开
                io.write_packet(&codec::err_packet(
                    1153,
                    "08S01",
                    "Got a packet bigger than 'max_allowed_packet' bytes",
                ))?;
                io.flush()?;
                return Ok(());
            }
            Err(codec::ReadError::Closed) => return Ok(()),
            Err(codec::ReadError::Io(e)) => return Err(e),
        }
    }
}

/// 阻塞监听（生产装配）：每连接一个线程，会话由 `Database` 生成。
pub fn serve(addr: &str, db: Arc<Database>, cfg: MyConfig) -> io::Result<()> {
    let factory: SessionFactory = {
        let db = db.clone();
        Arc::new(move || -> Box<dyn WireSession> { Box::new(db.new_session()) })
    };
    serve_with(addr, cfg, factory)
}

/// 同 [`serve`]，但会话由调用方工厂提供（嵌入/测试用）。
pub fn serve_with(addr: &str, cfg: MyConfig, factory: SessionFactory) -> io::Result<()> {
    MyServer::bind(addr, cfg, factory)?.run()
}

/// 在给定 listener 上服务（装配前置 bind 用：端口冲突在打印 ready 前暴露）
pub fn serve_listener(
    listener: std::net::TcpListener,
    cfg: MyConfig,
    factory: SessionFactory,
) -> io::Result<()> {
    MyServer::bind_on(listener, cfg, factory)?.run()
}

/// 可探测绑定地址的阻塞式服务（`:0` 端口 + 后台线程场景）。
pub struct MyServer {
    listener: TcpListener,
    cfg: MyConfig,
    factory: SessionFactory,
}

impl MyServer {
    pub fn bind(addr: &str, cfg: MyConfig, factory: SessionFactory) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind(addr)?;
        Self::bind_on(listener, cfg, factory)
    }

    /// 在给定 listener 上构造（装配前置 bind 用）
    pub fn bind_on(
        listener: std::net::TcpListener,
        cfg: MyConfig,
        factory: SessionFactory,
    ) -> io::Result<Self> {
        Ok(Self {
            listener,
            cfg,
            factory,
        })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// accept 循环：监听器关闭（incoming 结束）或不可恢复错误时返回。
    pub fn run(self) -> io::Result<()> {
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            let cfg = self.cfg.clone();
            let factory = self.factory.clone();
            std::thread::spawn(move || {
                let sess = factory();
                if let Err(e) = handle_connection(stream, sess, cfg) {
                    tracing::debug!(error = %e, "dendro-mywire: connection ended with io error");
                }
            });
        }
        Ok(())
    }
}
