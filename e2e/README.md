# 端到端测试：本机 Ollama × phi4

这套东西用来**真的跑一遍** Styx 的角色扮演链路，不走任何 mock。

## 为什么专门用 phi4

`phi4` 的 `capabilities` 只有 `completion`——**没有 tools、没有 vision**。

这正是「文本约定协议」要覆盖的最坏情况：模型不会 function calling，
表情包、记忆召回、联想这些通道全靠行首标签驱动。如果 phi4 能被协议驱动出
记忆召回和素材投递，那么支持 tools 的模型更没问题。

实测也证明了这一点：phi4 会主动发出 `[回忆] 伞`，内核执行后角色据此改口
（「你的伞还在，就在柜台后面。」）——两阶段生成在建模范式上跑通了。

## 跑起来

先确认 Ollama 在本机跑着，且已经拉到 phi4：

```bash
ollama list | grep phi4
```

然后在仓库根目录执行：

```bash
# 连通性（会真的打一次模型）
./target/debug/styx --config e2e/ollama.toml probe --deep

# 单轮，看结构化分流
./target/debug/styx --config e2e/ollama.toml say --json "你来了。外面下雨了。"

# 多轮 REPL（支持管道输入，非交互模式会跳过横幅）
printf '%s\n' "我昨天把伞落在你店里了。" "/events 20" "/quit" \
  | ./target/debug/styx --config e2e/ollama.toml repl

# web 前端 + 多轮脚本
./target/debug/styx --config e2e/ollama.toml web &
python e2e/e2e_phi4.py <会话名>
```

## 两个环境上的坑

**一、本机设了 HTTP 代理时会连 `127.0.0.1` 一起转发。**

这台机器上 `HTTP_PROXY=http://127.0.0.1:14249` 是设着的，于是后端一旦没起来，
看到的不是「连接被拒」而是代理吐的 **502 Bad Gateway**——把「服务没起」
误导成「服务起了但上游挂了」，排查方向直接跑偏。

- `curl` 要加 `--noproxy '*'`
- Python 要 `urllib.request.ProxyHandler({})`（脚本里已经这么做了）

**二、后台起服务不能用 `nohup … &`。**

主命令一返回，子进程就被回收，服务静默消失。用工具自带的后台任务机制。

## 脚本读什么

`e2e_phi4.py` 每轮把模型的四个通道**分开打印**——台词 / 动作 / 内心 / 素材。
这样一眼就能看出哪一路丢了，而不是只看到"回复好像有点怪"。

结尾会打一张通道计数表。**这张表是最有用的单一指标**：健康的一轮里
四个通道的数量应当大致相当（实测最好的一轮是
`user_input 5 / action 5 / speech 5 / thought 5`）。某个通道明显塌成 0
或者明显偏高，就说明有一类行落错了地方。

## 配置里几个刻意的选择

- `temperature = 1.0`：角色扮演要的是「这个人会怎么反应」，不是「最可能的下一句」。
- `protocol_hops = 1`：phi4 一次「取资料 → 重说」够用；设 2 会变成三倍延迟。
- `timeout_secs = 300`：14.7B Q4_K_M 首次推理要加载权重，60 秒默认值会假性超时。
- 记忆与联想**故意不走 Nebula / MightBe**：要验的是 Styx 自身的编排链路，
  让外部服务缺席，链路本身的问题才不会被「服务没起来」掩盖。
