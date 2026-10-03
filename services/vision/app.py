#!/usr/bin/env python3
"""# Styx 视觉 sidecar

给 [`styx-vision`](../../crates/styx-vision) 的可选后端提供两个本地端点：

| 端点 | 作用 | 对端的 Rust 类型 |
|---|---|---|
| `GET  /health` | 报告哪些模型真的加载成功了 | — |
| `POST /detect` | YOLOv8n ONNX 目标检测 | `RemoteVision` |
| `POST /tag`    | WD14 属性标签反推（"提示词反推"） | `RemoteTagger` |

## 这个服务是**可选**的，而且它自己也知道这一点

Rust 侧的四层结构是「本地事实 → 检测 → 反推 → 视觉模型」，本地事实永远
可用。所以这个服务的正确行为不是"尽量假装自己能干活"，而是**如实报告
自己有什么**：

- 模型文件不存在 → `/health` 里 `ok: false`，对应端点返回 503 与一句人话，
  Rust 侧会把它变成描述里的一条 note，回合照常进行；
- 模型存在但推理炸了 → 返回 500 + 异常类型，同样只变成一条 note。

**绝对不要把"模型没加载"伪装成"画面里什么都没有"。** 那会让角色得到
一个完全错误的印象（"图里没东西"），而真相是"我们没看清楚"。

## 运行

```bash
python app.py                      # 默认 127.0.0.1:8420，模型从 ./models 找
python app.py --models D:/models   # 指定模型目录
python app.py --port 9000
python fetch_models.py             # 一键下载两个模型
```

只依赖 `onnxruntime` + `numpy` + `Pillow`，都是纯 wheel，不拉 torch。

## 为什么不用 FastAPI / Flask

因为这一个文件就能说清楚的事，不值得再引入一个 web 框架：`http.server`
是标准库，端点是三个，请求体是一段 JSON。少一个依赖就少一种装不上的可能。
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import sys
import threading
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

try:
    import numpy as np
    from PIL import Image
except ImportError as exc:  # pragma: no cover
    sys.stderr.write(
        "缺少依赖：%s\n请先跑 `python fetch_models.py`，或手动装：\n"
        "  pip install onnxruntime numpy pillow\n" % exc
    )
    raise SystemExit(2)

try:
    import onnxruntime as ort
except ImportError as exc:  # pragma: no cover
    sys.stderr.write("缺少 onnxruntime：pip install onnxruntime\n(%s)\n" % exc)
    raise SystemExit(2)


# --------------------------------------------------------------------- 常量

VERSION = "0.1.0"

#: YOLOv8 的输入边长。导出成 ONNX 时固定为 640，改成别的值要重新导出。
YOLO_INPUT = 640

#: 检出阈值。0.25 是 ultralytics 的默认值，实际用下来对小物体偏严、
#: 对大物体偏松，但对"画面里有什么"这个问题足够了。
YOLO_CONF = 0.25

#: NMS 的 IoU 阈值。
YOLO_IOU = 0.45

#: WD14 的输入边长。
TAGGER_INPUT = 448

#: COCO 80 类。顺序必须和 YOLOv8 的训练标签一致——错了会让"猫"变成"狗"，
#: 而且是静默的，没人会发现。
COCO80 = [
    "person", "bicycle", "car", "motorcycle", "airplane", "bus", "train", "truck",
    "boat", "traffic light", "fire hydrant", "stop sign", "parking meter", "bench",
    "bird", "cat", "dog", "horse", "sheep", "cow", "elephant", "bear", "zebra",
    "giraffe", "backpack", "umbrella", "handbag", "tie", "suitcase", "frisbee",
    "skis", "snowboard", "sports ball", "kite", "baseball bat", "baseball glove",
    "skateboard", "surfboard", "tennis racket", "bottle", "wine glass", "cup",
    "fork", "knife", "spoon", "bowl", "banana", "apple", "sandwich", "orange",
    "broccoli", "carrot", "hot dog", "pizza", "donut", "cake", "chair", "couch",
    "potted plant", "bed", "dining table", "toilet", "tv", "laptop", "mouse",
    "remote", "keyboard", "cell phone", "microwave", "oven", "toaster", "sink",
    "refrigerator", "book", "clock", "vase", "scissors", "teddy bear",
    "hair drier", "toothbrush",
]

#: COCO 80 的中文名。给不出来的一律退回英文，宁可混着也不能空着——
#: 空字符串会让 Rust 侧的 `display_name()` 也无法回退。
COCO80_ZH = {
    "person": "人", "bicycle": "自行车", "car": "汽车", "motorcycle": "摩托车",
    "airplane": "飞机", "bus": "公交车", "train": "火车", "truck": "卡车",
    "boat": "船", "traffic light": "红绿灯", "fire hydrant": "消防栓",
    "stop sign": "停车标志", "parking meter": "停车计时器", "bench": "长椅",
    "bird": "鸟", "cat": "猫", "dog": "狗", "horse": "马", "sheep": "羊",
    "cow": "牛", "elephant": "大象", "bear": "熊", "zebra": "斑马",
    "giraffe": "长颈鹿", "backpack": "背包", "umbrella": "伞", "handbag": "手提包",
    "tie": "领带", "suitcase": "行李箱", "frisbee": "飞盘", "skis": "滑雪板",
    "snowboard": "单板滑雪板", "sports ball": "球", "kite": "风筝",
    "baseball bat": "棒球棒", "baseball glove": "棒球手套", "skateboard": "滑板",
    "surfboard": "冲浪板", "tennis racket": "网球拍", "bottle": "瓶子",
    "wine glass": "酒杯", "cup": "杯子", "fork": "叉子", "knife": "刀",
    "spoon": "勺子", "bowl": "碗", "banana": "香蕉", "apple": "苹果",
    "sandwich": "三明治", "orange": "橙子", "broccoli": "西兰花", "carrot": "胡萝卜",
    "hot dog": "热狗", "pizza": "披萨", "donut": "甜甜圈", "cake": "蛋糕",
    "chair": "椅子", "couch": "沙发", "potted plant": "盆栽", "bed": "床",
    "dining table": "餐桌", "toilet": "马桶", "tv": "电视", "laptop": "笔记本电脑",
    "mouse": "鼠标", "remote": "遥控器", "keyboard": "键盘", "cell phone": "手机",
    "microwave": "微波炉", "oven": "烤箱", "toaster": "烤面包机", "sink": "水槽",
    "refrigerator": "冰箱", "book": "书", "clock": "钟", "vase": "花瓶",
    "scissors": "剪刀", "teddy bear": "毛绒熊", "hair drier": "吹风机",
    "toothbrush": "牙刷",
}

#: WD14 词表里这几类的标签**不是**画面属性，是数据集本身的标注元信息。
#: `rating_questionable` 这种标签说的是"这张图的来源分级"，把它当成
#: 画面内容喂给角色会非常糟糕。所以按 category 过滤掉。
TAGGER_SKIP_CATEGORIES = {9}

#: category 9 的 4 个 rating 标签名。它们必须排在词表最前面——
#: 这既是 WD 系列的固定约定，也是我们用来发现"词表被重排过"的依据。
TAGGER_RATING_NAMES = ["general", "sensitive", "questionable", "explicit"]


# ------------------------------------------------------------------ 模型封装

class Unavailable(Exception):
    """模型没准备好。这个是**可预期**的状态，不是崩溃。"""


class Detector:
    """YOLOv8n ONNX 目标检测。"""

    def __init__(self, path: str):
        self.path = path
        opts = ort.SessionOptions()
        # 单线程 + 关掉并行执行：这个服务是给一个聊天回合做陪衬的，
        # 抢满 CPU 会让语言模型的推理变慢，反而更难受。
        opts.intra_op_num_threads = max(1, (os.cpu_count() or 4) // 2)
        opts.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.session = ort.InferenceSession(path, opts, providers=["CPUExecutionProvider"])
        spec = self.session.get_inputs()[0]
        self.input_name = spec.name
        self.layout, self.input_size = _read_input_layout(spec.shape, YOLO_INPUT)

    def detect(self, image: Image.Image) -> list[dict]:
        letterboxed, scale, pad = self._letterbox(image)
        # YOLOv8 要的是 RGB、0..1、CHW
        arr = np.asarray(letterboxed, dtype=np.float32) / 255.0
        if self.layout == "nchw":
            arr = np.transpose(arr, (2, 0, 1))
        arr = arr[None, ...]
        out = self.session.run(None, {self.input_name: arr})[0]

        # [1, 84, 8400] → [8400, 84]；有些导出是 [1, 8400, 84]，按尺寸判一下
        pred = out[0]
        if pred.shape[0] < pred.shape[1]:
            pred = pred.T
        boxes = pred[:, :4]          # cx, cy, w, h
        scores = pred[:, 4:]         # 80 个类别的分数
        best = scores.argmax(axis=1)
        conf = scores[np.arange(len(scores)), best]

        keep = conf >= YOLO_CONF
        if not keep.any():
            return []
        boxes, conf, best = boxes[keep], conf[keep], best[keep]

        xyxy = np.empty_like(boxes)
        xyxy[:, 0] = boxes[:, 0] - boxes[:, 2] / 2
        xyxy[:, 1] = boxes[:, 1] - boxes[:, 3] / 2
        xyxy[:, 2] = boxes[:, 0] + boxes[:, 2] / 2
        xyxy[:, 3] = boxes[:, 1] + boxes[:, 3] / 2

        # **按类别**各自做 NMS，而不是全局做一遍。
        # 全局 NMS 会让"坐在椅子上的人"和"椅子"互相压制：两个框的 IoU 很高，
        # 分高的那个会把分低的整个抹掉，于是画面里就只剩一样东西了。
        idx = []
        for cls in np.unique(best):
            sel = np.where(best == cls)[0]
            for i in _nms(xyxy[sel], conf[sel], YOLO_IOU):
                idx.append(int(sel[i]))

        results = []
        for i in idx:
            x1, y1, x2, y2 = xyxy[i]
            # 去掉 letterbox 补的边，再缩回原图尺寸
            x1 = (x1 - pad[0]) / scale
            y1 = (y1 - pad[1]) / scale
            x2 = (x2 - pad[0]) / scale
            y2 = (y2 - pad[1]) / scale
            x1, y1 = max(0.0, x1), max(0.0, y1)
            x2, y2 = min(float(image.width), x2), min(float(image.height), y2)
            if x2 <= x1 or y2 <= y1:
                continue
            name = COCO80[int(best[i])] if best[i] < len(COCO80) else str(int(best[i]))
            # 每个数都显式过一遍 float()：numpy 的标量（甚至 round() 的结果）
            # 都还是 np.float32，直接塞进 json.dumps 会 500 整个端点。
            ix1, iy1, ix2, iy2 = float(x1), float(y1), float(x2), float(y2)
            iw, ih = float(image.width), float(image.height)
            results.append({
                "label": name,
                "label_zh": COCO80_ZH.get(name, ""),
                # 归一化坐标：Rust 侧不用知道原图尺寸就能说"在左上角"
                "box": [
                    round(ix1 / iw, 4), round(iy1 / ih, 4),
                    round(ix2 / iw, 4), round(iy2 / ih, 4),
                ],
                "area": round((ix2 - ix1) * (iy2 - iy1) / (iw * ih), 4),
                "confidence": round(float(conf[i]), 4),
            })
        results.sort(key=lambda r: r["confidence"], reverse=True)
        return results[:20]

    def _letterbox(self, image: Image.Image):
        """等比缩放 + 居中补边到模型声明的边长。

        直接 resize 会拉伸画面，让瘦的物体变胖、圆的变椭圆，检测框也会歪。
        补边填 114 灰而不是黑：这是 ultralytics 的约定，也是模型训练时
        见过的填充色，换成黑色会让边缘出现一圈"暗物体"的假响应。
        """
        size = self.input_size
        w, h = image.size
        scale = min(size / w, size / h)
        nw, nh = max(1, int(round(w * scale))), max(1, int(round(h * scale)))
        resized = image.resize((nw, nh), Image.BILINEAR)
        canvas = Image.new("RGB", (size, size), (114, 114, 114))
        pad = ((size - nw) // 2, (size - nh) // 2)
        canvas.paste(resized, pad)
        return canvas, scale, pad


class Tagger:
    """WD14 属性反推（"提示词反推"）。

    ## 预处理参数不是猜的，是量出来的

    WD14 有多个导出脚本，布局/通道序/归一化三者都可能是别的样子。这里没有
    照抄某份参考实现，而是在真实的 `wd-v1-4-moat-tagger-v2` 上跑了四种组合
    （用一张白底 + 深色人形的合成图），看哪个给出合理的标签：

    | 预处理 | 前几个标签 | 判断 |
    |---|---|---|
    | **stretch + BGR + 0..255** | `sun=0.88, general=0.83, no_humans=0.63, solo=0.49, 1girl=0.40` | ✅ |
    | stretch + RGB + 0..255 | `general=0.85, sun=0.62, no_humans=0.44` | 每个都更弱 |
    | stretch + BGR + 0..1 | `black_background=0.94, monochrome=0.94, greyscale=0.90` | ❌ 白底图被判成黑底 |
    | stretch + BGR + -1..1 | `black_background=0.96, monochrome=0.94` | ❌ 同上 |

    BGR 在**每一个**标签上都强于 RGB（`sun` 0.88 vs 0.62、`no_humans` 0.63 vs
    0.44），而任何形式的归一化都会把画面读反——因为训练输入本来就是
    0..255 的原始像素值。

    布局不用猜：直接读模型自己声明的输入形状，末维是 3 就是 NHWC
    （moat 就是这样），第 1 维是 3 就是 NCHW。
    """

    def __init__(self, model_path: str, vocab_path: str, name: str):
        self.path = model_path
        self.name = name
        self.vocab = self._load_vocab(vocab_path)
        opts = ort.SessionOptions()
        opts.intra_op_num_threads = max(1, (os.cpu_count() or 4) // 2)
        opts.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.session = ort.InferenceSession(
            model_path, opts, providers=["CPUExecutionProvider"]
        )
        spec = self.session.get_inputs()[0]
        self.input_name = spec.name
        self.layout, self.input_size = _read_input_layout(spec.shape, TAGGER_INPUT)

    @staticmethod
    def _load_vocab(path: str) -> list[dict]:
        """读 `selected_tags.csv`。

        列是 `tag_id,name,category,count`。

        ## 唯一真正重要的事：**不重排**

        文件的行顺序就是模型输出向量的下标顺序。把它按 `name` 排一遍，
        或在前面插一行，会让所有标签整体错位——而错位是**静默**的，
        而且错得很"合理"（`1girl` 的位置上冒出 `blue_eyes`），肉眼查不出来。

        所以这里做两件事：
        1. 原样保留行顺序，只在末尾校验 `len(rows) == 模型输出维度`；
        2. 校验 WD 系列的一个固定约定：4 个 `rating_*`（category 9）必须
           排在文件最前面。文件一旦被重排（比如按字母序），
           `explicit` 就会跑到第一个，这个检查会立刻抓住。

        注意 `tag_id` **不是**下标，是 Danbooru 的原始 id，只有那 4 个
        rating 用 999999x 的哨兵值保证排在最前。所以不要对它做连续性校验。
        """
        rows = []
        with open(path, "r", encoding="utf-8") as fh:
            header = fh.readline().strip().split(",")
            try:
                i_name = header.index("name")
                i_cat = header.index("category")
            except ValueError:
                raise Unavailable(
                    f"词表 {path} 的列名不对，期望含 name/category，实际是 {header}"
                )
            for line in fh:
                parts = line.rstrip("\n").split(",")
                if len(parts) <= max(i_name, i_cat):
                    continue
                try:
                    cat = int(parts[i_cat])
                except ValueError:
                    cat = 0
                rows.append({"name": parts[i_name], "category": cat})
        if not rows:
            raise Unavailable(f"词表 {path} 是空的")

        leading = [r for r in rows[: len(TAGGER_RATING_NAMES)] if r["category"] == 9]
        if len(leading) != len(TAGGER_RATING_NAMES):
            found = [(r["name"], r["category"]) for r in rows[:6]]
            raise Unavailable(
                f"词表 {path} 的行顺序看起来被改过：期望最前面是 4 个 rating 标签"
                f"{TAGGER_RATING_NAMES}，实际开头是 {found}。"
                "行顺序就是模型输出的下标顺序，重排会让所有标签整体错位，"
                "所以宁可不用也不能猜。"
            )
        return rows

    def tag(self, image: Image.Image, limit: int) -> list[dict]:
        arr = _tagger_input(image, self.input_size, self.layout)
        out = self.session.run(None, {self.input_name: arr})[0][0]

        if len(out) != len(self.vocab):
            raise RuntimeError(
                f"模型输出 {len(out)} 维，但词表有 {len(self.vocab)} 条——"
                "模型和词表不是同一套，标签会全部错位。"
                "请确认 selected_tags.csv 与该模型来自同一个仓库"
            )

        order = np.argsort(-out)[: max(1, limit) * 3]
        results = []
        for i in order:
            entry = self.vocab[int(i)]
            # rating 类（category 9）不是画面属性。实测里 `general` 在
            # 一张白底简笔画上能拿 0.83，不滤掉就会挤进最前面。
            if entry["category"] in TAGGER_SKIP_CATEGORIES:
                continue
            score = float(out[int(i)])
            if score <= 0.0:
                continue
            results.append({
                # 空格换成下划线：WD14 词表里两种写法混着，统一了 Rust 侧才能去重
                "tag": entry["name"].replace(" ", "_"),
                "score": round(score, 4),
            })
            if len(results) >= max(1, limit):
                break
        return results


def _read_input_layout(shape, fallback: int) -> tuple[str, int]:
    """从模型声明的输入形状里读出 布局 与 空间边长。

    返回 `("nhwc" | "nchw", 边长)`。形状里有 `None`（动态维）时退回 `fallback`。
    """
    dims = [d for d in shape if isinstance(d, int)]
    if len(shape) == 4:
        tail = shape[-1]
        if isinstance(tail, int) and tail in (1, 3, 4):
            size = next((d for d in shape[1:3] if isinstance(d, int)), fallback)
            return "nhwc", size
    size = next((d for d in shape[2:] if isinstance(d, int)), None)
    if size is None:
        size = next((d for d in dims if d > 16), fallback)
    return "nchw", size


def _tagger_input(image: Image.Image, size: int, layout: str) -> "np.ndarray":
    """按实测结果准备 tagger 输入：拉伸到正方、BGR、原始 0..255。

    - **拉伸**而不是补边：补边会往画面里塞进一大片纯白，把
      `white_background` 这类标签凭空叫出来（实测 `sun` 从 0.88 涨到 0.94）。
    - **不归一化**：见 [`Tagger`] 的表格。
    """
    square = image.convert("RGB").resize((size, size), Image.BICUBIC)
    arr = np.asarray(square, dtype=np.float32)[:, :, ::-1]  # RGB → BGR
    if layout == "nchw":
        arr = np.transpose(arr, (2, 0, 1))
    return arr[None, ...]


def _nms(boxes: "np.ndarray", scores: "np.ndarray", iou_thr: float) -> list[int]:
    """单类别 NMS（调用方负责先按类别分组）。"""
    order = scores.argsort()[::-1]
    keep: list[int] = []
    while order.size > 0:
        i = int(order[0])
        keep.append(i)
        if order.size == 1:
            break
        rest = order[1:]
        xx1 = np.maximum(boxes[i, 0], boxes[rest, 0])
        yy1 = np.maximum(boxes[i, 1], boxes[rest, 1])
        xx2 = np.minimum(boxes[i, 2], boxes[rest, 2])
        yy2 = np.minimum(boxes[i, 3], boxes[rest, 3])
        inter = np.maximum(0.0, xx2 - xx1) * np.maximum(0.0, yy2 - yy1)
        area_i = (boxes[i, 2] - boxes[i, 0]) * (boxes[i, 3] - boxes[i, 1])
        area_r = (boxes[rest, 2] - boxes[rest, 0]) * (boxes[rest, 3] - boxes[rest, 1])
        union = area_i + area_r - inter
        iou = np.where(union > 0, inter / union, 0.0)
        order = rest[iou <= iou_thr]
    return keep


# -------------------------------------------------------------------- 服务

class Service:
    """把模型加载与租户一起收在一处，HTTP 层只管收发 JSON。"""

    def __init__(self, models_dir: str):
        self.dir = models_dir
        self.lock = threading.Lock()
        self.detector: Detector | None = None
        self.tagger: Tagger | None = None
        self.detect_error: str | None = None
        self.tag_error: str | None = None

    def load(self) -> None:
        yolo = os.path.join(self.dir, "yolov8n.onnx")
        if os.path.exists(yolo):
            try:
                self.detector = Detector(yolo)
                print(f"[vision] 目标检测就绪：{yolo}")
            except Exception as exc:  # noqa: BLE001 - 要如实报出任何失败原因
                self.detect_error = f"{type(exc).__name__}: {exc}"
                print(f"[vision] 目标检测加载失败：{self.detect_error}")
        else:
            self.detect_error = f"找不到 {yolo}"
            print(f"[vision] 跳过目标检测：{self.detect_error}")

        # 词表与模型必须成对；只找到一半时宁可整个跳过，
        # 因为标签下标错位是最难发现的错误。
        pairs = [
            ("tagger_moat.onnx", "selected_tags.csv", "wd-v1-4-moat"),
            ("tagger_convnext.onnx", "selected_tags.csv", "wd-v1-4-convnext"),
            ("tagger_swinv2.onnx", "selected_tags.csv", "wd-v1-4-swinv2"),
        ]
        for model_file, vocab_file, name in pairs:
            mp = os.path.join(self.dir, model_file)
            vp = os.path.join(self.dir, vocab_file)
            if not os.path.exists(mp):
                continue
            if not os.path.exists(vp):
                self.tag_error = f"有 {model_file} 但没有 {vocab_file}，无法确定标签顺序"
                print(f"[vision] 跳过属性反推：{self.tag_error}")
                break
            try:
                self.tagger = Tagger(mp, vp, name)
                print(f"[vision] 属性反推就绪：{name}（{len(self.tagger.vocab)} 个标签）")
            except Exception as exc:  # noqa: BLE001
                self.tag_error = f"{type(exc).__name__}: {exc}"
                print(f"[vision] 属性反推加载失败：{self.tag_error}")
            break
        if self.tagger is None and self.tag_error is None:
            self.tag_error = f"在 {self.dir} 里没找到 tagger_*.onnx"
            print(f"[vision] 跳过属性反推：{self.tag_error}")

    @property
    def ok(self) -> bool:
        return self.detector is not None or self.tagger is not None

    def health(self) -> dict:
        return {
            "ok": self.ok,
            "service": "styx-vision",
            "version": VERSION,
            "detector": None if self.detector is None else "yolov8n",
            "classes": len(COCO80) if self.detector else 0,
            "tagger": None if self.tagger is None else self.tagger.name,
            "vocabulary": None if self.tagger is None else len(self.tagger.vocab),
            "detect_error": self.detect_error,
            "tag_error": self.tag_error,
        }

    def run_detect(self, image: Image.Image) -> dict:
        if self.detector is None:
            raise Unavailable(self.detect_error or "目标检测未加载")
        with self.lock:  # onnxruntime 的 session 不是并发安全的
            return {"objects": self.detector.detect(image)}

    def run_tag(self, image: Image.Image, limit: int) -> dict:
        if self.tagger is None:
            raise Unavailable(self.tag_error or "属性反推未加载")
        with self.lock:
            return {"tags": self.tagger.tag(image, limit), "model": self.tagger.name}


def decode_image(payload: dict) -> Image.Image:
    raw = payload.get("image")
    if not isinstance(raw, str) or not raw:
        raise ValueError("请求体里没有 image")
    # 容忍 data URL 前缀，前端直接贴过来也能用
    if raw.startswith("data:"):
        raw = raw.split(",", 1)[-1]
    try:
        data = base64.b64decode(raw, validate=True)
    except Exception as exc:  # noqa: BLE001
        raise ValueError(f"image 不是合法的 base64：{exc}") from exc
    if not data:
        raise ValueError("image 解出来是空的")
    try:
        img = Image.open(io.BytesIO(data))
        img.load()
    except Exception as exc:  # noqa: BLE001
        raise ValueError(f"认不出这是什么图片：{exc}") from exc
    # 统一转 RGB：PNG 带 alpha / 灰度 / CMYK 都会在这里被规整，
    # 否则后面的 transpose 会拿到 4 通道，模型直接报维度不匹配。
    return img.convert("RGB")


def _jsonable(value):
    """把 numpy 标量/数组转成 JSON 能认的东西。

    这不是"防御性编程"的洁癖，而是有具体教训的：`round(np.float32, 4)`
    返回的**仍然是** `np.float32`，于是漏一个 `float()` 就会让 `/detect`
    整个端点 500——而错误信息（"Object of type float32 is not JSON
    serializable"）和"检测失败"毫无关系，排查要绕一大圈。

    在边界上兜住这一类问题，比在每个字段上小心翼翼更划算。
    """
    if hasattr(value, "item"):          # np.float32 / np.int64 / np.bool_
        return value.item()
    if hasattr(value, "tolist"):        # np.ndarray
        return value.tolist()
    raise TypeError(f"{type(value).__name__} 不是可以 JSON 化的类型")


class Handler(BaseHTTPRequestHandler):
    service: Service
    server_version = f"styx-vision/{VERSION}"

    # 默认的 BaseHTTPRequestHandler 会把每条请求打一行日志到 stderr，
    # 而调用方是 Rust 侧的后台进程——保留它，排查问题时很有用。
    def log_message(self, fmt, *args):  # noqa: A003
        sys.stderr.write("[vision] %s - %s\n" % (self.address_string(), fmt % args))

    def _send(self, status: int, body: dict) -> None:
        blob = json.dumps(body, ensure_ascii=False, default=_jsonable).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(blob)))
        self.end_headers()
        self.wfile.write(blob)

    def _read_json(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0:
            return {}
        raw = self.rfile.read(length)
        try:
            payload = json.loads(raw.decode("utf-8"))
        except Exception as exc:  # noqa: BLE001
            raise ValueError(f"请求体不是合法 JSON：{exc}") from exc
        if not isinstance(payload, dict):
            raise ValueError("请求体必须是一个 JSON 对象")
        return payload

    def do_GET(self):  # noqa: N802 - BaseHTTPRequestHandler 的约定
        path = self.path.split("?", 1)[0]
        if path in ("/health", "/", "/healthz"):
            self._send(200, self.service.health())
        else:
            self._send(404, {"error": f"没有这个端点：{path}"})

    def do_POST(self):  # noqa: N802
        path = self.path.split("?", 1)[0]
        try:
            payload = self._read_json()
        except ValueError as exc:
            self._send(400, {"error": str(exc)})
            return

        if path == "/detect":
            self._handle(payload, lambda img: self.service.run_detect(img))
        elif path == "/tag":
            limit = payload.get("limit", 40)
            try:
                limit = max(1, min(500, int(limit)))
            except (TypeError, ValueError):
                limit = 40
            self._handle(payload, lambda img: self.service.run_tag(img, limit))
        else:
            self._send(404, {"error": f"没有这个端点：{path}"})

    def _handle(self, payload: dict, fn) -> None:
        try:
            image = decode_image(payload)
        except ValueError as exc:
            self._send(400, {"error": str(exc)})
            return
        try:
            self._send(200, fn(image))
        except Unavailable as exc:
            # 503 而不是 500：这是"配置不全"，不是"服务坏了"。
            # Rust 侧两种都会降级成一条 note，但分开能让排查快很多。
            self._send(503, {"error": str(exc)})
        except Exception as exc:  # noqa: BLE001
            traceback.print_exc()
            self._send(500, {"error": f"{type(exc).__name__}: {exc}"})


def main(argv: list[str] | None = None) -> int:
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description="Styx 视觉 sidecar")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8420)
    ap.add_argument("--models", default=os.path.join(here, "models"))
    ap.add_argument(
        "--check",
        action="store_true",
        help="只报告模型状态然后退出（用于确认环境是否装好）",
    )
    args = ap.parse_args(argv)

    svc = Service(args.models)
    svc.load()

    if args.check:
        print(json.dumps(svc.health(), ensure_ascii=False, indent=2))
        return 0 if svc.ok else 1

    if not svc.ok:
        print(
            "[vision] 没有加载到任何模型，服务仍然会起来，但每个请求都会返回 503。\n"
            "[vision] 跑 `python fetch_models.py` 下载模型，或用 --models 指向已有的目录。",
            file=sys.stderr,
        )

    Handler.service = svc
    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"[vision] 监听 http://{args.host}:{args.port}（模型目录 {args.models}）")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\n[vision] 收到中断，退出")
    finally:
        httpd.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
