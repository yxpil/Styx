# styx-vision

**让没有视觉能力的语言模型也能「看见」图片。**

纯 Rust、零第三方依赖、不需要 GPU、不需要网络。你给它一张图的字节，
它给你一段中文描述——尺寸、明暗、冷暖、主色、疏密，以及（可选的）
画面里有什么、长什么样。

```rust
use styx_vision::analyze;

let facts = analyze(&std::fs::read("photo.jpg")?);
println!("{}", facts.describe());
// 一张 1280×720 的 JPEG 照片，整体偏亮，暖色调，主色是浅米色、暖橙。
// 画面元素疏朗，大部分是平坦的色块。
```

它不是一个"降级方案"，而是这套系统里图片理解的**主力实现**：因为没有
任何第三方依赖，它在任何机器上都一定会跑起来。可选的 ONNX 检测、
提示词反推、多模态模型只是叠加在它上面的额外信息——**全挂掉也不影响
本地这一层给出可用的描述**。

---

## 四层结构

```
  ┌─────────────────────────────────────────────────────────────┐
  │ ①  本地事实   facts::analyze                                │
  │    手写 inflate + PNG/BMP/JPEG 解码 → 尺寸/明暗/冷暖/主色/疏密│
  │    → 中文描述。零依赖，永远可用。                            │
  ├─────────────────────────────────────────────────────────────┤
  │ ②  目标检测   backend::RemoteVision（YOLOv8n ONNX sidecar） │
  │    可选。给出「画面里有什么、在哪个位置、多大」。            │
  ├─────────────────────────────────────────────────────────────┤
  │ ③  属性反推   backend::RemoteTagger（WD14 / CLIP 打分）     │
  │    可选。给出「提示词反推」式的属性标签：发色/神情/穿着/画风。│
  ├─────────────────────────────────────────────────────────────┤
  │ ④  视觉模型   backend::OpenAiVision（ollama / 兼容端点）     │
  │    可选。给出真正的语义描述。                                │
  └─────────────────────────────────────────────────────────────┘
                         ↓ 由 backend::Composer 合并
                   一段交给语言模型的自然语言描述
```

②③④ 全都是**可选且可替换**的，而且顺序是有意的：越靠下越"聪明"，也越贵、
越容易失效。删掉后三层，① 依然给出一段可用的描述。

### 第 ③ 层值得单独说一句

「画面里有什么」和「这个画面长什么样」是两回事。目标检测只能告诉你
「有一个人」，而**属性反推**能告诉你「长发、蓝眼睛、在笑、室内、动漫风」
——后者才是角色扮演真正需要的信息。

这正是 Stable Diffusion 生态里「提示词反推」（interrogator / tagger）在做的事，
本仓库把它做成了第 ③ 层：WD14 多标签分类，9083 个标签，纯 ONNX，
`onnxruntime` 直接能跑，不用拉 torch。

---

## 快速开始

### 只要本地事实（零第三方依赖）

```toml
[dependencies]
styx-vision = { git = "https://github.com/yxpil/styx-vision" }
```

### 带上可选后端

```toml
[dependencies]
styx-vision = { git = "https://github.com/yxpil/styx-vision", features = ["http"] }
```

如果这个 crate 就在你旁边（比如在 Styx 主仓库里）：

```toml
styx-vision = { path = "crates/styx-vision", features = ["http"] }
```

### 跑一下看看

```bash
# 只用本地事实
cargo run --example inspect -- photo.jpg

# 接上 sidecar（见 services/vision）
cargo run --example inspect --features http -- photo.jpg --base http://127.0.0.1:8420
```

`inspect` 会打印尺寸、格式、是否解出像素、明暗冷暖主色，然后是
真正交给语言模型的那段描述，以及检测出来的物体和反推出来的标签分组。

### 组装自己的链路

```rust
use styx_vision::{Composer, RemoteTagger, RemoteVision};

let composer = Composer::new()
    .with(Box::new(RemoteVision::new("http://127.0.0.1:8420")))
    .with(Box::new(RemoteTagger::new("http://127.0.0.1:8420")));

let (facts, description, used) = composer.describe(&image_bytes);
// description 一定非空：远程全挂了，本地事实也还在
```

