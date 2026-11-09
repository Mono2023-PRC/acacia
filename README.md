# Acacia

Rust 写的 Minecraft 基岩版客户端与 bot 库。`crates/acacia-agent` 是架在 bot 之上、能被 LLM 驱动的生存 agent。
基岩版 `1.26.52`（协议 `2193`），Rust `1.98` 以上。

> 预发布状态，API 会变。真机验证的账本不在仓库里（见 §8），
> 本文件不复制那张表。

---

## 1. 这是什么

一句话：`acacia-agent` 把一个真实进服的 Minecraft 玩家，包装成 LLM 可以调用的 27 个工具。

```
LLM（OpenAI 兼容：DeepSeek / 任意 base_url + model）
  ▲  对话 + tool_calls
  │
acacia-agent       工具层 27 个 · 感知快照 · 步数/墙钟/token 预算 · 护栏 · JSONL trace
  │  进程内直接调用，没有网络层
acacia-bot         寻路 A* · 挖掘 · 放置 · 合成 · 容器 · 战斗 · 拾取 · 睡觉 · 交易
  │
acacia-client + acacia-proto   基岩版 2193 协议：RakNet / 包编解码 / 登录
  │  RakNet → 127.0.0.1:19132
Shuttler（Go 协议桥，同机另一个仓库）  国际基岩 2193  ⇄  网易 898
  │  网易协议
网易租赁服
```

| 层 | 职责 | 代码 |
|---|---|---|
| agent | 把动作变成「LLM 能用的工具」，跑「思考—调用—观察」循环 | [`crates/acacia-agent`](crates/acacia-agent) |
| bot | 把报文变成「动作 + 世界状态」 | [`crates/acacia-bot`](crates/acacia-bot) |
| 协议 | 包编解码、RakNet、登录 | [`crates/acacia-client`](crates/acacia-client)、[`crates/acacia-proto`](crates/acacia-proto) |
| relay | 国际基岩 ⇄ 网易 898 的字段/命名/ID 换算 | `Shuttler/`（不在本仓库） |

另有 [`crates/acacia-netease`](crates/acacia-netease)：同一批能力以 **MCP 服务器**（stdio JSON-RPC）形态暴露，
约 14 个 `netease_*` 工具，给 Claude Desktop 这类宿主用。它和 `acacia-agent` 是两条独立入口，不要混着读。

## 2. 构建

团队机器上先加载工具链（把 rustup gnu + WinLibs mingw 放进 `PATH`，顺手把 `TEMP`/`TMP`/`TMPDIR`
指到 `.tmp`）：

```bash
source env.sh   # 本机工具链脚本；仓库里没有这一份，按你的环境自己准备
```

**临时目录必须可写**。不设 `TEMP`/`TMP`/`TMPDIR` 时，`aws-lc-sys` / `ring` / `mimalloc` 会在 `cc-rs` 里失败，
错误原文是：

```
Cannot create temporary file in %TEMP%\: Permission denied
```

（原文里那一段是本机的临时目录；换机器只会换这一段。）

外层只看到 `cc-rs ... exit status 0xc0000409`，很容易误判成编译器坏了。没有 `env.sh` 的机器上自己设三项：

```bash
export TEMP=.tmp
export TMP=$TEMP
export TMPDIR=$TEMP
mkdir -p "$TEMP"
```

不要写成一行 `export TEMP=… TMP=$TEMP`：bash 会在给 `TEMP` 赋值**之前**展开 `$TMP`，结果是空字符串。

构建与测试：

```bash
cargo build -p acacia-agent                     # agent（debug）
cargo build --release -p acacia-agent           # agent（release，真机用这个）
cargo build -p acacia-netease                   # MCP 桥
cargo test -p acacia-agent                      # agent 全量测试
cargo test -p acacia-bot                        # bot 层测试
```

cargo 有构建锁：不要同时开两个 cargo，否则会卡在 `Blocking waiting for file lock`。

## 3. 跑 agent

两种形态，都是同一个二进制。

### 3.1 `chat`：一个任务、一个进程

```bash
export ACACIA_LLM_API_KEY=sk-...            # 也可以用 DEEPSEEK_API_KEY

cargo run --release -p acacia-agent -- chat "collect 8 oak logs" \
    --address 127.0.0.1:19132 --name AIBot --max-steps 60
```

标准输出只有最终那句回答；过程在标准错误：

