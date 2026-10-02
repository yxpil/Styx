//! Nebula 有线协议 v2 的客户端侧实现。
//!
//! 协议分两阶段，握手阶段明文（但防重放），之后全部走加密帧。
//!
//! ```text
//! client ──▶ hello:     b"NEBULA2"(7)
//! server ──▶ ready:     0x02(1)
//! client ──▶ identity:  varint 用户名长度 ‖ 用户名(UTF-8)
//! server ──▶ challenge: salt(16) ‖ challenge(32)          共 48 字节
//! client ──▶ proof:     HMAC-SHA256(session_key, challenge)(32)
//! server ──▶ status:    0x01 通过 / 0x00 失败（随后断开）
//! ```
//!
//! 加密帧：`[u32 BE 长度] ‖ ChaCha20-Poly1305(nonce ‖ 密文 ‖ tag)`，
//! `AAD = "nebula/frame/up|down" ‖ seq(u64 BE)`，收发序号各自递增。
//! 重放、重排、跨方向注入的帧都无法通过认证。
//!
//! 帧载荷是 UTF-8 JSON（[`Request`] / [`Response`]）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::crypto::*;
use crate::error::{MemoryError, Result};

// ---------------------------------------------------------------- 协议常量

/// 协议魔数。
pub const PROTOCOL_MAGIC: &[u8; 7] = b"NEBULA2";
/// 版本确认字节。
pub const READY_BYTE: u8 = 0x02;
/// 用户名最大长度。
pub const MAX_USER_NAME: usize = 64;
/// 挑战长度。
pub const CHALLENGE_LEN: usize = 32;
/// challenge 帧总长。
pub const CHALLENGE_FRAME_LEN: usize = SALT_LEN + CHALLENGE_LEN;
/// 认证通过。
pub const STATUS_OK: u8 = 1;
/// 单帧上限 16 MiB。
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// 会话密钥派生信息前缀。
const SESSION_INFO: &[u8] = b"nebula/session/v2";
/// 帧 AAD 方向域前缀。
const FRAME_INFO_UP: &[u8] = b"nebula/frame/up";
const FRAME_INFO_DOWN: &[u8] = b"nebula/frame/down";

/// 帧方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// client → server。
    Up,
    /// server → client。
    Down,
}

/// 客户端请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// 执行一条 SQL。
    Sql { sql: String },
    /// 连通性探测。
    Ping,
    /// 优雅关闭。
    Close,
}

/// 服务端响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub affected: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// 是否成功。
    pub fn is_ok(&self) -> bool {
        self.ok
    }

    /// 转成 `Result`（SQL 逻辑错误在这里变成 Err）。
    pub fn into_result(self) -> Result<Self> {
        if self.ok {
            Ok(self)
        } else {
            Err(MemoryError::Sql(
                self.error.unwrap_or_else(|| "未知 SQL 错误".into()),
            ))
        }
    }

    /// 取列名。
    pub fn columns(&self) -> Vec<String> {
        self.columns.clone().unwrap_or_default()
    }

    /// 取某一行某一列（按列名）。
    pub fn cell(&self, row: usize, column: &str) -> Option<&str> {
        let cols = self.columns.as_ref()?;
        let idx = cols.iter().position(|c| c == column)?;
        self.rows
            .as_ref()?
            .get(row)?
            .get(idx)
            .map(|s| s.as_str())
    }
}

// ------------------------------------------------------------------ 编解码

/// LEB128 varint 编码。
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// LEB128 varint 解码。
pub fn take_varint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in bytes.iter().enumerate() {
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return Err(MemoryError::Protocol("varint 过长".into()));
        }
    }
    Err(MemoryError::Protocol("varint 被截断".into()))
}

