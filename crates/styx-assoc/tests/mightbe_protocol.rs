//! MightBe 行协议的真实 socket 端到端测试。
//!
//! MightBe 的服务端（M8 里程碑）尚未落地，`mightbe-server` 目前唯一
//! 已实现的部分是协议渲染器 `Response::to_wire`。这里把那段渲染逻辑
//! **逐字节照抄**（见下方 `to_wire`，来自
//! `MightBe/crates/mightbe-server/src/api.rs`），在真实 TCP socket 上
//! 驱动 Styx 的完整客户端链路：连接 → 发语句（`;` 补齐）→ 解析
//! `OK`/数据行/`INFO`/`END`/`ERR`。
//!
//! 等官方服务端落地后，这些测试应当原样通过——因为桩的每个字节都
//! 来自官方渲染器。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use styx_assoc::mightbe::{map_rows, MightBeAssoc, MightBeConfig};
use styx_core::ports::AssocPort;

// ---------------------------------------------------------------------------
// 以下 `Response` / `to_wire` 照抄自 mightbe-server/src/api.rs（官方渲染器）。
// （`Job` 变体仅为保真而保留，测试未触发。）
// ---------------------------------------------------------------------------

/// 线协议响应：`OK <cols>` + 数据行 + `END (rows=N, ms=X)`，错误为 `ERR <code> <msg>`。
#[allow(dead_code)] // `Job` 变体仅为协议保真而保留，测试未触发
#[derive(Debug, Clone)]
enum Response {
    Rows {
        cols: Vec<String>,
        rows: Vec<Vec<String>>,
        elapsed_ms: u128,
    },
    Affected {
        n: u64,
        info: String,
    },
    Job {
        job_id: String,
        info: String,
    },
    Empty {
        info: String,
    },
    Error {
        code: u16,
        message: String,
    },
}

impl Response {
    /// 渲染为线协议文本（不含结尾换行）。
    fn to_wire(&self) -> String {
        match self {
            Response::Rows {
                cols,
                rows,
                elapsed_ms,
            } => {
                let mut out = String::with_capacity(64 + cols.len() * 16 + rows.len() * 32);
                out.push_str("OK ");
                out.push_str(&cols.join("|"));
                for row in rows {
                    out.push('\n');
                    out.push_str(&row.join("|"));
                }
                out.push_str(&format!("\nEND (rows={}, ms={})", rows.len(), elapsed_ms));
                out
            }
            Response::Affected { n, info } => {
                format!("OK affected={}\nINFO {}\nEND (rows=1, ms=0)", n, info)
            }
            Response::Job { job_id, info } => {
                format!("OK job={}\nINFO {}\nEND (rows=0, ms=0)", job_id, info)
            }
            Response::Empty { info } => format!("OK\nINFO {}\nEND (rows=0, ms=0)", info),
            Response::Error { code, message } => format!("ERR {} {}", code, message),
        }
    }
}

// ---------------------------------------------------------------------------
// 桩服务端
// ---------------------------------------------------------------------------

/// 桩服务端：读一条语句（以 `;` 结尾），用 [`Response::to_wire`] 回帧。
///
/// 每个新连接开一个处理线程——客户端在出错时会断开重连（auto_reconnect），
/// 这与真实服务端行为一致。listener 在空闲 2 秒后自动关闭（测试收集
/// 结果用），避免 join 永久阻塞。
fn serve(responses: mpsc::Receiver<Response>) -> (String, JoinHandle<Vec<String>>) {
    let responses = std::sync::Arc::new(std::sync::Mutex::new(responses));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking 不会失败");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut last_activity = std::time::Instant::now();
        let mut workers = Vec::new();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    last_activity = std::time::Instant::now();
                    let seen = seen.clone();
                    let responses = responses.clone();
                    workers.push(std::thread::spawn(move || {
                        handle_conn(stream, &responses, &seen);
                    }));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // 连接全部结束后（最后一个 worker 已退出）再等 300ms
                    // 没有新连接就收摊；测试里客户端最多重连一次
                    let all_workers_done = workers.iter().all(|w| w.is_finished());
                    if all_workers_done
                        && !seen.lock().unwrap().is_empty()
                        && last_activity.elapsed() > Duration::from_millis(300)
                    {
                        break;
                    }
                    if std::time::Instant::now() > deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        // 不 join worker：它们阻塞在对端的长连接上，等测试进程退出即可。
        // seen 是 Arc，worker 里的引用在进程存续期间始终有效。
        std::mem::forget(workers);
        let collected = seen.lock().unwrap().clone();
        collected
    });
    (addr, handle)
}

/// 处理一个连接：循环「读一条语句 → 回一帧」，直到对端关闭。
type SharedReceiver = std::sync::Arc<std::sync::Mutex<mpsc::Receiver<Response>>>;

fn handle_conn(
    stream: std::net::TcpStream,
    responses: &SharedReceiver,
    seen: &std::sync::Mutex<Vec<String>>,
) {
    stream.set_nonblocking(false).ok();
    let mut reader = BufReader::new(stream.try_clone().expect("clone socket 不会失败"));
    let mut writer = stream;
    loop {
        // 读一条语句：MightBe 请求以 `;` 结尾
        let mut stmt = String::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return, // 对端关闭
                Ok(_) => {}
            }
            let trimmed = line.trim_end();
            stmt.push_str(trimmed);
            if trimmed.ends_with(';') {
                break;
            }
            stmt.push(' ');
        }
        seen.lock().unwrap().push(stmt.trim_end().to_string());

        let resp = match responses.lock().unwrap().try_recv() {
            Ok(r) => r,
            Err(_) => Response::Empty {
                info: "no scripted response".into(),
            },
        };
        if writer
            .write_all(resp.to_wire().as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .and_then(|_| writer.flush())
            .is_err()
        {
            return;
        }
    }
}