```
acacia-agent: stop=answer steps=23/60 tool_calls=41 tokens=... compactions=0 elapsed=...s
acacia-agent: trace=.acacia-agent/runs/20261110-003000-1a2b.jsonl
```

每场 run 都会写一份 JSONL trace（`runs_dir`），里面是每一步的请求、工具调用与结果。

退出码：`0` 有回答 / `1` 协议或 IO 错 / `2` 配置或参数错 / `3` 没连上或模型调用失败 /
`4` 被取消 / `5` 预算或墙钟用尽 / `6` 跑到一半掉线。

没有服务器时只验提示词与工具表：

```bash
cargo run --release -p acacia-agent -- chat "say hi" --no-connect --max-steps 3
```

### 3.2 `serve`：一套长连接、多场任务

给 MCP 宿主或自己的驱动器用：stdin/stdout 上一行一个 JSON-RPC 2.0 对象，日志走 stderr，启动时就开始连。

```bash
cargo run --release -p acacia-agent -- serve --address 127.0.0.1:19132 --name AIBot
```

最小交互（进程起来后调一次 `status`）：

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status","arguments":{}}}' \
  | ./target/release/acacia-agent serve --address 127.0.0.1:19132
```

方法：`initialize` / `ping` / `tools/list` / `tools/call` / `connect` /
`agent/run` / `agent/status` / `agent/cancel` / `agent/trace`。
`agent/run` 立刻返回 run id，进度用 `agent/event` 通知推回。参数与通知格式见
[`docs/agent-usage.md`](docs/agent-usage.md) 的 §4。

### 3.3 另外两个子命令

```bash
./target/release/acacia-agent tools                    # 按当前配置过滤后的工具 schema（默认 26 条，放开 chat 才是 27）
./target/release/acacia-agent runs --dir .acacia-agent/runs --last 5
```

## 4. 工具一览（27 个）

以 [`crates/acacia-agent/src/tools/spec.rs`](crates/acacia-agent/src/tools/spec.rs) 的 `SPECS` 为唯一来源；
`acacia-agent tools` 打印的就是它。**表里少一行就是漂移。**

两个数字别混：catalogue 是 **27** 个；默认 `tools.deny` 里有 `chat`，所以模型（以及 `acacia-agent tools`
的默认输出）只看到 **26** 个。要让 `chat` 也进表，把 `tools.deny` 设成 `[]` 或给它放行。

### 4.1 感知（9 个，只读，永不占用动作位）

| 工具 | 一句话 |
|---|---|
| `status` | 连接状态、坐标、生命与饥饿、正在执行的动作、上一条错误 |
| `observe` | 周围世界：自身、背包、实体、附近资源与扫描结果；`delta:true` 只给与上次快照的差异 |
| `find` | 按名字找方块，由近到远；支持 `*_log`、`minecraft:coal_*` 与别名 |
| `inventory` | 手上与背包的物品、护甲、空槽 |
| `block` | 读一格：名字、是否实心、手持工具下的破坏耗时、区块是否已加载 |
| `entities` | 附近实体按距离排序，带敌对标记 |
| `recipes` | 服务器下发的配方、原料，以及是否适配 2×2 |
| `capabilities` | 此刻能做什么：可用工具、可合成项、够得着的工作站、食物 |
| `trades` | 读村民的报价（`buy_a`/`buy_b`/`sell`），每条带 `payable` 与缺什么 |

### 4.2 动作（18 个，写世界，一次一件）

| 工具 | 一句话 |
|---|---|
| `move_to` | A* 走到一个位置，允许时自己挖或搭 |
| `dig` | 挖掉某格方块；成功失败都带自己的位置同步字段 |
| `place` | 拿手上的方块贴某个面放下；同样自带位置同步字段 |
| `climb_out` | 从坑里爬回地表：侧挖、跳、等落地，反复 |
| `attack` | 按 runtime id、种类或最近的实体开打，进入攻击距离后一直挥 |
| `container` | 开容器并列表、取、放、关；只有要求时才保持打开 |
| `craft` | 按配方书合成，或手工摆格子 |
| `smelt` | 熔炉里烧：进料、数量、可选燃料 |
| `eat` | 吃指定食物，或吃背包里最合适的那份 |
| `equip` | 穿护甲，或把物品移到主手/副手（`item` 与 `from`、`slot` 与 `to` 互为别名） |
| `look` | 把视角转向 yaw/pitch 或某个方块 |
| `chat` | 发一条聊天（纯文本，不发命令）；**默认被 `tools.deny` 关掉** |
| `hold` | 把背包里的物品拿到手上并选中 |
| `sleep` | 睡附近的床：走过去、躺下、报告是否真的睡着 |
| `pickup` | 走到掉落物旁等服务器交货；返回收到的堆、旁边剩下的、位置参考 |
| `stop` | 放下正在做的一切（走、挖、用、睡、骑），回到空闲 |
| `trade` | 与村民交易；`offer` 是 `trades` 的下标或 `"first_payable"` |
| `drop` | 从背包丢掉一组：热栏被垃圾塞满时的出口 |

完整参数（含必填、别名、默认值）见 [`docs/agent-usage.md`](docs/agent-usage.md) 的 §3；
失败原因的完整词表见 [`docs/agent-design.md`](docs/agent-design.md) 的 reason 表
（`crates/acacia-agent/src/tools/result.rs` 的 `REASONS` 是它的唯一来源）。

## 5. 配置与环境变量

优先级：命令行（`--address`、`--name`、`--allow`、`--max-steps`、`--wall-secs`）> 环境变量 >
`config.json` > 内置默认。

| 环境变量 | 作用 | 默认 | 非法值 |
|---|---|---|---|
| `ACACIA_LLM_BASE_URL` | OpenAI 兼容端点 | `https://api.deepseek.com/v1` | 原样使用，由服务端报错 |
| `ACACIA_LLM_MODEL` | 模型名 | `deepseek-chat` | 同上 |
| `ACACIA_LLM_API_KEY` | API key | 无（`DEEPSEEK_API_KEY` 兜底，再退到 `config.json`） | 没有 key 时请求直接失败 |
| `ACACIA_LLM_API_KEY_ENV` | 换个环境变量名读 key | `ACACIA_LLM_API_KEY` | 同上 |
| `ACACIA_LLM_MAX_TOKENS` | 单次回复预算 | `4096`，范围 `1..=8192` | 回退 `4096` 并在 stderr 警告一次 |
| `ACACIA_ADDRESS` | 服务器地址 | `127.0.0.1:19132` | 原样使用 |
| `ACACIA_NAME` | 玩家名 | `AIBot` | 原样使用 |
| `ACACIA_AGENT_RUNS_DIR` | trace 目录 | `.acacia-agent/runs` | 原样使用 |
| `ACACIA_PLAN_TRACE` | 每次规划往 stderr 打一行 `[plan] nodes=… cost=…` | 关 | `1/true/yes/on` 开，`0/false/no/off` 关，其它值回退为关并警告一次 |

