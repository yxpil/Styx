# Styx

> 一个**可联动的角色扮演内核**：把「角色」在「场景」中随时间演化的状态，与语言模型、长期记忆、联想、共享记忆池、工具编排成一个回合闭环。

Styx 不是一个聊天界面，也不是又一个"提示词模板集合"。它是一个**内核**——
只依赖五个端口（trait），不依赖任何具体后端。同一份内核：

- 可以跑在 [Nebula](https://github.com/yxpil/Nebula)（加密长期记忆）+ [MightBe](https://github.com/yxpil/MightBe)（可解释联想）+ [MemoryPool](https://github.com/yxpil/MemoryPool)（跨 agent 共享记忆）组成的集群上；
- 也可以**完全离线**跑内存兜底实现——不是打桩，是真 BM25、真共现图、真 PMI 多跳。

---

## 30 秒上手

```bash
git clone https://github.com/yxpil/Styx.git
cd Styx

# 1) 完全离线地看一眼它能做什么（不联网、不需要任何 API key）
cargo run -p styx -- demo --turns 3

# 2) 生成配置和示例角色卡
cargo run -p styx -- init

# 3) 看看哪些后端真的能用（毫秒级，不消耗额度）
cargo run -p styx -- probe

# 4) 开始对话
cargo run -p styx -- repl
```

`repl` 里直接打字就是在演戏；以 `/` 开头的行是命令，**不会**被当成台词：

```
> /state
心情：戒备（效价-0.05 唤醒0.10）  精力：0.95  紧张：0.12
陈默(好感-0.05/信任0.40)
> 我想看看那本相册。
（她把手里的书合上，指腹在书脊上停了半秒，才抬眼。）
你来了。
> /quit
```

---

## 它解决什么问题

### 1. 人设漂移

大模型演上十几轮就会开始"变温柔""变话痨""开始用网络流行语"。Styx 的做法不是靠祈祷，而是三件事：

| 机制 | 位置 | 作用 |
|---|---|---|
| **角色卡与动态状态分离** | `styx-core::character` / `styx-core::state` | 静态人设（不可变）和心情/好感/信任（可演化）是两个东西，模型不能靠"心情变化"把设定改掉 |
| **行首标签 DSL** | `styx-core::reply` | 模型必须用 `[说] [做] [想] [情] [关系] [忆] [场]` 分字段输出，内核才能审计与结算 |
| **一致性守护 Guard** | `styx-core::session` | 禁用词/缺台词 → 硬违规 → 带着角色卡禁则自动重试；台词明显长于示例口吻 → 软提醒注入下一轮 |

### 2. 记性问题

角色"忘了三回合前说过什么"是体验杀手。Styx 的记忆不是一句"总结上文"，而是：

- **召回**：每回合拿用户输入去 BM25 检索（Nebula 或内存实现）；
- **联想**：抽关键词去 MightBe 做多跳发散，带证据与置信度；
- **共享**：能从 MemoryPool 读到"别的 agent 也知道的事实"；
- **预算**：提示词有硬预算，记忆不够用时动态向历史借额度（`styx-core::prompt`）。

### 3. 后端不可用

记忆服务没起、模型还没配好——用户依然希望敲一句话就看到角色开口。因此：

```text
语言模型  配置端点 → STYX_LLM_* 环境变量 → 探测本机 Ollama/LM Studio → 离线 Mock
长期记忆  Nebula   → 内存实现（真 BM25，退出即失）
联想发散  MightBe  → 内存共现图（PMI + 多跳 + 弃判）
共享记忆  MemoryPool（健康检查失败就不注入，回合自动跳过）
```

任何一步降级都会在 `styx probe` 和 `/status` 里**显式可见**，而不是静默劣化。

---

## 与其它仓库的联动

Styx 采用**协议适配器优先**：把对方当作"服务 + 协议"，以原生客户端身份接入，
而不是把对方源码编译进来。因此内核零硬依赖、离线可编译可跑，
但接入后是真·字节级互通。

| 仓库 | 联动方式 | Styx 侧实现 | 互通程度 |
|---|---|---|---|
| [Nebula](https://github.com/yxpil/Nebula) | TCP 加密协议 | `styx-memory` | 复刻 `NEBULA2` 握手、Argon2id + HKDF + HMAC 挑战应答、ChaCha20-Poly1305 加密帧 |
| [MightBe](https://github.com/yxpil/MightBe) | TCP 行分隔文本协议 | `styx-assoc` | `OK cols\|...` / 数据行 / `INFO` / `END (rows=N, ms=X)` / `ERR` |
| [MemoryPool](https://github.com/yxpil/MemoryPool) | HTTP REST + BIT Remote | `styx-pool` | `/health` `/mem` `/mem/search` `/invoke` |
| [RLBCM](https://github.com/yxpil/RLBCM) | 设计参照 | `styx-llm::scheduler` | 多端点加权轮询 + 熔断 + 半开试探（客户端侧对应物） |
| [BIT](https://github.com/yxpil/bit) | 信封协议 | `styx-tools::BitRemoteToolPort` | `{"tool":…,"invoked_by":…,"params":{…}}` |
| 任意 MCP 服务器 | Streamable HTTP + JSON-RPC 2.0 | `styx-tools::McpToolPort` | 兼容 `text/event-stream` 响应与 `Mcp-Session-Id` |

细节见 [INTEGRATION.md](INTEGRATION.md)。

### 反过来：Styx 的一部分也会单独发布

上面的表是「Styx 接入别人」。反方向也存在一处：[**styx-vision**](https://github.com/yxpil/styx-vision)
——「让没有视觉能力的语言模型看懂一张图」这件事本身可以独立使用，
不需要角色扮演内核。

| | 地址 | 内容 |
|---|---|---|
| 主仓库（**事实来源**） | [yxpil/Styx](https://github.com/yxpil/Styx) | 全部内容：视觉 + 内核 + 语音 + 前端 |
| 镜像 | [yxpil/styx-vision](https://github.com/yxpil/styx-vision) | 只有视觉：`crates/styx-vision` + `crates/styx-http` + `services/vision` |

**本仓库永远包含全部内容**：裸克隆就是完整的、能编译的、不需要 `--recursive`。
镜像由 `tools/sync_vision_repo.py` 物化出来，它会自动算依赖闭包、生成独立的
根 `Cargo.toml`，所以将来给视觉加内部依赖时镜像不会静默编译失败。

```bash
python tools/sync_vision_repo.py --target ../styx-vision          # 同步
python tools/sync_vision_repo.py --target ../styx-vision --check  # 只校验漂移
```

镜像里的代码不要直接改（下次同步会覆盖），要改就改这里。取舍的理由见
[`tools/README.md`](tools/README.md)。

---

## 架构

```text
                    ┌──────────────────────────────────────────┐
                    │              styx-core（内核）            │
                    │  角色卡 · 场景 · 动态状态 · 事件流          │
                    │  提示词装配 · 标签 DSL 解析 · 一致性守护     │
                    └───────────────┬──────────────────────────┘
                                    │ 只依赖五个端口（trait）
        ┌───────────────┬───────────┼───────────┬───────────────┐
        ▼               ▼           ▼           ▼               ▼
   LlmPort        MemoryPort   AssocPort    PoolPort       ToolPort
        │               │           │           │               │
   styx-llm       styx-memory   styx-assoc  styx-pool      styx-tools
        │               │           │           │               │
  OpenAI 兼容        Nebula      MightBe    MemoryPool      内置工具
  多端点 SWRR        加密 TCP      文本 TCP     HTTP REST     MCP / BIT Remote
  熔断 / 半开         内存 BM25    内存共现图   LocalPool
        │
   styx-http（极小 HTTP 客户端：std 明文 / ureq+TLS）

   外层：styx-server（TCP JSON-lines 多会话服务） · styx-cli（bin `styx`）
         styx-web（黑白简约前端 + 文本约定协议）
         styx-vision（本地图像理解：零依赖事实 + 可选 ONNX 检测/反推）
```

一个回合发生了什么（`styx-core::kernel::Kernel::turn`）：

```text
用户输入
   │
   ├─① 记忆召回 (MemoryPort)          ← Nebula: BM25 / 标签 / 重要度
   ├─② 联想发散 (AssocPort)           ← MightBe: 多跳 + 证据 + 置信度
   ├─③ 共享记忆 (PoolPort)            ← MemoryPool: 跨 agent 事实
   ├─④ 提示词装配 (PromptBuilder)      ← 角色卡+场景+状态+记忆+联想+历史，带预算与借额
   ├─⑤ 生成     (LlmPort)             ← OpenAI 兼容 + 多端点加权调度
   ├─⑥ 结构化解析 (Reply)             ← 先试 JSON，再走 [说]/[做]/[想]/… 标签 DSL
   ├─⑦ 一致性守护 (Guard)             ← 硬违规自动重试，软提醒留给下一轮
   ├─⑧ 状态结算 (DynamicState)        ← 心情/好感/信任/紧张/意图演化 + 衰减
   └─⑨ 落库     (MemoryPort/PoolPort) ← [忆] 条目与高重要度事件写回
```

设计取舍见 [ARCHITECTURE.md](ARCHITECTURE.md)。

---

## 角色卡

Markdown 写人物，中文小标题，解析器接受多种同义写法：

```markdown
# 林夏

## 定位
旧书店「拾光」的店主。二十七岁，话少，动作比语言慢半拍。

## 口吻
短句。多用句号，少用问号和感叹号。绝不使用网络流行语。

## 示例台词
- 不卖。
- 你来了。

## 禁则
- 绝不承认自己害怕孤独

## 禁用词
- 绝绝子

## 关系
- 陈默 | 常客 | 每周三来一次 | 0.05

## 世界书
- 相册,旧照片 :: 相册最上面那一格夹着一张泛黄的旧照片。 :: 9
```

`## 禁则` 与 `## 禁用词` 不是装饰——前者会被塞进重试指令，后者会被硬性拦截。

支持的小标题别名：`定位/一句话`、`人格/性格设定`、`口吻/语气/说话风格`、`示例台词/例句`、
`特质/性格`、`目标/动机`、`恐惧/弱点`、`禁则/边界/底线`、`禁用词/禁忌词`、`别名/称呼`、
`关系/人物关系`、`世界书/lore`、`背景/世界观`。

校验：`cargo run -p styx -- card examples/cards/linxia.md`

---

## CLI

```text
styx repl          交互式角色扮演（含 /state /scene /events /tools /call /recall /assoc
                   /remember /observe /raw /report /debug /reset /save /load /probe）
styx say "文本"    单次对话；--json 输出结构化结果，适合脚本
styx serve         启动多会话 TCP 服务（一行一个 JSON）
styx probe         逐个探测后端；--deep 会真的发一次最小请求
styx demo          完全离线跑通整条链路（CI 冒烟测试也用它）
styx card FILE     校验 / 规范化角色卡；--normalize / --json
styx init          生成 styx.toml 与示例角色卡
```

---

## 作为库使用

```rust
use std::sync::Arc;
use styx_core::{CharacterCard, Kernel, KernelConfig, Scene};
use styx_assoc::InMemoryAssoc;
use styx_memory::InMemoryMemory;

let card = CharacterCard::parse_markdown(CARD_MD)?;   // 或 CharacterCard::load(path)?
let mut kernel = Kernel::builder(card, Scene::new("拾光旧书店"))
    .llm(styx_llm::offline_llm())                      // 换成端点池即可联网
    .memory(Arc::new(InMemoryMemory::new()))           // 换成 NebulaMemory 即可持久化
    .assoc(Arc::new(InMemoryAssoc::new()))             // 换成 MightBeAssoc 即可多跳发散
    .config(KernelConfig::default())
    .build()?;

let outcome = kernel.turn("我想看看那本相册。")?;
println!("{}", outcome.render());
```

想自己实现端口？只要满足 `styx_core::ports::*`，内核完全不关心你背后是什么。

---

## 测试

```bash
cargo test --workspace           # 全部单测，全程不联网、不 sleep
cargo run -p styx -- demo        # 端到端冒烟
cargo clippy --workspace --all-targets -- -D warnings
```

需要真实服务的部分用 `probe` 显式验证，而不是让单测去依赖环境——
这也是把「调度」与「传输」分开的原因：注入一个假后端就能精确断言
"哪个端点在什么时刻被选中、熔断何时打开、冷却后是否回归"。

---

## 许可

MIT © 2026 yxpil