fn assoc_at(addr: &str) -> MightBeAssoc {
    let cfg = MightBeConfig {
        addr: addr.to_string(),
        network: "doc_rnn".into(),
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    MightBeAssoc::connect(cfg).unwrap()
}

// ---------------------------------------------------------------------------
// 端到端测试
// ---------------------------------------------------------------------------

#[test]
fn associate_round_trip_over_real_socket() {
    let (tx, rx) = mpsc::channel();
    let (addr, server) = serve(rx);

    // 官方 Rows 渲染：OK word|score + 数据行 + END
    tx.send(Response::Rows {
        cols: vec!["word".into(), "score".into()],
        rows: vec![
            vec!["相册".into(), "2.41".into()],
            vec!["旧照片".into(), "1.87".into()],
            vec!["母亲".into(), "1.02".into()],
        ],
        elapsed_ms: 3,
    })
    .unwrap();

    let assoc = assoc_at(&addr);
    let hits = assoc.associate("照片", 5).unwrap();
    let words: Vec<&str> = hits.iter().map(|h| h.word.as_str()).collect();
    assert_eq!(words, vec!["相册", "旧照片", "母亲"]);
    assert!((hits[0].score - 2.41).abs() < 1e-5);
    // 服务端没给 confidence 列 → 缺省 1.0（有该列时会被归一化到 [0,1]）
    assert!(hits.iter().all(|h| (0.0..=1.0).contains(&h.confidence)));

    let stmt = &server.join().unwrap()[0];
    assert_eq!(
        stmt,
        "SELECT word, score FROM ASSOCIATE(doc_rnn, '照片') LIMIT 5;"
    );
}

#[test]
fn evidence_template_adds_text_from_extra_queries() {
    let (tx, rx) = mpsc::channel();
    let (addr, server) = serve(rx);

    tx.send(Response::Rows {
        cols: vec!["word".into(), "score".into()],
        rows: vec![vec!["相册".into(), "2.4".into()]],
        elapsed_ms: 1,
    })
    .unwrap();
    // 证据查询：返回 text 列
    tx.send(Response::Rows {
        cols: vec!["text".into()],
        rows: vec![vec!["她把相册放回抽屉".into()]],
        elapsed_ms: 1,
    })
    .unwrap();

    let cfg = MightBeConfig {
        addr: addr.clone(),
        evidence_template: Some("SELECT text FROM EVIDENCE({net}, {seed}) LIMIT {limit}".into()),
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let assoc = MightBeAssoc::connect(cfg).unwrap();
    let hits = assoc.associate("照片", 3).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].evidence, vec!["她把相册放回抽屉".to_string()]);

    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1],
        "SELECT text FROM EVIDENCE(doc_rnn, '相册') LIMIT 2;"
    );
}

#[test]
fn err_frame_propagates_as_failure() {
    let (tx, rx) = mpsc::channel();
    let (addr, server) = serve(rx);

    // 官方 Error 渲染：ERR <code> <message>。
    // 发两条：第一条触发错误后客户端 auto_reconnect 重试一次，重试也必须收到 ERR。
    for _ in 0..2 {
        tx.send(Response::Error {
            code: 3001,
            message: "network not found: doc_rnn".into(),
        })
        .unwrap();
    }

    let assoc = assoc_at(&addr);
    let err = assoc.associate("照片", 5).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("3001") || msg.contains("network not found"),
        "{msg}"
    );

    server.join().unwrap();
}

#[test]
fn affected_variant_is_recognised_as_ack() {
    let (tx, rx) = mpsc::channel();
    let (addr, server) = serve(rx);

    // 官方 Affected 渲染：OK affected=N + INFO + END (rows=1)
    tx.send(Response::Affected {
        n: 7,
        info: "7 associations recorded".into(),
    })
    .unwrap();

    let assoc = assoc_at(&addr);
    // 一个纯写语句：无列名 → 解析器应当识别为 Ack（cols 为空），而不是错误
    let (cols, rows) = assoc.query("LEARN doc_rnn FROM '照片，相册';").unwrap();
    assert!(cols.is_empty(), "Affected 响应不应当解析出列名：{cols:?}");
    assert!(rows.is_empty());

    server.join().unwrap();
}

#[test]
fn map_rows_tolerates_column_renaming() {
    // MightBe 的列名是服务端决定的；客户端按宽松别名匹配
    let rows = map_rows(
        &["neighbor".into(), "pmi".into()],
        &[vec!["相册".into(), "0.9".into()]],
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].word, "相册");
    assert!((rows[0].score - 0.9).abs() < 1e-5);
}

/// 静态检查：连接失败要给出可读的错误（而不是 panic 或吞掉）。
#[test]
fn unreachable_server_fails_with_clear_error() {
    let err = MightBeAssoc::connect(MightBeConfig {
        addr: "127.0.0.1:1".into(),
        timeout: Duration::from_secs(2),
        auto_reconnect: false,
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.to_string().contains("127.0.0.1:1"), "{err}");
}