被 `max_tokens` 截断的回复会告诉模型预算与来源（`ACACIA_LLM_MAX_TOKENS=256` /
`config.json max_tokens=256` / `the default 4096`），**不会被当成工具调用执行**。

最小 `config.json`：

```json
{
  "address": "127.0.0.1:19132",
  "name": "AIBot",
  "llm": {
    "base_url": "https://api.deepseek.com/v1",
    "model": "deepseek-chat",
    "context_window": 128000,
    "temperature": 0.2
  },
  "agent": { "max_steps": 60, "wall_secs": 600, "plan_trace": false },
  "tools": { "allow": ["perceive", "act"], "deny": ["chat"] },
  "safety": { "protected_regions": [[150000, 60, 149800, 150030, 75, 149830]] },
  "runs_dir": ".acacia-agent/runs"
}
```

```bash
cargo run --release -p acacia-agent -- chat "collect 8 oak logs" --config config.json
```

全部键（`llm.*`、`agent.*`、`tools.*`、`chat.*`、`safety.*`）见
[`docs/agent-usage.md`](docs/agent-usage.md) 的 §2。护栏要点：写操作受 `tools.allow` / `tools.deny`
白名单约束（`chat` 默认拒绝），跑满 `--max-steps` 或 `--wall-secs` 就停。

## 6. 真机铁律

任何一次真机结论，先过这三条。每条的踩坑记录在仓库外的验证笔记里（§8），不入库。

**① 二进制身份：exe 必须晚于所有源文件的 mtime。**

```bash
find crates -name '*.rs' -newer target/release/acacia-agent.exe | head
```

有输出 ⇒ 陈旧，先重编。**不要**用「exe 晚于 HEAD 提交时间」判断：提交总是在编译之后发生，这条会把刚编好的
新 exe 误判成旧的。`cargo build` 打印 `Finished` 且上面命令无输出，才算真的最新。

**② 登录预算：一次进程 = 一次进服，别硬刷。**

