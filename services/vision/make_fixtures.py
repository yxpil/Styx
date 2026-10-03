#!/usr/bin/env python3
"""生成基线 JPEG 的参考数据，供 `crates/styx-vision/tests/jpeg_reference.rs` 比对。

## 为什么要留参考数据在仓库里

因为"我实现了一个 JPEG 解码器"和"我这个 JPEG 解码器是对的"是两件事。
JPEG 的坑都非常隐蔽：色度升采样差半格、DC 差分忘了累加、0xFF00 填充没处理、
IDCT 电平偏移漏了——它们都会**解出一张能看的图**，只是颜色糊了、暗部偏了、
或者右边缘错位。没有逐像素比对，这些错误可以一路活到线上。

参考数据由 Pillow 生成（它是 libjpeg 的封装，可以当权威），覆盖：

| 文件 | 覆盖的路径 |
|---|---|
| `rgb_444_q92` | 4:4:4，色度不降采样 |
| `ycbcr_420_q75` | 4:2:0，最常见的默认 |
| `ycbcr_422_q85` | 4:2:2，只横向降采样 |
| `gray_q80` | 1 个分量，灰度直通 |
| `tiny_8x8` | 恰好一个块，边界条件 |
| `odd_37x19` | 奇数尺寸，MCU 需要补边 |
| `wide_321x9` | 极端长宽比 + 补边 |
| `flat_white` | 全白（DC 只有一个非零系数） |
| `flat_dark` | 全暗且带蓝（低频色度，最容易看出 DC 漂移） |
| `hires_q95` | 160×120 高画质，块数够多、误差不会被平摊掉 |
| `progressive.jpg` | **故意**留一份渐进式，验证我们"如实拒绝"而不是解出乱码 |

每张图同时写一份 `.raw`（PIL 解出的 RGB 字节）与 `.json`（尺寸与统计量）。

用法：

```bash
python make_fixtures.py            # 写入 tests/fixtures/jpeg/
python make_fixtures.py --out DIR
```
"""

from __future__ import annotations

import argparse
import json
import os

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover
    raise SystemExit("需要 Pillow：pip install pillow")

HERE = os.path.dirname(os.path.abspath(__file__))
# HERE 是 <repo>/services/vision，所以要先退两级才到仓库根
DEFAULT_OUT = os.path.normpath(
    os.path.join(HERE, "..", "..", "crates", "styx-vision", "tests", "fixtures", "jpeg")
)


def gradient(w: int, h: int) -> Image.Image:
    """一张有横向色相渐变 + 纵向明暗渐变的图。

    这种图能同时暴露三类问题：色度升采样（横向彩色条纹会糊）、
    IDCT 精度（纵向平滑渐变会出现块状台阶）、DC 差分（整体明暗漂移）。
    """
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            px[x, y] = (
                int(255 * x / max(1, w - 1)),
                int(255 * y / max(1, h - 1)),
                int(255 * (1 - (x + y) / max(1, w + h - 2))),
            )
    return img


