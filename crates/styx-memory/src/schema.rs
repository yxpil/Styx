//! 记忆 ↔ Nebula SQL 的映射。
//!
//! Nebula 的记忆表叫 `memories`，列见其 README：
//! `id / content / keywords / key_points / tags / source / importance / created_at / updated_at`。
//!
//! 这里只做两件事：**生成安全 SQL** 与 **把结果集映射回 [`Recalled`]**。
//! 列名匹配刻意做得宽松（`content` 或 `text`、`score` 或 `relevance` 都能认），
//! 这样 Nebula 未来调整结果集列名时，Styx 不需要跟着改。

use styx_core::ports::{MemoryNote, Recalled};
use styx_core::text::sql_quote;

/// 默认逻辑库名。
pub const DEFAULT_DB: &str = "main";

/// 给标识符（库名 / 表名）加上安全引号前的白名单校验。
///
/// Nebula 的库名会被直接拼进 SQL，这里只允许字母数字下划线，
/// 避免任何形式的注入。
pub fn safe_ident(name: &str) -> crate::error::Result<&str> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(name)
    } else {
        Err(crate::error::MemoryError::Config(format!(
            "非法标识符：{name:?}（只允许字母数字、下划线与连字符）"
        )))
    }
}

/// 生成 `INSERT`。
///
/// 用显式列名而不是全列顺序——Nebula 的 `INSERT INTO memories VALUES (...)`
/// 要求按 `content, keywords, source, importance` 顺序排，
/// 显式列名更不容易因为表结构演进写错。
pub fn insert_sql(note: &MemoryNote) -> String {
    let tags = note.tags.join(", ");
    let mut sql = format!(
        "INSERT INTO memories (content, tags, source, importance) VALUES ({}, {}, {}, {})",
        sql_quote(&note.text),
        sql_quote(&tags),
        sql_quote(&note.source),
        fmt_f32(note.importance)
    );
    // 写入后立刻回读 id 代价太高，这里让调用方按"最近一条"兜底。
    sql.push(';');
    sql
}

/// 生成 `SEARCH`（BM25 全文检索）。
pub fn search_sql(query: &str, limit: usize) -> String {
    format!("SEARCH {} LIMIT {};", sql_quote(query), limit.max(1))
}

/// 带 `WHERE keyword = '...'` 的精确关键词查询。
pub fn keyword_sql(keyword: &str, limit: usize) -> String {
    format!(
        "SELECT id, content, tags, importance, created_at FROM memories WHERE keyword = {} LIMIT {};",
        sql_quote(keyword),
        limit.max(1)
    )
}

/// 最近的记忆（按创建时间倒序）。
pub fn recent_sql(limit: usize) -> String {
    format!(
        "SELECT id, content, tags, importance, created_at FROM memories ORDER BY created_at DESC LIMIT {};",
        limit.max(1)
    )
}

/// `RELATED TO <id>`（共现图跳数扩展）。
pub fn related_sql(id: &str, limit: usize) -> String {
    let id = id.trim();
    if id.chars().all(|c| c.is_ascii_digit()) {
        format!("RELATED TO {} LIMIT {};", id, limit.max(1))
    } else {
        // 非纯数字 id 时退化为按内容检索，避免拼出非法 SQL
        format!("SEARCH {} LIMIT {};", sql_quote(id), limit.max(1))
    }
}

/// `DELETE`。
pub fn delete_sql(id: &str) -> String {
    format!("DELETE FROM memories WHERE id = {};", sql_quote(id))
}

/// `SELECT` 一条。
pub fn get_sql(id: &str) -> String {
    format!(
        "SELECT id, content, tags, importance, created_at FROM memories WHERE id = {};",
        sql_quote(id)
    )
}

fn fmt_f32(v: f32) -> String {
    // 保证小数点是 '.'，且不出现科学计数法
    format!("{:.4}", v.clamp(0.0, 1.0))
}

/// 把结果集映射为 [`Recalled`]。
///
/// `origin` 用于标记来源（`"nebula"`）。列名匹配是**宽松**的：
/// 大小写不敏感，且对同一语义接受多个别名。
pub fn rows_to_recalled(
    columns: &[String],
    rows: &[Vec<String>],
    origin: &str,
) -> Vec<Recalled> {
    let lower: Vec<String> = columns.iter().map(|c| c.to_lowercase()).collect();
    let idx = |names: &[&str]| -> Option<usize> {
        names
            .iter()
            .find_map(|n| lower.iter().position(|c| c == n))
    };
    let i_id = idx(&["id", "memory_id", "rowid"]);
    let i_text = idx(&["content", "text", "body", "memory"]);
    let i_score = idx(&["score", "relevance", "rank", "bm25"]);
    let i_imp = idx(&["importance", "weight"]);
    let i_tags = idx(&["tags", "tag", "labels"]);
    let i_at = idx(&["created_at", "created", "timestamp", "at"]);

    let mut out = Vec::with_capacity(rows.len());
    for (n, row) in rows.iter().enumerate() {
        let get = |i: Option<usize>| i.and_then(|i| row.get(i)).cloned();
        let text = match get(i_text) {
            Some(t) if !t.trim().is_empty() => t,
            _ => continue,
        };
        // 没有 score 列时，用行序当分数（后端已排好序）
        let score = get(i_score)
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or_else(|| 1.0 / (n as f32 + 1.0));
        let importance = get(i_imp)
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.5);
        let tags = get(i_tags)
            .map(|s| {
                s.split([',', '，', '|'])
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let created_at = get(i_at).and_then(|s| parse_timestamp(&s));
        let id = get(i_id).unwrap_or_else(|| format!("row{n}"));

        out.push(Recalled {
            id,
            text,
            score,
            importance,
            tags,
            created_at,
            origin: origin.to_string(),
        });
    }
    out
}

/// 时间戳解析：兼容毫秒、秒、以及 ISO-8601 的 `YYYY-MM-DDTHH:MM:SS` 前缀。
fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<i64>() {
        // 10 位当作秒，13 位当作毫秒
        return Some(if v < 10_000_000_000 { v * 1000 } else { v });
    }
    // 极简 ISO-8601 → 毫秒（只处理 date[ T]time，忽略时区偏移）
    let (date, rest) = s.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    let mut t = rest.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next().unwrap_or("0").parse().ok()?;
    let se: i64 = t
        .next()
        .unwrap_or("0")
        .trim_end_matches('Z')
        .split('.')
        .next()
        .unwrap_or("0")
        .parse()
        .ok()?;
    Some(days_from_civil(y, mo, da) * 86_400_000 + (h * 3600 + mi * 60 + se) * 1000)
}

