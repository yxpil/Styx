#!/usr/bin/env python3
"""# 视觉 sidecar 的端到端自检

`app.py --check` 只回答「模型能不能加载」。这个脚本回答**下一层**问题：
「端点的输入输出**形状**是不是我们约好的那样」。

存在的理由：Rust 侧的 `parse_objects` / `parse_tags` 是宽容解析，字段名错一点
也不会报错，只会**静默地少读几个字段**。比如服务端哪天把 `confidence` 改成
`conf`，Rust 侧照样解析成功，只是每个框的分数都是 0——这种 bug 靠肉眼看
日志是发现不了的。所以这里逐字段断言。

## 用法

```bash
python app.py &                       # 先把服务起起来
python smoke.py                       # 默认打 http://127.0.0.1:8420
python smoke.py --base http://127.0.0.1:9000
python smoke.py --image photo.jpg     # 用真照片（推荐，合成图检测不出东西）
```

## 关于测试图

默认用 Pillow 现画一张「白底 + 深色人形」的简笔画。它**不是**为了测准确率，
只是为了：

- 有确定的颜色（白底：主色必须是白，否则说明预处理把画面读反了）；
- 有一段确定的几何（人形在画面中央偏下，归一化坐标应当落在 0.3~0.7 / 0.1~0.9）；
- 不依赖任何外部文件，`git clone` 完就能跑。

想验证真实准确率就传 `--image`，那是另一回事。
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import sys
import urllib.error
import urllib.request

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover
    raise SystemExit("需要 Pillow：pip install -r requirements.txt")


# ---------------------------------------------------------------- 测试图

def sketch() -> Image.Image:
    """白底 + 深蓝人形 + 一个橙色方块。

    三样东西各自有明确用途：
    - 白底：主色必须判成白。WD14 的预处理一旦除 255，白底会被读成黑底，
      这里不会失败（那是模型层的事），但 Rust 侧的本地分析会立刻露馅；
    - 人形：给 YOLO 一个"可能检测出 person"的机会。它不一定真的检出来，
      检不出来也不判失败——合成简笔画本来就不在 COCO 的分布里；
    - 橙色方块：给标签反推一点明确的颜色特征。
    """
    img = Image.new("RGB", (448, 448), (250, 250, 248))
    d = ImageDraw.Draw(img)
    # 头
    d.ellipse((180, 60, 268, 148), fill=(46, 52, 74))
    # 身
    d.polygon([(150, 160), (298, 160), (322, 360), (126, 360)], fill=(46, 52, 74))
    # 橙色方块
    d.rectangle((30, 380, 110, 430), fill=(232, 138, 40))
    return img


def to_data_url(img: Image.Image) -> str:
    buf = io.BytesIO()
    img.save(buf, format="JPEG", quality=90)
    return "data:image/jpeg;base64," + base64.b64encode(buf.getvalue()).decode()


# ---------------------------------------------------------------- HTTP

def post(base: str, path: str, body: dict, timeout: float) -> tuple[int, dict]:
    raw = json.dumps(body).encode()
    req = urllib.request.Request(
        base.rstrip("/") + path, data=raw, headers={"Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        payload = exc.read()
        try:
            return exc.code, json.loads(payload)
        except Exception:  # noqa: BLE001
            return exc.code, {"error": payload.decode("utf-8", "replace")}


def get(base: str, path: str, timeout: float) -> tuple[int, dict]:
    try:
        with urllib.request.urlopen(base.rstrip("/") + path, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        return exc.code, {}


# ---------------------------------------------------------------- 断言

class Check:
    def __init__(self) -> None:
        self.passed = 0
        self.failed: list[str] = []

    def ok(self, cond: bool, what: str) -> bool:
        if cond:
            self.passed += 1
            print(f"  [ok] {what}")
        else:
            self.failed.append(what)
            print(f"  [!!] {what}")
        return cond

    def note(self, what: str) -> None:
        """说明「这一节没测到东西」以及**为什么可以接受**。

        存在的理由：一个在空数组上逐个字段断言、全部通过的循环，会打出一片
        `[ok]`，看起来比实际可靠得多。这些断言在 0 个元素时一条也没执行。
        所以空的时候必须显式说出来，而不是让它伪装成通过。
        """
        print(f"  [--] {what}")

    def report(self) -> int:
        print()
        if self.failed:
            print(f"{len(self.failed)} 项不符合约定：")
            for f in self.failed:
                print(f"  - {f}")
            return 1
        print(f"全部 {self.passed} 项通过。")
        return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="视觉 sidecar 端到端自检")
    ap.add_argument("--base", default="http://127.0.0.1:8420")
    ap.add_argument("--image", help="用这张图代替内置简笔画")
    ap.add_argument("--timeout", type=float, default=120.0, help="单次请求超时（秒）")
    args = ap.parse_args(argv)

    c = Check()
    print(f"目标：{args.base}")

    if args.image:
        with open(args.image, "rb") as fh:
            payload = base64.b64encode(fh.read()).decode()
        image = "data:image/jpeg;base64," + payload
        print(f"测试图：{args.image}")
    else:
        image = to_data_url(sketch())
        print("测试图：内置简笔画（白底 + 深色人形 + 橙色方块）")

    # ---- /health ----
    print("\n[health]")
    status, h = get(args.base, "/health", args.timeout)
    c.ok(status == 200, f"/health 返回 200（实际 {status}）")
    c.ok(h.get("service") == "styx-vision", "service 字段是 styx-vision")
    c.ok(isinstance(h.get("version"), str) and h["version"], "version 是非空字符串")
    print(f"  模型状态：{json.dumps({k: h.get(k) for k in ('detector','classes','tagger','vocabulary','detect_error','tag_error')}, ensure_ascii=False)}")

    # ---- 错误路径先测：它们不该依赖模型好不好 ----
    print("\n[错误处理]")
    status, e = post(args.base, "/tag", {"image": "!!!not base64!!!"}, args.timeout)
    c.ok(status == 400, f"坏 base64 返回 400（实际 {status}）")
    c.ok(bool(e.get("error")), "坏 base64 带回一句人话")

    status, e = post(args.base, "/tag", {}, args.timeout)
    c.ok(status == 400, f"缺 image 字段返回 400（实际 {status}）")

    status, e = post(args.base, "/nope", {"image": image}, args.timeout)
    c.ok(status == 404, f"未知端点返回 404（实际 {status}）")

    # ---- /detect ----
    print("\n[detect]")
    status, det = post(args.base, "/detect", {"image": image}, args.timeout)
    if status == 503:
        print(f"  跳过：目标检测模型未加载（{det.get('error')}）")
    else:
        c.ok(status == 200, f"/detect 返回 200（实际 {status}）")
        objs = det.get("objects")
        c.ok(isinstance(objs, list), "objects 是数组")
        if isinstance(objs, list) and not objs:
            # 合成简笔画本来就不在 COCO 的分布里。这里**不是**通过，是没测到——
            # 想真正验证这条路径，传 --image 用一张真照片。
            c.note(
                "0 个物体，所以下面这些逐字段断言一条都没执行。"
                "简笔画检出 0 个是正常的；要验证字段形状请传 --image 用真照片"
            )
        if isinstance(objs, list):
            for i, o in enumerate(objs):
                if not c.ok(isinstance(o.get("label"), str) and o["label"], f"objects[{i}].label 是非空字符串"):
                    break
                c.ok(isinstance(o.get("label_zh"), str), f"objects[{i}].label_zh 存在（可为空串）")
                c.ok(isinstance(o.get("confidence"), (int, float)), f"objects[{i}].confidence 是数字")
                box = o.get("box")
                if not c.ok(isinstance(box, list) and len(box) == 4, f"objects[{i}].box 是 4 个数的数组"):
                    break
                c.ok(
                    all(isinstance(v, (int, float)) and 0.0 <= v <= 1.0 for v in box),
                    f"objects[{i}].box 全在 0..1（归一化，不是像素）",
                )
                c.ok(isinstance(o.get("area"), (int, float)), f"objects[{i}].area 是数字")
            print(f"  检出 {len(objs)} 个物体：" + ("、".join(
                f"{o.get('label_zh') or o.get('label')} {o.get('confidence')}" for o in objs) or "（无）"))
            # 分数必须降序，Rust 侧依赖这个顺序做"最显眼的东西"的判断
            confs = [o.get("confidence", 0) for o in objs]
            if len(confs) >= 2:
                c.ok(confs == sorted(confs, reverse=True), "objects 按 confidence 降序")

    # ---- /tag ----
    print("\n[tag]")
    status, tg = post(args.base, "/tag", {"image": image, "limit": 30}, args.timeout)
    if status == 503:
        print(f"  跳过：属性反推模型未加载（{tg.get('error')}）")
    else:
        c.ok(status == 200, f"/tag 返回 200（实际 {status}）")
        c.ok(isinstance(tg.get("model"), str) and tg["model"], "model 字段带上了模型名（Rust 侧靠它判断二次元偏见）")
        tags = tg.get("tags")
        c.ok(isinstance(tags, list), "tags 是数组")
        if isinstance(tags, list):
            c.ok(0 < len(tags) <= 30, f"标签数在 1..limit 之间（实际 {len(tags)}）")
            for i, t in enumerate(tags):
                if not c.ok(isinstance(t.get("tag"), str) and t["tag"], f"tags[{i}].tag 是非空字符串"):
                    break
                c.ok(" " not in t["tag"], f"tags[{i}].tag 里没有空格（已换成下划线）")
                c.ok(isinstance(t.get("score"), (int, float)), f"tags[{i}].score 是数字")
            scores = [t.get("score", 0) for t in tags]
            c.ok(scores == sorted(scores, reverse=True), "tags 按 score 降序")
            # rating_* 必须被服务端滤掉：它们不是画面属性
            c.ok(
                not any(t.get("tag", "").startswith("rating_") for t in tags),
                "没有 rating_* 混进来（category 9 已滤）",
            )
            print("  前 12 个标签：" + "、".join(f"{t['tag']} {t['score']}" for t in tags[:12]))

    return c.report()


if __name__ == "__main__":
    raise SystemExit(main())
