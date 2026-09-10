//! v10 握手与认证流程 — SPEC 06 §3「握手/认证」。
//!
//! ```text
//! server greeting(seq 0): protocol=10, version, conn_id, auth-plugin-data 20B,
//!                         capability(不含 DEPRECATE_EOF), charset 45, status 2,
//!                         auth_plugin "mysql_native_password"
//! client HandshakeResponse41(seq 1)
//! server: 校验 native password → OK(seq 2) / ERR 1045 "28000"（并断开）
//!         客户端插件名不一致 → AuthSwitchRequest 后收 20B token
//! ```

use crate::auth::{self, NATIVE};
use crate::codec::{
    err_packet, ok_packet, Reader, Truncated, WireIo, AUTH_SWITCH_HEADER,
    CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
    SERVER_CAPABILITIES, SERVER_STATUS_AUTOCOMMIT, UTF8MB4_GENERAL_CI,
};
use crate::MyConfig;
use std::io::{self, Read, Write};

/// 握手响应解析失败原因
#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// 32B 定长包 = SSLRequest（本端未宣告 CLIENT_SSL，v1 无 TLS）
    SslRequest,
    /// 非 PROTOCOL_41 的老客户端
    OldClient,
    /// 包被截断
    Truncated,
}

/// HandshakeResponse41 解析结果
#[derive(Debug, Clone)]
pub struct HandshakeResponse41 {
    pub capabilities: u32,
    pub max_packet: u32,
    pub charset: u8,
    pub username: String,
    pub auth_response: Vec<u8>,
    pub database: Option<String>,
    /// CLIENT_PLUGIN_AUTH 时的客户端插件名
    pub plugin: Option<String>,
}

pub fn parse_handshake_response(p: &[u8]) -> Result<HandshakeResponse41, HandshakeError> {
    // SSLRequest 恰为 32B：caps(4)+max_packet(4)+charset(1)+reserved(23)，无用户名
    if p.len() <= 32 {
        return Err(HandshakeError::SslRequest);
    }
    let mut r = Reader::new(p);
    let capabilities = r.u32_le().map_err(|_| HandshakeError::Truncated)?;
    if capabilities & CLIENT_PROTOCOL_41 == 0 {
        return Err(HandshakeError::OldClient);
    }
    let max_packet = r.u32_le().map_err(|_| HandshakeError::Truncated)?;
    let charset = r.u8().map_err(|_| HandshakeError::Truncated)?;
    r.skip(23).map_err(|_| HandshakeError::Truncated)?; // 保留字段
    let username = r.nul_terminated().map_err(|_| HandshakeError::Truncated)?;
    // auth response：lenenc（PLUGIN_AUTH_LENENC）> 1B 长度前缀（SECURE_CONNECTION）> NUL 串（老式）
    let auth_response = if capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        r.lenenc_bytes()
            .map_err(|_| HandshakeError::Truncated)?
            .to_vec()
    } else if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        let n = r.u8().map_err(|_| HandshakeError::Truncated)? as usize;
        r.take(n).map_err(|_| HandshakeError::Truncated)?.to_vec()
    } else {
        r.nul_terminated()
            .map_err(|_| HandshakeError::Truncated)?
            .to_vec()
    };
    // [database NUL]：仅当客户端置 CLIENT_CONNECT_WITH_DB 时才存在
    let database = if capabilities & crate::codec::CLIENT_CONNECT_WITH_DB != 0 {
        match r.nul_terminated() {
            Ok(db) => Some(String::from_utf8_lossy(db).into_owned()),
            Err(Truncated) => None,
        }
    } else {
        None
    };
    // [plugin NUL]：仅当客户端置 CLIENT_PLUGIN_AUTH 时才存在
    let plugin = if capabilities & crate::codec::CLIENT_PLUGIN_AUTH != 0 {
        match r.nul_terminated() {
            Ok(pl) => Some(String::from_utf8_lossy(pl).into_owned()),
            Err(Truncated) => None,
        }
    } else {
        None
    };
    Ok(HandshakeResponse41 {
        capabilities,
        max_packet,
        charset,
        username: String::from_utf8_lossy(username).into_owned(),
        auth_response,
        database,
        plugin,
    })
}