/// 写身份帧。
pub fn write_identity(stream: &mut TcpStream, user: &str) -> Result<()> {
    let bytes = user.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_USER_NAME {
        return Err(MemoryError::Protocol(format!(
            "用户名长度必须在 1..{MAX_USER_NAME} 字节之间"
        )));
    }
    let mut frame = Vec::with_capacity(bytes.len() + 1);
    put_varint(&mut frame, bytes.len() as u64);
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

/// 由主密钥与挑战派生会话密钥。
pub fn session_key(master: &[u8; KEY_LEN], challenge: &[u8; CHALLENGE_LEN]) -> [u8; KEY_LEN] {
    let mut info = Vec::with_capacity(SESSION_INFO.len() + CHALLENGE_LEN);
    info.extend_from_slice(SESSION_INFO);
    info.extend_from_slice(challenge);
    hkdf_sha256(master, &info)
}

/// 认证证明。
pub fn auth_proof(session_key: &[u8; KEY_LEN], challenge: &[u8; CHALLENGE_LEN]) -> [u8; KEY_LEN] {
    hmac_sha256(session_key, challenge)
}

/// 帧 AAD。
fn frame_aad(dir: Direction, seq: u64) -> Vec<u8> {
    let prefix = match dir {
        Direction::Up => FRAME_INFO_UP,
        Direction::Down => FRAME_INFO_DOWN,
    };
    let mut aad = Vec::with_capacity(prefix.len() + 8);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(&seq.to_be_bytes());
    aad
}

/// 写一帧。
pub fn write_frame(
    stream: &mut TcpStream,
    key: &[u8; KEY_LEN],
    dir: Direction,
    seq: u64,
    payload: &[u8],
) -> Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(MemoryError::Protocol(format!(
            "载荷 {} 字节超过单帧上限 {MAX_FRAME}",
            payload.len()
        )));
    }
    let blob = seal(key, payload, &frame_aad(dir, seq));
    let len = u32::try_from(blob.len())
        .map_err(|_| MemoryError::Protocol("帧长度超出 u32".into()))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&blob)?;
    stream.flush()?;
    Ok(())
}

/// 读一帧。
pub fn read_frame(
    stream: &mut TcpStream,
    key: &[u8; KEY_LEN],
    dir: Direction,
    seq: u64,
) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| wrap_eof(e, "长度前缀"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(MemoryError::Protocol(format!(
            "帧长度 {len} 超过上限 {MAX_FRAME}"
        )));
    }
    if len == 0 {
        return Err(MemoryError::Protocol("收到零长度帧".into()));
    }
    let mut blob = vec![0u8; len];
    stream
        .read_exact(&mut blob)
        .map_err(|e| wrap_eof(e, "帧体"))?;
    open(key, &blob, &frame_aad(dir, seq)).map_err(MemoryError::Protocol)
}

fn wrap_eof(e: std::io::Error, stage: &str) -> MemoryError {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
            MemoryError::Protocol("连接已关闭".into())
        }
        _ => MemoryError::Io(format!("读取{stage}失败：{e}")),
    }
}

// -------------------------------------------------------------------- 客户端

/// 已认证的 Nebula TCP 会话。
pub struct NebulaClient {
    stream: TcpStream,
    key: [u8; KEY_LEN],
    send_seq: u64,
    recv_seq: u64,
    addr: String,
    user: String,
}

impl std::fmt::Debug for NebulaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NebulaClient")
            .field("addr", &self.addr)
            .field("user", &self.user)
            .field("send_seq", &self.send_seq)
            .field("recv_seq", &self.recv_seq)
            .finish_non_exhaustive()
    }
}

impl NebulaClient {
    /// 以 admin 身份连接。
    pub fn connect(addr: &str, password: &str, timeout: Duration) -> Result<Self> {
        Self::connect_as(addr, "admin", password, timeout)
    }

    /// 以指定用户连接并完成 v2 认证。
    pub fn connect_as(
        addr: &str,
        user: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let mut stream = TcpStream::connect(addr)
            .map_err(|e| MemoryError::Io(format!("无法连接 Nebula 服务 {addr}：{e}")))?;
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();
        stream.set_nodelay(true).ok();

        // 1) hello → ready
        stream.write_all(PROTOCOL_MAGIC)?;
        stream.flush()?;
        let mut ready = [0u8; 1];
        stream
            .read_exact(&mut ready)
            .map_err(|e| wrap_eof(e, "ready 字节"))?;
        if ready[0] != READY_BYTE {
            return Err(MemoryError::Protocol(format!(
                "服务端拒绝了协议版本（收到 0x{:02x}，期望 0x{READY_BYTE:02x}）",
                ready[0]
            )));
        }

        // 2) 身份声明
        write_identity(&mut stream, user)?;

        // 3) 读 challenge：salt(16) ‖ challenge(32)
        let mut frame = [0u8; CHALLENGE_FRAME_LEN];
        stream
            .read_exact(&mut frame)
            .map_err(|e| wrap_eof(e, "challenge"))?;
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&frame[..SALT_LEN]);
        let mut challenge = [0u8; CHALLENGE_LEN];
        challenge.copy_from_slice(&frame[SALT_LEN..]);

        // 4) 本地派生密钥并回 proof（密码不明文上网）
        let master = derive_master_key(password, &salt);
        let key = session_key(&master, &challenge);
        let proof = auth_proof(&key, &challenge);
        stream.write_all(&proof)?;
        stream.flush()?;

