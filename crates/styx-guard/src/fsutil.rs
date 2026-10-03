//! 文件写入的原子性。
//!
//! `fs::write` 是"打开 → 截断 → 写入"，**中间任何一刻断电、崩溃、被
//! 强杀，目标文件就停在半截**。对一个 checkpoint 来说，半截比没有更糟：
//! 没有的话至少能从头再来，半截的会让下次启动直接解析失败——而且旧的
//! 完整版本已经被截断掉了。
//!
//! 原子写的做法是：写到**同目录**的临时文件 → `fsync` → `rename` 覆盖。
//! 同一文件系统内的 `rename` 是原子的，于是任何观察者看到的要么是
//! 完整的旧内容，要么是完整的新内容，不存在中间态。
//!
//! # 崩溃会留下一具临时文件
//!
//! 进程恰好死在"建好临时文件"和"改名"之间时，目标文件不受影响（还是旧的
//! 完整内容），但会留下一个 `.<名字>.<pid>.<纳秒>.<序号>.tmp`。
//!
//! 这里**刻意不做自动清理**。要判断"这具临时文件还有没有人要"就得知道
//! 写它的进程是否还活着，而 pid 会被复用；判错的代价是删掉另一个进程
//! 正在写的临时文件，让它的 `rename` 失败——那是在**制造**错误，
//! 比多几 KB 的僵尸糟得多。僵尸不参与任何读取路径，留着无害。
//!
//! 真要清（比如长期运行的服务重启时），应当在**能确认没有其它写入者**的
//! 时机做，而不是在每次写入里顺手删。

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 同一次运行的临时文件计数，避免同一纳秒内的并发写撞名。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// 原子地把 `bytes` 写进 `path`（覆盖已有内容）。
pub fn atomic_write(path: impl AsRef<Path>, bytes: &[u8]) -> std::io::Result<()> {
    let path = path.as_ref();
    let tmp = temp_sibling(path);

    // 先落盘，再改名。少了 `sync_all`，`rename` 之后仍然可能断电丢内容——
    // 那样"原子"只保证了顺序，没保证持久。
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 改名失败就别把临时文件留在人家目录里。
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 读出 `path`；不存在时返回 `None`。
pub fn read_if_exists(path: impl AsRef<Path>) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path.as_ref()) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 同目录的临时文件名。
///
/// **必须同目录**：跨目录（更别说跨设备）的 `rename` 会退化成
/// "复制 + 删除"，那就不再是原子的——这正是这个函数不放进系统临时目录的原因。
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_string());
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.{pid}.{nanos}.{seq}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "styx-fsutil-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn writes_and_overwrites_the_target() {
        let dir = tmpdir("basic");
        let f = dir.join("snap.json");

        atomic_write(&f, b"first").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "first");

        atomic_write(&f, b"second-and-longer").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "second-and-longer");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_no_temp_files_behind() {
        let dir = tmpdir("clean");
        let f = dir.join("snap.json");
        atomic_write(&f, b"x").unwrap();
        atomic_write(&f, b"y").unwrap();

        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["snap.json".to_string()], "不该留下临时文件");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_if_exists_distinguishes_missing_from_broken() {
        let dir = tmpdir("read");
        let f = dir.join("nope.json");
        assert!(read_if_exists(&f).unwrap().is_none());

        atomic_write(&f, b"hello").unwrap();
        assert_eq!(read_if_exists(&f).unwrap().as_deref(), Some("hello"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 真正的原子性主张：**一个并发的读者永远不会看到半截内容。**
    ///
    /// 这条测试不是在验证"我们调用了 rename"，而是在验证那个承诺本身。
    /// 拿 `fs::write` 做同样的事，用足够大的内容就能读到半截——
    /// 也就是说这条测试确实在测东西。
    #[test]
    fn a_concurrent_reader_never_sees_a_partial_file() {
        let dir = tmpdir("atomic");
        let f = dir.join("snap.json");

        let payloads: Vec<String> = (0..4)
            .map(|i| format!("payload-{i}-").repeat(20_000))
            .collect();
        atomic_write(&f, payloads[0].as_bytes()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let f = f.clone();
            let stop = Arc::clone(&stop);
            let expected = payloads.clone();
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(text) = fs::read_to_string(&f) {
                        assert!(
                            expected.iter().any(|p| p == &text),
                            "读到了既不是旧值也不是新值的内容（长度 {}）——原子性被破坏了",
                            text.len()
                        );
                        reads += 1;
                    }
                }
                reads
            })
        };

        let writers: Vec<_> = payloads
            .clone()
            .into_iter()
            .map(|p| {
                let f = f.clone();
                std::thread::spawn(move || {
                    for _ in 0..12 {
                        atomic_write(&f, p.as_bytes()).unwrap();
                    }
                })
            })
            .collect();

        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let reads = reader.join().unwrap();
        assert!(reads > 0, "读线程应当至少成功读过一次");

        let _ = fs::remove_dir_all(&dir);
    }
}