还有一个关键性质：**`Composer::describe` 永远返回一段描述**。调用方不需要
处理「没有描述」的分支。后端连不上、超时、返回垃圾，都只会变成描述末尾的
一句说明（`（tagger 不可用，已跳过；）`），而不是错误。

---

## 支持的格式

| 格式 | 能力 | 原因 |
|---|---|---|
| PNG | 完整像素 | 无损 + zlib，手写 inflate 就能拿到真像素 |
| BMP | 完整像素 | 无压缩直存，几乎是白送的 |
| JPEG | 完整像素（基线顺序） | 自写 Huffman + 反量化 + 浮点 8×8 IDCT + IJG 三角滤波升采样 |
| 渐进式 JPEG | 仅尺寸 | 同一张图要走多趟扫描，实现成本高于收益 |
| 其它 | 仅尺寸（若认得） | — |

JPEG 值得解释一下：**真实用户发来的照片几乎全是 JPEG**，只能报个尺寸等于
「看不见」。所以这里自己写了一个基线解码器（`src/jpeg.rs`），并用 10 组夹具
逐像素与 libjpeg 比对：

```
rgb_444_q92.jpg      max=2  mean=0.025   误差直方图(0..8+) = [3011, 46, 15, 0, ...]
ycbcr_420_q75.jpg    max=2  mean=0.007   [3056, 10, 6, 0, ...]
ycbcr_422_q85.jpg    max=2  mean=0.023   [3019, 35, 18, 0, ...]
gray_q80.jpg         max=1  mean=0.003   [3062, 10, 0, ...]
flat_white.jpg       max=0  mean=0.000   [3072, ...]          逐字节完全相同
flat_dark.jpg        max=0  mean=0.000   [3072, ...]          逐字节完全相同
```

剩下的 ±2 来自浮点 IDCT 与 libjpeg 整数 IDCT 之间的规范允许差异，不是 bug。

拿不到像素时（渐进式 JPEG、不认识的格式），`ImageFacts::decoded` 为 `false`，
描述里会**明说**「本地没能解出像素，不要据此推断画面内容」——而不是让模型
把「没有信息」误读成「画面很普通」。

---

## 反推标签是怎么加工成「一段话」的

WD14 的输出是给 Stable Diffusion 吃的逗号长串，无结构、三十来个下划线词。
直接丢给语言模型有两个问题：抓不住重点，而且不同维度混在一起（人数 / 发色 /
构图 / 画风）会被当成「画面内容」。

所以 `src/tags.rs` 做四件事：

1. **按维度分组** —— 主体、外貌、神情、穿着、场景、构图、画风。
2. **每组限流 4 个、总分 ≥ 0.35、总量上限 18**，先按分数排序再占名额。
3. **复合标签由「词头名词」定组** —— `animal_ears` 归「外貌」而不是「主体」，
   因为 `animal` 只是修饰语。
4. **带上来源模型名和免责提醒** —— WD14 在 Danbooru 上训练，给它一张真人照片
   会把成年男性标成 `multiple boys`。所以描述里会写「自动标注，未必准确」
   并注明来自哪个模型，让语言模型知道这批标签偏二次元、别当真。

加工前后的对比（同一张图、同一个模型）：

```
改前  穿着：shirt、white shirt、hairclip
      其他：spoken heart、heart、sailor collar、long sleeves
改后  穿着：sailor collar、long sleeves、shirt、white shirt
```

「水手领 + 长袖 + 衬衫 + 白衬衫」这才读得出是制服。

---

## 可选后端：两个 ONNX 模型

`services/vision/` 是一个零框架的 Python sidecar（`http.server`，三个端点），
提供第 ②③ 层：

| 端点 | 模型 | 作用 |
|---|---|---|
| `POST /detect` | YOLOv8n（12.9 MB） | COCO 80 类目标检测 |
| `POST /tag` | WD14 moat（311 MB） | 9083 类属性标签反推 |

