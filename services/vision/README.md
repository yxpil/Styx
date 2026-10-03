# Styx 视觉 sidecar

给 [`styx-vision`](../../crates/styx-vision) 的可选后端提供两个本地 HTTP 端点。
**纯可选**：服务不跑，Rust 侧的本地图像分析照常工作。

```
  ┌─────── Rust 侧（crates/styx-vision）───────────────────────┐
  │  ① 本地事实    零依赖，永远可用（含基线 JPEG 解码）        │
  │  ② 目标检测    ─┐                                          │
  │  ③ 属性反推    ─┼─  这三个都是「有更好，没有也行」         │
  │  ④ 视觉模型    ─┘                                          │
  └────────────────┬───────────────────────────────────────────┘
                   │  HTTP
        ┌──────────┴──────────┐        ┌─────────────────────┐
        │ 这个目录（本机）    │        │ 视觉模型（远端）    │
        │ YOLOv8n + WD14 ONNX │        │ OpenAI 兼容多模态   │
        └─────────────────────┘        └─────────────────────┘
```

为什么要有第 ③ 层，一句话：**「画面里有什么」和「这个画面长什么样」是两回事。**
目标检测只能告诉你「有一个人」，而属性反推能告诉你「长发、蓝眼睛、在笑、
室内、动漫风」——后者才是角色扮演真正需要的信息。这正是 Stable Diffusion
生态里「提示词反推」在做的事。

---

## 快速开始

```bash
cd services/vision

# 1. 装依赖（三个纯 wheel，不拉 torch）
pip install -r requirements.txt

# 2. 下模型（可以只下一个）
python fetch_models.py                  # 两个都下，约 324 MB
python fetch_models.py --only detector  # 只要检测，13 MB

# 3. 确认能真的加载起来（不是只确认文件在）
python app.py --check

# 4. 起服务
python app.py                           # 127.0.0.1:8420
```

`--check` 的输出长这样：

```json
{
  "ok": true,
  "service": "styx-vision",
  "version": "0.1.0",
  "detector": "yolov8n",
  "classes": 80,
  "tagger": "wd-v1-4-moat",
  "vocabulary": 9083,
  "detect_error": null,
  "tag_error": null
}
```

## 命令行

| 命令 | 说明 |
|---|---|
| `python app.py` | 默认 `127.0.0.1:8420`，模型从 `./models` 找 |
| `python app.py --port 9000` | 换端口 |
| `python app.py --models D:/models` | 换模型目录 |
| `python app.py --check` | 只报告模型状态后退出，有模型时返回码 0 |
| `python smoke.py` | **端到端自检**：逐字段验证两个端点的输入输出形状 |
| `python smoke.py --image photo.jpg` | 用真照片自检（合成图检测不出东西） |
| `python check_vocab.py` | 把 `tags.rs` 的归组词表拿真实词表核一遍 |
| `python fetch_models.py` | 下载两个模型 |
| `python fetch_models.py --only tagger` | 只下标签模型 |
| `python fetch_models.py --check` | 只检查现有文件大小对不对 |
| `python fetch_models.py --hf-base https://huggingface.co` | 走官方源（默认走 `hf-mirror.com`） |
| `python make_fixtures.py` | 重新生成 JPEG 解码器的参考数据 |

### `smoke.py` 补的是哪一层

`app.py --check` 只回答「模型能不能加载」。`smoke.py` 回答**下一层**：
「端点的输入输出**形状**是不是我们约好的那样」。

为什么需要它：Rust 侧的 `parse_objects` / `parse_tags` 是宽容解析，字段名错一点
不会报错，只会**静默地少读几个字段**。服务端哪天把 `confidence` 改成 `conf`，
Rust 侧照样解析成功，只是每个框的分数都是 0——这种 bug 看日志是发现不了的。

实测输出（拿一张真实插图跑，106 项断言）：

```
[detect]  检出 1 个物体：人 0.5404
[tag]     前 12 个标签：1girl 0.997、solo 0.981、spoken_heart 0.973、heart 0.972、
          animal_ears 0.967、sailor_collar 0.964、long_sleeves 0.957、closed_eyes 0.954、
          animal_ear_fluff 0.953、shirt 0.947、white_shirt 0.943、black_background 0.943
```

