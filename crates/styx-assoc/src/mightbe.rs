//! [`AssocPort`] 的 MightBe 实现。
//!
//! 对接 [MightBe](https://github.com/yxpil/MightBe) 的 TCP 文本协议，
//! 用它的联想检索与可解释推理能力给角色提供"发散思维"。
//!
//! # 为什么查询模板是配置项
//!
//! MightBe 的方言仍在演进（其 README 明确标注 ES 阶段），而"联想"在 Styx 里
//! 是一个**可替换的能力**而不是硬编码依赖。所以这里把语句做成模板：
//!
//! ```text
//! 默认：SELECT word, score FROM ASSOCIATE({net}, {seed}) LIMIT {limit}
//! ```
//!
//! 可用占位符：`{net}`（网络名）、`{seed}`（种子词）、`{limit}`（条数）。
//! Nebula/MightBe 换语法时，改配置即可，不用改代码。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use styx_core::ports::{AssocPort, Association};

use crate::error::{AssocError, Result};
use styx_core::error::Result as StyxResult;
use crate::wire::MightBeClient;

/// MightBe 连接配置。
#[derive(Debug, Clone)]
pub struct MightBeConfig {
    /// 服务地址，例如 `127.0.0.1:9527`。
    pub addr: String,
    /// 联想使用的网络名。
    pub network: String,
    /// 联想语句模板。
    pub query_template: String,
    /// 可选：取证据的语句模板（留空则不打证据）。
    pub evidence_template: Option<String>,
    /// 超时。
    pub timeout: Duration,
    /// 出错时是否自动重连一次。
    pub auto_reconnect: bool,
}

impl Default for MightBeConfig {
    fn default() -> Self {
        MightBeConfig {
            addr: "127.0.0.1:9527".into(),
            network: "doc_rnn".into(),
            query_template: "SELECT word, score FROM ASSOCIATE({net}, {seed}) LIMIT {limit}"
                .into(),
            evidence_template: None,
            timeout: Duration::from_secs(10),
            auto_reconnect: true,
        }
    }
}

impl MightBeConfig {
    /// 从环境变量补齐：`MIGHTBE_ADDR` / `MIGHTBE_NETWORK`。
    pub fn with_env(mut self) -> Self {
        if let Ok(a) = std::env::var("MIGHTBE_ADDR") {
            if !a.is_empty() && self.addr == "127.0.0.1:9527" {
                self.addr = a;
            }
        }
        if let Ok(n) = std::env::var("MIGHTBE_NETWORK") {
            if !n.is_empty() && self.network == "doc_rnn" {
                self.network = n;
            }
        }
        self
    }

    /// 渲染联想语句。
    pub fn render_query(&self, seed: &str, limit: usize) -> String {
        self.query_template
            .replace("{net}", &self.network)
            .replace("{seed}", &escape_quoted(seed))
            .replace("{limit}", &limit.max(1).to_string())
    }
}

/// 把种子词包成 SQL 字符串字面量。
///
/// 注入的经典手法是「闭合引号 → 追加语句 → 注释掉尾巴」。所以这里的做法是
/// **中和**：引号、分号、换行一律替换成空格，再把连续空白压成一个空格。
/// 刻意不去黑名单匹配 `DROP` / `NETWORK` 这类关键词——黑名单永远会漏，
/// 而只要能保证种子整体被关在一对单引号里，里面的文本就是惰性的。
fn escape_quoted(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if matches!(c, '\'' | ';' | '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let squashed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("'{squashed}'")
}

/// 基于 MightBe 的联想后端。
pub struct MightBeAssoc {
    cfg: MightBeConfig,
    inner: Mutex<Option<MightBeClient>>,
    connected: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl std::fmt::Debug for MightBeAssoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MightBeAssoc")
            .field("addr", &self.cfg.addr)
            .field("network", &self.cfg.network)
            .field("connected", &self.connected.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl MightBeAssoc {
    /// 立即连接。失败时返回 Err，调用方据此降级到共现图实现。
    pub fn connect(cfg: MightBeConfig) -> Result<Self> {
        let cfg = cfg.with_env();
        let client = MightBeClient::connect(&cfg.addr, cfg.timeout)?;
        Ok(MightBeAssoc {
            cfg,
            inner: Mutex::new(Some(client)),
            connected: AtomicBool::new(true),
            last_error: Mutex::new(None),
        })
    }

    /// 配置。
    pub fn config(&self) -> &MightBeConfig {
        &self.cfg
    }

    /// 服务端是否可达。
    pub fn health(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// 最近一次错误。
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|e| e.clone())
    }

    /// 透传一条语句（调试用）。
    pub fn query(&self, stmt: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        self.with_client(|c| c.rows(stmt))
    }

    fn with_client<T>(&self, f: impl Fn(&mut MightBeClient) -> Result<T>) -> Result<T> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| AssocError::Other("MightBe 客户端锁被污染".into()))?;
        if guard.is_none() {
            *guard = Some(MightBeClient::connect(&self.cfg.addr, self.cfg.timeout)?);
        }
        let first = f(guard.as_mut().expect("上面刚确保非空"));
        match first {
            Ok(v) => {
                self.connected.store(true, Ordering::Relaxed);
                self.set_last_error(None);
                Ok(v)
            }
            Err(e) => {
                self.set_last_error(Some(e.to_string()));
                if !self.cfg.auto_reconnect {
                    self.connected.store(false, Ordering::Relaxed);
                    return Err(e);
                }
                *guard = None;
                let client = MightBeClient::connect(&self.cfg.addr, self.cfg.timeout)?;
                *guard = Some(client);
                let retry = f(guard.as_mut().expect("刚写入的客户端"));
                match retry {
                    Ok(v) => {
                        self.connected.store(true, Ordering::Relaxed);
                        self.set_last_error(None);
                        Ok(v)
                    }
                    Err(e2) => {
                        self.connected.store(false, Ordering::Relaxed);
                        self.set_last_error(Some(e2.to_string()));
                        Err(e2)
                    }
                }
            }
        }
    }

    fn set_last_error(&self, msg: Option<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = msg;
        }
    }
}

