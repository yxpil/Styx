# styx-vision

**让没有视觉能力的语言模型也能「看见」图片。**

纯 Rust、零第三方依赖、不需要 GPU、不需要网络。你给它一张图的字节，
它给你一段中文描述：尺寸、明暗、冷暖、主色、疏密，以及（可选的）画面里
有什么、长什么样。

```rust
use styx_vision::analyze;

let facts = analyze(&std::fs::read("photo.jpg")?);
println!("{}", facts.describe());
// 一张 1280×720 的 JPEG 照片，整体偏亮，暖色调，主色是浅米色、暖橙。
// 画面元素疏朗，大部分是平坦的色块。
```

```bash
cargo run --example inspect -- photo.jpg
```

---

## 这个仓库是什么

它是 [Styx](https://github.com/yxpil/Styx)（角色扮演内核）里视觉部分的
**镜像**。单独开一个仓库，是因为「让语言模型看懂一张图」这件事本身可以
独立使用——你不需要角色扮演内核，只需要图片理解时，clone 这一个仓库就够了。

| | 地址 | 内容 |
|---|---|---|
| 主仓库（**事实来源**） | [yxpil/Styx](https://github.com/yxpil/Styx) | 全部内容：视觉 + 角色扮演内核 + 语音 + 前端 |
| 本仓库（镜像） | yxpil/styx-vision | 只是视觉这一块，可以脱离 Styx 独立编译与使用 |

**不要直接在本仓库改代码。** 它由主仓库的 `tools/sync_vision_repo.py`
物化出来，改动会在下次同步时被覆盖。要改请改主仓库，然后：

```bash
cd Styx
python tools/sync_vision_repo.py --target ../styx-vision   # 同步
python tools/sync_vision_repo.py --target ../styx-vision --check   # 只校验漂移
cd ../styx-vision && git add -A && git commit && git push
```

目录布局与主仓库保持一致（`crates/...` + `services/...`），所以两边文档里的
相对链接都是对的。

---

## 四层结构

```
  ┌─────────────────────────────────────────────────────────────┐
  │ ①  本地事实   facts::analyze                                │
  │    手写 inflate + PNG/BMP/JPEG 解码 → 尺寸/明暗/冷暖/主色/疏密│
  │    → 中文描述。零依赖，永远可用。                            │
  ├─────────────────────────────────────────────────────────────┤
  │ ②  目标检测   backend::RemoteVision（YOLOv8n ONNX sidecar） │
  ├─────────────────────────────────────────────────────────────┤
  │ ③  属性反推   backend::RemoteTagger（WD14 / CLIP 打分）     │
  ├─────────────────────────────────────────────────────────────┤
  │ ④  视觉模型   backend::OpenAiVision（ollama / 兼容端点）     │
  └─────────────────────────────────────────────────────────────┘
                         ↓ 由 backend::Composer 合并
                   一段交给语言模型的自然语言描述
```

②③④ 全是**可选且可替换**的。删掉后三层，① 依然给出一段可用的描述——
这不是「降级方案」，而是这套东西能在任何机器上跑起来的原因。

---

## 快速开始

```toml
# 只要本地事实（零第三方依赖）
[dependencies]
styx-vision = { git = "https://github.com/yxpil/styx-vision" }

# 带上可选后端（检测 / 反推 / 视觉模型）
styx-vision = { git = "https://github.com/yxpil/styx-vision", features = ["http"] }
```

```rust
use styx_vision::{Composer, RemoteTagger, RemoteVision};

let composer = Composer::new()
    .with(Box::new(RemoteVision::new("http://127.0.0.1:8420")))
    .with(Box::new(RemoteTagger::new("http://127.0.0.1:8420")));

let (facts, description, used) = composer.describe(&image_bytes);
// description 一定非空：远程全挂了，本地事实也还在
```

可选后端需要一个本地的 ONNX sidecar（第 ②③ 层），也可以直接接任何
OpenAI 兼容的多模态端点（第 ④ 层）：

```bash
cd services/vision
pip install -r requirements.txt
python fetch_models.py     # YOLOv8n 12.9 MB + WD14 311 MB，默认走 hf-mirror.com
python app.py              # 127.0.0.1:8420
```

---

## 想进一步了解

| 想知道 | 去哪看 |
|---|---|
| 完整的组件说明 | [`crates/styx-vision/README.md`](crates/styx-vision/README.md) |
| 四层设计的取舍 | [`crates/styx-vision/src/lib.rs`](crates/styx-vision/src/lib.rs) 的模块文档 |
| JPEG 解码器怎么写的 | [`crates/styx-vision/src/jpeg.rs`](crates/styx-vision/src/jpeg.rs) |
| 标签怎么归组成一句话 | [`crates/styx-vision/src/tags.rs`](crates/styx-vision/src/tags.rs) |
| sidecar 的端点契约与两个实测坑 | [`services/vision/README.md`](services/vision/README.md) |

## 测试

```bash
cargo test                     # 本地路径：零第三方依赖
cargo test --features http     # 加上远程后端的解析逻辑
```

97 个单元测试 + 3 个与 libjpeg 的逐像素比对。逐像素夹具在
`crates/styx-vision/tests/fixtures/jpeg/`（10 组基线的 `.jpg` + PIL 解出的
`.raw` + 元信息 `.json`）。

## License

MIT © 2026 yxpil