/// Howard Hinnant 的 `days_from_civil`。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_escapes_quotes_and_formats_importance() {
        let n = MemoryNote::new("it's a test\nline2")
            .with_tags(["a", "b"])
            .with_importance(0.87654)
            .with_source("styx:林夏");
        let sql = insert_sql(&n);
        assert!(sql.starts_with("INSERT INTO memories (content, tags, source, importance) VALUES ("));
        assert!(sql.contains("'it''s a test\\nline2'"));
        assert!(sql.contains("'a, b'"));
        assert!(sql.contains("'styx:林夏'"));
        assert!(sql.contains("0.8765"), "{sql}");
        assert!(sql.ends_with(';'));
    }

    #[test]
    fn insert_clamps_importance() {
        let sql = insert_sql(&MemoryNote::new("x").with_importance(5.0));
        assert!(sql.contains("1.0000"), "{sql}");
    }

    #[test]
    fn search_and_related_are_well_formed() {
        assert_eq!(search_sql("旧照片", 5), "SEARCH '旧照片' LIMIT 5;");
        assert_eq!(related_sql("12", 3), "RELATED TO 12 LIMIT 3;");
        // 非数字 id 退化为检索，不会拼出非法 SQL
        assert!(related_sql("1; DROP TABLE", 3).starts_with("SEARCH "));
        assert_eq!(delete_sql("7"), "DELETE FROM memories WHERE id = '7';");
    }

    #[test]
    fn safe_ident_allows_and_rejects() {
        assert!(safe_ident("main").is_ok());
        assert!(safe_ident("my_db-1").is_ok());
        assert!(safe_ident("").is_err());
        assert!(safe_ident("a;drop").is_err());
        assert!(safe_ident("中文").is_err());
    }

    #[test]
    fn maps_search_result_columns() {
        let cols = vec![
            "id".to_string(),
            "score".to_string(),
            "content".to_string(),
            "tags".to_string(),
            "importance".to_string(),
            "created_at".to_string(),
        ];
        let rows = vec![
            vec![
                "3".into(),
                "2.75".into(),
                "母亲留下了一张旧照片".into(),
                "照片,母亲".into(),
                "0.9".into(),
                "1735689600000".into(),
            ],
            vec![
                "4".into(),
                "1.20".into(),
                "陈默借走了相册".into(),
                "相册".into(),
                "0.6".into(),
                "2025-01-01T00:00:00Z".into(),
            ],
        ];
        let got = rows_to_recalled(&cols, &rows, "nebula");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "3");
        assert_eq!(got[0].text, "母亲留下了一张旧照片");
        assert!((got[0].score - 2.75).abs() < 1e-6);
        assert_eq!(got[0].tags, vec!["照片", "母亲"]);
        assert_eq!(got[0].created_at, Some(1_735_689_600_000));
        assert_eq!(got[1].created_at, Some(1_735_689_600_000));
        assert_eq!(got[0].origin, "nebula");
    }

    #[test]
    fn tolerates_renamed_columns_and_missing_score() {
        let cols = vec!["memory_id".to_string(), "text".to_string(), "weight".to_string()];
        let rows = vec![
            vec!["a".into(), "第一条".into(), "0.8".into()],
            vec!["b".into(), "第二条".into(), "0.4".into()],
        ];
        let got = rows_to_recalled(&cols, &rows, "nebula");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "a");
        assert!((got[0].importance - 0.8).abs() < 1e-6);
        // 无 score 列时按行序给分
        assert!(got[0].score > got[1].score);
    }

    #[test]
    fn rows_without_text_are_skipped() {
        let cols = vec!["id".to_string(), "content".to_string()];
        let rows = vec![
            vec!["1".into(), "".into()],
            vec!["2".into(), "有内容".into()],
        ];
        let got = rows_to_recalled(&cols, &rows, "nebula");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "2");
    }

    #[test]
    fn timestamp_parsing_variants() {
        assert_eq!(parse_timestamp("1735689600000"), Some(1_735_689_600_000));
        assert_eq!(parse_timestamp("1735689600"), Some(1_735_689_600_000));
        assert_eq!(parse_timestamp("2025-01-01T00:00:00"), Some(1_735_689_600_000));
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("not-a-date"), None);
    }
}
