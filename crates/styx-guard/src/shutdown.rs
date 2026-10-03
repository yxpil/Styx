//! 优雅关闭。
//!
//! 默认的 Ctrl+C 是**直接杀进程**：正在跑的那个回合（可能已经等模型等了
//! 二十秒）连半句台词都留不下，而用户看到的是一个没有任何解释的退出。
//!
//! 这里做三件事：
//!
//! 1. 把 Ctrl+C 变成"**置一个标志**"而不是"杀进程"；
//! 2. 让正在处理的连接能**登记自己在飞**（[`Shutdown::enter`]），
//!    从而有一个"还有几个没干完"的准确数字；
//! 3. 提供一个**等到清空为止**的方法（[`Shutdown::wait_idle`]），
//!    以及一个"再按一次就强制退出"的逃生口。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 关闭协调器。
pub struct Shutdown {
    flag: AtomicBool,
    active: AtomicUsize,
    /// 只用来做通知，不保护任何数据（计数走原子量）。
    lock: Mutex<()>,
    idle: Condvar,
}

impl Shutdown {
    /// 新建。
    pub fn new() -> Arc<Self> {
        Arc::new(Shutdown {
            flag: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            lock: Mutex::new(()),
            idle: Condvar::new(),
        })
    }

    /// 是否已经进入关闭流程。
    pub fn is_shutting_down(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 当前在飞的处理数。
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// 进入在飞区间。
    ///
    /// 已经在关闭中则返回 `None`——调用方据此拒绝这条连接，
    /// 而不是让它进来跑一半再被砍掉。
    pub fn enter(&self) -> Option<ShutdownGuard<'_>> {
        if self.flag.load(Ordering::SeqCst) {
            return None;
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        // 上面两行之间可能刚好触发了关闭：补一次检查，避免漏网。
        if self.flag.load(Ordering::SeqCst) {
            self.leave();
            return None;
        }
        Some(ShutdownGuard { shutdown: self })
    }

    /// 触发关闭。
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        // 持锁通知：不持锁的话，可能刚好在"置位"与"进等锁"之间
        // 把这次唤醒错过，于是等待方一直睡到超时。
        let _guard = self.lock.lock();
        self.idle.notify_all();
    }

    /// 阻塞直到关闭被触发（用于叫醒正在 accept 的循环）。
    pub fn wait_triggered(&self) {
        let Ok(mut guard) = self.lock.lock() else {
            return;
        };
        while !self.flag.load(Ordering::SeqCst) {
            match self.idle.wait_timeout(guard, Duration::from_millis(200)) {
                Ok((g, _)) => guard = g,
                Err(_) => return,
            }
        }
    }

    /// 等到在飞清空为止；超时返回 `false`。
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let Ok(mut guard) = self.lock.lock() else {
            return self.active() == 0;
        };
        while self.active.load(Ordering::SeqCst) > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.idle.wait_timeout(guard, left) {
                Ok((g, wait)) => {
                    guard = g;
                    if wait.timed_out() && self.active.load(Ordering::SeqCst) > 0 {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
        true
    }

    /// 把 Ctrl+C 接到这个协调器上。
    ///
    /// 第二次按 Ctrl+C 会**立即退出**：收尾卡住的时候，人总得有个
    /// 不用去开任务管理器就能走人的办法。
    pub fn install_ctrlc(self: &Arc<Self>) -> Result<(), String> {
        let me = Arc::clone(self);
        ctrlc::set_handler(move || {
            if me.is_shutting_down() {
                eprintln!("\n再按一次：立即退出。");
                std::process::exit(130);
            }
            eprintln!("\n收到中断，正在收尾（再按一次可强制退出）…");
            me.trigger();
        })
        .map_err(|e| format!("装不上信号处理器：{e}"))
    }

    fn leave(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        let _guard = self.lock.lock();
        self.idle.notify_all();
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        // `Default` 拿不到 `Arc`，所以这里返回一个裸的；需要共享时用 `new`。
        Shutdown {
            flag: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            lock: Mutex::new(()),
            idle: Condvar::new(),
        }
    }
}

/// 在飞凭证。Drop 时销账。
pub struct ShutdownGuard<'a> {
    shutdown: &'a Shutdown,
}

impl Drop for ShutdownGuard<'_> {
    fn drop(&mut self) {
        self.shutdown.leave();
    }
}

impl std::fmt::Debug for ShutdownGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownGuard").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_tracks_in_flight() {
        let s = Shutdown::new();
        let a = s.enter().expect("未关闭时应当能进");
        let b = s.enter().expect("未关闭时应当能进");
        assert_eq!(s.active(), 2);
        drop(a);
        assert_eq!(s.active(), 1);
        drop(b);
        assert_eq!(s.active(), 0);
    }

    #[test]
    fn trigger_refuses_new_entrants() {
        let s = Shutdown::new();
        let held = s.enter().unwrap();
        s.trigger();
        assert!(s.is_shutting_down());
        assert!(s.enter().is_none(), "关闭中不该再收新连接");
        drop(held);
    }

    #[test]
    fn wait_idle_returns_when_drained() {
        let s = Shutdown::new();
        let held = s.enter().unwrap();
        s.trigger();

        let s2 = Arc::clone(&s);
        let waiter = std::thread::spawn(move || s2.wait_idle(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        drop(held);
        assert!(waiter.join().unwrap(), "清空后应当返回 true");
    }

    #[test]
    fn wait_idle_times_out_when_stuck() {
        let s = Shutdown::new();
        let _stuck = s.enter().unwrap();
        s.trigger();
        assert!(
            !s.wait_idle(Duration::from_millis(60)),
            "没清空就该超时返回 false"
        );
    }

    #[test]
    fn wait_triggered_wakes_up() {
        let s = Shutdown::new();
        let s2 = Arc::clone(&s);
        let waiter = std::thread::spawn(move || s2.wait_triggered());
        std::thread::sleep(Duration::from_millis(30));
        s.trigger();
        waiter.join().unwrap();
    }
}
