//! 内存兜底联想：[共现图](https://en.wikipedia.org/wiki/Co-occurrence_network)
//! + PMI + 多跳扩展 + 置信度 + 弃判。
//!
//! 语义上刻意与 MightBe 的"联想检索 + 可解释推理"对齐，只是把神经网络换成了
//! 共现统计。三件事因此得以保留：
//!
//! 1. **每条联想都带证据**——来自哪句话；
//! 2. **每条联想都带置信度**——共现支撑越多、PMI 越高，越可信；
//! 3. **弃判（abstain）**——置信度低于阈值时返回空，让角色"想不起来"
//!    而不是硬编。这比编造一个联想安全得多。
//!
//! ```text
//! PMI(a,b) = ln( P(a,b) / (P(a)·P(b)) )
//!
//! score(a,b) = max(0, PMI) · ln(1 + support(a,b))
//! confidence = (1 − e^(−support/4)) · (1 − e^(−score/2))
//! ```
//!
//! # 已知局限（刻意的取舍）
//!
//! 中文没有词边界，`tokenize` 只能给出 1-gram 与 2-gram。于是图里的"词项"既
//! 有真词（`相册`、`抽屉`），也有碎片（`了一`、`张旧`），而且高频单字天然
//! 连边多、路径权重高。结果是：**联想结果的排序会混入碎片**，种子越短越明显。
//!
//! 这没有用启发式去掩盖（比如"优先多字"），因为那只是把噪音换一种形态。
//! 兜底图保证的是**可追溯**：每条联想都能指回语料原句，且带置信度供上层
//! 过滤（见 `styx-core::prompt` 的低置信度过滤）。要真正干净的词，接 MightBe。
//!
//! 同理，`window` 的单位是 token 而不是字——`tokenize` 会让 token 数约为字数的
//! 两倍，所以窗口取 8 实际上只覆盖原文里四五个字。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use styx_core::ports::{AssocPort, Association};
use styx_core::text::{summarize, tokenize};

use styx_core::error::Result as StyxResult;

/// 共现图参数。
#[derive(Debug, Clone, Copy)]
pub struct GraphParams {
    /// 共现窗口（以 token 计）。
    pub window: usize,
    /// 最低共现支撑次数，低于它不算数。
    pub min_support: f32,
    /// 置信度低于此值的联想被丢弃（**弃判**）。
    pub abstain_below: f32,
    /// 最大跳数。
    pub hops: usize,
    /// 每多一跳的权重衰减。
    pub hop_decay: f32,
    /// 每跳最多保留多少个候选，防止图大时组合爆炸。
    pub beam: usize,
    /// 每条边最多保存几条证据。
    pub max_evidence: usize,
}

impl Default for GraphParams {
    fn default() -> Self {
        GraphParams {
            // 注意 `tokenize` 对中文会同时产出 1-gram 与 2-gram，token 数约为
            // 字数的两倍。所以这里的 8 换算到原文里其实只覆盖四五个字——
            // 「旧书店 柜台 抽屉」这种紧邻关系才算得上共现。
            window: 8,
            min_support: 1.0,
            abstain_below: 0.18,
            hops: 2,
            hop_decay: 0.55,
            beam: 24,
            max_evidence: 2,
        }
    }
}

/// 共现图。
#[derive(Default)]
struct Graph {
    docs: usize,
    /// 全部 token 数（含重复）。
    total_tokens: f64,
    /// 距离加权的共现质量，只用于诊断展示（PMI 的分母见 `total_sup`）。
    total_pairs: f64,
    /// 被计数的词对总数——每对算 1 次，与距离无关，这是 PMI 的分母。
    total_sup: f64,
    freq: HashMap<String, f64>,
    /// a → b → 共现强度（按距离加权，越近越强）
    adj: HashMap<String, HashMap<String, f64>>,
    /// a → b → 共现**次数**（与距离无关）
    sup: HashMap<String, HashMap<String, f64>>,
    /// 无序对 → 证据片段
    ev: HashMap<(String, String), Vec<String>>,
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("docs", &self.docs)
            .field("tokens", &self.freq.len())
            .field("pairs", &self.total_pairs)
            .finish()
    }
}

/// 内存联想后端。
#[derive(Debug)]
pub struct InMemoryAssoc {
    graph: Mutex<Graph>,
    params: GraphParams,
    label: String,
}

