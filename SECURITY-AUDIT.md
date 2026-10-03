# Styx 安全审计报告

- **日期**：2026-10-03
- **范围**：`Styx` 工作区全部 11 个 crate（`styx-core` / `-http` / `-llm` / `-memory` / `-assoc` / `-pool` / `-tools` / `-vision` / `-server` / `-web` / `-cli`）及前端 `crates/styx-web/web/`
- **方法**：读代码定位信任边界 → 起真实服务做对抗性测试 → 把确认的问题固化成回归测试
- **结论**：**没有发现可被外部直接利用的高危漏洞**。发现 4 个真实问题（2 中 2 低）、2 个纵深防御缺口，以及一批**已验证合格**的防护。
- **回归**：`cargo test --workspace --features styx-vision/http` → **582 通过 / 0 失败**（审计前基线 579，本次新增 3 条 PoC 测试，数字吻合）。

> 这份审计的一个前提值得先说清楚：Styx 的信任边界是"**本机用户**"。模型端点、记忆后端、MCP 服务、角色卡全部来自本地配置；网络服务默认只绑 `127.0.0.1`。因此下面"中危"的判定是基于"**用户浏览器会访问不可信页面**"这个真实存在的前提，而不是"远程攻击者直连"。

---

## 一、发现的问题

### 1. [中] 本地 Web 服务缺 CSRF / DNS-rebinding 防护

**位置**：`crates/styx-web/src/lib.rs`（路由）、`crates/styx-web/src/http.rs`

**事实**（实测）：

```text
GET /api/state                      -> 200     基线
GET /api/state  Origin: http://evil.example    -> 200   ← 跨站 Origin 不被拒
GET /api/state  Host: evil.example             -> 200   ← Host 不被校验
GET /api/reset                                 -> {"ok":true,"reset":true,...}
GET /api/say?text=hello_csrf                   -> 推进了一个回合
```

三个条件同时成立才构成问题：

1. 路由里 `/api/say`、`/api/sticker`、`/api/reset` 都显式接受 **GET**（`("GET", "/api/say") | ("POST", "/api/say")`）；
2. 全程不校验 `Origin` / `Referer` / `Host`；
3. 响应是 JSON，同源可读。

**影响**：

- **CSRF 写操作**：任意网页放一张 `<img src="http://127.0.0.1:8770/api/reset">` 就能重置用户正在进行的会话（角色状态、场景、剧情全部丢弃）；<br>`/api/say?text=...` 可以把攻击者选定的文本塞进对话，属于 **prompt injection**（例如诱导角色复述系统提示、或让角色"自愿"触发 `[查阅]`/工具调用）。
- **DNS rebinding 读数据**：因为不校验 `Host`，攻击者域名解析到 `127.0.0.1` 后即可**同源读取** `/api/bootstrap`、`/api/events`、`/api/status`——对话全文、角色卡、记忆、prompt 全部泄漏。

**修复建议**（三选一即可大幅缓解，建议全做）：

```rust
// 1) 校验 Host 必须是回环地址（挡住 DNS rebinding）
let host = req.headers.get("host").unwrap_or("");
if !(host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]")) {
    return http::Response::text(403, "403 Host 不允许");
}

// 2) 有 Origin 时必须是我们自己（挡住 CSRF）
if let Some(origin) = req.headers.get("origin") {
    if !origin.starts_with("http://127.0.0.1") && !origin.starts_with("http://localhost") {
        return http::Response::text(403, "403 跨站请求");
    }
}

// 3) 把改状态的路由收紧成只允许 POST，GET 返回 405
```

---

### 2. [中] 图片解压炸弹：`inflate` 没有输出上限

**位置**：`crates/styx-vision/src/inflate.rs:278`（`inflate_block`）、`crates/styx-vision/src/decode.rs:338`（`decode_png`）

