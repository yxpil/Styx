//! 内存兜底实现：**BM25 全文检索 + 共现联想**。
//!
//! 这不是一个"打桩（stub）"实现，而是一个真的能用的降级后端：
//!
//! - Nebula 没启动时，角色仍然记得住、想得起，只是记忆不落盘；
//! - 单元测试不必依赖外部服务，因此可以跑得很快、很稳；
//! - 它同时也是**参照实现**——Nebula 的 BM25 与 `RELATED` 语义在这里被
//!   写成几十行可读代码，方便对照调试。
//!
//! 打分公式（标准 BM25，k1=1.2, b=0.75）：
//!
//! ```text
//! score(q, d) = Σ_{t∈q} IDF(t) · tf(t,d)·(k1+1) / (tf(t,d) + k1·(1 − b + b·|d|/avgdl))
//! IDF(t)      = ln(1 + (N − df(t) + 0.5) / (df(t) + 0.5))
//! ```
//!
//! 再乘一个重要度因子 `0.5 + 0.5·importance`，与 Nebula 的"重要度参与排序"一致。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use styx_core::error::{Result as StyxResult, StyxError};
use styx_core::ports::{MemoryNote, MemoryPort, Recalled};
use styx_core::text::tokenize;

/// BM25 参数。
#[derive(Debug, Clone, Copy)]
pub struct Bm25Params {
    /// 词频饱和系数。
    pub k1: f32,
    /// 长度归一化系数。
    pub b: f32,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Bm25Params { k1: 1.2, b: 0.75 }
    }
}

/// 内存记忆库。
pub struct InMemoryMemory {
    inner: Mutex<Inner>,
    params: Bm25Params,
    /// 名称（便于在状态里区分"这是兜底实现"）。
    label: String,
}

struct Inner {
    next_id: u64,
    entries: Vec<Entry>,
    /// 倒排索引：term → 命中该 term 的 entry 下标集合。
    inverted: HashMap<String, HashSet<usize>>,
}

struct Entry {
    id: String,
    note: MemoryNote,
    created_at: i64,
    tf: BTreeMap<String, usize>,
    len: usize,
}

impl Default for InMemoryMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryMemory {
    /// 新建空记忆库。
    pub fn new() -> Self {
        InMemoryMemory {
            inner: Mutex::new(Inner {
                next_id: 0,
                entries: Vec::new(),
                inverted: HashMap::new(),
            }),
            params: Bm25Params::default(),
            label: "memory".into(),
        }
    }

    /// 带自定义 BM25 参数。
    pub fn with_params(params: Bm25Params) -> Self {
        InMemoryMemory {
            params,
            ..Self::new()
        }
    }

    /// 换一个展示名（例如 `"memory(兜底)"`）。
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// 已有条目数。
    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.entries.len()).unwrap_or(0)
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 导出全部条目（用于落盘或迁移到 Nebula）。
    pub fn dump(&self) -> Vec<MemoryNote> {
        self.inner
            .lock()
            .map(|i| i.entries.iter().map(|e| e.note.clone()).collect())
            .unwrap_or_default()
    }
}

impl MemoryPort for InMemoryMemory {
    fn name(&self) -> &str {
        &self.label
    }

    fn health(&self) -> bool {
        true
    }