impl Default for InMemoryAssoc {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryAssoc {
    /// 空图。
    pub fn new() -> Self {
        InMemoryAssoc {
            graph: Mutex::new(Graph::default()),
            params: GraphParams::default(),
            label: "assoc(memory)".into(),
        }
    }

    /// 自定义参数。
    pub fn with_params(params: GraphParams) -> Self {
        InMemoryAssoc {
            params,
            ..Self::new()
        }
    }

    /// 换展示名。
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// 参数。
    pub fn params(&self) -> GraphParams {
        self.params
    }

    /// 喂一段语料进图。
    ///
    /// 这是兜底实现"变聪明"的唯一途径：把角色的记忆、场景、对白
    /// 不断喂进来，共现图就会长成这个角色的语义世界。
    pub fn observe(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let tokens = tokenize(text);
        if tokens.len() < 2 {
            return;
        }
        let Ok(mut g) = self.graph.lock() else {
            return;
        };
        self.observe_inner(&mut g, &tokens, text);
    }

    /// 批量喂入。
    pub fn observe_many<I, S>(&self, texts: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for t in texts {
            self.observe(t.as_ref());
        }
    }

    fn observe_inner(&self, g: &mut Graph, tokens: &[String], text: &str) {
        g.docs += 1;
        let snippet = summarize(text, 100);

        let mut seen_in_doc: HashSet<&str> = HashSet::new();
        for t in tokens {
            *g.freq.entry(t.clone()).or_insert(0.0) += 1.0;
            g.total_tokens += 1.0;
            seen_in_doc.insert(t.as_str());
        }

        let w = self.params.window.max(1);
        for i in 0..tokens.len() {
            let hi = (i + w + 1).min(tokens.len());
            for j in (i + 1)..hi {
                let a = &tokens[i];
                let b = &tokens[j];
                if a == b {
                    continue;
                }
                let weight = 1.0 / (j - i) as f64;
                g.total_pairs += weight;
                g.total_sup += 1.0;

                *g.adj
                    .entry(a.clone())
                    .or_default()
                    .entry(b.clone())
                    .or_insert(0.0) += weight;
                *g.adj
                    .entry(b.clone())
                    .or_default()
                    .entry(a.clone())
                    .or_insert(0.0) += weight;

                let key = if a < b {
                    (a.clone(), b.clone())
                } else {
                    (b.clone(), a.clone())
                };
                *g.sup
                    .entry(key.0.clone())
                    .or_default()
                    .entry(key.1.clone())
                    .or_insert(0.0) += 1.0;
                let evs = g.ev.entry(key).or_default();
                if evs.len() < self.params.max_evidence && !evs.contains(&snippet) {
                    evs.push(snippet.clone());
                }
            }
        }
    }

    /// 语料规模：`(文档数, 词项数)`。
    pub fn size(&self) -> (usize, usize) {
        self.graph
            .lock()
            .map(|g| (g.docs, g.freq.len()))
            .unwrap_or((0, 0))
    }

    /// 直接查两个词的共现支撑、关联强度与证据。
    pub fn pair(&self, a: &str, b: &str) -> Option<(f32, f32, Vec<String>)> {
        let g = self.graph.lock().ok()?;
        let (score, support, _conf, ev) = g.edge(a, b, self.params.min_support)?;
        Some((support as f32, score as f32, ev))
    }

    /// 从一段文本出发联想，返回带证据与置信度的结果。
    ///
    /// 这是对 [`Self::associate`] 的可读封装——种子可以是整句。
    pub fn explain(&self, seed: &str, limit: usize) -> Vec<Association> {
        let mut seeds = seed_tokens(seed);
        if seeds.is_empty() {
            let s = seed.trim().to_lowercase();
            if !s.is_empty() {
                seeds.push(s);
            }
        }
        self.expand_from(&seeds, limit)
    }

