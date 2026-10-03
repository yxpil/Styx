# Styx 测试说明 / Testing Guide

- 测试完成：是（2026-10-04）
- 测试日期：2026-10-04
- 测试内容：各 crate 既有 `#[cfg(test)]` 单元与既有集成测试；本次新增外部视角集成——styx-web `tests/injection_security.rs` 8（编码路径穿越 `%2e%2e%2f` 白名单拒绝、`/`与`\`段拒绝、NUL/空白拒绝、文件名上限、超大 body 读前拒绝、XSS 查询串不反射、垃圾请求行拒绝）；styx-tools `tests/registry_hooks.rs` 8（ToolRegistry/ChainedTools 注册→列出→调用、未知工具 UnknownTool、失败传播、同名覆盖、失败隔离、链式去重/第一端口胜出/失败回退）；styx-guard `tests/atomic_write_security.rs` 3（原子写往返、缺失读 None、不残留 .tmp）。
- 运行命令：`cargo test --workspace --no-fail-fast`（单 crate：`cargo test -p styx-web` / `-p styx-tools` / `-p styx-guard`）
- 测试框架：Rust `#[cfg(test)]` + 外部 `tests/` 集成测试
- 模型：豆包（Doubao）生成

Styx 是一个多 crate 的 Cargo workspace（`crates/` 下 13 个 crate）。各 crate 内部
早已广泛使用 `#[cfg(test)]` 单元测试，并有若干 `tests/` 集成测试。本次补测在**不改动
产品代码**的前提下，为安全/钩子相关的 crate 新增了从外部视角的集成测试。

## 测试目录位置

- 单元测试：各 crate `src/**` 内 `#[cfg(test)]`（原有，覆盖 core/llm/memory/guard/
  web/tools/server/observ/assoc/pool/vision/cli 等几乎全部模块）。
- 集成测试 `tests/`：
  - `crates/styx-guard/tests/`：原有 `crash_safety.rs`；新增 `atomic_write_security.rs`。
  - `crates/styx-web/tests/`：原有 `http_smoke.rs`；新增 `injection_security.rs`。
  - `crates/styx-tools/tests/`：**新增目录** `registry_hooks.rs`。
  - （原有）`crates/styx-assoc/tests/mightbe_protocol.rs`、`crates/styx-vision/tests/*`。
- 说明文档：本文件 `TESTING.md`。

## 运行命令

```powershell
# 整个 workspace 全部测试（编译全部 13 个 crate，首次较慢）
cargo test --workspace

# 只跑本次补测相关的三个 crate
cargo test -p styx-web -p styx-tools -p styx-guard

# 只跑某一个集成测试文件
cargo test -p styx-web   --test injection_security
cargo test -p styx-tools --test registry_hooks
cargo test -p styx-guard --test atomic_write_security

# 只跑某个用例
cargo test -p styx-web --test injection_security encoded_path_traversal_is_decoded_then_rejected
```

## 测了什么 / 预期通过数

本次补测目标（三个 crate）本地结果：

| crate | 目标 | 通过 | 失败 |
|---|---|---|---|
| styx-guard | 单元测试 | 26 | 0 |
| styx-guard | 集成 `crash_safety.rs`（原有） | 1 | 0 |
| styx-guard | 集成 `atomic_write_security.rs`（新增） | 3 | 0 |
| styx-tools | 单元测试 | 19 | 0 |
| styx-tools | 集成 `registry_hooks.rs`（新增） | 8 | 0 |
| styx-web | 单元测试 | 22 | 0 |
| styx-web | 集成 `http_smoke.rs`（原有） | 13 | 0 |
| styx-web | 集成 `injection_security.rs`（新增） | 8 | 0 |

> 其余 crate（styx-core / styx-llm / styx-memory / styx-assoc / styx-pool /
> styx-observ / styx-server / styx-vision / styx-cli）的单元测试由 `cargo test --workspace`
> 统一运行，本仓库原本即为绿色。

## 输入注入测试（`crates/styx-web/tests/injection_security.rs`，共 8 个）

针对本机 HTTP 服务的安全边界，从外部验证：

1. `encoded_path_traversal_is_decoded_then_rejected` —— `%2e%2e%2f` 编码的 `../`
   经百分号解码后仍含 `..`，被 `is_safe_segment` 白名单拒绝（表情包/静态资源不会被
   目录穿越读到）。
2. `separator_and_backslash_segments_rejected` —— `/` 与 `\` 段被拒绝。
3. `control_chars_and_null_byte_rejected` —— NUL、tab、空格段被拒绝。
4. `overlong_name_rejected` —— 文件名长度上限 128（129 拒绝、128 放行）。
5. `oversized_body_rejected_before_read` —— `Content-Length > MAX_BODY` 在读取 body
   之前即报错（防内存耗尽）。
6. `xss_query_param_is_inert_not_reflected_into_headers` —— `<script>…</script>` 查询串
   作为惰性数据解析，且响应绝不反射用户输入（`write_response` 输出不含 payload），
   并始终带 `X-Content-Type-Options: nosniff`。
7. `query_parsing_treats_encoded_payload_as_data` —— `%3Cimg%20onerror%3D…%3E` 解码为
   字符串数据。
8. `garbage_request_line_is_rejected` —— 非法请求行（无 path 的单 token）被拒。

## 钩子/插件机制测试（`crates/styx-tools/tests/registry_hooks.rs`，共 8 个）

验证工具注册表与链式端口的钩子语义：

1. `register_list_and_invoke_roundtrip` —— 注册→列出→调用往返。
2. `unknown_tool_is_rejected` —— 未注册工具返回 `StyxError::UnknownTool`。
3. `tool_failure_propagates_without_panicking` —— 工具错误正常向上传播，不 panic。
4. `registration_overwrites_same_name` —— 同名注册覆盖而非重复。
5. `failure_isolation_between_tools` —— **失败隔离**：一个工具失败，其它工具仍可正常调用。
6. `chained_dedupes_and_first_port_wins` —— 链式端口去重、靠前端口胜出。
7. `chained_falls_back_when_first_port_errors` —— 第一个端口失败时回退到后续端口。
8. `chained_unknown_tool_errors` —— 整条链上都没有的工具返回 `UnknownTool`。

## 其它新增集成测试

`crates/styx-guard/tests/atomic_write_security.rs`（3 个）：从外部验证 `atomic_write`
覆盖写完整落盘、`read_if_exists` 对缺失文件返回 `None`、成功写入后不残留 `.tmp` 临时文件。
