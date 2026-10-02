# 架构

> 这份文档解释 Styx 为什么长成现在这样。每一条取舍都对应一个具体的失败场景——
> 不是"这样更优雅"，而是"不这样就会出事"。

---

## 0. 一句话

**内核只依赖端口，端口各有适配器，适配器不可用时降级到内存实现。**

```text
styx-core    ← 没有网络、没有文件、没有时间依赖之外的东西
   ↑ trait
适配器        ← 唯一知道"外面长什么样"的地方
   ↑
外层          ← CLI / TCP 服务 / 你的程序
```

`styx-core` 的 `Cargo.toml` 里只有 `serde` / `serde_json` / `thiserror`。
没有 tokio、没有 reqwest、没有 HTTP 客户端、没有数据库。
这意味着内核可以在任何地方编译，也意味着它可以被完整地单测。

---

## 1. 为什么是「端口」而不是「插件」

角色扮演内核的外部依赖有一个特点：**它们的可用性是波动的**。

一台开发机上，Nebula 可能正在跑，MightBe 可能还没起，模型可能是本机 Ollama，
也可能是某个中转站的 key 刚好过期了。如果内核直接 `use nebula_engine::Db`，
那么"其中任何一个不在"就等于"整个程序起不来"。

端口（`styx-core::ports`）把这五个能力抽象成对象安全的 trait：

| 端口 | 一个方法 | 一句话 |
|---|---|---|
| `MemoryPort` | `remember` / `recall` / `related` / `forget` | 这个角色的私人经历 |
| `AssocPort` | `associate` / `confident` | 从一个词想到另一个词 |
| `PoolPort` | `remember` / `recall` / `stats` | 多个 agent 之间共享的事实 |
| `LlmPort` | `complete` | 把提示词变成文本 |
| `ToolPort` | `list` / `invoke` | 作用于世界 |

每个端口都有 `health()`。上层据此决定是否降级——**降级是内核的合法运行状态**，
不是异常。

### 为什么 `MemoryPort` 和 `PoolPort` 是两个端口

因为它们**生命周期和可见性不同**：

- 长期记忆属于**一个角色**，随角色成长累积，可以加密、可以私有；
- 共享记忆池属于**一个协作现场**，多个 agent 读写同一份事实。

把二者混为一谈，最后一定会出现"某个角色的私密往事被别的 agent 读到了"。
这个错误在数据模型层面就该被阻止，而不是靠调用方自觉。

---

## 2. 角色卡（静态）与动态状态（可变）分离

这是"防人设漂移"的地基。

```rust
pub struct CharacterCard {   // 运行期不变，会被快照
    pub name: String,
    pub archetype: String,   // 定位：一句话人设
    pub persona: String,     // 人格：长文本
    pub speech_style: String,
    pub speech_samples: Vec<String>,  // 口吻的"可模仿样本"
    pub boundaries: Vec<String>,      // 禁则（进重试指令）
    pub banned_phrases: Vec<String>,  // 禁用词（硬拦截）
    pub relations: Vec<Relation>,
    pub lore: Vec<LoreEntry>,         // 关键词触发的世界书
    pub background: String,
}

pub struct DynamicState {    // 每回合演化
    pub mood: Mood,          // 效价-唤醒 二维
    pub energy: f32,
    pub tension: f32,
    pub affinity: BTreeMap<String, f32>,
    pub trust: BTreeMap<String, f32>,
    pub agenda: Vec<String>, // 当前意图
    pub flags: BTreeSet<String>,
    pub turn: u64,
}
```

模型可以通过 `[情]` 改变心情、通过 `[关系]` 改变好感，
但它**没有任何字段可以改掉 `persona` 或 `boundaries`**。
"我因为太生气了所以决定坦白"这种漂移，在数据结构层面就不成立。

`Mood` 用 valence-arousal 二维而不是一个枚举：因为"戒备→愤怒"和"戒备→低落"
在二维上是两条不同的路径，枚举会丢掉这个信息。每回合按 `decay_rate` 向中性衰减，
于是情绪有惯性但不会永久粘住。

---

## 3. 标签 DSL：结构化输出但不强求 JSON

要求模型输出 JSON 有三个现实问题：容易漏引号、长文本里换行要转义、
小模型经常整段崩掉。而行首标签有几个好处：

- 天然容忍换行（文本就是值，不用转义）；
- 人也能读——`styx say --raw` 直接看就知道模型干了什么；
- 缺字段是可恢复的（缺 `[情]` 只是这回合状态不变）。

