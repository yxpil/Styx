//! 文本工具：token 估算、预算裁剪、轻量关键词抽取、摘要。
//!
//! 这些工具刻意保持"零依赖 + 可预测"，因为提示词预算必须在**离线**情况下
//! 也能算准。真实的 BPE 分词交给模型侧，内核只需要一个保守上界。

/// 一个"字符"占用的 token 估算。
///
/// - CJK / 全角标点：约 1 token / 字（保守取 1.0）
/// - ASCII 字母数字：约 4 字符 / token
/// - 其他（空白、标点）：按 0.5 计
fn char_cost(c: char) -> f32 {
    if is_cjk(c) {
        // CJK：约 1 token / 字
        1.0
    } else if c.is_ascii() {
        // ASCII：约 4 字符 / token
        0.25
    } else {
        // 全角标点、emoji 等：保守按 1 token
        1.0
    }
}

/// 是否为 CJK **汉字/假名**（即可以参与 n-gram 的字符）。
///
/// 刻意**不**包含 U+FF00–FFEF「全角形式」：那一段里既有全角字母数字，
/// 也有全角标点（`，` U+FF0C、`。` U+FF0E、`！` U+FF01 …）。一旦把全角标点
/// 当成汉字，`tokenize("旧照片，照片上")` 就会切出 `"片，"`、`"，照"` 这种
/// 跨标点的伪词，污染整个共现图。全角标点在这里应当和 ASCII 标点一样，
/// 扮演**词边界**的角色。
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF      // 假名
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x4E00..=0x9FFF    // 基本区
        | 0xF900..=0xFAFF    // 兼容表意
        | 0x20000..=0x2FA1F  // 扩展 B~F
    )
}

/// 把 `f32` 收敛到 **3 位小数**，再交给 JSON / 展示层。
///
/// `f32` 存不下 0.85：直接序列化会得到 `0.8500000238418579`。这既让请求体
/// 难读，也让"我明明设的是 0.85"这种对比在测试里翻车。生成参数与情绪维度
/// 的有效精度都远用不到三位小数，所以统一先收敛再输出。
pub fn round3(x: f32) -> f64 {
    ((x as f64) * 1000.0).round() / 1000.0
}

/// 粗略估算文本的 token 数。
///
/// 结果偏保守（宁可高估），用于提示词预算裁剪。
/// 行为示例：`estimate_tokens("hello world") <= 6`、`estimate_tokens("你好") >= 1`
/// （见 `tests::estimate_tokens_is_conservative`）。
pub fn estimate_tokens(s: &str) -> usize {
    let cost: f32 = s.chars().map(char_cost).sum();
    // 每段文本有固定的开头/结尾开销
    (cost.ceil() as usize) + 2
}

/// 把文本裁剪到不超过 `budget` 个估算 token，超出时在尾部加省略号。
///
/// 从头部保留（对提示词而言，靠前的信息通常更重要）。
pub fn truncate_to_tokens(s: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if estimate_tokens(s) <= budget {
        return s.to_string();
    }
    // 为省略号预留 2 个 token
    let target = budget.saturating_sub(2);
    let mut used = 0f32;
    let mut out = String::new();
    for c in s.chars() {
        let c_cost = char_cost(c);
        if used + c_cost > target as f32 {
            break;
        }
        used += c_cost;
        out.push(c);
    }
    out.push('…');
    out
}

/// 从尾部裁剪：保留文本末尾 `budget` 个估算 token（用于保留最新的对话）。
pub fn truncate_tail_to_tokens(s: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if estimate_tokens(s) <= budget {
        return s.to_string();
    }
    let target = budget.saturating_sub(2);
    let chars: Vec<char> = s.chars().collect();
    let mut used = 0f32;
    let mut start = chars.len();
    for i in (0..chars.len()).rev() {
        let c_cost = char_cost(chars[i]);
        if used + c_cost > target as f32 {
            break;
        }
        used += c_cost;
        start = i;
    }
    let mut out = String::from("…");
    out.extend(&chars[start..]);
    out
}