    fn remember(&self, note: &MemoryNote) -> StyxResult<String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StyxError::Memory("记忆库锁被污染".into()))?;
        inner.next_id += 1;
        let id = inner.next_id.to_string();
        let tokens = tokenize(&note.text);
        let idx = inner.entries.len();
        let mut tf: BTreeMap<String, usize> = BTreeMap::new();
        for t in &tokens {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        for t in tf.keys() {
            inner.inverted.entry(t.clone()).or_default().insert(idx);
        }
        let created_at = styx_core::event::now_millis();
        let len = tokens.len();
        inner.entries.push(Entry {
            id: id.clone(),
            note: note.clone(),
            created_at,
            tf,
            len,
        });
        Ok(id)
    }

    fn recall(&self, query: &str, limit: usize) -> StyxResult<Vec<Recalled>> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| StyxError::Memory("记忆库锁被污染".into()))?;
        if inner.entries.is_empty() {
            return Ok(Vec::new());
        }

        let q_terms: HashSet<String> = tokenize(query).into_iter().collect();
        if q_terms.is_empty() {
            return Ok(Vec::new());
        }

        let n = inner.entries.len() as f32;
        let avgdl = inner.entries.iter().map(|e| e.len).sum::<usize>() as f32 / n.max(1.0);
        let avgdl = if avgdl <= 0.0 { 1.0 } else { avgdl };

        let mut scored: Vec<(f32, usize)> = Vec::new();
        // 只在倒排索引命中的条目上打分 —— 这既是性能也是正确性（未命中的分数为 0）
        let mut candidates: HashSet<usize> = HashSet::new();
        for t in &q_terms {
            if let Some(set) = inner.inverted.get(t) {
                candidates.extend(set.iter().copied());
            }
        }
        for i in candidates {
            let e = &inner.entries[i];
            // 按术语逐个计算 IDF —— 这才是标准 BM25
            let mut score = 0.0f32;
            for t in &q_terms {
                let tf = match e.tf.get(t) {
                    Some(v) => *v as f32,
                    None => continue,
                };
                let df_t = inner
                    .inverted
                    .get(t)
                    .map(|s| s.len() as f32)
                    .unwrap_or(0.0);
                let idf = (1.0 + (n - df_t + 0.5) / (df_t + 0.5)).ln();
                let denom = tf + self.params.k1 * (1.0 - self.params.b + self.params.b * e.len as f32 / avgdl);
                if denom <= 0.0 {
                    continue;
                }
                score += idf * (tf * (self.params.k1 + 1.0)) / denom;
            }
            if score > 0.0 {
                // 重要度参与排序（与 Nebula 的语义一致）
                score *= 0.5 + 0.5 * e.note.importance.clamp(0.0, 1.0);
                scored.push((score, i));
            }
        }

        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                // 同分时新记忆优先
                .then_with(|| inner.entries[b.1].created_at.cmp(&inner.entries[a.1].created_at))
        });

        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(score, i)| to_recalled(&inner.entries[i], score, &self.label))
            .collect())
    }

    fn related(&self, id: &str, limit: usize) -> StyxResult<Vec<Recalled>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| StyxError::Memory("记忆库锁被污染".into()))?;
        let Some(seed_idx) = inner.entries.iter().position(|e| e.id == id) else {
            return Ok(Vec::new());
        };
        let seed = &inner.entries[seed_idx];
        let n = inner.entries.len() as f32;

        // 共现扩展：共享 term 越多、term 越稀有，关联越强
        let mut scores: HashMap<usize, f32> = HashMap::new();
        for t in seed.tf.keys() {
            let Some(set) = inner.inverted.get(t) else {
                continue;
            };
            let df = set.len() as f32;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for &j in set {
                if j == seed_idx {
                    continue;
                }
                *scores.entry(j).or_insert(0.0) += idf;
            }
        }

        let mut ranked: Vec<(f32, usize)> = scores.into_iter().map(|(i, s)| (s, i)).collect();
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(ranked
            .into_iter()
            .take(limit)
            .map(|(score, i)| to_recalled(&inner.entries[i], score, &self.label))
            .collect())
    }

    fn forget(&self, id: &str) -> StyxResult<bool> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StyxError::Memory("记忆库锁被污染".into()))?;
        let Some(pos) = inner.entries.iter().position(|e| e.id == id) else {
            return Ok(false);
        };
        inner.entries.remove(pos);
        // 下标全乱了一位，重建倒排索引（内存实现里条目数不大，够用）。
        // 这里把 `Inner` 的两个字段拆成互不重叠的可变借用，避免"读 entries 的同时写 inverted"。
        let Inner {
            entries, inverted, ..
        } = &mut *inner;
        inverted.clear();
        for (i, e) in entries.iter().enumerate() {
            for t in e.tf.keys() {
                inverted.entry(t.clone()).or_default().insert(i);
            }
        }
        Ok(true)
    }

    fn status(&self) -> String {
        format!("{}（内存兜底，{} 条）", self.label, self.len())
    }
}