```
[说] 不卖。                          ← 台词
[做] 她把相册合上                    ← 动作 / 场面
[想] 又是为了那件事                  ← 内心独白
[情] 心情=戒备 效价=-0.1 紧张=+0.2    ← 当前状态（增量）
[关系] 陈默 好感=-0.1                 ← 关系（增量）
[忆] 陈默想看母亲的相册 | 标签=相册,母亲 | 重要度=0.9   ← 写入长期记忆
[场] 时间=深夜                        ← 场景变更
```

`Reply::parse` 的容错策略：先试 JSON（模型如果给了 JSON 就用），失败再走标签；
完全不认识的行**降级成台词**而不是丢弃（宁可多一句废话，也不要把模型说的话吃掉）。

`[情]` 和 `[关系]` 是**增量**而非绝对值——因为模型很难准确维持一个绝对值，
但"这次让我更紧张了"它是说得清的。

---

## 4. 一致性守护：硬违规重试，软漂移提醒

`Guard::audit` 输出两类东西：

| 类别 | 触发条件 | 后果 |
|---|---|---|
| `violations`（硬） | 出现禁用词；完全没有台词 | 带角色卡禁则**自动重试一次** |
| `warnings`（软） | 台词长度远超示例口吻；禁则清单 | 作为提醒注入**下一轮**提示词 |

重试不是无条件采纳：新结果只有**违规项变少**才替换，且重试时温度下调 0.2
（减少再次踩同一个坑的概率）。`new_audit.violations.len() < audit.violations.len()`
这一行保证了"重试不会让情况变糟"。

为什么口吻漂移只提醒不重试？因为"台词太长"是主观的，为它花一次额外的生成
不划算；而"出现了禁用词"是客观的，必须修掉。**把成本和收益对齐**是这里的全部考虑。

---

## 5. 提示词预算与动态借额

上下文窗口是稀缺资源。`PromptBuilder` 把系统提示切成若干段并各自分配预算，
总预算覆盖 `default`(6000) / `compact`(3000) / `large`(16000) 三档。

关键设计是**动态借额**：如果本回合没有召回记忆（新角色、冷启动），
记忆段的预算会借给历史段——而不是留着一块空地。
反过来，历史太长时从**尾部**取（最近的对话最重要），并在开头显式标出省略。

`PromptReport` 把每个段实际用了多少、丢弃了多少记下来，
通过 `TurnOutcome::report` 暴露给上层。角色扮演调试里最常见的问题是
"模型为什么没按我说的做"——十有八九是那段内容根本没进提示词。

---

## 6. LLM 层：调度与传输分离

```text
LlmPort  ← 内核只看见这个
   │
EndpointPool     加权轮询 + 熔断 + 半开试探 + 故障转移（纯逻辑）
   │
ChatBackend      真正的传输：OpenAiBackend（HTTP）/ MockBackend（离线）
```

**为什么分开**：调度逻辑是可验证的，传输不是。
注入一个假后端就能精确断言"哪个端点在什么时刻被选中、熔断何时打开、
冷却后是否回归"，全程不联网、不 `sleep`。如果你的调度逻辑和 HTTP 调用
缠在一起，这些断言就只能靠集成测试去撞，而集成测试永远跑不稳。

调度用 **SWRR（平滑加权轮询）** 而不是随机：随机在高权重下会有抖动，
SWRR 保证 3:1 的权重在长期严格 3:1，短期也均匀。

熔断的细节：

- 连续失败 `max_failures` 次 → 打开冷却；
- 冷却期内该端点**不会被选中**（而不是"选中后立刻失败"）；
- **全部端点都在冷却**时放行一个做半开试探——否则服务会永久卡死。

### 为什么只实现 OpenAI 兼容协议

因为它已经事实上成了通用语：官方 OpenAI、DeepSeek、通义千问 compatible-mode、
Moonshot、Groq，以及本机的 Ollama / LM Studio / vLLM / one-api 全都说这一套。
只实现它，等于一次性接上了几乎全部模型服务。

端点之间的差异（地址、密钥、模型名、权重、超时、熔断阈值）都在 `Endpoint` 里，
**不在协议里**。所以加一个新供应商 = 加一行配置，不是加一个模块。

---

## 7. Nebula：为什么要复刻协议而不是依赖 crate

`styx-memory` 有两种可能的做法：

1. 依赖 `nebula-engine` crate，在进程内开一个数据库；
2. 复刻 Nebula 的 TCP 协议，作为一个客户端连过去。

选 2，理由有三条：

- **记忆库是一个独立的数据资产**，应该由它的主人（Nebula 进程）持有，
  而不是被嵌进每一个使用它的程序里。Nebula 自己就是这么设计的：
  单文件 `.ndb` + `serve` 多客户端。
