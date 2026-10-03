//! MightBe 有线协议（客户端侧编解码）。
//!
//! MightBe 的协议是**行分隔文本**，比 Nebula 的二进制加密帧简单得多：
//!
//! ```text
//! 请求：<语句>;\n
//!
//! 响应：OK <col1>|<col2>|…        结果集列名（无列名时仅 "OK"）
//!       <v1>|<v2>|…              数据行，任意多行
//!       INFO <自由文本>           可选，ACK / 长任务说明
//!       END (rows=N, ms=X)       终止帧
//!
//!       ERR <code> <message>     错误响应，单帧
//! ```
//!
//! 与 `mightbe-cli` 的编码端保持独立实现——只依赖协议，不依赖对方的 crate。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::error::{AssocError, Result};

/// 服务端回复。
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// 带列名的结果集。
    Rows {
        cols: Vec<String>,
        rows: Vec<Vec<String>>,
        ms: u128,
    },
    /// 写操作回执。
    Ack { info: Vec<String>, ms: u128 },
    /// 错误。
    Error { code: u16, message: String },
}

impl Reply {
    /// 耗时（毫秒）。
    pub fn ms(&self) -> u128 {
        match self {
            Reply::Rows { ms, .. } | Reply::Ack { ms, .. } => *ms,
            Reply::Error { .. } => 0,
        }
    }

    /// 结果集（非结果集返回空）。
    pub fn into_rows(self) -> (Vec<String>, Vec<Vec<String>>) {
        match self {
            Reply::Rows { cols, rows, .. } => (cols, rows),
            _ => (Vec::new(), Vec::new()),
        }
    }

    /// 转成 `Result`。
    pub fn into_result(self) -> Result<Self> {
        match self {
            Reply::Error { code, message } => Err(AssocError::Server { code, message }),
            ok => Ok(ok),
        }
    }
}

/// 发送一条语句。`;` 是协议的一部分，缺失时自动补上。
pub fn write_request(w: &mut impl Write, stmt: &str) -> std::io::Result<()> {
    let trimmed = stmt.trim_end();
    let body = if trimmed.ends_with(';') {
        trimmed.to_string()
    } else {
        format!("{trimmed};")
    };
    // 语句里若混入了换行会破坏行协议，压成空格
    let body = body.replace(['\n', '\r'], " ");
    writeln!(w, "{body}")?;
    w.flush()
}