```bash
cd services/vision
pip install -r requirements.txt
python fetch_models.py     # 默认走 hf-mirror.com
python app.py              # 127.0.0.1:8420
```

细节见 [`services/vision/README.md`](../../services/vision/README.md)——里面记了
两个实测出来的坑（WD14 的预处理必须是 **stretch + BGR + 原始 0..255**；
`selected_tags.csv` 的行顺序**不能重排**，因为 `tag_id` 不是下标）。

第 ④ 层不需要额外的服务：任何 OpenAI 兼容的多模态端点都行
（ollama、vLLM、one-api……），用 `OpenAiVision` 接上即可。

---

## 测试

```bash
cargo test                          # 本地路径：零第三方依赖
cargo test --features http          # 加上远程后端的解析逻辑
```

| 测试 | 数量 | 说明 |
|---|---|---|
| 单元测试 | 97 | 解码、inflate、JPEG、事实提取、归组、宽容解析 |
| `tests/jpeg_reference.rs` | 3 | 与 libjpeg 逐像素比对（判据：均值 ≤2、离群 ≤2%） |
| `tests/live_sidecar.rs` | 1 | `#[ignore]`，需要真的 sidecar 在跑 |

逐像素比对的夹具在 `tests/fixtures/jpeg/`（10 组基线的 `.jpg` + PIL 解出的
`.raw` + 元信息 `.json`），重新生成用 `services/vision/make_fixtures.py`。

跑整链集成测试：

```bash
STYX_VISION_TEST_IMAGE=photo.jpg \
STYX_VISION_BASE=http://127.0.0.1:8420 \
cargo test --features http --test live_sidecar -- --ignored --nocapture
```

---

## 诚实地说明它做不到什么

- **不认识人脸。** 它能说「一个人，在画面中央，占了 40%」，说不出是谁。
- **不做 OCR。** 图里的文字读不出来。
- **反推标签偏二次元。** WD14 是拿 Danbooru 训的，真人照片上的标签要打折看——
  描述里已经带了这条提醒。
- **「画风」这一组在 WD14 上几乎不出现。** 因为 `anime` / `manga` /
  `photorealistic` 这些词**根本不在它的词表里**（它走画师名和
  `1990s_(style)` 那套约定）。这是模型性质，不是缺陷。
- **判不清空间关系。** 「猫在桌上」和「猫在桌下」给的是同一批标签。
- **复合标签的归组不是完美的。** `coat_on_shoulders`、`hat_with_ears` 这类
  `X_on_Y` / `X_with_Y` 结构会被判成「外貌」。试过「遇到介词就改看词头」的规则，
  修对 4 个、修错 2 个（会把 `bags_under_eyes` 判成穿着），于是放弃并把它
  写进测试锁住。

这些不是「以后再说」的待办，而是当前实现**确定的能力边界**——描述里该说的
都会说出来，不会让语言模型以为自己拿到了一张完整的画面理解。

---

## 和 Styx 的关系

这个 crate 既可以作为 [Styx](https://github.com/yxpil/Styx)（角色扮演内核）
的一部分使用，也可以单独取用。

**事实来源是 Styx 主仓库**，它包含全部内容（视觉 + 角色扮演内核 + 语音 + 前端）。
独立仓库 [yxpil/styx-vision](https://github.com/yxpil/styx-vision) 是这里
`crates/styx-vision` + `services/vision` 的镜像，由主仓库里的
`tools/sync_vision_repo.py` 物化出来——**不要直接改镜像里的代码**，
下次同步会覆盖掉。镜像的布局与本仓库保持一致，所以上面的相对链接两边都成立。

## 依赖

默认只有 `serde` 和 `serde_json`。`http` feature 会拉进 `styx-http`
（本项目的极简同步 HTTP 客户端，不带 tokio/reqwest），它再可选地拉 `ureq` 提供 TLS。

---

## License

MIT © 2026 yxpil
