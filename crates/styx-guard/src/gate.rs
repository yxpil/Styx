//! 连接准入。
//!
//! `std` 没有信号量。用 `Mutex<usize> + Condvar` 拼一个就够了——这里只有
//! "拿名额 / 还名额 / 等名额"三件事，不值得为它引一个并发库。
//!
//! 名额挂在 [`Permit`] 上，**Drop 即归还**：连接处理线程就算 panic，
//! 名额也不会漏掉。漏名额比拒绝服务更糟——它是个只进不出的漏斗。

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 连接准入闸门。
pub struct Gate {
    inner: Mutex<usize>,
    idle: Condvar,
    max: usize,
}

impl Gate {
    /// 造一个闸门；`max == 0` 表示不限（只计数，不拦截）。
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Gate {
            inner: Mutex::new(0),
            idle: Condvar::new(),
            max,
        })
    }

    /// 当前在飞连接数。
    pub fn in_flight(&self) -> usize {
        self.inner.lock().map(|n| *n).unwrap_or(0)
    }

    /// 上限；`0` 表示不限。
    pub fn max(&self) -> usize {
        self.max
    }

    /// 是否不限。
    pub fn is_unlimited(&self) -> bool {
        self.max == 0
    }

    /// 申请一个名额：最多等 `timeout`；不限时直接通过。
    ///
    /// 返回 `None` 表示"等不到了，请拒绝这条连接"。
    pub fn acquire(self: &Arc<Self>, timeout: Duration) -> Option<Permit> {
        let Ok(mut n) = self.inner.lock() else {
            // 锁中毒（有线程持锁时 panic 过）。此时**放行**比拒绝好：
            // 放过一条连接，比整个服务从此拒绝一切要轻。
            return Some(Permit {
                gate: Arc::clone(self),
            });
        };

        if self.max == 0 || *n < self.max {
            *n += 1;
            return Some(Permit {
                gate: Arc::clone(self),
            });
        }

        let deadline = Instant::now() + timeout;
        while *n >= self.max {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let (guard, wait) = match self.idle.wait_timeout(n, left) {
                Ok(v) => v,
                Err(_) => return None,
            };
            n = guard;
            if wait.timed_out() && *n >= self.max {
                return None;
            }
        }
        *n += 1;
        Some(Permit {
            gate: Arc::clone(self),
        })
    }

    fn release(&self) {
        if let Ok(mut n) = self.inner.lock() {
            *n = n.saturating_sub(1);
        }
        self.idle.notify_one();
    }
}

/// 一个已占用的名额。Drop 时自动归还。
pub struct Permit {
    gate: Arc<Gate>,
}

impl Permit {
    /// 这张凭证来自哪个闸门。
    pub fn gate(&self) -> &Gate {
        &self.gate
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permit")
            .field("in_flight", &self.gate.in_flight())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_gate_never_refuses() {
        let g = Gate::new(0);
        let permits: Vec<_> = (0..1000)
            .filter_map(|_| g.acquire(Duration::ZERO))
            .collect();
        assert_eq!(permits.len(), 1000);
        assert_eq!(g.in_flight(), 1000);
        drop(permits);
        assert_eq!(g.in_flight(), 0);
    }

    #[test]
    fn bounded_gate_refuses_when_full() {
        let g = Gate::new(2);
        let a = g.acquire(Duration::ZERO).expect("第一个");
        let b = g.acquire(Duration::ZERO).expect("第二个");
        assert!(g.acquire(Duration::ZERO).is_none(), "第三个应当被拒");
        assert_eq!(g.in_flight(), 2);

        // 还一个就能再进一个
        drop(a);
        assert_eq!(g.in_flight(), 1);
        let c = g.acquire(Duration::ZERO).expect("还了就该能进");
        assert_eq!(g.in_flight(), 2);
        drop((b, c));
        assert_eq!(g.in_flight(), 0);
    }

    #[test]
    fn permit_returns_its_slot_even_on_panic() {
        let g = Gate::new(1);
        let g2 = Arc::clone(&g);
        let _ = std::thread::spawn(move || {
            let _p = g2.acquire(Duration::ZERO).unwrap();
            panic!("连接处理线程炸了");
        })
        .join();
        assert_eq!(g.in_flight(), 0, "panic 不该把名额漏掉");
        assert!(g.acquire(Duration::ZERO).is_some());
    }

    #[test]
    fn waiting_acquire_gets_the_slot_when_one_frees() {
        let g = Gate::new(1);
        let held = g.acquire(Duration::ZERO).unwrap();
        let g2 = Arc::clone(&g);
        let waiter = std::thread::spawn(move || {
            let p = g2.acquire(Duration::from_secs(5));
            assert!(p.is_some(), "等到了就该拿到");
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(held);
        waiter.join().unwrap();
    }

    #[test]
    fn waiting_acquire_gives_up_on_timeout() {
        let g = Gate::new(1);
        let _held = g.acquire(Duration::ZERO).unwrap();
        let started = Instant::now();
        assert!(g.acquire(Duration::from_millis(80)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(70));
    }
}