        // 5) 读认证状态
        let mut status = [0u8; 1];
        stream
            .read_exact(&mut status)
            .map_err(|e| wrap_eof(e, "认证状态"))?;
        if status[0] != STATUS_OK {
            return Err(MemoryError::Auth(format!(
                "Nebula 拒绝了用户名或密码（用户：{user}）"
            )));
        }

        Ok(NebulaClient {
            stream,
            key,
            send_seq: 0,
            recv_seq: 0,
            addr: addr.to_string(),
            user: user.to_string(),
        })
    }

    /// 服务地址。
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// 当前用户。
    pub fn user(&self) -> &str {
        &self.user
    }

    /// 发送一个请求并等待响应。
    fn request(&mut self, req: &Request) -> Result<Response> {
        let payload = serde_json::to_vec(req)?;
        write_frame(&mut self.stream, &self.key, Direction::Up, self.send_seq, &payload)?;
        self.send_seq += 1;

        let resp_payload = read_frame(
            &mut self.stream,
            &self.key,
            Direction::Down,
            self.recv_seq,
        )?;
        self.recv_seq += 1;
        let resp: Response = serde_json::from_slice(&resp_payload).map_err(|e| {
            MemoryError::Protocol(format!(
                "响应不是合法 JSON：{e}；载荷前 200 字节：{}",
                String::from_utf8_lossy(&resp_payload[..resp_payload.len().min(200)])
            ))
        })?;
        Ok(resp)
    }

    /// 执行一条 SQL（SQL 逻辑错误以 Err 返回，连接不受影响）。
    pub fn sql(&mut self, sql: &str) -> Result<Response> {
        self.request(&Request::Sql { sql: sql.to_string() })?
            .into_result()
    }

    /// 执行一条查询并返回 `(列名, 行)`。
    pub fn query(&mut self, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let resp = self.sql(sql)?;
        Ok((resp.columns(), resp.rows.unwrap_or_default()))
    }

    /// 连通性探测。
    pub fn ping(&mut self) -> Result<bool> {
        Ok(self.request(&Request::Ping)?.ok)
    }

    /// 优雅关闭。
    pub fn close(mut self) -> Result<()> {
        let _ = self.request(&Request::Close);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    // ---------------- 一个"服务端侧"的最小实现，用来验证协议是对称的 ----

    /// 在后台起一个假 Nebula 服务：完成握手后，对每条 SQL 回一个固定响应。
    fn spawn_fake_nebula(password: &'static str) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 7];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, PROTOCOL_MAGIC);
            stream.write_all(&[READY_BYTE]).unwrap();

            // 读身份帧
            let mut len_byte = [0u8; 1];
            stream.read_exact(&mut len_byte).unwrap();
            let (n, _) = take_varint(&len_byte).unwrap();
            let mut name = vec![0u8; n as usize];
            stream.read_exact(&mut name).unwrap();

            // 发 challenge
            let salt = [11u8; SALT_LEN];
            let challenge = [22u8; CHALLENGE_LEN];
            let mut frame = Vec::new();
            frame.extend_from_slice(&salt);
            frame.extend_from_slice(&challenge);
            stream.write_all(&frame).unwrap();

            // 校验 proof
            let mut proof = [0u8; KEY_LEN];
            stream.read_exact(&mut proof).unwrap();
            let master = derive_master_key(password, &salt);
            let key = session_key(&master, &challenge);
            let expected = auth_proof(&key, &challenge);
            if !ct_eq(&expected, &proof) {
                stream.write_all(&[0u8]).unwrap();
                return Vec::new();
            }
            stream.write_all(&[STATUS_OK]).unwrap();

            // 帧循环
            let mut recv_seq = 0u64;
            let mut send_seq = 0u64;
            let mut seen = Vec::new();
            while let Ok(payload) = read_frame(&mut stream, &key, Direction::Up, recv_seq) {
                recv_seq += 1;
                let req: Request = serde_json::from_slice(&payload).unwrap();
                let resp = match req {
                    Request::Sql { sql } => {
                        seen.push(sql.clone());
                        if sql.contains("boom") {
                            Response {
                                ok: false,
                                error: Some("unknown database 'xx'".into()),
                                ..Default::default()
                            }
                        } else if sql.contains("SELECT") {
                            Response {
                                ok: true,
                                columns: Some(vec!["id".into(), "content".into()]),
                                rows: Some(vec![vec!["1".into(), "示例内容".into()]]),
                                affected: 0,
                                ..Default::default()
                            }
                        } else {
                            Response {
                                ok: true,
                                message: Some("OK, inserted memory main.1".into()),
                                affected: 1,
                                ..Default::default()
                            }
                        }
                    }
                    Request::Ping => Response {
                        ok: true,
                        message: Some("pong".into()),
                        ..Default::default()
                    },
                    Request::Close => {
                        let body = serde_json::to_vec(&Response {
                            ok: true,
                            message: Some("bye".into()),
                            ..Default::default()
                        })
                        .unwrap();
                        write_frame(&mut stream, &key, Direction::Down, send_seq, &body).unwrap();
                        break;
                    }
                };
                let body = serde_json::to_vec(&resp).unwrap();
                write_frame(&mut stream, &key, Direction::Down, send_seq, &body).unwrap();
                send_seq += 1;
            }
            seen
        });
        (format!("127.0.0.1:{}", addr.port()), handle)
    }

    #[test]
    fn full_handshake_and_sql_roundtrip() {
        let (addr, server) = spawn_fake_nebula("s3cret");
        let mut c = NebulaClient::connect(&addr, "s3cret", Duration::from_secs(5)).unwrap();
        assert_eq!(c.user(), "admin");

        assert!(c.ping().unwrap());
        let resp = c
            .sql("INSERT INTO memories (content) VALUES ('x')")
            .unwrap();
        assert_eq!(resp.affected, 1);
        assert!(resp.message.unwrap().contains("inserted"));

        let (cols, rows) = c.query("SELECT id, content FROM memories").unwrap();
        assert_eq!(cols, vec!["id", "content"]);
        assert_eq!(rows, vec![vec!["1".to_string(), "示例内容".to_string()]]);

        c.close().unwrap();
        let seen = server.join().unwrap();
        // 服务端只记录 SQL 语句：ping / close 各有独立请求类型，不算 SQL
        assert_eq!(seen.len(), 2);
        assert!(seen[0].contains("INSERT"));
        assert!(seen[1].contains("SELECT"));
    }

    #[test]
    fn sql_error_keeps_connection_usable() {
        let (addr, server) = spawn_fake_nebula("pw");
        let mut c = NebulaClient::connect(&addr, "pw", Duration::from_secs(5)).unwrap();
        let err = c.sql("SELECT * FROM boom").unwrap_err();
        assert!(err.to_string().contains("unknown database"));
        // 连接仍然可用
        assert!(c.ping().unwrap());
        let _ = c.close();
        let _ = server.join();
    }

    #[test]
    fn wrong_password_is_rejected() {
        let (addr, server) = spawn_fake_nebula("right");
        let err = NebulaClient::connect(&addr, "wrong", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, MemoryError::Auth(_)), "got {err:?}");
        assert!(err.to_string().contains("拒绝了用户名或密码"));
        let _ = server.join();
    }

    #[test]
    fn tampered_frame_fails_authentication() {
        let key = [5u8; KEY_LEN];
        let blob = seal(&key, b"{\"type\":\"ping\"}", &frame_aad(Direction::Up, 0));
        // 换方向、换序号都必须失配
        assert!(open(&key, &blob, &frame_aad(Direction::Down, 0)).is_err());
        assert!(open(&key, &blob, &frame_aad(Direction::Up, 1)).is_err());
        assert!(open(&key, &blob, &frame_aad(Direction::Up, 0)).is_ok());
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            let (got, used) = take_varint(&out).unwrap();
            assert_eq!(got, v);
            assert_eq!(used, out.len());
        }
    }

    #[test]
    fn varint_rejects_truncated_input() {
        assert!(take_varint(&[0x80, 0x80]).is_err());
        assert!(take_varint(&[]).is_err());
    }

    #[test]
    fn identity_length_is_validated() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let mut s = TcpStream::connect(addr).unwrap();
        assert!(write_identity(&mut s, "").is_err());
        assert!(write_identity(&mut s, &"x".repeat(65)).is_err());
        assert!(write_identity(&mut s, "admin").is_ok());
        let _ = h.join();
    }

    #[test]
    fn response_helpers() {
        let r = Response {
            ok: true,
            columns: Some(vec!["id".into(), "content".into()]),
            rows: Some(vec![vec!["7".into(), "内容".into()]]),
            ..Default::default()
        };
        assert_eq!(r.cell(0, "content"), Some("内容"));
        assert_eq!(r.cell(0, "nope"), None);
        assert_eq!(r.cell(5, "id"), None);
        assert!(r.clone().into_result().is_ok());

        let bad = Response {
            ok: false,
            error: Some("bad".into()),
            ..Default::default()
        };
        assert!(bad.into_result().unwrap_err().to_string().contains("bad"));
    }
}