fn to_recalled(e: &Entry, score: f32, origin: &str) -> Recalled {
    Recalled {
        id: e.id.clone(),
        text: e.note.text.clone(),
        score,
        importance: e.note.importance,
        tags: e.note.tags.clone(),
        created_at: Some(e.created_at),
        origin: origin.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_with(notes: &[(&str, f32, &[&str])]) -> InMemoryMemory {
        let m = InMemoryMemory::new();
        for (text, imp, tags) in notes {
            m.remember(
                &MemoryNote::new(*text)
                    .with_importance(*imp)
                    .with_tags(tags.iter().map(|t| t.to_string())),
            )
            .unwrap();
        }
        m
    }

    #[test]
    fn tokenize_mixes_cjk_and_ascii() {
        let t = tokenize("Rust 的所有权规则 ownership rules");
        assert!(t.contains(&"所".to_string()));
        assert!(t.contains(&"所有".to_string()));
        assert!(t.contains(&"ownership".to_string()));
        assert!(t.contains(&"rules".to_string()));
        // 单字符 ASCII 与纯数字被丢弃
        let t = tokenize("a 123 bb");
        assert!(!t.contains(&"a".to_string()));
        assert!(!t.contains(&"123".to_string()));
        assert!(t.contains(&"bb".to_string()));
    }

    #[test]
    fn remember_and_recall_ranks_by_relevance() {
        let m = mem_with(&[
            ("陈默三年前借走了一本《海边的卡夫卡》", 0.7, &["陈默"]),
            ("母亲留下了一张旧照片，照片上的人笑着", 0.9, &["母亲"]),
            ("今天天气不错", 0.3, &[]),
        ]);
        let hits = m.recall("旧照片", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits[0].text.contains("旧照片"),
            "最相关的应当是照片那条，实际：{:?}",
            hits[0].text
        );
        // 打分应降序
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
        assert_eq!(hits[0].origin, "memory");
    }

    #[test]
    fn importance_breaks_ties() {
        let m = mem_with(&[
            ("旧照片 甲乙丙", 0.1, &[]),
            ("旧照片 甲乙丙", 0.95, &[]),
        ]);
        let hits = m.recall("旧照片", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].importance > hits[1].importance);
    }

    #[test]
    fn missing_terms_return_nothing() {
        let m = mem_with(&[("只有这一条内容", 0.5, &[])]);
        assert!(m.recall("完全无关的查询词缀", 5).unwrap().is_empty());
        assert!(m.recall("", 5).unwrap().is_empty());
        assert!(m.recall("x", 0).unwrap().is_empty());
    }

    #[test]
    fn related_finds_cooccurring_entries() {
        let m = mem_with(&[
            ("母亲留下了一张旧照片", 0.9, &["母亲"]),
            ("相册里夹着那张照片", 0.8, &["相册"]),
            ("隔壁的老猫在打盹", 0.2, &[]),
        ]);
        let hits = m.related("1", 5).unwrap();
        assert!(!hits.is_empty());
        // 应当匹配到"照片"共现的那条，而不是猫
        assert!(
            hits.iter().all(|h| !h.text.contains("老猫")),
            "不该联想出无关条目：{hits:?}"
        );
        assert!(hits[0].text.contains("照片"));
    }

    #[test]
    fn related_on_unknown_id_is_empty() {
        let m = mem_with(&[("x", 0.5, &[])]);
        assert!(m.related("999", 5).unwrap().is_empty());
    }

    #[test]
    fn forget_removes_and_reindexes() {
        let m = mem_with(&[
            ("第一条 关于照片", 0.5, &[]),
            ("第二条 关于照片", 0.5, &[]),
        ]);
        assert_eq!(m.len(), 2);
        assert!(m.forget("1").unwrap());
        assert_eq!(m.len(), 1);
        assert!(!m.forget("1").unwrap());
        // 删掉一条后，剩下的仍可被检索（倒排索引已重建）
        let hits = m.recall("照片", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "2");
    }

    #[test]
    fn dump_returns_all_notes() {
        let m = mem_with(&[("a 内容", 0.5, &[]), ("b 内容", 0.5, &[])]);
        assert_eq!(m.dump().len(), 2);
        assert!(m.health());
        assert!(m.status().contains("内存兜底"));
    }

    #[test]
    fn long_document_does_not_beat_short_on_length_normalization() {
        // 两条都含"照片"：短的那条应当占优（b=0.75 的长度归一化）
        let long = format!("照片 {}", "无关内容".repeat(200));
        let m = mem_with(&[(&long, 0.5, &[]), ("照片", 0.5, &[])]);
        let hits = m.recall("照片", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "照片", "短文档应当胜出");
    }
}