- **Styx 因此可以零硬依赖编译**。没有 Nebula 的机器上照样 `cargo build`。
- **协议是稳定接口**，API 不是。对方重构内部模块时，协议通常不动。

但"走协议"不等于"弱集成"——`styx-memory::crypto` 与 `wire` 逐位对齐：

| 环节 | 参数 |
|---|---|
| 魔数 | `NEBULA2`（7 字节） |
| 握手 | `0x02` ready → varint 身份 → salt(16) + challenge(32) → HMAC proof(32) → status |
| 密钥派生 | Argon2id(m=19 MiB, t=2, p=1) → HKDF-SHA256(info=`"nebula/session/v2"` ‖ challenge) |
| 帧加密 | ChaCha20-Poly1305，输出 `nonce ‖ ct ‖ tag` |
| 帧 AAD | `"nebula/frame/up"` / `"nebula/frame/down"` ‖ seq(u64 BE) |

单测里包含一个**自己写的"服务端侧"假实现**，做完整的握手往返——
这样字节级互通是被测试保证的，而不是靠"我读代码读得很仔细"。

---

## 8. 兜底实现是"真能用"，不是 stub

`InMemoryMemory` 实现了**真的 BM25**（k1=1.2, b=0.75，含倒排索引与长度归一化），
`InMemoryAssoc` 实现了**真的共现图 + PMI + 多跳 beam 搜索 + 弃判**。

理由有三个，缺一不可：

1. Nebula 没启动时，角色仍然记得住、想得起，只是记忆不落盘——体验不塌方；
2. 单元测试不必依赖外部服务，因此跑得很快、很稳；
3. 它同时是**参照实现**：Nebula 的 BM25 与 `RELATED` 语义在这里被写成
   几十行可读代码，对照调试时非常好用。

"弃判（abstain）"值得单独说：`AssocPort::confident()` 对应 MightBe 的
"没把握就不给"。角色"想不起来"，比编造一段似是而非的记忆安全得多。
这是把**诚实**做进接口，而不是写进提示词。

---

## 9. 线程模型：一连接一线程，同步内核

`styx-server` 用一连接一线程，没有 async runtime。

角色扮演是**低并发、高延迟**的场景：每回合要等模型几秒到几十秒，
并发数通常是个位数。这种负载下，异步运行时换来的收益（内存、上下文切换）
远不如"代码简单、能塞进 CLI、能被线程池驱动"来得实在。

内核是同步的，因此：

- 想包成 async？在外层用 `spawn_blocking` 就行，内核不用改；
- 想放进 GUI？直接在后台线程跑 `turn()` 就行。

服务端持有一张 `会话名 → Kernel` 表，但**内核只在处理请求时被短暂取出**——
网络 IO 期间不持锁，因此不同会话之间互不阻塞。

协议用 JSON-lines（一行一个请求、一行一个响应）而不是二进制帧，
因为调试角色行为时，"肉眼能不能看懂"远比"字节效率"重要：

```bash
echo '{"op":"say","text":"我想看看那本相册。"}' | nc 127.0.0.1 7879
```

---

## 10. 工具：为什么注册表要"只注册能工作的"

`register_builtins` 在没有记忆后端时**不会注册** `recall` 工具。

让模型看见一个必然失败的工具，比看不见它更糟——它会尝试、会失败、
然后在后续回合里反复尝试，每次都要浪费一次工具往返。

同一个原则体现在工具来源的透明性上：内置工具、MCP 工具、BIT Remote 工具
通过 `ChainedTools` 串成一个 `ToolPort` 交给内核，**内核完全不需要知道
某个工具来自哪里**。这与 BIT / TentacleTool 的工具模型是同构的，
因此两边可以互相挂载。

---

## 11. 错误分层

```text
styx-core::StyxError      内核级：端口不可用 / 生成失败 / 解析失败 / 配置非法
  ├─ PortUnavailable { port, reason }   ← 可降级
  ├─ Llm / EmptyCompletion
  ├─ Memory / Assoc / Pool / Tool
  ├─ InvalidCharacter / InvalidConfig
  └─ Protocol / Auth / Io / Json / Other
```

`is_degradable()` 明确回答"这个错误该不该让上层换后端"。
适配器层的错误（`MemoryError` / `AssocError` / `PoolError` / `LlmError`）
各自独立，只在边界处 `From` 成 `StyxError`——**一个 crate 里只写一处
`From` impl**，避免出现"同一对类型有两个 From"这种编译错误，
也避免错误转换逻辑散落在各处。