/// 读一个完整响应帧。连接正常关闭且无数据时返回 `None`。
pub fn read_reply(r: &mut impl BufRead) -> Result<Option<Reply>> {
    let mut head = String::new();
    let n = r
        .read_line(&mut head)
        .map_err(|e| AssocError::Io(format!("读响应首行失败：{e}")))?;
    if n == 0 {
        return Ok(None);
    }
    let head = head.trim_end_matches(['\r', '\n']).to_string();

    if let Some(rest) = head.strip_prefix("ERR") {
        return Ok(Some(parse_error(rest)?));
    }
    let Some(cols_part) = head.strip_prefix("OK") else {
        return Err(AssocError::Protocol(format!(
            "期望 OK / ERR 帧，实际收到 {head:?}"
        )));
    };
    let cols_part = cols_part.trim_start();

    // 官方写回执：`OK affected=N` / `OK job=<id>`（见 mightbe-server
    // `Response::to_wire` 的 Affected/Job 变体）。它们不是列名，必须
    // 识别为 Ack，否则会被误读成名为 "affected=7" 的结果集。
    if cols_part.starts_with("affected=") || cols_part.starts_with("job=") {
        let mut info = Vec::new();
        let ms;
        loop {
            let mut line = String::new();
            let n = r
                .read_line(&mut line)
                .map_err(|e| AssocError::Io(format!("读响应帧失败：{e}")))?;
            if n == 0 {
                return Err(AssocError::Protocol("连接在 END 帧之前关闭".into()));
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if let Some(tail) = line.strip_prefix("END") {
                ms = parse_end(tail);
                break;
            }
            if let Some(rest) = line.strip_prefix("INFO") {
                info.push(rest.trim().to_string());
                continue;
            }
        }
        return Ok(Some(Reply::Ack { info, ms }));
    }

    let mut cols = split_cells(cols_part.trim_start());
    if cols.len() == 1 && cols[0].is_empty() {
        cols.clear();
    }

    let mut info: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let ms;
    loop {
        let mut line = String::new();
        let n = r
            .read_line(&mut line)
            .map_err(|e| AssocError::Io(format!("读响应帧失败：{e}")))?;
        if n == 0 {
            return Err(AssocError::Protocol("连接在 END 帧之前关闭".into()));
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();

        if let Some(tail) = line.strip_prefix("END") {
            ms = parse_end(tail);
            break;
        }
        if let Some(rest) = line.strip_prefix("INFO") {
            info.push(rest.trim().to_string());
            continue;
        }
        rows.push(split_cells(&line));
    }

    // 协议只有两种帧：`OK` 后面跟着列名 → 结果集；`OK` 后面什么都没有 → 写操作回执。
    // 所以「有没有列名头」就是唯一的判别依据（没有列名的裸数据行是没有意义的，忽略即可）。
    if cols.is_empty() {
        Ok(Some(Reply::Ack { info, ms }))
    } else {
        Ok(Some(Reply::Rows { cols, rows, ms }))
    }
}

fn parse_error(rest: &str) -> Result<Reply> {
    let rest = rest.trim();
    let mut it = rest.splitn(2, ' ');
    let code = it.next().unwrap_or("").trim().parse::<u16>().unwrap_or(0);
    let message = it.next().unwrap_or("").trim().to_string();
    Ok(Reply::Error { code, message })
}

fn parse_end(tail: &str) -> u128 {
    // `(rows=3, ms=12)` / `(ms=12)` / 空
    let tail = tail.trim().trim_start_matches('(').trim_end_matches(')');
    for part in tail.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("ms=") {
            if let Ok(ms) = v.trim().parse::<u128>() {
                return ms;
            }
        }
    }
    0
}

/// 按 `|` 切分单元格。MightBe 用裸 `|` 分隔，无引号转义机制。
fn split_cells(line: &str) -> Vec<String> {
    if line.is_empty() {
        return Vec::new();
    }
    line.split('|').map(|c| c.trim().to_string()).collect()
}

/// MightBe TCP 客户端。
pub struct MightBeClient {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    addr: String,
}

impl std::fmt::Debug for MightBeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MightBeClient")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl MightBeClient {
    /// 连接。
    pub fn connect(addr: &str, timeout: Duration) -> Result<Self> {
        let stream = TcpStream::connect(addr)
            .map_err(|e| AssocError::Io(format!("无法连接 MightBe 服务 {addr}：{e}")))?;
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| AssocError::Io(format!("复制套接字失败：{e}")))?,
        );
        Ok(MightBeClient {
            stream,
            reader,
            addr: addr.to_string(),
        })
    }

    /// 服务地址。
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// 执行一条语句。
    pub fn query(&mut self, stmt: &str) -> Result<Reply> {
        write_request(&mut self.stream, stmt)
            .map_err(|e| AssocError::Io(format!("发送语句失败：{e}")))?;
        match read_reply(&mut self.reader)? {
            Some(r) => r.into_result(),
            None => Err(AssocError::Protocol("服务端关闭了连接".into())),
        }
    }

    /// 执行一条语句并取结果集。
    pub fn rows(&mut self, stmt: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        Ok(self.query(stmt)?.into_rows())
    }

    /// 连通性探测：`SHOW STATUS;` 什么都不坏，能拿到任何响应即算通。
    pub fn ping(&mut self) -> bool {
        // 用一条几乎不可能报致命错的语句探活
        self.query("SHOW STATUS").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    fn reply_from(text: &str) -> Result<Option<Reply>> {
        let mut c = Cursor::new(text.as_bytes().to_vec());
        read_reply(&mut c)
    }

    #[test]
    fn parses_result_set() {
        let r = reply_from(
            "OK word|score|confidence\n所有权|0.87|0.91\n借用|0.62|0.55\nEND (rows=2, ms=7)\n",
        )
        .unwrap()
        .unwrap();
        let (cols, rows) = r.into_rows();
        assert_eq!(cols, vec!["word", "score", "confidence"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "所有权");
        assert_eq!(rows[1][1], "0.62");
    }

    #[test]
    fn parses_ack_with_info() {
        let r = reply_from("OK\nINFO inserted 1 row\nEND (ms=3)\n")
            .unwrap()
            .unwrap();
        match r {
            Reply::Ack { info, ms } => {
                assert_eq!(info, vec!["inserted 1 row".to_string()]);
                assert_eq!(ms, 3);
            }
            other => panic!("期望 Ack，得到 {other:?}"),
        }
    }

    #[test]
    fn parses_error_frame() {
        let r = reply_from("ERR 1146 unknown network 'doc_rnn'\n")
            .unwrap()
            .unwrap();
        match r {
            Reply::Error { code, message } => {
                assert_eq!(code, 1146);
                assert_eq!(message, "unknown network 'doc_rnn'");
            }
            other => panic!("期望 Error，得到 {other:?}"),
        }
        let err = reply_from("ERR 1146 unknown network 'x'\n")
            .unwrap()
            .unwrap()
            .into_result()
            .unwrap_err();
        assert!(err.to_string().contains("1146"));
        assert!(err.to_string().contains("unknown network"));
    }

    #[test]
    fn empty_result_set_has_no_columns() {
        let r = reply_from("OK\nEND (rows=0, ms=1)\n").unwrap().unwrap();
        let (cols, rows) = r.into_rows();
        assert!(cols.is_empty());
        assert!(rows.is_empty());
    }

    #[test]
    fn rejects_unexpected_frame() {
        let e = reply_from("WAT something\n").unwrap_err();
        assert!(e.to_string().contains("期望 OK / ERR"));
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let e = reply_from("OK a|b\n1|2\n").unwrap_err();
        assert!(e.to_string().contains("END 帧之前关闭"));
    }

    #[test]
    fn eof_returns_none() {
        assert!(reply_from("").unwrap().is_none());
    }

    #[test]
    fn request_appends_semicolon_and_flattens_newlines() {
        let mut out: Vec<u8> = Vec::new();
        write_request(&mut out, "SELECT * FROM x").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "SELECT * FROM x;\n");

        let mut out: Vec<u8> = Vec::new();
        write_request(&mut out, "SELECT 1;\nDROP").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "SELECT 1; DROP;\n");
    }

    #[test]
    fn end_frame_without_ms() {
        assert_eq!(parse_end(" (rows=1)"), 0);
        assert_eq!(parse_end(" (rows=1, ms=42)"), 42);
        assert_eq!(parse_end(""), 0);
    }

    #[test]
    fn round_trip_against_a_scripted_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                assert!(
                    line.trim_end().ends_with(';'),
                    "语句必须以 ; 结尾：{line:?}"
                );
                if line.contains("ASSOCIATE") {
                    s.write_all("OK word|score\n照片|0.9\nEND (rows=1, ms=2)\n".as_bytes())
                        .unwrap();
                } else {
                    s.write_all(b"ERR 1064 syntax error\n").unwrap();
                }
            }
        });

        let mut c = MightBeClient::connect(
            &format!("127.0.0.1:{}", addr.port()),
            Duration::from_secs(3),
        )
        .unwrap();
        let (cols, rows) = c
            .rows("SELECT word, score FROM ASSOCIATE(nt, 'x')")
            .unwrap();
        assert_eq!(cols, vec!["word", "score"]);
        assert_eq!(rows[0][0], "照片");

        let err = c.query("BAD SQL").unwrap_err();
        assert!(err.to_string().contains("syntax error"));

        drop(c);
        let _ = h.join();
    }
}