/// Server Greeting v10（SPEC 06 §3：version "8.0.36-dendro"、20B scramble、
/// charset utf8mb4_general_ci(45)、status AUTOCOMMIT(2)、mysql_native_password）
pub fn greeting_packet(cfg: &MyConfig, conn_id: u32, scramble: &[u8; 20]) -> Vec<u8> {
    let caps = SERVER_CAPABILITIES;
    let mut b = Vec::with_capacity(96);
    b.push(10); // protocol version
    b.extend_from_slice(cfg.server_version.as_bytes());
    b.push(0);
    b.extend_from_slice(&conn_id.to_le_bytes());
    b.extend_from_slice(&scramble[..8]); // auth-plugin-data-part-1
    b.push(0); // filler
    b.extend_from_slice(&((caps & 0xFFFF) as u16).to_le_bytes()); // capability 低 16 位
    b.push(UTF8MB4_GENERAL_CI);
    b.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
    b.extend_from_slice(&((caps >> 16) as u16).to_le_bytes()); // capability 高 16 位
    b.push(21); // auth-plugin-data 总长（20 + 结尾 NUL）
    b.extend_from_slice(&[0u8; 10]); // 保留
    b.extend_from_slice(&scramble[8..]); // auth-plugin-data-part-2（12B）
    b.push(0); // part-2 结尾 NUL
    b.extend_from_slice(NATIVE.as_bytes());
    b.push(0);
    b
}

/// AuthSwitchRequest：0xfe + plugin NUL + scramble(20B) + 0x00
fn auth_switch_packet(scramble: &[u8; 20]) -> Vec<u8> {
    let mut b = vec![AUTH_SWITCH_HEADER];
    b.extend_from_slice(NATIVE.as_bytes());
    b.push(0);
    b.extend_from_slice(scramble);
    b.push(0);
    b
}

/// 连接 id 自增（每连接唯一）
static CONN_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn next_conn_id() -> u32 {
    use std::sync::atomic::Ordering;
    CONN_ID.fetch_add(1, Ordering::Relaxed)
}

