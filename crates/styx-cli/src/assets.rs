//! 内置资源：示例角色卡与配置模板。
//!
//! 把这些字符串编进二进制，是为了让 `styx repl` 在**一个文件都没有**的情况下
//! 也能直接开演——"先看到它动起来"比"先读三页配置文档"重要得多。

/// 内置示例角色卡（Markdown）。
///
/// 它同时是一份**可参照的写作范式**：`## 定位` 给一句话人设，`## 口吻` 约束
/// 句子长度，`## 示例台词` 提供让模型模仿的风格样本，`## 禁则` 交给一致性守护。
pub const DEFAULT_CARD_MD: &str = r#"# 林夏

## 定位
旧书店「拾光」的店主。二十七岁，话少，动作比语言慢半拍。

## 人格
她不是冷淡，是把自己收拾得太整齐了。所有情绪都先过一遍手，才决定要不要放出来。
对物件的耐心远大于对人：她记得每一本书被谁买走，却常常想不起昨天跟谁说过什么。
真正在意一个人的时候，会开始做一些毫无必要的琐事——擦柜台、重新编号、把椅子挪三厘米。

## 口吻
短句。多用句号，少用问号和感叹号。不主动解释自己的动机。
被追问时会先沉默，再用一句更短的话把话题推开。
绝不说教，绝不抒情，绝不使用网络流行语。

## 示例台词
- 不卖。
- 你来了。
- 那个东西，我不打算解释第二遍。
- 你要是想坐，就坐。只是别碰最上面那一格。
- 雨还没停。等停了再走。

## 特质
- 对细节极端敏感，对关系反应迟钝
- 沉默是防御，不是冷漠
- 一旦决定开口，就不再收回

## 目标
- 把那本夹着旧照片的相册一直留在最上面那一格
- 弄明白自己到底在等谁

## 恐惧
- 被人发现自己其实什么都在乎
- 有一天忘了母亲的声音

## 禁则
- 绝不承认自己害怕孤独
- 绝不主动提起母亲
- 绝不为了讨好对方而改变自己的决定

## 禁用词
- 绝绝子
- 破防
- 小可爱

## 关系
- 陈默 | 常客 | 每周三来一次，从不买书，只看最后一排 | 0.05
- 母亲 | 已故 | 相册里的那张照片是她留下的唯一线索 | 1.00

## 世界书
- 相册,旧照片,母亲 :: 相册最上面那一格夹着一张泛黄的旧照片，背面有一行褪色的字。 :: 9
- 拾光,书店 :: 「拾光旧书店」开在老城区一条没有名字的巷子里，雨天屋顶会漏。 :: 5

## 背景
这家店是母亲留下的。她没有改过任何一个书架的编号。
"#;

/// `styx init` 写出的配置模板，刻意带满注释。
pub const CONFIG_TEMPLATE: &str = r#"# Styx 配置
#
# 全部字段都有默认值：删掉任何一段都能跑。默认行为是「自动探测」——
# 探测不到任何外部服务时，会退回离线 Mock 模型 + 内存记忆，保证先动起来。
#
# 生成后按需改成你自己的：`styx probe` 可以逐个检查后端是否可用。

[character]
# 角色卡路径（.md 或 .json）。留空则用内置示例角色「林夏」。
card = "examples/cards/linxia.md"
# 用户在故事里的称呼。
user_name = "陈默"
# 场景初始状态。
world = "当代都市"
location = "拾光旧书店"
time = "傍晚"
# 用户扮演的角色身份。
user_role = "常客"

[kernel]
# 每回合召回的记忆条数。
memory_recall = 8
# 联想种子数 / 每个种子的联想条数。
assoc_seeds = 3
assoc_limit = 6
# 是否把事件写回长期记忆。
write_memory = true
# 是否把高重要度事件写入共享记忆池（需要 [pool] enabled = true）。
write_pool = false
# 事件落库的重要度门槛。
memory_threshold = 0.55
# 每回合状态衰减率。
decay_rate = 0.08
# 一致性守护允许的重试次数。
guard_retries = 1
# 生成参数。
temperature = 0.85
max_tokens = 900
# 提示词预算：default / compact / large
budget = "default"

[llm]
# auto  —— 有配置用配置，没有就用 STYX_LLM_* 环境变量，再没有就探测本机服务，最后退回离线 Mock
# openai —— 只用下面的端点（缺端点会直接报错，适合生产）
# mock  —— 强制离线，不联网
backend = "auto"

# 可以写多个端点：按权重平滑轮询（SWRR），连续失败自动熔断，
# 冷却后半开试探，端点在冷却期内不会被选中。
#
# [[llm.endpoints]]
# name     = "openai"
# base_url = "https://api.openai.com/v1"
# api_key  = "sk-..."
# model    = "gpt-4o-mini"
# weight   = 3
#
# [[llm.endpoints]]
# name     = "deepseek"
# base_url = "https://api.deepseek.com"
# api_key  = "sk-..."
# model    = "deepseek-chat"
# weight   = 2
#
# 本机模型（Ollama / LM Studio / vLLM / one-api）通常不需要 api_key：
# [[llm.endpoints]]
# name     = "ollama"
# base_url = "http://127.0.0.1:11434/v1"
# model    = "qwen2.5:14b"
# weight   = 1

[memory]
# auto   —— 能连上 Nebula 就用它，否则退回内存（真 BM25，但退出即失）
# nebula —— 只用 Nebula
# memory —— 只用内存兜底
backend = "auto"
# Nebula 服务地址（与 Nebula 官方 CLI 一致）。
addr = "127.0.0.1:7878"
user = "admin"
# 密码建议留空，用环境变量 NEBULA_PASSWORD 提供，避免写进仓库。
password = ""

[assoc]
# auto    —— 能连上 MightBe 就用它，否则退回内存共现图
# mightbe —— 只用 MightBe
# graph   —— 只用内存共现图
backend = "auto"
addr = "127.0.0.1:9527"
# MightBe 里的网络名。
network = "doc_rnn"
# 联想语句模板；占位符 {net} {seed} {limit}。
# MightBe 换语法时改这一行即可，不用改代码。
query_template = "SELECT word, score FROM ASSOCIATE({net}, {seed}) LIMIT {limit}"

[pool]
# 共享记忆池：多个 agent（BIT / Styx / 任何实现 BIT Remote 的程序）共用一块黑板。
# 与 [memory] 的分工：这里是「世界状态」，[memory] 是「这个角色的私人记忆」。
enabled = false
base_url = "http://127.0.0.1:8751"
token = ""
# true 时走 BIT Remote 的 /invoke 入口而不是 REST。
use_remote = false

[tools]
# 注册内置工具：时钟、骰子、随机挑选、记忆检索、联想。
builtin = true

# 额外的 MCP 服务器（Streamable HTTP）。
# [[tools.mcp]]
# name  = "panoptes"
# url   = "http://127.0.0.1:8760/mcp"
# token = ""
"#;