/// 简易停用词（中英混合），用于关键词抽取时剔除噪音。
pub const STOPWORDS: &[&str] = &[
    // 中文
    "的", "了", "在", "是", "我", "你", "他", "她", "它", "们", "这", "那", "有", "和", "与",
    "就", "都", "也", "不", "很", "会", "要", "把", "被", "给", "对", "从", "到", "为", "着",
    "一个", "什么", "怎么", "这个", "那个", "自己", "已经", "还是", "但是", "因为", "所以",
    "然后", "如果", "可以", "没有", "知道", "觉得", "现在", "时候", "一样", "这样", "那样",
    // 英文
    "the", "a", "an", "and", "or", "but", "if", "then", "of", "to", "in", "on", "at", "for",
    "with", "is", "are", "was", "were", "be", "been", "am", "do", "does", "did", "have", "has",
    "had", "i", "you", "he", "she", "it", "we", "they", "this", "that", "these", "those", "as",
    "by", "from", "not", "no", "yes", "so", "my", "your", "his", "her", "its", "our", "their",
];

/// 是否为停用词。
pub fn is_stopword(w: &str) -> bool {
    let lower = w.to_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

/// 把文本切成若干「连续汉字段」。
///
/// 关键词抽取与分词都必须**按段**处理，不能先把非汉字过滤掉再拼接滑窗：
/// `相册，照片` 一旦被过滤拼接就成了 `相册照片`，滑窗会切出 `册照片`
/// 这种根本不存在的词，直接污染共现图与联想结果。
fn cjk_runs(s: &str) -> Vec<Vec<char>> {
    let mut runs: Vec<Vec<char>> = Vec::new();
    let mut cur: Vec<char> = Vec::new();
    for c in s.chars() {
        if is_cjk(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

/// 轻量关键词抽取：CJK 双字组合 + ASCII 词，按词频与长度加权排序。
///
/// 这不是 BM25，只是**给联想后端提供种子词**的启发式；真正的相关度排序
/// 由 Nebula 的 BM25 与 MightBe 的联想图负责。
/// 行为示例见 `tests::keywords_extracts_content_words`。
pub fn keywords(s: &str, limit: usize) -> Vec<String> {
    use std::collections::HashMap;

    let mut freq: HashMap<String, f32> = HashMap::new();
    let push = |term: String, weight: f32, freq: &mut HashMap<String, f32>| {
        if term.is_empty() || is_stopword(&term) {
            return;
        }
        *freq.entry(term).or_insert(0.0) += weight;
    };

    // 1) CJK：先按连续汉字段切开，再在**段内**取 2~4 字组合。
    //
    // 权重是「短的高于长的」：2 字组合是中文检索的通用单位，3/4 字组合
    // 只有在反复出现时才靠累计次数压上来。反过来（越长越重）会让
    // 「我想看看」这种跨词片段仅凭长度就挤掉真正的词。
    for run in cjk_runs(s) {
        for (n, weight) in [(2usize, 1.0f32), (3, 0.6), (4, 0.4)] {
            if run.len() < n {
                continue;
            }
            for win in run.windows(n) {
                push(win.iter().collect(), weight, &mut freq);
            }
        }
    }

    // 2) ASCII / 数字：按非字母数字切分
    for raw in s.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')) {
        let w = raw.trim();
        if w.chars().count() < 2 {
            continue;
        }
        // 纯 ASCII 且短于 2 的丢掉；纯数字丢掉
        if w.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        push(w.to_lowercase(), 1.2, &mut freq);
    }

    let mut ranked: Vec<(String, f32)> = freq.into_iter().collect();
    // 权重优先，其次长词（更具体），最后按词面定序——
    // 最后一档是为了**结果可复现**：HashMap 的迭代顺序是随机的，
    // 少了这一档，同分词的先后每次运行都不一样。
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.chars().count().cmp(&a.0.chars().count()))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(t, _)| t)
        .collect()
}

/// 通用分词：CJK 单字 + 双字组合，ASCII 按非字母数字切分。
///
/// 这是**兜底实现共用**的分词器：Nebula 有自己的 `nebula-tokenizer`，
/// MightBe 有自己的 `mightbe-nlp`；但当它们不在线时，内核需要一个
/// 零依赖、中英混排都能用的分词器来驱动内存 BM25 与共现图。
///
/// 保留 1-gram 是为了让"照片"和"相片"这类词至少能在单字层面部分命中；
/// 保留 2-gram 是因为中文里绝大多数语义单元是双字词。
///
/// 行为示例：`tokenize("旧书店的 Rust 柜台")` 同时含 `"旧书"` 与 `"rust"`
/// （见 `tests::tokenize_mixes_cjk_and_ascii`）。
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();

    // CJK 连续段：取 1-gram 与 2-gram
    let mut run: Vec<char> = Vec::new();
    let flush = |run: &mut Vec<char>, out: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        for c in run.iter() {
            out.push(c.to_string());
        }
        for w in run.windows(2) {
            out.push(w.iter().collect::<String>());
        }
        run.clear();
    };
    for c in s.chars() {
        if is_cjk(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);

    // ASCII / 数字：按非字母数字切分，丢掉单字符与纯数字
    for raw in s.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let w = raw.trim().to_lowercase();
        if w.chars().count() >= 2 && !w.chars().all(|c| c.is_ascii_digit()) {
            out.push(w);
        }
    }
    out
}