/// 完整握手 + 认证。返回 `false` = 认证失败/客户端提前离开（ERR 已写、连接应关闭）。
pub fn handshake<T: Read + Write>(io: &mut WireIo<T>, cfg: &MyConfig) -> io::Result<bool> {
    let scramble = auth::new_scramble();
    io.write_packet(&greeting_packet(cfg, next_conn_id(), &scramble))?;
    io.flush()?;

    let Some(pkt) = io.read_packet(cfg.max_allowed_packet)? else {
        return Ok(false);
    };
    let hs = match parse_handshake_response(&pkt.payload) {
        Ok(hs) => hs,
        Err(e) => {
            let (code, msg) = match e {
                HandshakeError::SslRequest => (
                    1043,
                    "TLS not supported by dendro (v1): retry without ssl-mode=REQUIRED",
                ),
                HandshakeError::OldClient => (1043, "old pre-4.1 clients not supported"),
                HandshakeError::Truncated => (1043, "malformed handshake response"),
            };
            io.write_packet(&err_packet(code, "08S01", msg))?;
            io.flush()?;
            return Ok(false);
        }
    };

    // 认证 token：插件名不匹配（如客户端默认 caching_sha2_password）→ AuthSwitchRequest
    let token = if hs.plugin.as_deref().unwrap_or(NATIVE) == NATIVE {
        hs.auth_response.clone()
    } else {
        io.write_packet(&auth_switch_packet(&scramble))?;
        io.flush()?;
        match io.read_packet(cfg.max_allowed_packet)? {
            Some(p) => p.payload,
            None => return Ok(false),
        }
    };

    // cfg.password：None/空串 = 放行；Some = native password 校验
    let expect = cfg.password.as_deref().filter(|p| !p.is_empty());
    let ok = match expect {
        None => true,
        Some(pw) => auth::verify_native(pw, &scramble, &token),
    };
    if ok {
        io.write_packet(&ok_packet(0, 0, SERVER_STATUS_AUTOCOMMIT, None))?;
        io.flush()?;
        Ok(true)
    } else {
        // ER_ACCESS_DENIED_ERROR
        io.write_packet(&err_packet(
            1045,
            "28000",
            &format!(
                "Access denied for user '{}' (using password: {})",
                hs.username,
                expect.is_some()
            ),
        ))?;
        io.flush()?;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::DEFAULT_MAX_ALLOWED_PACKET;
    use crate::MyConfig;

    #[test]
    fn greeting_layout() {
        let cfg = MyConfig::default();
        let scramble: [u8; 20] = std::array::from_fn(|i| (i * 7 + 3) as u8);
        let p = greeting_packet(&cfg, 42, &scramble);
        let mut r = Reader::new(&p);
        assert_eq!(r.u8().unwrap(), 10);
        let ver = r.nul_terminated().unwrap();
        assert_eq!(ver, cfg.server_version.as_bytes());
        assert_eq!(r.u32_le().unwrap(), 42); // conn id
        let part1 = r.take(8).unwrap();
        assert_eq!(part1, &scramble[..8]);
        assert_eq!(r.u8().unwrap(), 0); // filler
        let caps_lo = r.u16_le().unwrap();
        assert_eq!(r.u8().unwrap(), UTF8MB4_GENERAL_CI);
        assert_eq!(r.u16_le().unwrap(), SERVER_STATUS_AUTOCOMMIT);
        let caps_hi = r.u16_le().unwrap();
        let auth_len = r.u8().unwrap() as usize;
        assert_eq!(auth_len, 21);
        r.skip(10).unwrap();
        let part2 = r.take(13).unwrap();
        assert_eq!(&part2[..12], &scramble[8..]);
        assert_eq!(part2[12], 0);
        let plugin = r.nul_terminated().unwrap();
        assert_eq!(plugin, NATIVE.as_bytes());
        assert!(r.is_empty());
        let caps = (caps_lo as u32) | ((caps_hi as u32) << 16);
        assert_eq!(caps, SERVER_CAPABILITIES);
        // 关键：不宣告 DEPRECATE_EOF（走 EOF 包）
        assert_eq!(caps & (1 << 24), 0);
        // 不宣告 SSL
        assert_eq!(caps & crate::codec::CLIENT_SSL, 0);
    }

    #[test]
    fn parse_response41() {
        // 构造一个带 lenenc auth + db + plugin 的响应
        let mut p = Vec::new();
        let caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | crate::codec::CLIENT_CONNECT_WITH_DB
            | crate::codec::CLIENT_PLUGIN_AUTH;
        p.extend_from_slice(&caps.to_le_bytes());
        p.extend_from_slice(&DEFAULT_MAX_ALLOWED_PACKET.to_le_bytes()[..4]);
        p.push(45);
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"root");
        p.push(0);
        crate::codec::write_lenenc_str(&mut p, &[1u8; 20]); // auth response（lenenc）
        p.extend_from_slice(b"cambium");
        p.push(0);
        p.extend_from_slice(NATIVE.as_bytes());
        p.push(0);
        let hs = parse_handshake_response(&p).unwrap();
        assert_eq!(hs.username, "root");
        assert_eq!(hs.auth_response, vec![1u8; 20]);
        assert_eq!(hs.database.as_deref(), Some("cambium"));
        assert_eq!(hs.plugin.as_deref(), Some(NATIVE));

        // 32B 定长 = SSLRequest
        assert!(matches!(
            parse_handshake_response(&[0u8; 32]),
            Err(HandshakeError::SslRequest)
        ));
        // 1B 长度前缀形态（SECURE_CONNECTION、无 LENENC）
        let mut p = Vec::new();
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION;
        p.extend_from_slice(&caps.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.push(45);
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"u");
        p.push(0);
        p.push(3);
        p.extend_from_slice(b"abc");
        let hs = parse_handshake_response(&p).unwrap();
        assert_eq!(hs.auth_response, b"abc");
        assert_eq!(hs.database, None);
        assert_eq!(hs.plugin, None);
    }
}
