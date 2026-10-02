# 与其它仓库的联动

Styx 采用**协议适配器优先**的策略：把其它仓库当作"服务 + 协议"，
以**原生客户端**身份接入，而不是把对方源码编译进来。

这个选择带来三个结果：

1. **零硬依赖**：`cargo build` 不需要任何外部服务，也不需要对方的 crate；
2. **真联动**：接入后在字节层面互通，不是"接口长得像"；
3. **归属清晰**：数据资产由它的主人（那个服务进程）持有，不被复制进每个使用者。

---

## 总览

| 仓库 | 协议 | Styx 侧 | 默认地址 | 环境变量 |
|---|---|---|---|---|
| [Nebula](https://github.com/yxpil/Nebula) | TCP 加密（NEBULA2） | `styx-memory::NebulaMemory` | `127.0.0.1:7878` | `NEBULA_ADDR` `NEBULA_USER` `NEBULA_PASSWORD` |
| [MightBe](https://github.com/yxpil/MightBe) | TCP 行分隔文本 | `styx-assoc::MightBeAssoc` | `127.0.0.1:9527` | `MIGHTBE_ADDR` `MIGHTBE_NETWORK` |
| [MemoryPool](https://github.com/yxpil/MemoryPool) | HTTP REST / BIT Remote | `styx-pool::MemoryPool` | `http://127.0.0.1:8751` | `MEMORYPOOL_URL` `MEMORYPOOL_TOKEN` |
| 任意 OpenAI 兼容服务 | HTTPS / HTTP | `styx-llm::EndpointPool` | 配置 | `STYX_LLM_<NAME>_BASE_URL` `_MODEL` `_API_KEY` |
| 任意 MCP 服务器 | Streamable HTTP + JSON-RPC 2.0 | `styx-tools::McpToolPort` | 配置 | — |
| [BIT](https://github.com/yxpil/bit) | Remote 信封 | `styx-tools::BitRemoteToolPort` | 配置 | — |

一条命令确认全部：

```bash
cargo run -p styx -- probe          # 只测连通性，毫秒级
cargo run -p styx -- probe --deep   # 模型端点会真的发一次最小请求
```

---

## Nebula —— 加密长期记忆

### 为什么复刻协议

`styx-memory` 有两种做法：依赖 `nebula-engine` 在进程内开库，或复刻协议做客户端。
选后者，因为：

- 记忆库是**独立的数据资产**，应该由 Nebula 进程持有；
- Nebula 自己就是这么设计的：单文件 `.ndb` + `serve` 多客户端；
- 协议比 API 稳定——对方重构内部模块时，协议通常不动。

### 有线协议（v2）

```text
客户端                                服务端
  │ ── "NEBULA2" (7 字节魔数) ──────────▶ │
  │ ◀──────────────  0x02 (READY) ─────  │   版本不匹配时返回别的字节
  │ ── varint 长度 + 用户名 ──────────▶ │
  │ ◀──── salt(16) ‖ challenge(32) ────  │
  │ ── HMAC-SHA256 proof(32) ─────────▶ │
  │ ◀──────────────  status(1) ────────  │   0x01 = OK
  │                                      │
  │ ◀═════ ChaCha20-Poly1305 帧 ═══════▶ │   seq 从 0 起双向独立
```

派生链：

```text
master_key    = Argon2id(password, salt, m=19 MiB, t=2, p=1, out=32)
session_key   = HKDF-SHA256(ikm = master_key,
                            info = "nebula/session/v2" ‖ challenge,
                            out  = 32)
proof         = HMAC-SHA256(key = session_key, msg = challenge)
```

帧加密（`seal` / `open`）：

```text
帧体 = nonce(12) ‖ ciphertext ‖ tag(16)
AAD  = "nebula/frame/up"   ‖ seq(u64 big-endian)    ← 客户端 → 服务端
       "nebula/frame/down" ‖ seq(u64 big-endian)    ← 服务端 → 客户端
上限 = 16 MiB/帧
```

AAD 里带方向和序号，是为了让"重放第 N 帧"和"把上行帧当下行帧塞回去"
在密码学层面就失效——而不是依赖应用层去检查。

握手阶段是独立的字节序列（不走帧）；握手完成后，加密帧里的载荷是同一种
请求对象：`Sql { sql }` / `Ping` / `Close`。响应带回 `ok` 标志、列名与行数据。

### 映射到 `MemoryPort`

| 端口方法 | Nebula SQL |
|---|---|
| `remember` | `INSERT INTO memories (content, tags, source, importance) VALUES (…)` |
| `recall` | `SEARCH '<query>' LIMIT n`（BM25）或带 `WHERE` 的 `SELECT` |
| `related` | `RELATED TO <id> LIMIT n`（共现图跳数扩展） |
| `forget` | `DELETE FROM memories WHERE id = '<id>'` |

读回统一走 `SELECT id, content, tags, importance, created_at FROM memories …`。
语句由 `styx-memory::schema` 的构造函数拼装，标识符经 `safe_ident`、
字符串经 `sql_quote` 处理，避免角色名里的引号把语句拼坏。

连接错误时会**自动重连一次**（`auto_reconnect`），仍失败则由内核降级到内存实现。

### 按角色的命名空间

`KernelConfig` 会把 `source` 写成 `styx:<角色命名空间>`，
CLI 还会把会话名并进去（`林夏-repl`、`林夏-<session_id>`），
于是"林夏"和"别的角色"、"调试会话"和"正式会话"的记忆天然隔离。

---

## MightBe —— 可解释联想

### 文本协议

行分隔，请求以 `;` 结尾（缺失时 Styx 自动补），语句里的换行会被压成空格：

```text
→ SELECT word, score FROM ASSOCIATE(doc_rnn, '照片') LIMIT 6;

← OK word|score
← 相册|0.83
← 底片|0.41
← INFO 2 rows from net doc_rnn
← END (rows=2, ms=3)
```

失败：

```text
← ERR 42 unknown net 'doc_rnn'
```

### 查询模板是配置项

MightBe 的方言仍在演进（其 README 明确标注 ES 阶段），而"联想"在 Styx 里
是一个**可替换的能力**而不是硬编码依赖。所以语句做成模板：

```toml
[assoc]
query_template = "SELECT word, score FROM ASSOCIATE({net}, {seed}) LIMIT {limit}"
```

可用占位符：`{net}`（网络名）、`{seed}`（种子词）、`{limit}`（条数）。
换语法时改配置，不改代码。

种子词会做转义（去掉单引号、换行、分号）后再包进 `'…'`，防止用户输入里的
引号把语句拼成两句。

### 三个语义对齐点

| 概念 | MightBe | Styx |
|---|---|---|
| 证据 | 支撑联想的原句 / 文档片段 | `Association::evidence` |
| 置信度 | confidence | `Association::confidence` |
| **弃判** | abstain | `AssocPort::confident()` 返回 false → 不给联想 |

"弃判"是这个端口最值得说的一条：角色"想不起来"，比编造一段似是而非的记忆
安全得多。把它做进接口，比写进提示词可靠。

### 语义映射

```rust
SELECT word, score FROM ASSOCIATE({net}, {seed}) LIMIT {limit}
   ↓ map_rows
Vec<Association> { word, score, evidence, confidence }
```

`normalize_confidence` 会把后端给的不同量纲（0-1 / 0-100 / 对数分数）
统一到 0..1，因此内核不需要知道"这次连的是哪个 MightBe 版本"。

---

## MemoryPool —— 跨 agent 共享记忆

### 为什么和长期记忆分开

| 端口 | 存什么 | 生命周期 | 可见性 |
|---|---|---|---|
| `MemoryPort` | 这个角色经历的细节 | 随角色成长 | 私有、可加密 |
| `PoolPort` | 多个 agent 之间要共享的事实 | 随协作现场 | 所有 agent 可见 |

混在一起，最后一定会出现"某个角色的私密往事被别的 agent 读到了"。
这是数据模型层面的错误，就该在数据模型层面阻止。

### 两种入口

**REST**（`use_remote = false`）：

```text
GET  /health                       → {"ok":true,"count":42}
POST /mem      {"text":"…","tags":[…],"importance":0.7,"source":"styx:林夏"}
GET  /mem/search?q=…&limit=4
```

`/health` 是唯一不做 token 保护的端点——这样探活不需要凭据，
`styx probe` 才能在没有 token 的情况下告诉你"服务起没起"。

**BIT Remote**（`use_remote = true`）：

```json
POST /invoke
{"tool":"memorypool","invoked_by":"styx","params":{"action":"search","query":"…","limit":4}}
```

这个信封与 BIT 的工具调用协议同构，因此 MemoryPool 可以被 BIT 当作工具调用，
也可以被 Styx 当作记忆池——**同一份服务，两个身份**。

### 健康检查失败的处理

`PoolPort::health()` 会真的发一次 `GET /health`。不可达时：

- 端口仍然被注入（`KernelStatus` 里标记为降级），
- 但回合里读它失败只会追加一条 `notice`，**不会让回合失败**。

---

## 语言模型端点

只实现 **OpenAI 兼容** 协议，因为它已经事实上是通用语：官方 OpenAI、DeepSeek、
通义千问 compatible-mode、Moonshot、Groq，以及本机的 Ollama / LM Studio /
vLLM / one-api 全都说这一套。

```toml
[[llm.endpoints]]
name     = "openai"
base_url = "https://api.openai.com/v1"
api_key  = "sk-…"
model    = "gpt-4o-mini"
weight   = 3
max_failures = 3     # 连续失败几次熔断
cooldown_secs = 30   # 熔断后冷却多久

[[llm.endpoints]]
name     = "ollama"
base_url = "http://127.0.0.1:11434/v1"
model    = "qwen2.5:14b"
weight   = 1
```

路径推导规则（覆盖现实里最常见的几种写法）：

| `base_url` | 实际请求 |
|---|---|
| `https://api.openai.com/v1` | `…/v1/chat/completions` |
| `https://api.deepseek.com` | `…/v1/chat/completions` |
| `http://127.0.0.1:11434/v1` | `…/v1/chat/completions` |
| `http://h:3000/proxy/v1/chat/completions` | 原样使用 |

需要更奇怪的路径时用 `chat_path` 直接指定。

也可用环境变量，便于在 CI / 容器里注入：

```bash
export STYX_LLM_PRIMARY_BASE_URL=https://api.deepseek.com
export STYX_LLM_PRIMARY_MODEL=deepseek-chat
export STYX_LLM_PRIMARY_API_KEY=sk-…
```

候选名：`primary` `secondary` `tertiary` `openai` `deepseek` `ollama` `local`。

调度行为（对应 RLBCM 那类负载均衡模块在客户端侧的形态）：

- **SWRR 平滑加权轮询**：3:1 的权重长期严格 3:1，短期也均匀（不是随机）；
- **熔断**：连续失败 `max_failures` 次后进入冷却，冷却期内该端点**不会被选中**；
- **半开试探**：**全部**端点都在冷却时放行一个，否则服务会永久卡死；
- **单次调用不重复试同一个端点**，`max_attempts` 限制总尝试次数。

---

## MCP 与 BIT 工具

三个来源通过 `ChainedTools` 串成一个 `ToolPort`，内核不知道工具从哪来：

```toml
[tools]
builtin = true           # 时钟 / 骰子 / 挑选 / 记忆检索 / 联想

[[tools.mcp]]
name  = "panoptes"
url   = "http://127.0.0.1:8760/mcp"
token = ""
```

MCP 客户端实现的是 **Streamable HTTP**：

- `initialize` → `notifications/initialized` → `tools/list` → `tools/call`；
- 响应体既接受 `application/json`，也接受 `text/event-stream`（SSE，取 `data:` 行）；
- 复用服务端返回的 `Mcp-Session-Id` 头，会话不掉线。

`register_builtins` 只会注册**当前真的能工作**的工具：没有记忆后端时不注册
`recall`。让模型看见一个必然失败的工具，比看不见它更糟——它会尝试、会失败、
然后在后续回合里反复尝试。

---

## 降级矩阵

| 场景 | 结果 |
|---|---|
| Nebula 没起 / 没密码 | 内存 BM25 兜底，`/status` 标记降级 |
| MightBe 没起 | 内存共现图兜底（PMI + 多跳 + 弃判） |
| MemoryPool 没起 | 仍注入端口但回合跳过；或 `enabled = false` 完全不注入 |
| 模型端点全挂 | `turn()` 返回错误；可先用 `probe --deep` 定位 |
| 配置 `backend = "openai"` 但无端点 | **硬报错**（这是配置自相矛盾，不该静默） |

前四种是"环境波动"，第五种是"你写错了"——两者必须区别对待，
否则用户会一直以为自己配置对了。
