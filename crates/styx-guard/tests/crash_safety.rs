//! 崩溃恢复：**真的把进程打死**，再看目标文件是不是完整的。
//!
//! 单元测试能证明"并发读者永远不会读到半截"，但那是在同一个进程里。
//! 进程被强杀是**另一种**失败：没有 unwind、没有 `Drop`、没有任何收尾，
//! 所以它值得单独测一遍。
//!
//! 做法是用自己当写入器：起一个子进程（就是本测试二进制，靠环境变量
//! 进入"只写不读"的模式），让它疯狂原子写；父进程一边**高频采样目标文件
//! 的长度**，一边在某个时刻强杀它。
//!
//! 断言只有一条：**目标文件的长度从来不会小于一份完整载荷**。
//! 原子写下只可能观察到"上一份完整的"或"这一份完整的"。
//!
//! 为什么必须一边采样一边杀，而不是杀完再看：在 Windows 上，一次
//! `WriteFile` 对缓存文件是**整体生效**的，所以"写到一半"的长度根本
//! 不存在于文件系统里——只在杀完之后看，非原子的 `fs::write` 也能
//! 蒙混过关，测试就成了空转。但 `fs::write` 是"打开 → **截断** → 写入"，
//! 那个截断后的零长度窗口是真实存在的，只要采样够密就能抓到。

use std::fs;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 子进程模式的环境变量名（值为目标文件路径）。
const CHILD_ENV: &str = "STYX_CRASH_TEST_TARGET";

/// 填充部分的大小。载荷总长比它大一点。
const FILLER: usize = 512 * 1024;

/// 任何一份**完整**载荷的长度都大于这个下界；半截的（含被截断成 0）会小于它。
const MIN_COMPLETE_LEN: u64 = FILLER as u64;

/// 一回合内容：形状固定、体量够大。
fn payload(i: u64) -> String {
    format!("{{\"turn\":{i},\"filler\":\"{}\"}}", "x".repeat(FILLER))
}

/// 子进程模式：一直原子写，直到被杀。
fn child_writer(target: &str) {
    let mut i = 0u64;
    loop {
        // 写失败也无所谓（比如父进程已经在收尾），继续写下一份。
        let _ = styx_guard::fsutil::atomic_write(target, payload(i).as_bytes());
        i += 1;
    }
}

#[test]
fn a_killed_writer_never_leaves_a_torn_file() {
    // ---- 子进程模式 ----
    if let Ok(target) = std::env::var(CHILD_ENV) {
        child_writer(&target);
        return; // 正常情况走不到这里（会被杀）
    }

    // ---- 父进程模式 ----
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("styx-crash-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let target = dir.join("snap.json");
    let exe = std::env::current_exe().unwrap();

    styx_guard::fsutil::atomic_write(&target, payload(0).as_bytes()).unwrap();

    // 采样线程：记录**看到过的最小长度**与样本数。
    // 记录"最小"而不是"最后一次"，因为破坏性的一刻可能很短。
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new((u64::MAX, 0u64, 0u64))); // (最小长度, 样本数, 短样本数)
    let watcher = {
        let target = target.clone();
        let stop = Arc::clone(&stop);
        let stats = Arc::clone(&stats);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // 采不到就跳过：采样只是尽力而为，不该因为时机错位而误判。
                if let Ok(meta) = fs::metadata(&target) {
                    let len = meta.len();
                    let mut s = stats.lock().unwrap();
                    s.0 = s.0.min(len);
                    s.1 += 1;
                    if len < MIN_COMPLETE_LEN {
                        s.2 += 1;
                    }
                }
            }
        })
    };

    let mut child = Command::new(&exe)
        .args([
            "--exact",
            "a_killed_writer_never_leaves_a_torn_file",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("应当能起子进程");

    // 让它写一会儿：既让子进程真正跑起来（Windows 起进程本身要几十毫秒），
    // 也给采样线程攒够样本。
    std::thread::sleep(Duration::from_millis(700));
    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, Ordering::Relaxed);
    watcher.join().unwrap();

    let (min_len, samples, short) = *stats.lock().unwrap();
    assert!(
        samples > 200,
        "只采到 {samples} 个样本，测量太稀，这条测试没测到什么"
    );
    assert_eq!(
        short, 0,
        "有 {short} 次（共 {samples} 次）看到目标文件短于一份完整载荷，最短 {min_len} 字节——\
         写入过程中暴露了中间态，原子性被破坏了"
    );

    // 杀完之后再确认一次：收尾状态同样必须是完整的。
    let after = fs::read_to_string(&target).expect("目标文件不该消失");
    assert!(
        after.len() as u64 >= MIN_COMPLETE_LEN,
        "强杀之后目标文件只剩 {} 字节——原子性被破坏了",
        after.len()
    );

    let _ = fs::remove_dir_all(&dir);
}
