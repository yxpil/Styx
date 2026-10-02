//! 共享记忆池的本地兜底实现。
//!
//! 直接复用 [`styx_memory::InMemoryMemory`] 的 BM25 检索，只是把它包装成
//! [`PoolPort`] 的语义。这样做的原因很实际：**共享记忆与私人记忆的检索需求
//! 本来就一样**，差别只在"谁看得到"，不该为此再写一个检索器。

use styx_core::ports::{MemoryNote, MemoryPort, PoolPort, PoolStats, Recalled};
use styx_memory::InMemoryMemory;

use styx_core::error::Result as StyxResult;

/// 进程内的共享记忆池。
pub struct LocalPool {
    mem: InMemoryMemory,
}

impl Default for LocalPool {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalPool {
    /// 新建。
    pub fn new() -> Self {
        LocalPool {
            mem: InMemoryMemory::new().with_label("pool(local)"),
        }
    }

    /// 已有条目数。
    pub fn len(&self) -> usize {
        self.mem.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.mem.is_empty()
    }
}

impl PoolPort for LocalPool {
    fn name(&self) -> &str {
        "pool(local)"
    }

    fn health(&self) -> bool {
        true
    }

    fn remember(&self, note: &MemoryNote) -> StyxResult<String> {
        self.mem.remember(note)
    }

    fn recall(&self, query: &str, limit: usize) -> StyxResult<Vec<Recalled>> {
        self.mem.recall(query, limit)
    }

    fn stats(&self) -> StyxResult<PoolStats> {
        Ok(PoolStats {
            count: self.mem.len(),
            detail: "本地兜底（进程内，退出即失）".into(),
        })
    }

    fn status(&self) -> String {
        format!("pool(local)（内存兜底，{} 条）", self.mem.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_recall_round_trip() {
        let p = LocalPool::new();
        p.remember(
            &MemoryNote::new("陈默今晚会在城南的旧书店过夜")
                .with_tags(["陈默", "约定"])
                .with_importance(0.8),
        )
        .unwrap();
        let hits = p.recall("城南 旧书店", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("城南"));
        assert_eq!(hits[0].tags, vec!["陈默", "约定"]);
        assert!(p.health());
        assert_eq!(p.stats().unwrap().count, 1);
        assert!(p.status().contains("内存兜底"));
    }

    #[test]
    fn empty_pool_is_safe() {
        let p = LocalPool::new();
        assert!(p.is_empty());
        assert!(p.recall("任何", 5).unwrap().is_empty());
        assert!(p.recall("", 5).unwrap().is_empty());
        assert_eq!(p.stats().unwrap().count, 0);
    }
}
