#!/usr/bin/env python3
"""# 下载 Styx 视觉 sidecar 需要的模型

两个模型都是**可选**的，只下其中一个也能用：

| 文件 | 大小 | 作用 |
|---|---|---|
| `models/yolov8n.onnx` | 13 MB | 目标检测（"画面里有一个人"） |
| `models/tagger_moat.onnx` + `selected_tags.csv` | 311 MB + 253 KB | 属性反推（"1girl, long_hair, smile"） |

> 反推模型比检测模型大一个数量级，这是正常的：它是 9083 类的多标签
> 分类器，而检测器只有 80 类。嫌大就换 `--only detector`，只丢掉的
> 是「细粒度属性」这一层，检测和本地分析照常。

## 关于下载源

- 检测模型走 GitHub Releases（ultralytics 官方资产，直连通常没问题）；
- 标签模型在 HuggingFace 上，国内直连经常失败，所以默认走 `hf-mirror.com`。
  想走官方源就加 `--hf-base https://huggingface.co`。

## 用法

```bash
python fetch_models.py                 # 两个都下
python fetch_models.py --only tagger   # 只下标签模型
python fetch_models.py --force         # 已经存在的也重下
python fetch_models.py --check         # 只检查现有文件是否完整
```

## 为什么不用 huggingface_hub

因为它是另一个依赖，而这个脚本要做的事用 `urllib` + `tarfile` 就够了。
少一个依赖就少一种装不上的可能——上一轮 FunASR 就是栽在这上面。
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import sys
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_MODELS = os.path.join(HERE, "models")

YOLO_URL = "https://github.com/ultralytics/assets/releases/download/v8.4.0/yolov8n.onnx"

#: 越小越快。moat 是三个 WD14 v2 版本里最小的（MobileNetV3 主干），
#: 在 CPU 上单张图不到一秒，对"发张图给角色看"这个场景够用了。
#: 想更准就换 convnext / swinv2——换完把文件名对齐即可，代码不用动。
TAGGER_REPO = "SmilingWolf/wd-v1-4-moat-tagger-v2"
TAGGER_FILES = [("model.onnx", "tagger_moat.onnx"), ("selected_tags.csv", "selected_tags.csv")]

UA = "styx-vision-fetch/0.1 (+local model downloader)"


def human(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.1f} {unit}"
        n /= 1024  # type: ignore[assignment]
    return f"{n:.1f} GB"


def download(url: str, dest: str, force: bool = False) -> bool:
    """下载到 `dest`。先写 `.part` 再改名，中断不会留下半个文件冒充完整的。"""
    if os.path.exists(dest) and not force:
        print(f"  已存在，跳过：{dest}（{human(os.path.getsize(dest))}）")
        return True
    tmp = dest + ".part"
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            total = int(resp.headers.get("Content-Length") or 0)
            got = 0
            with open(tmp, "wb") as fh:
                while True:
                    chunk = resp.read(1 << 16)
                    if not chunk:
                        break
                    fh.write(chunk)
                    got += len(chunk)
                    if total:
                        pct = got * 100 // total
                        sys.stdout.write(f"\r  {pct:3d}%  {human(got)} / {human(total)}")
                        sys.stdout.flush()
        print()
    except (urllib.error.URLError, OSError) as exc:
        print(f"\n  下载失败：{exc}")
        if os.path.exists(tmp):
            os.remove(tmp)
        return False

    if total and got != total:
        print(f"  大小不符（拿到 {got}，期望 {total}），丢弃")
        os.remove(tmp)
        return False
    os.replace(tmp, dest)
    print(f"  完成：{dest}（{human(os.path.getsize(dest))}）")
    return True


def sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def check(models: str) -> int:
    """只做一件事：确认现有文件**看起来**是完整的。

    刻意不做"能不能推理"的验证——那需要加载 onnxruntime，而这个脚本的
    职责只是下载。真正的推理验证是 `python app.py --check` 的事。
    """
    print(f"模型目录：{models}")
    if not os.path.isdir(models):
        print("  目录不存在")
        return 1

    ok = 0
    yolo = os.path.join(models, "yolov8n.onnx")
    if os.path.exists(yolo):
        size = os.path.getsize(yolo)
        good = size > 10_000_000
        print(f"  [{'OK' if good else '!!'}] yolov8n.onnx  {human(size)}")
        ok += good
    else:
        print("  [--] yolov8n.onnx  缺失（目标检测不可用）")

    tagger = next(
        (os.path.join(models, f) for f in
         ("tagger_moat.onnx", "tagger_convnext.onnx", "tagger_swinv2.onnx")
         if os.path.exists(os.path.join(models, f))),
        None,
    )
    vocab = os.path.join(models, "selected_tags.csv")
    if tagger:
        size = os.path.getsize(tagger)
        good = size > 20_000_000
        print(f"  [{'OK' if good else '!!'}] {os.path.basename(tagger)}  {human(size)}")
        ok += good
    else:
        print("  [--] tagger_*.onnx  缺失（属性反推不可用）")
    if os.path.exists(vocab):
        with open(vocab, "r", encoding="utf-8") as fh:
            lines = sum(1 for _ in fh)
        print(f"  [OK] selected_tags.csv  {lines - 1} 个标签")
    else:
        print("  [--] selected_tags.csv  缺失（有模型也没用，标签下标定不下来）")

    print("  有模型可用" if ok else "  一个模型都没有，服务会起但每个请求都返回 503")
    return 0 if ok else 1


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="下载 Styx 视觉 sidecar 的模型")
    ap.add_argument("--models", default=DEFAULT_MODELS)
    ap.add_argument("--only", choices=["detector", "tagger"], help="只下其中一个")
    ap.add_argument("--force", action="store_true", help="已存在也重新下载")
    ap.add_argument("--check", action="store_true", help="只检查现有文件")
    ap.add_argument(
        "--hf-base",
        default="https://hf-mirror.com",
        help="HuggingFace 基址；国内直连 huggingface.co 常常失败",
    )
    args = ap.parse_args(argv)

    os.makedirs(args.models, exist_ok=True)

    if args.check:
        return check(args.models)

    failed = []
    if args.only in (None, "detector"):
        print("目标检测模型 yolov8n.onnx")
        if not download(YOLO_URL, os.path.join(args.models, "yolov8n.onnx"), args.force):
            failed.append("yolov8n.onnx")

    if args.only in (None, "tagger"):
        print(f"属性反推模型 {TAGGER_REPO}")
        for remote, local in TAGGER_FILES:
            url = f"{args.hf_base.rstrip('/')}/{TAGGER_REPO}/resolve/main/{remote}"
            if not download(url, os.path.join(args.models, local), args.force):
                failed.append(local)

    print()
    if failed:
        print(f"以下文件没拿到：{'、'.join(failed)}")
        print(f"没拿到的部分对应的功能会不可用，但 Styx 的本地图像分析不受影响——")
        print(f"删掉整个 sidecar，系统只是少说几句话。")
    print("用 `python app.py --check` 确认服务能真的加载起来。")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