    fn expand_from(&self, seeds: &[String], limit: usize) -> Vec<Association> {
        let Ok(g) = self.graph.lock() else {
            return Vec::new();
        };
        if seeds.is_empty() || g.freq.is_empty() {
            return Vec::new();
        }

        /// 累积的候选信息。
        struct Cand {
            weight: f64,
            best_support: f64,
            best_pmi: f64,
            evidence: Vec<String>,
            hops: usize,
        }

        let mut cands: HashMap<String, Cand> = HashMap::new();
        let mut visited: HashSet<String> = seeds.iter().cloned().collect();

        // 第 0 跳：种子词（路径权重 1.0）
        let mut frontier: Vec<(String, f64)> = seeds.iter().map(|s| (s.clone(), 1.0)).collect();

        for hop in 1..=self.params.hops.max(1) {
            let mut next: Vec<(String, f64)> = Vec::new();
            for (token, path_w) in &frontier {
                let Some(neighbors) = g.adj.get(token) else {
                    continue;
                };
                // 邻居按共现强度排序，只取前 beam 个
                let mut ns: Vec<(&String, &f64)> = neighbors.iter().collect();
                ns.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
                ns.truncate(self.params.beam);

                for (nb, _w) in ns {
                    if visited.contains(nb) {
                        continue;
                    }
                    let Some((score, support, conf, ev)) =
                        g.edge(token, nb, self.params.min_support)
                    else {
                        continue;
                    };
                    // 置信度太低就不走这条边（弃判，避免几步之后发散成噪音）
                    if conf < self.params.abstain_below {
                        continue;
                    }
                    let decay = (self.params.hop_decay as f64).powi(hop as i32 - 1);
                    let step_w = path_w * decay * score;
                    if step_w <= 0.0 {
                        continue;
                    }
                    let entry = cands.entry(nb.clone()).or_insert(Cand {
                        weight: 0.0,
                        best_support: 0.0,
                        best_pmi: 0.0,
                        evidence: Vec::new(),
                        hops: hop,
                    });
                    entry.weight += step_w;
                    entry.best_support = entry.best_support.max(support);
                    entry.best_pmi = entry.best_pmi.max(score);
                    entry.hops = entry.hops.min(hop);
                    for e in ev {
                        if entry.evidence.len() < self.params.max_evidence
                            && !entry.evidence.contains(&e)
                        {
                            entry.evidence.push(e);
                        }
                    }
                    next.push((nb.clone(), step_w));
                }
            }
            if next.is_empty() {
                break;
            }
            // beam 搜索：只保留最有希望的分支继续走
            next.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            next.truncate(self.params.beam);
            for (t, _) in &next {
                visited.insert(t.clone());
            }
            frontier = next;
        }

        let mut out: Vec<(f64, Association, usize)> = cands
            .into_iter()
            .filter(|(_, c)| c.weight > 0.0)
            .map(|(word, c)| {
                let conf = confidence(c.best_support, c.best_pmi);
                (
                    c.weight,
                    Association {
                        word,
                        score: c.weight as f32,
                        evidence: c.evidence,
                        confidence: conf,
                    },
                    c.hops,
                )
            })
            // 弃判：置信度不足的一律不出现在结果里
            .filter(|(_, a, _)| a.confidence >= self.params.abstain_below)
            .collect();
        // 关联强度优先；同样强时跳数少的优先（更直接、更容易解释）
        out.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.2.cmp(&b.2))
        });
        out.into_iter()
            .take(limit.max(1))
            .map(|(_, a, _)| a)
            .collect()
    }
}

impl Graph {
    /// 计算 `(score, support, confidence, evidence)`。
    fn edge(&self, a: &str, b: &str, min_support: f32) -> Option<(f64, f64, f32, Vec<String>)> {
        // `sup` / `ev` 都是按「字典序无序对」存的（见 `observe_inner`），
        // 所以查表也必须先归一化顺序。否则 `edge("相册", "照片")` 会因为
        // 「照」<「相」而查不到——而 `adj` 是对称存的，于是表现为
        // 「有的边有效、有的边凭空消失」，非常难查。
        let key = if a < b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        };
        let support = *self.sup.get(&key.0)?.get(&key.1)?;
        if support < min_support as f64 {
            return None;
        }
        let w_ab = *self.adj.get(a)?.get(b)?;
        if w_ab <= 0.0 || self.total_sup <= 0.0 || self.total_tokens <= 0.0 {
            return None;
        }
        // PMI 用「共现次数」而不是「距离加权质量」算联合概率。
        // 混用会出问题：边缘概率 p(a) 用的是 token 频次（次数口径），
        // 若联合概率用距离加权口径，两者量纲不一致，会把「隔了四五个 token
        // 但确实反复同现」的词对压成负 PMI 直接被丢掉。距离的作用只体现在
        // `adj`（决定谁是强邻居），不该再悄悄改写 PMI。
        let p_ab = support / self.total_sup;
        let p_a = self.freq.get(a).copied().unwrap_or(0.0) / self.total_tokens;
        let p_b = self.freq.get(b).copied().unwrap_or(0.0) / self.total_tokens;
        if p_a <= 0.0 || p_b <= 0.0 {
            return None;
        }
        let pmi = (p_ab / (p_a * p_b)).ln();
        let score = pmi.max(0.0) * (1.0 + support).ln();
        let conf = confidence(support, score);
        let ev = self.ev.get(&key).cloned().unwrap_or_default();
        Some((score, support, conf, ev))
    }
}