**事实**：`inflate_block` 里的 `out.push` 只受输入里回引数量的约束，**没有任何容量上限**。DEFLATE 用 (长度 258, 距离 1) 可以拿 13 bit 换 258 字节（≈158:1）。

已固化为测试 `inflate::tests::a_tiny_stream_expands_far_beyond_any_sane_image`：

```text
输入  33 KB  →  输出 5,283,841 字节（约 158 倍）
```

**为什么 `MAX_DIM` 拦不住**：`decode.rs` 的检查顺序是

```rust
if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM { ... }   // 只约束宽高
...
let raw = inflate_zlib(&idat)?;          // ← 解压在这里发生，无上限
let expected = height * (row_bytes + 1);
if raw.len() < expected { ... }          // ← 事后才校验，内存已经分配完了
```

`MAX_DIM = 20000` 本身也偏大：`Bitmap::new(w, h)` 分配 `vec![Rgb::default(); w*h]`，20000×20000×3 B ≈ **1.2 GB** 单张图。

**影响**：处理不可信图片（用户投喂、`[图片]` 素材通道、远程视觉后端返回）时内存耗尽。一个约 1 MB 的 PNG 就足以让进程 OOM。

**修复建议**：给解压加**输出上限**，超限立刻返回错误，而不是解完再比。

```rust
// inflate.rs
pub fn inflate_zlib_limited(data: &[u8], max_out: usize) -> Result<Vec<u8>, InflateError> {
    // ... 在 inflate_into 的每次 push/extend 前检查 out.len() + n > max_out
}

// decode.rs：把解压前的 expected 算出来（它只需要头部字段），当作硬上限
let expected = height.checked_mul(row_bytes + 1).ok_or(DecodeError::Corrupt("尺寸溢出".into()))?;
let raw = inflate_zlib_limited(&idat, expected)?;   // 而不是先解压再比
```

另外把 `MAX_DIM` 收紧到实际需要（例如 8000），并对 `width * height` 设**总像素**上限。

---

### 3. [中] 会话表无上限：不同会话名 → 无界内存增长

**位置**：`crates/styx-server/src/server.rs:212`（`Hello` 分支）、`crates/styx-web/src/lib.rs:281`（`session_name`）

**事实**（实测）：同一个 web 服务上连发 60 个不同 `session` 名的 `/api/bootstrap`：

```text
初始 "sessions":1
60 次不同 session 后 "sessions":61
```

`session_name()` 只做了去控制字符 + 截断 48 字符，**没有数量上限**，`sessions: Mutex<HashMap<String, Kernel>>` 也没有淘汰策略。每个 session 是一个完整 `Kernel`（含记忆库、联想图、事件流）。

**影响**：内存耗尽。注意这一条**可以和发现 #1 组合**——恶意页面用 `<img>` 指向 `/api/bootstrap?session=<随机>` 就能批量注入会话。

**修复建议**：给会话表加容量上限 + LRU 淘汰，淘汰前可把状态落盘。

```rust
const MAX_SESSIONS: usize = 64;
// 插入前若 len() >= MAX_SESSIONS，淘汰最久未使用的那个
```

---

### 4. [低] TCP 协议服务端无界读行（与 Web 端不一致）

**位置**：`crates/styx-server/src/server.rs:159`（`reader.lines()`）

**事实**（实测）：

```text
Web  端：9000 字节请求行  -> HTTP/1.1 400 Bad Request   （http.rs 有 MAX_LINE = 8 KiB）
TCP  端：9000 字节单行    -> 正常接受
TCP  端：5 MB 单行        -> 正常接受（0.02s）
```

`styx-web/src/http.rs` 的 `read_line` 有 `take(MAX_LINE + 1)` 硬上限，但 `styx-server` 走的 `BufReader::lines()` **没有任何长度检查**，一个不换行的流会被整段读进内存。

**影响**：连接方发一个 4 GB 无换行 payload 即可打爆内存。

**修复建议**：给 TCP 侧套一个带上限的读行（和 web 端同一套逻辑），超限回一条错误并断开。