标签里没有 `rating_*`（category 9 已在服务端滤掉），分数降序，空格已换成下划线。

### `check_vocab.py` 补的是哪一层

`crates/styx-vision/src/tags.rs` 里那 428 个归组关键词是**猜**出来的。猜错的代价
不是报错，而是静默地把标签分错组——`hair_style_7` 因为词表里多了一个 `style`
就被分进「画风」，看起来毫无异常。

脚本直接**解析 `tags.rs`**（不抄一份，抄一份就会失同步），然后：

1. 报告**命中 0 个真实标签**的关键词 → 死重量，删掉不改变行为；
2. 报告**词头跨组**的标签 → 供人工复核「词头优先」决定得对不对。

实测：428 个关键词里 59 个命中 0 个标签；689 处「修饰语命中、但分组由词头决定」——
这一节**有输出是正常的**，它默认认为词头是对的。

## 模型

| 文件 | 大小 | 来源 | 作用 |
|---|---|---|---|
| `yolov8n.onnx` | 12.9 MB | [ultralytics v8.4.0](https://github.com/ultralytics/assets/releases/tag/v8.4.0) | COCO 80 类目标检测 |
| `tagger_moat.onnx` | 311 MB | [SmilingWolf/wd-v1-4-moat-tagger-v2](https://huggingface.co/SmilingWolf/wd-v1-4-moat-tagger-v2) | 9083 类属性标签反推 |
| `selected_tags.csv` | 253 KB | 同上 | 标签词表（下标 → 标签名） |

模型不进仓库（见 `.gitignore`）：体积大、一条命令可复现、上游本来就会更新。
两者独立，缺一个另一个照常工作。

想换更准的反推模型：把 `TAGGER_REPO` 改成 `wd-v1-4-convnext-tagger-v2` 或
`swinv2`，文件名对齐成 `tagger_convnext.onnx` / `tagger_swinv2.onnx` 即可，
**Python 与 Rust 两侧都不用改**——`fetch_models.py --check` 和 `Service.load()`
都按前缀 `tagger_*` 探测。

---

## 端点

### `GET /health`

永远返回 200。字段见上面 `--check` 的输出。`detect_error` / `tag_error` 是
加载失败时的原始异常文本，方便定位。

### `POST /detect`

```jsonc
// 请求
{ "image": "<base64 或 data URL>" }

// 响应 200（最多 20 条，按 confidence 降序）
{ "objects": [
    { "label": "person", "label_zh": "人", "confidence": 0.91,
      "box": [0.12, 0.08, 0.64, 0.95],   // 归一化 xyxy，原点是左上角
      "area": 0.45 }                     // 占整图面积比
] }
```

`label` 是 COCO 的英文类名（词表缺失时就写数字），`label_zh` 是补的中文对照。
`box` 是 **0..1 的归一化坐标**，不是像素——letterbox 补的边和缩放都在服务端
还原过了，Rust 侧不需要知道输入图被怎么处理过，直接就能说"在左上角"。

### `POST /tag`

```jsonc
// 请求
{ "image": "<base64 或 data URL>", "limit": 40 }   // limit 可选，夹到 1..500

// 响应 200
{ "tags": [
    { "tag": "long_hair", "score": 0.87 }
  ],
  "model": "wd-v1-4-moat" }
```

服务端刻意**只做两件最低限度的事**：

- 丢掉 `score <= 0` 的；
- 丢掉 4 个 `rating_*`（category 9）。它们不是画面属性，实测里 `general`
  在一张白底简笔画上能拿 0.83，不滤掉就会挤进最前面。

**中文名、按维度分组、每组限流、低分裁剪、二次元偏见免责提醒全在 Rust 侧**
（[`crates/styx-vision/src/tags.rs`](../../crates/styx-vision/src/tags.rs)）。
分界线是：服务端只管"跑模型"，Rust 侧管"给语言模型看什么"。把归组逻辑
放在 Python 里会让它没法单测，而这块恰恰是最需要单测的。

`tag` 里的空格已换成下划线（WD14 词表里两种写法混着，统一了才能去重）。

> **归组的一条规则值得单独提**：复合标签里最后一个词（词头名词）决定维度。
> 所以 `animal_ears` 归"外貌"而不是"主体"——`animal` 只是修饰语。
> 这条规则让「穿着：水手领、长袖、衬衫、白衬衫」读得通，而同一条标签串在
> 归组之前是「其他：sailor collar、long sleeves、heart」。
> 详情和已知的取舍见 [`tags.rs`](../../crates/styx-vision/src/tags.rs) 里
> `group_of` 的文档。

### 错误约定

| 情况 | 状态码 | 行为 |
|---|---|---|
| 模型没加载 | `503` | `{"error": "..."}`，Rust 侧变成描述里的一条 note，回合照常 |
| 推理时抛异常 | `500` | `{"error": "RuntimeError: ..."}`，同样只变成 note |
| 请求体不是 JSON / 缺 image / base64 坏了 | `400` | `{"error": "..."}` |

**绝对不要把"模型没加载"伪装成"画面里什么都没有"。** 那会让角色得到一个
完全错误的印象（"图里没东西"），而真相是"我们没看清楚"。

---

## 两个容易踩的坑（都是实测出来的）

### 1. WD14 的预处理：stretch + BGR + 原始 0..255

不是 `resize` 到 448 保持比例，不是 RGB，更不能除 255。四种组合实测对照：

| 预处理 | `sun` | `black_background` | `monochrome` |
|---|---|---|---|
| **stretch + BGR + 0..255** | **0.88** | — | — |
| stretch + RGB + 0..255 | 0.62 | — | — |
| stretch + BGR + `/255` | — | **0.94** | **0.94** |

最后一行是把白底简笔画读成「黑背景 + 单色」——完全颠倒。模型 IO 是
`input_1:0 [1,448,448,3]`（**NHWC**）→ `predictions_sigmoid [1,9083]`
（已经过 sigmoid，别再自己套一次）。

### 2. `selected_tags.csv` 的行顺序**不能重排**

`tag_id` 是 Danbooru 的原始 id（例如 `long_hair` 是 `1516405`），**不是下标**。
行顺序就是模型输出的下标顺序，重排一次全盘错位。`Service._load_vocab` 会
校验「前 4 行都是 `rating_*`（category 9）」，因为这是唯一可靠的顺序线索。

## 这个服务为什么不用 FastAPI

因为一个文件就能说清楚的事，不值得再引入一个 web 框架：`http.server` 是
标准库，端点是三个，请求体是一段 JSON。少一个依赖就少一种装不上的可能——
真正拖垮部署的从来不是代码，而是「这个包在你的平台上没有 wheel」。

## 相关文档

- [`crates/styx-vision/src/lib.rs`](../../crates/styx-vision/src/lib.rs) —— Rust 侧四层设计的模块文档
- [`crates/styx-vision/src/tags.rs`](../../crates/styx-vision/src/tags.rs) —— 归组、限流、中文名、免责提醒
- [`crates/styx-vision/src/jpeg.rs`](../../crates/styx-vision/src/jpeg.rs) —— 零依赖基线 JPEG 解码器（第 ① 层的一部分）
- [`crates/styx-vision/README.md`](../../crates/styx-vision/README.md) —— 这个组件自己的说明（也就是独立仓库的首页）
- [Styx 的 `INTEGRATION.md`](https://github.com/yxpil/Styx/blob/main/INTEGRATION.md)
  —— 怎么把这条链子接到角色上。**写成绝对地址是有意的**：本目录同时存在于
  Styx 主仓库和独立的 `styx-vision` 仓库里，只有绝对地址两边都点得开。

## 一句话总结

这个 sidecar 提供的两个模型，是「角色能真正看懂你发的图」这件事上**可选但值得**的部分。
没有它，Styx 依然能说出一张图有多亮、什么色调、多大尺寸——但它说不出画面里有谁、
在做什么、什么风格。有了第 ③ 层，它才第一次能说出「一个长发的女孩在笑，
室内，动漫风」这样的话。