/// 由共现支撑与关联强度折算置信度，落在 `[0, 1)`。
fn confidence(support: f64, score: f64) -> f32 {
    let a = 1.0 - (-support / 4.0).exp();
    let b = 1.0 - (-score / 2.0).exp();
    ((a * b) as f32).clamp(0.0, 1.0)
}

/// 从种子文本里挑出联想起点。
///
/// 中文没有词边界，[`tokenize`] 会同时给出 1-gram 与 2-gram。但如果把 1-gram
/// 也当起点，「相」「册」这类高频单字会变成图上的枢纽——它们连的边多，累积
/// 到的路径权重自然大，于是把真正该被想起来的词（「照片」）挤出结果。
///
/// 所以规则是：**句子里只要有多字词，就只用多字词作起点**；只有当整句都是
/// 一个字一个字的时候（例如种子就是「甲」），才退回单字。
fn seed_tokens(text: &str) -> Vec<String> {
    let all = tokenize(text);
    let multi: Vec<String> = all
        .iter()
        .filter(|t| t.chars().count() >= 2)
        .cloned()
        .collect();
    if multi.is_empty() {
        all
    } else {
        multi
    }
}

impl AssocPort for InMemoryAssoc {
    fn name(&self) -> &str {
        &self.label
    }

    fn health(&self) -> bool {
        true
    }