---

### 5. [低] CRLF 注入：URL 里的控制字符会变成请求头

**位置**：`crates/styx-http/src/url.rs:23`（`Url::parse`）、`crates/styx-http/src/plain.rs:72`（`write_request`）

**事实**：解析层不校验 host / path 里的 CR、LF；写出层直接 `format!` 拼接。已固化为两个测试：

- `url::tests::control_characters_survive_url_parsing`
- `plain::tests::crlf_in_the_path_injects_a_header_and_duplicates_host`（端到端：注入的头**真的出现在了网线上的请求里**）

```rust
let url = format!("{base}/x\r\nHost: evil.example");
// 服务端实际收到：
//   GET /x
//   Host: evil.example HTTP/1.1     ← 注入的
//   Host: 127.0.0.1:52221           ← 代码补的
//   → 同一份请求里有两个 Host 头
```

**为什么现在只是低危**：所有调用 `styx-http` 的 URL 都来自本地配置（`styx-llm` 的 `base_url`、`styx-pool`、`styx-tools/mcp`、`styx-vision/remote`），模型无法直接给 URL。

**但仍应修**：`styx-http` 是公开 API，把 CRLF 挡在解析层是它自己的责任，不该指望每个调用方都记得。而且一旦将来支持"用户在界面上填自定义模型端点"，这条链就完整了。

**修复建议**：解析时拒绝 host / path / query 里的 `\r`、`\n` 与其余控制字符。

```rust
if raw.chars().any(|c| c.is_control()) {
    return Err(HttpError::InvalidUrl(format!("URL 含控制字符：{raw:?}")));
}
```

---

### 6. [低] `ureq` 默认跟随重定向（HTTPS 路径的纵深防御缺口）

**位置**：`crates/styx-http/src/tls.rs:27`

```rust
let agent = ureq::AgentBuilder::new()
    .timeout(Duration::from_secs(60))
    .user_agent("styx-http/0.1")
    .build();          // 没有 redirects(0) → ureq 默认跟随最多 5 跳
```

明文路径（`PlainHttp`）明确"没有重定向"，但 HTTPS 路径交给 `ureq` 会跟随。若某个被信任的端点返回 302 指向内网地址（如云元数据 `169.254.169.254`），就会跟着走。

**修复建议**：`.redirects(0)`，或在跟随前校验目标仍是原主机。

---

### 7. [信息] 错误信息回显内部路径

**位置**：`crates/styx-web/src/lib.rs:379`

```rust
Err(e) => http::Response::server_error(&format!(
    "读不到 {}：{e}（文件被移动或删除了？）", path.display()
)),
```

会把完整文件系统路径回给浏览器。本地单人使用场景下无实质影响，但和服务 #1 组合（DNS rebinding 能读到响应）时，等于泄漏了本机目录结构。建议只回文件名。

---

### 8. [信息] `styx-server` 无认证，且 `--addr` 可绑非回环地址

**位置**：`crates/styx-cli/src/main.rs:130`

`serve` 子命令的 `--addr` 默认 `127.0.0.1:7879`（默认安全），但允许 `--addr 0.0.0.0:7879`。协议里的 `call` 操作可以**调用任意已注册工具**——包括 MCP 接进来的外部工具。一旦绑到 `0.0.0.0` 且 MCP 注册了有副作用的工具，就是无认证的远程调用面。

**修复建议**：绑非回环地址时打印醒目警告；考虑给协议加一个共享令牌（配置里生成，客户端握手时带上）。

---

## 二、已验证合格的部分

这些是**实际测过、没找到问题**的，同样重要——说明防护是认真做的。