/// 把长文本压成一行摘要（用于把事件写进记忆前的瘦身）。
pub fn summarize(s: &str, max_chars: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    let mut out = String::new();
    let mut last_space = false;
    for c in flat.chars() {
        if c == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(c);
    }
    let out = out.trim().to_string();
    if out.chars().count() <= max_chars {
        return out;
    }
    let mut truncated: String = out.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// 转义为 SQL 字符串字面量（含外层单引号），并转义 Nebula / MightBe 方言中的 `'`。
pub fn sql_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_monotonic_and_conservative() {
        let short = estimate_tokens("hi");
        let long = estimate_tokens("hi there, this is a longer sentence.");
        assert!(long > short);
        // ASCII: 4 字符 / token 的保守上界
        assert!(estimate_tokens("abcdefgh") <= 8);
        // CJK 一字符至少一 token
        assert!(estimate_tokens("一二三四五") >= 5);
    }

    #[test]
    fn truncate_respects_budget() {
        let s = "这是一段需要被裁剪的很长很长的文本内容用来测试预算裁剪是否正确工作";
        let cut = truncate_to_tokens(s, 8);
        assert!(estimate_tokens(&cut) <= 10, "got {cut:?}");
        assert!(cut.ends_with('…'));
        // 预算充足时原样返回
        assert_eq!(truncate_to_tokens("短", 100), "短");
        assert_eq!(truncate_to_tokens("x", 0), "");
    }

    #[test]
    fn truncate_tail_keeps_the_end() {
        let s = "开头部分内容结尾Keyword";
        let cut = truncate_tail_to_tokens(s, 10);
        assert!(cut.starts_with('…'));
        assert!(cut.contains("Keyword"), "got {cut:?}");
    }

    #[test]
    fn keywords_pick_out_content_words() {
        let kws = keywords("林夏把母亲留下的旧照片放回抽屉，那是一张褪色的照片", 6);
        assert!(!kws.is_empty());
        // 停用词不应出现在结果里
        assert!(!kws.iter().any(|k| k == "的" || k == "把"));
    }

    #[test]
    fn estimate_tokens_is_conservative() {
        assert!(estimate_tokens("hello world") <= 6);
        assert!(estimate_tokens("你好") >= 1);
        assert_eq!(estimate_tokens(""), 2); // 只有首尾固定开销
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
    fn tokenize_handles_empty_and_punctuation_only() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("，。！ ").is_empty());
    }

    #[test]
    fn sql_quote_escapes_quotes() {
        assert_eq!(sql_quote("it's"), "'it''s'");
        assert_eq!(sql_quote("a\nb"), "'a\\nb'");
    }

    #[test]
    fn summarize_flattens_and_clips() {
        let s = "line one\n\n   line   two";
        assert_eq!(summarize(s, 100), "line one line two");
        assert_eq!(summarize("abcdefghij", 5), "abcd…");
    }
}