    fn associate(&self, seed: &str, limit: usize) -> StyxResult<Vec<Association>> {
        if seed.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self.explain(seed, limit))
    }

    fn confident(&self, seed: &str) -> StyxResult<bool> {
        Ok(!self.explain(seed, 1).is_empty())
    }

    fn status(&self) -> String {
        let (docs, terms) = self.size();
        format!("{}（共现图：{} 篇 / {} 词项）", self.label, docs, terms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定语料。单独抽出来是为了能断言「联想词必须出自这里」。
    const CORPUS: [&str; 6] = [
        "母亲留下了一张旧照片，照片上的女人笑得很温柔",
        "那张旧照片被夹在相册的第一页里",
        "她把相册放回抽屉，照片也跟着暗了下去",
        "隔壁的老猫在午后打盹，谁叫都不理",
        "雨水沿着书店的玻璃窗往下淌",
        "旧书店的柜台后面堆满了没拆封的纸箱",
    ];

    fn corpus() -> InMemoryAssoc {
        let a = InMemoryAssoc::new();
        a.observe_many(CORPUS);
        a
    }

    #[test]
    fn edge_lookup_is_symmetric() {
        let a = corpus();
        // 共现次数是按「字典序无序对」存的，所以两个方向都必须查得到。
        // 这里是有意反向查一次：曾经只按传入顺序查表，
        // 「照」<「相」导致 `pair("相册", "照片")` 凭空查不到。
        let fwd = a.pair("照片", "相册").expect("照片↔相册 应当有边");
        let rev = a
            .pair("相册", "照片")
            .expect("相册↔照片 应当有边（拉平顺序后对称）");
        assert_eq!(fwd.0, rev.0, "两个方向读到的支撑数应当一致");
        assert!(fwd.0 >= 1.0);
        assert!(!fwd.2.is_empty(), "边应当带证据");
    }

    #[test]
    fn associates_related_words_with_evidence() {
        let a = corpus();
        let hits = a.associate("照片", 8).unwrap();
        assert!(!hits.is_empty(), "应当能联想出与'照片'相关的词");

        // 每条联想都要带证据，而且**必须出自语料**——兜底实现宁可不联想，
        // 也不能凭空造词。
        //
        // 注意断言的是「来源可追溯」而不是「结果都是好词」：裸二元共现没有
        // 词典，必然混进「了一张」「张旧」这类碎片。这是刻意接受的局限——
        // 想要真词，接 MightBe。
        assert!(hits.iter().all(|h| !h.evidence.is_empty()), "{hits:?}");
        let text: String = CORPUS.concat();
        for h in &hits {
            assert!(text.contains(&h.word), "联想词应当来自语料：{}", h.word);
        }

        // 打分降序
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn abstains_on_unknown_seed() {
        let a = corpus();
        // 图里根本没这个词 → 弃判（返回空），而不是硬凑
        let hits = a.associate("量子纠缠", 5).unwrap();
        assert!(hits.is_empty(), "未知种子应当弃判，实际：{hits:?}");
        assert!(!a.confident("量子纠缠").unwrap());
        assert!(a.confident("照片").unwrap());
    }

    #[test]
    fn empty_graph_returns_nothing() {
        let a = InMemoryAssoc::new();
        assert!(a.associate("任何词", 5).unwrap().is_empty());
        assert!(a.associate("", 5).unwrap().is_empty());
        assert!(a.associate("x", 0).unwrap().is_empty());
    }

    #[test]
    fn cooccurrence_beats_incidental_mention() {
        // "甲"与"乙"反复同现；"甲"与"丙"只碰巧同现一次
        let a = InMemoryAssoc::new();
        for _ in 0..5 {
            a.observe("甲乙 甲乙 甲乙");
        }
        a.observe("甲 丙");
        a.observe("丙 丁 戊 己 庚 辛");
        let hits = a.associate("甲", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].word, "乙", "强共现的邻居应当排第一：{hits:?}");
    }

    #[test]
    fn multi_hop_expansion_reaches_second_order_neighbours() {
        let a = InMemoryAssoc::new();
        for _ in 0..4 {
            a.observe("旧书店 柜台 抽屉");
            a.observe("抽屉 照片 相册");
        }
        // "旧书店" 与 "相册" 不相邻，但通过 "抽屉" 两跳可达。
        // limit 取得大一些：这里要验证的是**可达性**，不是排序——
        // 两跳的候选天然会排在若干一跳高频词后面。
        let hits = a.associate("旧书店", 24).unwrap();
        let words: Vec<&str> = hits.iter().map(|h| h.word.as_str()).collect();
        assert!(
            words.contains(&"相册"),
            "两跳应当能摸到相册，实际：{words:?}"
        );
        // 一跳的邻居分数应当高于两跳的
        let drawer = hits.iter().find(|h| h.word == "抽屉").map(|h| h.score);
        let album = hits.iter().find(|h| h.word == "相册").map(|h| h.score);
        assert!(
            matches!((drawer, album), (Some(d), Some(al)) if d > al),
            "一跳({drawer:?}) 应当高于两跳({album:?})"
        );
    }

    #[test]
    fn hops_zero_means_direct_neighbours_only() {
        let a = InMemoryAssoc::with_params(GraphParams {
            hops: 1,
            ..Default::default()
        });
        for _ in 0..4 {
            a.observe("旧书店 柜台 抽屉");
            a.observe("抽屉 照片 相册");
        }
        let hits = a.associate("旧书店", 10).unwrap();
        let words: Vec<&str> = hits.iter().map(|h| h.word.as_str()).collect();
        assert!(words.contains(&"抽屉"));
        assert!(!words.contains(&"相册"), "只有一跳时不该出现两跳结果");
    }

    #[test]
    fn confidence_grows_with_support() {
        let low = confidence(1.0, 0.5);
        let high = confidence(20.0, 3.0);
        assert!(high > low);
        assert!((0.0..=1.0).contains(&low));
        assert!((0.0..=1.0).contains(&high));
    }

    #[test]
    fn min_support_filters_singletons() {
        let strict = InMemoryAssoc::with_params(GraphParams {
            min_support: 5.0,
            ..Default::default()
        });
        strict.observe("甲 乙");
        // 只共现过一次，达不到支撑门槛
        assert!(strict.associate("甲", 5).unwrap().is_empty());
    }

    #[test]
    fn status_and_size_are_reported() {
        let a = corpus();
        let (docs, terms) = a.size();
        assert_eq!(docs, 6);
        assert!(terms > 10);
        assert!(a.status().contains("共现图"));
        assert!(a.health());
    }

    #[test]
    fn evidence_is_deduplicated_and_capped() {
        let a = InMemoryAssoc::new();
        for _ in 0..10 {
            a.observe("照片 相册 照片 相册");
        }
        let hits = a.associate("照片", 3).unwrap();
        assert!(!hits.is_empty());
        let ev = &hits[0].evidence;
        assert!(ev.len() <= a.params().max_evidence);
        let mut uniq = ev.clone();
        uniq.dedup();
        assert_eq!(uniq.len(), ev.len(), "证据不应重复：{ev:?}");
    }

    #[test]
    fn observe_ignores_degenerate_input() {
        let a = InMemoryAssoc::new();
        a.observe("   ");
        a.observe("单");
        assert_eq!(a.size(), (0, 0));
    }
}
