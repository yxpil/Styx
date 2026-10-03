//! 集成测试：原子写工具（从 crate 外部视角）。
//!
//! 验证 `atomic_write` / `read_if_exists` 的真实行为：覆盖写完整落盘、
//! 读缺失文件返回 None、成功写入后不残留临时文件。

use std::fs;
use std::path::PathBuf;
use styx_guard::fsutil::{atomic_write, read_if_exists};

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "styx-guard-it-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn atomic_write_roundtrip_and_overwrite() {
    let dir = tmpdir("write");
    let f = dir.join("snap.json");

    atomic_write(&f, b"first").unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "first");

    atomic_write(&f, b"second-and-longer").unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "second-and-longer");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_if_exists_missing_returns_none() {
    let dir = tmpdir("read");
    let f = dir.join("nope.json");
    assert!(read_if_exists(&f).unwrap().is_none());

    atomic_write(&f, b"hello").unwrap();
    assert_eq!(read_if_exists(&f).unwrap().as_deref(), Some("hello"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_temp_files_leftover_after_writes() {
    let dir = tmpdir("clean");
    let f = dir.join("snap.json");
    atomic_write(&f, b"x").unwrap();
    atomic_write(&f, b"y").unwrap();

    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["snap.json".to_string()], "不应残留 .tmp 临时文件");

    let _ = fs::remove_dir_all(&dir);
}
