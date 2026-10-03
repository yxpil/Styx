#!/usr/bin/env python3
"""把 Styx 主仓库里的视觉部分物化成一个能独立编译、独立发布的仓库。

## 为什么是「物化」而不是 submodule / subtree

需求是：视觉要能单独发一个仓库，而角色扮演的主仓库**仍然包含全部内容**。
submodule 会让主仓库裸克隆变成半残（少一次 `--recursive` 就编译不过），
subtree 处理不了「两个前缀 + 需要改写清单文件」这件事。所以选最土也最直白的
办法：主仓库是**唯一事实来源**，这个脚本把它的一个子集复制出去。

代价是镜像里不能直接改代码（下次同步会覆盖）。这是刻意的取舍——两个仓库
都能裸克隆、都能直接编译，比省一次复制重要得多。

## 它到底做了什么

1. **算依赖闭包**。从 `crates/styx-vision` 出发，跟着 `[workspace.dependencies]`
   里带 `path` 的条目递归，把用到的内部 crate 全找出来。这一步是自动的，
   所以将来给视觉加一个内部依赖时，镜像不会静默地编译失败。
2. **生成根 `Cargo.toml`**。主仓库的两个 crate 都用 `version.workspace = true`
   这种继承写法，脱离工作区就编不过。生成的清单里 `[workspace.package]` 和
   `[profile.*]` 逐行照搬主仓库（只把 `repository` 改成这个仓库自己的地址），
   `[workspace.dependencies]` 只保留闭包内 crate 真正引用的那些。照搬而不是
   重写，意味着以后主仓库加了字段，镜像自动跟上。
3. **复制源码**。crate 与 `services/vision` 按原布局搬过去——布局不变，
   所以两边 README 里的相对链接都是对的。
4. **搬根文件**。`LICENSE` / `.gitattributes` / `.gitignore` 直接照抄主仓库，
   外加一份镜像专用的 `README.md` 与 CI 配置（在 `tools/vision-repo/`）。
5. **清理陈旧文件**。目标目录里不属于这次物化结果的文件会被删掉，
   这样主仓库里删掉的文件在镜像里也会消失。`.git` 永远不动。

## 用法

```bash
python tools/sync_vision_repo.py                       # 默认写到 ../styx-vision
python tools/sync_vision_repo.py --target D:/work/styx-vision
python tools/sync_vision_repo.py --check               # 只校验有没有漂移，不写盘
```

`--check` 会把物化结果先写进临时目录，再和目标的实际内容逐个文件比对字节。
适合放进 CI 或者提交前的钩子：镜像和主仓库不同步是件很容易发生、又很难
靠肉眼看出来的事。
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import sys
import tempfile
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent  # Styx/tools
ROOT = HERE.parent  # Styx
TEMPLATES = HERE / "vision-repo"

#: 镜像的入口 crate。依赖闭包从它出发算。
ENTRY_CRATES = ["crates/styx-vision"]

#: 与 crate 依赖无关、但要一起搬过去的目录（原布局照搬）。
EXTRA_DIRS = ["services/vision"]

#: 直接照抄主仓库版本的根文件。抄而不是各写一份，就没有失同步的可能。
COPY_FILES = ["LICENSE", ".gitattributes", ".gitignore"]

#: 物化时跳过的东西：本地环境、下载物、构建产物。
SKIP_DIRS = {"__pycache__", ".venv", ".uv-cache", "models", "target", ".git", ".pytest_cache"}

#: 永远不会被清理的文件（即使脚本不生成它们）。
#:
#: `Cargo.lock` 是 cargo 自己解析出来的，脚本不该去生成它（那意味着在这里跑
#: 一次构建），但它必须留在仓库里——否则每次同步都会把它删掉，镜像的构建
#: 每次都重新解析依赖版本。
KEEP_IF_PRESENT = {"Cargo.lock"}

#: 镜像仓库自己的地址（写进生成的 `[workspace.package] repository`）。
DEFAULT_REPO_URL = "https://github.com/yxpil/styx-vision.git"

DEP_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")


# --------------------------------------------------------------------------
# 读主仓库
# --------------------------------------------------------------------------


def load_toml(path: Path) -> dict:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def split_sections(text: str) -> list[tuple[str | None, list[str]]]:
    """把 TOML 顶层切成 ``[(表头, 行)]``，表头为 ``None`` 表示文件开头部分。

    这是个**够用就好**的切分：它逐行找 ``[表头]``，所以不处理多行字符串里
    出现方括号的情况。主仓库的根 `Cargo.toml` 里没有这种写法，而且这个
    脚本存在的意义正是为了让这类「够用就好」的取舍写得清楚、看得见。
    """
    sections: list[tuple[str | None, list[str]]] = []
    header: str | None = None
    lines: list[str] = []
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("[") and s.endswith("]"):
            sections.append((header, lines))
            header = s
            lines = []
        else:
            lines.append(line)
    sections.append((header, lines))
    return sections


def section_lines(sections, header: str) -> list[str]:
    for h, lines in sections:
        if h == header:
            return lines
    return []


def referenced_workspace_deps(crate_toml: dict) -> set[str]:
    """一个 crate 引用了哪些 `xxx.workspace = true` 形式的依赖。"""
    used: set[str] = set()
    for section in DEP_SECTIONS:
        for name, spec in (crate_toml.get(section) or {}).items():
            # `foo = { workspace = true }` 和 `foo.workspace = true` 在 tomllib
            # 里是同一个形状（后者会被解析成前者）。
            if isinstance(spec, dict) and spec.get("workspace") is True:
                used.add(name)
    return used


def dependency_closure() -> tuple[list[str], set[str]]:
    """算出要搬的 crate 目录，以及它们真正要用到的工作区依赖名。

    只有带 `path` 的工作区依赖才是「内部 crate」需要递归；像 `serde` 那种
    没有 `path` 的，只把名字记下来（要写进生成的 `[workspace.dependencies]`）。
    """
    root_manifest = load_toml(ROOT / "Cargo.toml")
    ws_deps = root_manifest.get("workspace", {}).get("dependencies", {}) or {}

    crates: list[str] = []
    needed: set[str] = set()
    seen: set[str] = set()

    queue = list(ENTRY_CRATES)
    while queue:
        rel = queue.pop(0)
        if rel in seen:
            continue
        seen.add(rel)
        crates.append(rel)

        manifest_path = ROOT / rel / "Cargo.toml"
        if not manifest_path.exists():
            raise SystemExit(f"找不到 {manifest_path}")
        for name in sorted(referenced_workspace_deps(load_toml(manifest_path))):
            needed.add(name)
            spec = ws_deps.get(name)
            if spec is None:
                raise SystemExit(
                    f"{rel} 引用了工作区依赖 {name}，但根 Cargo.toml 的 "
                    f"[workspace.dependencies] 里没有它。镜像没法自动处理这种情况，"
                    f"请先在主仓库里补上。"
                )
            # 工作区依赖有两种写法：`foo = "1"` 是纯字符串，
            # `foo = { version = "1", path = "..." }` 才是表。只有表才可能带 path。
            path = spec.get("path") if isinstance(spec, dict) else None
            if path and path not in seen:
                queue.append(path)

    return sorted(crates), needed


def filter_dep_lines(lines: list[str], keep: set[str]) -> list[str]:
    """只保留名字在 `keep` 里的依赖条目，注释与其他行原样照搬。

    段内一个条目的值可能是多行的，所以「缩进的后续行」跟着它所属的键一起
    保留或丢弃，而不是按行判断。
    """
    out: list[str] = []
    keeping = True
    for line in lines:
        s = line.strip()
        if s and not line[:1].isspace() and "=" in s:
            keeping = s.split("=", 1)[0].strip() in keep
        if keeping:
            out.append(line)
    return out


def has_meaningful_lines(lines: list[str]) -> bool:
    return any(l.strip() and not l.strip().startswith("#") for l in lines)


def generate_root_manifest(crates: list[str], needed: set[str], repo_url: str) -> str:
    """生成镜像的根 `Cargo.toml`。

    策略是「照搬 + 过滤」而不是「从解析结果重新渲染」：注释、字段顺序、
    以后主仓库新增的字段都会自动带上。
    """
    src = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    sections = split_sections(src)
    root_manifest = load_toml(ROOT / "Cargo.toml")
    resolver = root_manifest.get("workspace", {}).get("resolver", "2")

    parts: list[str] = []
    parts.append("### 这个文件由 Styx 主仓库的 tools/sync_vision_repo.py 生成。")
    parts.append("### 不要手改——下次同步会覆盖掉。要改请改主仓库的根 Cargo.toml。")
    parts.append("")
    parts.append("[workspace]")
    parts.append(f'resolver = "{resolver}"')
    parts.append("members = [")
    for c in crates:
        parts.append(f'    "{c}",')
    parts.append("]")
    parts.append("")

    pkg = section_lines(sections, "[workspace.package]")
    if pkg:
        parts.append("[workspace.package]")
        for line in pkg:
            if line.strip().startswith("repository"):
                parts.append(f'repository = "{repo_url}"')
            else:
                parts.append(line)
        parts.append("")

    dep_lines = filter_dep_lines(section_lines(sections, "[workspace.dependencies]"), needed)
    if has_meaningful_lines(dep_lines):
        parts.append("[workspace.dependencies]")
        parts.extend(dep_lines)
        parts.append("")

    for header, lines in sections:
        if header and header.startswith("[profile"):
            parts.append(header)
            parts.extend(lines)
            parts.append("")

    text = "\n".join(parts).rstrip() + "\n"
    while "\n\n\n" in text:
        text = text.replace("\n\n\n", "\n\n")
    return text


# --------------------------------------------------------------------------
# 物化
# --------------------------------------------------------------------------


def walk_files(base: Path) -> list[Path]:
    out: list[Path] = []
    for p in sorted(base.rglob("*")):
        if p.is_dir():
            continue
        rel = p.relative_to(base)
        if any(part in SKIP_DIRS for part in rel.parts):
            continue
        out.append(rel)
    return out


def materialize(target: Path, repo_url: str, quiet: bool = False) -> list[str]:
    """把镜像内容写进 `target`（会先清掉不属于本次结果的文件）。返回动作清单。"""
    actions: list[str] = []
    crates, needed = dependency_closure()

    if not quiet:
        print(f"依赖闭包：{len(crates)} 个 crate —— {', '.join(crates)}")
        print(f"工作区依赖：{len(needed)} 个 —— {', '.join(sorted(needed))}")

    # 先算清楚「应该有什么」，再动手，这样清理陈旧文件是安全的。
    expected: dict[Path, bytes | Path] = {}

    for c in crates:
        base = ROOT / c
        for rel in walk_files(base):
            expected[Path(c) / rel] = base / rel

    for d in EXTRA_DIRS:
        base = ROOT / d
        if not base.exists():
            raise SystemExit(f"找不到 {base}")
        for rel in walk_files(base):
            expected[Path(d) / rel] = base / rel

    for f in COPY_FILES:
        expected[Path(f)] = ROOT / f

    # 镜像专用的模板。文件在 tools/vision-repo/ 下的名字刻意与落点不同名，
    # 免得 `root-README.md` 这种「其实是镜像首页」的东西在源码树里被误当文档。
    for tmpl, dst in (
        ("root-README.md", Path("README.md")),
        ("ci.yml", Path(".github/workflows/ci.yml")),
    ):
        src = TEMPLATES / tmpl
        if not src.exists():
            raise SystemExit(f"缺少模板 {src}")
        expected[dst] = src

    expected[Path("Cargo.toml")] = generate_root_manifest(crates, needed, repo_url).encode("utf-8")

    target.mkdir(parents=True, exist_ok=True)

    # 1) 写
    for rel, src in sorted(expected.items()):
        out = target / rel
        data = src if isinstance(src, bytes) else src.read_bytes()
        out.parent.mkdir(parents=True, exist_ok=True)
        if out.exists() and out.read_bytes() == data:
            continue
        verb = "改" if out.exists() else "加"
        out.write_bytes(data)
        actions.append(f"{verb} {rel.as_posix()}")

    # 2) 删（`.git` 与构建产物永不触碰）
    #
    # 这一步曾经有个代价很大的 bug：它用 `target.rglob("*")` 遍历整个目录树，
    # 于是**把镜像自己的 `target/` 逐文件删掉了**——一次同步报出 1139 处改动，
    # 其中 1138 处是 cargo 的构建产物，而且耗时四分钟。于是每次同步都强制
    # 全量重编译。现在按目录剪枝，不进 `SKIP_DIRS`。
    skip = SKIP_DIRS | {".git"}
    for dirpath, dirnames, filenames in os.walk(target, topdown=True):
        base = Path(dirpath)
        rel_dir = base.relative_to(target)
        if any(part in skip for part in rel_dir.parts):
            dirnames[:] = []
            continue
        dirnames[:] = [d for d in dirnames if d not in skip]
        for name in filenames:
            rel = rel_dir / name
            if rel.as_posix() in KEEP_IF_PRESENT or rel in expected:
                continue
            (base / name).unlink()
            actions.append(f"删 {rel.as_posix()}")

    # 3) 收拾因此变空的目录。从最深的开始，否则父目录删不掉。
    for dirpath, _dirnames, _filenames in os.walk(target, topdown=False):
        base = Path(dirpath)
        rel_dir = base.relative_to(target)
        if not rel_dir.parts or any(part in skip for part in rel_dir.parts):
            continue
        if not any(base.iterdir()):
            base.rmdir()

    return actions


# --------------------------------------------------------------------------
# 校验
# --------------------------------------------------------------------------


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(base: Path) -> dict[str, str]:
    """目录里「脚本负责的那部分」内容的指纹。

    必须和 `materialize` 的清理逻辑用同一套跳过规则，否则 `--check` 会把
    构建产物和 `Cargo.lock` 报成漂移——而它们本来就不该由脚本管。
    """
    skip = SKIP_DIRS | {".git"}
    out: dict[str, str] = {}
    for dirpath, dirnames, filenames in os.walk(base, topdown=True):
        here = Path(dirpath)
        rel_dir = here.relative_to(base)
        if any(part in skip for part in rel_dir.parts):
            dirnames[:] = []
            continue
        dirnames[:] = [d for d in dirnames if d not in skip]
        for name in filenames:
            rel = rel_dir / name
            if rel.as_posix() in KEEP_IF_PRESENT:
                continue
            out[rel.as_posix()] = digest(here / name)
    return out


def check(target: Path, repo_url: str) -> int:
    if not target.exists():
        print(f"目标 {target} 还不存在，先去物化一次再校验。")
        return 1

    with tempfile.TemporaryDirectory(prefix="styx-vision-check-") as tmp:
        fresh = Path(tmp)
        materialize(fresh, repo_url, quiet=True)
        want = snapshot(fresh)
        have = snapshot(target)

        only_src = sorted(set(want) - set(have))
        only_dst = sorted(set(have) - set(want))
        changed = sorted(k for k in set(want) & set(have) if want[k] != have[k])

    for k in only_src:
        print(f"  镜像里缺    {k}")
    for k in only_dst:
        print(f"  镜像里多    {k}")
    for k in changed:
        print(f"  内容不一样  {k}")

    total = len(only_src) + len(only_dst) + len(changed)
    if total:
        print()
        print(f"{total} 处漂移。跑一次不带 --check 的同步即可修正。")
        return 1
    print(f"镜像与主仓库一致（{len(want)} 个文件）。")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="把 Styx 的视觉部分物化成独立仓库",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--target", default="../styx-vision", help="目标目录（默认 ../styx-vision）")
    ap.add_argument("--check", action="store_true", help="只校验有没有漂移，不写盘")
    ap.add_argument("--repo-url", default=DEFAULT_REPO_URL, help="写进生成的清单里的仓库地址")
    ap.add_argument("-q", "--quiet", action="store_true", help="只打印改动的文件")
    args = ap.parse_args(argv)

    target = Path(args.target)
    if not target.is_absolute():
        target = (ROOT / target).resolve()

    # 镜像不能落在主仓库里面：那会在工作树里嵌一个 .git，`git status`
    # 会把它当成一个未跟踪的怪物，而且主仓库自己的 target/ 也可能被污染。
    if ROOT == target or ROOT in target.parents:
        raise SystemExit(f"目标 {target} 在主仓库内部，不行。换个地方。")

    if args.check:
        return check(target, args.repo_url)

    actions = materialize(target, args.repo_url, quiet=args.quiet)
    print()
    if actions:
        for a in actions:
            print(f"  {a}")
        print()
        print(f"物化完成：{len(actions)} 处改动 → {target}")
    else:
        print(f"已经是最新的，没有改动 → {target}")
    print()
    print("接下来（脚本本身不碰 git，免得在你不想要的时候产生提交）：")
    print(f"  cd {target}")
    print("  git add -A && git commit -m '...' && git push")
    return 0


if __name__ == "__main__":
    sys.exit(main())