impl AssocPort for MightBeAssoc {
    fn name(&self) -> &str {
        "mightbe"
    }

    fn health(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn associate(&self, seed: &str, limit: usize) -> StyxResult<Vec<Association>> {
        if seed.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let stmt = self.cfg.render_query(seed, limit);
        let (cols, rows) = self.with_client(|c| c.rows(&stmt))?;
        let mut out = map_rows(&cols, &rows);

        // 可选：为每条联想补证据
        if let Some(tpl) = &self.cfg.evidence_template {
            for a in out.iter_mut() {
                let stmt = tpl
                    .replace("{net}", &self.cfg.network)
                    .replace("{seed}", &escape_quoted(&a.word))
                    .replace("{limit}", "2");
                if let Ok((_c, rows)) = self.with_client(|c| c.rows(&stmt)) {
                    for row in rows.iter().take(2) {
                        let text = row
                            .iter()
                            .find(|cell| !cell.is_empty() && cell.parse::<f32>().is_err())
                            .cloned();
                        if let Some(t) = text {
                            if !a.evidence.contains(&t) {
                                a.evidence.push(t);
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn status(&self) -> String {
        let state = if self.health() { "在线" } else { "离线" };
        match self.last_error() {
            Some(e) => format!(
                "mightbe {}@{} [{}]（最近错误：{}）",
                self.cfg.network, self.cfg.addr, state, e
            ),
            None => format!("mightbe {}@{} [{}]", self.cfg.network, self.cfg.addr, state),
        }
    }
}

/// 结果集 → 联想列表。列名匹配是宽松的。
pub fn map_rows(cols: &[String], rows: &[Vec<String>]) -> Vec<Association> {
    let lower: Vec<String> = cols.iter().map(|c| c.to_lowercase()).collect();
    let idx = |names: &[&str]| -> Option<usize> {
        names.iter().find_map(|n| lower.iter().position(|c| c == n))
    };
    let i_word = idx(&["word", "term", "keyword", "token", "neighbor"]);
    let i_score = idx(&["score", "weight", "sim", "similarity", "pmi", "relevance"]);
    let i_conf = idx(&["confidence", "conf", "prob", "certainty"]);
    let i_ev = idx(&["evidence", "evidence_text", "source", "context", "sentence"]);

    let mut out = Vec::with_capacity(rows.len());
    for (n, row) in rows.iter().enumerate() {
        let get = |i: Option<usize>| i.and_then(|i| row.get(i)).cloned();
        let Some(word) = get(i_word) else {
            // 只有一列时，把第一列当词
            if row.len() == 1 && !row[0].is_empty() {
                out.push(Association::new(
                    row[0].clone(),
                    1.0 / (n as f32 + 1.0),
                ));
            }
            continue;
        };
        if word.trim().is_empty() {
            continue;
        }
        let score = get(i_score)
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or_else(|| 1.0 / (n as f32 + 1.0));
        let confidence = get(i_conf)
            .and_then(|s| s.trim().parse::<f32>().ok())
            .map(normalize_confidence)
            .unwrap_or(1.0);
        let evidence: Vec<String> = get(i_ev)
            .filter(|s| !s.trim().is_empty())
            .map(|s| vec![s])
            .unwrap_or_default();

        out.push(Association {
            word: word.trim().to_string(),
            score,
            evidence,
            confidence,
        });
    }
    // 按分数降序，与 Nebula/MightBe 的返回顺序语义一致
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// MightBe 的 confidence 已经是 0~1；若是百分数则换算。
fn normalize_confidence(c: f32) -> f32 {
    if c > 1.0 {
        (c / 100.0).clamp(0.0, 1.0)
    } else {
        c.clamp(0.0, 1.0)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MightBeConfig {
        MightBeConfig::default()
    }

    #[test]
    fn query_template_renders_placeholders() {
        let sql = cfg().render_query("旧照片", 7);
        assert_eq!(
            sql,
            "SELECT word, score FROM ASSOCIATE(doc_rnn, '旧照片') LIMIT 7"
        );
    }

    #[test]
    fn seed_is_sanitised_against_injection() {
        // 要守住的不变量：种子整体被关在一对单引号里，且无法引入
        // 引号 / 分号 / 换行——而不是去黑名单匹配 "DROP NETWORK"。
        let sql = cfg().render_query("a'; DROP NETWORK x; --\n", 3);
        assert!(!sql.contains('\n'), "got {sql}");
        assert!(!sql.contains(';'), "got {sql}");
        assert_eq!(sql.matches('\'').count(), 2, "got {sql}");
        assert!(sql.contains("'a DROP NETWORK x --'"), "got {sql}");
    }

    #[test]
    fn custom_template_is_honoured() {
        let mut c = cfg();
        c.network = "kb_net".into();
        c.query_template = "ASSOCIATE {net} {seed};".into();
        assert_eq!(c.render_query("甲", 5), "ASSOCIATE kb_net '甲';");
    }

    #[test]
    fn maps_standard_columns() {
        let cols = vec![
            "word".to_string(),
            "score".to_string(),
            "confidence".to_string(),
            "evidence".to_string(),
        ];
        let rows = vec![
            vec![
                "相册".into(),
                "0.87".into(),
                "0.91".into(),
                "那张旧照片被夹在相册的第一页".into(),
            ],
            vec!["照片".into(), "0.62".into(), "0.55".into(), "".into()],
        ];
        let got = map_rows(&cols, &rows);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].word, "相册");
        assert!((got[0].score - 0.87).abs() < 1e-6);
        assert!((got[0].confidence - 0.91).abs() < 1e-6);
        assert_eq!(got[0].evidence.len(), 1);
        // 分数降序
        assert!(got[0].score > got[1].score);
    }

    #[test]
    fn tolerates_renamed_columns() {
        let cols = vec!["term".to_string(), "weight".to_string()];
        let rows = vec![vec!["甲".into(), "3.5".into()]];
        let got = map_rows(&cols, &rows);
        assert_eq!(got[0].word, "甲");
        assert!((got[0].score - 3.5).abs() < 1e-6);
        // 没有 confidence 列时默认满分（不因为缺列就弃判）
        assert_eq!(got[0].confidence, 1.0);
    }

    #[test]
    fn single_column_result_is_accepted() {
        let got = map_rows(&["word".to_string()], &[vec!["甲".into()], vec!["乙".into()]]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].word, "甲");
        assert!(got[0].score > got[1].score);
    }

    #[test]
    fn empty_and_blank_rows_are_skipped() {
        let got = map_rows(
            &["word".to_string(), "score".to_string()],
            &[vec!["".into(), "1".into()], vec!["乙".into(), "0.5".into()]],
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].word, "乙");
    }

    #[test]
    fn percent_confidence_is_normalised() {
        assert!((normalize_confidence(91.0) - 0.91).abs() < 1e-6);
        assert!((normalize_confidence(0.91) - 0.91).abs() < 1e-6);
        assert_eq!(normalize_confidence(500.0), 1.0);
        assert_eq!(normalize_confidence(-1.0), 0.0);
    }

    #[test]
    fn connect_failure_is_reported() {
        let mut c = cfg();
        c.addr = "127.0.0.1:1".into();
        c.timeout = Duration::from_millis(300);
        let err = MightBeAssoc::connect(c).unwrap_err();
        assert!(matches!(err, AssocError::Io(_)), "got {err:?}");
    }

    #[test]
    fn assoc_error_maps_into_styx_error() {
        let e: styx_core::error::StyxError = AssocError::Server {
            code: 1146,
            message: "no net".into(),
        }
        .into();
        assert!(matches!(e, styx_core::error::StyxError::Assoc(_)));
        let e: styx_core::error::StyxError = AssocError::Offline("down".into()).into();
        assert!(matches!(
            e,
            styx_core::error::StyxError::PortUnavailable { port: "assoc", .. }
        ));
    }
}