def blocks(w: int, h: int) -> Image.Image:
    """硬边色块：炸 AC 系数，能暴露 VLC 解码或反量化的问题。"""
    img = Image.new("RGB", (w, h), (250, 250, 250))
    d = ImageDraw.Draw(img)
    d.rectangle((0, 0, w // 3, h // 2), fill=(200, 30, 40))
    d.rectangle((w // 3, 0, 2 * w // 3, h // 2), fill=(30, 180, 70))
    d.rectangle((2 * w // 3, 0, w - 1, h // 2), fill=(40, 70, 210))
    d.ellipse((2, h // 2 + 2, w // 2, h - 2), fill=(10, 10, 10))
    return img


CASES = [
    ("rgb_444_q92", lambda: gradient(64, 48), dict(quality=92, subsampling=0)),
    ("ycbcr_420_q75", lambda: gradient(64, 48), dict(quality=75, subsampling=2)),
    ("ycbcr_422_q85", lambda: blocks(64, 48), dict(quality=85, subsampling=1)),
    ("gray_q80", lambda: gradient(64, 48).convert("L"), dict(quality=80)),
    ("tiny_8x8", lambda: blocks(8, 8), dict(quality=90, subsampling=0)),
    ("odd_37x19", lambda: blocks(37, 19), dict(quality=88, subsampling=2)),
    ("wide_321x9", lambda: gradient(321, 9), dict(quality=80, subsampling=2)),
    ("flat_white", lambda: Image.new("RGB", (24, 16), (255, 255, 255)), dict(quality=95)),
    ("flat_dark", lambda: Image.new("RGB", (24, 16), (18, 22, 40)), dict(quality=95)),
    ("hires_q95", lambda: gradient(160, 120), dict(quality=95, subsampling=0)),
]


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="生成 JPEG 参考数据")
    ap.add_argument("--out", default=DEFAULT_OUT)
    args = ap.parse_args(argv)
    os.makedirs(args.out, exist_ok=True)

    made = []
    for name, build, opts in CASES:
        src = build()
        # 参考值是**无损重开**的结果：PIL 保存到内存再读回来，
        # 就是 libjpeg 的解码输出。用原图当参考会把自己编码器的
        # 有损误差算到解码器头上。
        jpg_path = os.path.join(args.out, f"{name}.jpg")
        src.save(jpg_path, format="JPEG", **opts)
        decoded = Image.open(jpg_path)
        decoded.load()
        rgb = decoded.convert("RGB")

        raw_path = os.path.join(args.out, f"{name}.raw")
        with open(raw_path, "wb") as fh:
            fh.write(rgb.tobytes())

        px = list(rgb.getdata())
        n = len(px)
        luma = [0.299 * r + 0.587 * g + 0.114 * b for r, g, b in px]
        meta = {
            "file": f"{name}.jpg",
            "raw": f"{name}.raw",
            "width": rgb.width,
            "height": rgb.height,
            "jpeg_size": os.path.getsize(jpg_path),
            "mean_luma": round(sum(luma) / n / 255.0, 6),
            "mean_rgb": [
                round(sum(p[0] for p in px) / n, 4),
                round(sum(p[1] for p in px) / n, 4),
                round(sum(p[2] for p in px) / n, 4),
            ],
            "options": {k: v for k, v in opts.items()},
            "mode": decoded.mode,
        }
        with open(os.path.join(args.out, f"{name}.json"), "w", encoding="utf-8") as fh:
            json.dump(meta, fh, ensure_ascii=False, indent=2)
        made.append(meta)
        print(
            f"  {name:16s} {rgb.width:4d}×{rgb.height:<4d} "
            f"{meta['jpeg_size']:6d}B  mean_luma={meta['mean_luma']:.4f}  {opts}"
        )

    # 渐进式：**故意**生成一份，用来验证我们"如实拒绝"而不是解出乱码
    prog_path = os.path.join(args.out, "progressive.jpg")
    gradient(64, 48).save(prog_path, format="JPEG", quality=85, progressive=True)
    im = Image.open(prog_path)
    im.load()
    with open(os.path.join(args.out, "progressive.json"), "w", encoding="utf-8") as fh:
        json.dump(
            {
                "file": "progressive.jpg",
                "width": im.width,
                "height": im.height,
                "expect": "unsupported",
            },
            fh,
            ensure_ascii=False,
            indent=2,
        )
    print(f"  {'progressive':16s} {im.width:4d}×{im.height:<4d} （应当被拒绝）")

    index = {
        "generated_by": "Pillow（libjpeg 封装），见 make_fixtures.py",
        "cases": made,
    }
    with open(os.path.join(args.out, "index.json"), "w", encoding="utf-8") as fh:
        json.dump(index, fh, ensure_ascii=False, indent=2)

    print(f"\n共 {len(made)} 个基线样例 + 1 个渐进式样例，写入 {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