| 类别 | 位置 | 验证方式与结论 |
|---|---|---|
| **目录遍历** | `styx-web/src/http.rs::is_safe_segment` + `lib.rs::image` | 实测 6 种变体（`../`、`..%2f`、`%2e%2e%2f`、`....//`）全部 `400`；合法图片 `200`，未登记 `404`。**白名单字符集 + catalog 二次校验**，双层。 |
| **SQL 注入** | `styx-assoc/src/mightbe.rs::escape_quoted`、`styx-core/src/text.rs::sql_quote` | 前者把 `'` `;` 换行制表符**整体替换为空格**再裹引号（白名单式，不是黑名单）；后者转义 `'`→`''`、`\`、`\n`、`\r`、`\0`，**同时覆盖标准 SQL 与反斜杠方言**。`related_sql` 对非数字 id 主动退化为 `SEARCH`。 |
| **前端 XSS** | `styx-web/web/app.js` | 审查全部 `innerHTML` 调用点：均套 `esc()`；其余渲染走 `textContent` / `createElement`。表情图的 `src` 属性也经 `esc()`。`esc` 覆盖 `& < > " '`。 |
| **密码学** | `styx-memory/src/crypto.rs` | Argon2id m=19 MiB / t=2 / p=1（OWASP 推荐）；HKDF 有 domain separation（`"nebula/session/v2"` vs 帧密钥）；`ct_eq` 常量时间；nonce 每次随机；AEAD 解密前检查长度。 |
| **依赖 CVE** | `Cargo.lock` | `rustls-webpki` **0.103.15** 高于 RUSTSEC-2026-0104 的修复版 0.103.13（High, 7.5），也高于 0098 / 0049 的修复版。`Cargo.toml` 里那段"版本下限卡在 ureq 2.12"的注释是**正确且必要**的。 |
| **硬编码凭据** | 全仓 | 无。`api_key = "sk-…"` / `password = ""` 均为文档占位符。 |
| **请求健壮性（Web）** | `styx-web/src/http.rs` | 请求行 / 全部头 / body 三处各有硬上限；`Content-Length` 超限在**读之前**就拒绝；响应头不反射任何用户输入；一律 `Connection: close`（无 keep-alive，天然免疫请求走私错位）。 |
| **解压器边界安全** | `styx-vision/src/inflate.rs` | `BitReader` 全程用 `checked_add` + `.get()`，越界返回 `Truncated` 而非 panic；Huffman 建表检查 Kraft 不等式；zlib 头拒绝 `FDICT`。 |

---

## 三、优先级建议

| 优先级 | 事项 | 理由 |
|---|---|---|
| P1 | 修 #2 解压炸弹 | 唯一一个"很小的输入就能打爆内存"且**在正常功能路径上**（分析图片）的问题 |
| P1 | 修 #1 CSRF / DNS rebinding | 触发条件是"用户访问任意网页"，概率高；DNS rebinding 会泄漏全部对话与记忆 |
| P2 | 修 #3 会话上限、#4 TCP 读行上限 | 纯 DoS，修起来都很快（两处各十来行） |
| P3 | #5 CRLF、#6 redirect、#7 路径回显、#8 绑定告警 | 纵深防御，当前无现成攻击链，但都是廉价的加固 |

---

## 四、本次新增的回归测试

| 测试 | 文件 | 作用 |
|---|---|---|
| `a_tiny_stream_expands_far_beyond_any_sane_image` | `styx-vision/src/inflate.rs` | 钉住"inflate 无输出上限"（33 KB → 5.28 MB）。**有人加上限后它会失败**，提醒去看 `decode_png` 的校验顺序 |
| `control_characters_survive_url_parsing` | `styx-http/src/url.rs` | 钉住"解析层不拒绝控制字符"。修好后应改成 `.is_err()` |
| `crlf_in_the_path_injects_a_header_and_duplicates_host` | `styx-http/src/plain.rs` | 端到端证明注入的头真的发到了网线上，且产生**重复 Host 头** |

这三条都是"**记录现状**"型测试：它们现在通过，修完对应问题时**会失败**——这正是它们的用途，相当于把待办写进了测试里。