Shuttler 在客户端断开后自行退出。租赁服对频繁重登限流，撞上 `Reason=55`
（`netease.report.kick.hint`）、`FailedServerFull` 或 `Transport(Timeout)` 就等 1–3 分钟再试。
同一场测试里不要反复连断。

**③ 19132 独占：一个端口只能有一个监听者。**

```bash
taskkill //F //IM Shuttler.exe
```

跑之前先杀掉旧的 Shuttler，再起新的；起完等日志出现「基岩版服务器创建完成 / 请使用基岩版Minecraft连接至
127.0.0.1:19132」，才算桥就绪。

补充一条（判读用）：服务端**不回**玩家位置，`position_desync` 这类信号不能当作「没同步」的证据。

## 7. 验证用的东西不入库

跑器、证据、日志、验收账本都在仓库外（**本机验证用，不入库**），统一放在工作区的 `.memory/`：
`Shuttler.exe`、`harness.py`、`evidence/`、`acacia-docs/`（验收账本、真机判据、过程记录）。
仓库里只有代码与真正的项目文档。

## 8. 文档怎么读

仓库内的：

- 设计 → [`docs/agent-design.md`](docs/agent-design.md)：工具层、感知快照、循环、护栏、能力边界的理由
- 怎么配、怎么调工具 → [`docs/agent-usage.md`](docs/agent-usage.md)
- 上游原有文档 → `docs/` 下其余文件（`DESIGN.md`、`proto.md`、`pathfind-plan.md` 等），保持原样

仓库外的 `.memory/`（不入库）：最终验收账本、真机前置与铁律、逐场过程记录、桥侧缺口记录。

## 9. Crates

| Crate | Role |
|---|---|
| [`acacia-agent`](crates/acacia-agent) | LLM 驱动的生存 agent：工具表、感知快照、`serve` / `chat` |
| [`acacia-netease`](crates/acacia-netease) | 同一批能力的 MCP 服务器（stdio JSON-RPC，`netease_*` 工具） |
| [`acacia-bot`](crates/acacia-bot) | 高层 `Bot`：游戏状态、事件、物理、寻路、动作 |
| [`acacia-client`](crates/acacia-client) | tokio 驱动、SOCKS5 代理、`Client` 连接 API、`ping` |
| [`acacia-session`](crates/acacia-session) | 无网络会话层：批处理、压缩、加密、登录序列 |
| [`acacia-raknet`](crates/acacia-raknet) | 无网络 RakNet：握手、可靠性、分片、排序、ACK/NACK |
| [`acacia-nethernet`](crates/acacia-nethernet) | 无网络 NetherNet：信令、分帧、基于 str0m 的 WebRTC |
| [`acacia-proto`](crates/acacia-proto) | 全部报文，由 `tools/codegen` 生成，按需解码 |
| [`acacia-auth`](crates/acacia-auth) | 登录载荷；Microsoft / Xbox / PlayFab 在 `online` feature 后面 |
| [`acacia-world`](crates/acacia-world) | 方块注册表、区块解码、共享区块存储 |
| [`acacia-physics`](crates/acacia-physics) | 玩家移动模拟（移植自 bedsim） |
| [`acacia-testserver`](crates/acacia-testserver) | 回放录制的服务器会话，离线跑测试 |
| [`acacia-mitm`](crates/acacia-mitm) | 代理：录制 vanilla 抓包，或丢弃/改写/注入报文 |

依赖只向下指。无网络的 crate 从不打开 socket、不 spawn 任务，所以同一套代码同时驱动真连接、回放测试与压测。
架构与取舍见 [`docs/DESIGN.md`](docs/DESIGN.md)。

不用 agent、直接调 bot 库：`crates/acacia-bot/examples/` 有 53 个可跑的例子（`cargo run -p acacia-bot --example actions`），
裸连接看 `crates/acacia-client/examples/`。

## 10. 上游与许可

本仓库源自 [acacia-rs/acacia](https://github.com/acacia-rs/acacia)，加上网易 2193⇄898 链路所需的
`acacia-bot` / `acacia-agent` / `acacia-netease` 改动。许可 [MIT](LICENSE)，含 minecraft-data、Boar、
bedsim、gophertunnel 的 MIT 数据与代码，见 [THIRD-PARTY.md](THIRD-PARTY.md)。
贡献与漏洞报告见 [CONTRIBUTING.md](CONTRIBUTING.md) 与 [SECURITY.md](SECURITY.md)。
