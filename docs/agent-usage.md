# acacia-agent 快速上手

`acacia-agent` 把 `acacia-bot` 的能力包成 27 个工具，交给一个 OpenAI 兼容的模型去调用：模型看到
`status` / `observe` / `find` 这类感知结果，决定调 `move_to` / `dig` / `craft` 这类动作，再根据结果
继续或换招。本文件讲怎么把它跑起来、有哪些配置、每个工具叫什么。**设计与失败分类**见
[`agent-design.md`](agent-design.md)，**哪些能力在真机上验证过**只看仓库外的验收账本
（工作区 `.memory/`，不入库）。

## 1. 前置

| 项 | 说明 |
|---|---|
| 游戏版本 | Minecraft Bedrock `1.26.51`，协议 `2193`（与仓库当前版本一致） |
| 服务器地址 | `--address HOST:PORT`。默认 `127.0.0.1:19132`，通常指向 Shuttler 桥（2193 ↔ 网易 898） |
| 登录方式 | **离线登录**：agent 的连接路径固定走 `Client::builder(addr).offline(name)`，因为桥侧关闭了认证。在线（Xbox Live）登录在 `acacia-bot` / `docs/auth.md` 里可用，但本 CLI 还没有开关 |
| 名称 | `--name NAME`，默认 `AIBot` |
| 模型 | 任何 OpenAI 兼容的 `/chat/completions`：默认 `https://api.deepseek.com/v1` + `deepseek-chat` |

## 2. 配置

优先级从高到低：**环境变量 → `--config FILE` → `./config.json` → 内置默认**。文件缺省不报错。
字段全部有默认值，所以一个空文件也是合法配置。

### 2.1 环境变量（推荐：密钥不落盘）

| 变量 | 作用 | 默认 |
|---|---|---|
| `ACACIA_LLM_API_KEY` | API 密钥（默认读的就是这个名字） | — |
| `DEEPSEEK_API_KEY` | 上面那个没设时读它 | — |
| `ACACIA_LLM_API_KEY_ENV` | 换一个变量名去读密钥 | `ACACIA_LLM_API_KEY` |
| `ACACIA_LLM_BASE_URL` | 模型 endpoint | `https://api.deepseek.com/v1` |
| `ACACIA_LLM_MODEL` | 模型名 | `deepseek-chat` |
| `ACACIA_LLM_MAX_TOKENS` | 单次回复的 token 上限（范围 `1..=8192`；按 `max_tokens` 预扣费的代理上，直接决定一次请求冻结多少钱） | `4096` |
| `ACACIA_PLAN_TRACE` | 每次规划操作往标准错误打一行 `[plan] nodes=320 cost=12.50`（示例，真实值随规划变化；`cost` 恒两位小数），只为排查「这一步到底有没有真的规划过」 | 关闭 |
| `ACACIA_ADDRESS` / `ACACIA_NAME` | 服务器地址 / 玩家名 | `127.0.0.1:19132` / `AIBot` |
| `ACACIA_AGENT_RUNS_DIR` | 每场 trace 写在哪 | `.acacia-agent/runs` |

`ACACIA_LLM_MAX_TOKENS` 的值不合法（`0`、非数字、超过 `8192`、空串、非 UTF-8）时不会启动失败：
回退到 `4096` 并在标准错误打一行说明，例如

```
ACACIA_LLM_MAX_TOKENS="abc" ignored: "abc" is not a whole number; using 4096
```

预算调小后要留意一种情况：回复可能在**工具调用写到一半**时被截断（`finish_reason=length`）。
这种回复不会被执行，也不算一次工具调用失败——模型会收到一条说明「回复被 max_tokens 截断（本次
max_tokens=256，来自 ACACIA_LLM_MAX_TOKENS=256）」的提示，trace 里对应一条 `kind` 为 `truncated`
的 `nudge` 记录（带 `max_tokens` 与 `max_tokens_origin`）。如果你在日志里看到模型「反复调错工具」，
先看有没有这种 `truncated` 记录：那是预算太小，不是模型的问题。

`llm.api_key` 也可以直接写进 `config.json`，但那份文件会被 `llm.api_key` 原样保存——除非你要在
`serve` 后面挂一个只读密钥的 manager，否则用环境变量。

### 2.1.1 看寻路有没有真的发生：`ACACIA_PLAN_TRACE`

默认关闭。设 `ACACIA_PLAN_TRACE=1`（也接受 `true` / `yes` / `on`；`0` / `false` / `no` / `off` 关闭）后，
每次 `move_to` / `dig` / `place` 收尾都会往标准错误打**恰好一行**：

```
[plan] nodes=320 cost=12.50
```

一行 = **一次工具操作**，不是一次搜索：同一次调用里若重规划过多次（heal 改道、place 换候选格），
只报最后那次的数字。`cost` 恒为两位小数；`cost=0.00` 且 `nodes` 很大时不要读成「便宜」或「到达」——
那是「代价没写进这条出口」，当未知看。

值写错（`ture`、`2`、空串、非 UTF-8）不会启动失败：保持关闭，并在标准错误打一行含变量名的警告。
关闭时仍然会把上一轮的规划统计取走（drain），所以中途打开不会读到旧数字。

### 2.2 用户真正会改的字段

| 字段 | 一句话 | 默认 |
|---|---|---|
| `address` / `name` | 连哪儿、叫什么 | `127.0.0.1:19132` / `AIBot` |
| `llm.base_url` / `llm.model` | 模型端点与模型名 | 见上 |
| `llm.context_window` | 模型上下文长度；压缩阈值按它的比例算 | `128000` |
| `llm.max_tokens` / `llm.temperature` / `llm.timeout_s` | 单次回复上限（可用 `ACACIA_LLM_MAX_TOKENS` 覆盖）、采样温度、请求超时 | `4096` / `0.2` / `120` |
| `agent.max_steps` | 一场任务最多几步（工具调用轮），硬上限 200 | `80` |
| `agent.wall_secs` | 一场任务的墙钟上限（秒） | `900` |
| `agent.compact_threshold` | 用掉上下文多少比例就压缩历史（0–1） | `0.8` |
| `agent.compact_keep_recent` | 压缩时原样保留最近几条消息 | `6` |
| `agent.max_total_tokens` | 全场累计 token 上限，超了就停 | `1500000` |
| `agent.post_action_observe` | 每个动作后自动附带的观察：`brief` 或 `full` | `brief` |
| `agent.reconnects` | 掉线后自动重连次数（0 关掉） | `1` |
| `tools.allow` / `tools.deny` | 工具白名单 / 黑名单，元素是工具名或族名 `perceive` / `act` | `["perceive","act"]` / `["chat"]` |
| `chat.enabled` | 是否允许模型主动发聊天 | `false` |
| `chat.prefix` / `chat.max_chars` / `chat.min_interval_ms` | 聊天前缀、单条字数、两条之间最小间隔 | `[Acacia]` / `200` / `3000` |
| `safety.confirm` | 交互模式下，动到受保护区域或保留方块前问 `y/N` | `false` |
| `safety.allow_protected` | `serve` 模式没人可问时，是否允许那些写入 | `false` |
| `safety.protected_regions` | 禁止挖/放的盒子 `[x0,y0,z0,x1,y1,z1]` 列表 | `[]` |
| `safety.keep_blocks` | 不经确认绝不动方块名 | `[]` |
| `runs_dir` | trace 目录 | `.acacia-agent/runs` |

`chat` 工具默认在 `tools.deny` 里：模型不能自己开口说话。要放开就显式给白名单，例如
`--allow perceive,act,chat`（命令行给了 `--allow` 会整体替换配置里的白名单，并清空 `deny`）。

### 2.3 一份能跑的最小 `config.json`

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
  "agent": { "max_steps": 60, "wall_secs": 600, "post_action_observe": "brief" },
  "tools": { "allow": ["perceive", "act"], "deny": ["chat"] },
  "safety": { "protected_regions": [[150000, 60, 149800, 150030, 75, 149830]] },
  "runs_dir": ".acacia-agent/runs"
}
```

## 3. 工具目录（27 个）

下面是 `crates/acacia-agent/src/tools/spec.rs` 的机械抄录：名字、一句话用途、**必填**参数
（`—` 表示都可不填）。完整的 JSON Schema 用 `acacia-agent tools` 打印。

### 3.1 感知（9 个，只读，永不占用动作位）

| 工具 | 用途 | 必填 |
|---|---|---|
| `status` | 连接状态、坐标、生命/饥饿、正在执行的动作、上一条错误 | — |
| `observe` | 周围世界：自身、背包、实体、附近资源与扫描结果；`delta:true` 只给与上次快照的差异 | — |
| `find` | 按名字找方块，由近到远；支持 `*_log`、`minecraft:coal_*`、别名（`bed`/`wool`/`planks`/`log`） | `block` |
| `inventory` | 手上与背包的物品、护甲、空槽 | — |
| `block` | 读一格方块：名字、是否实心、手持工具下的破坏耗时、区块是否已加载 | —（给 `at` 或 `offset`） |
| `entities` | 附近实体按距离排序，带敌对标记 | —（`radius`、`kinds` 可选） |
| `recipes` | 服务器下发的配方、输入、能否放进 2×2 | —（`item` 过滤） |
| `capabilities` | 此刻能做什么：工具、可合成物、够得着的工作站、食物 | —（`keyword` 过滤） |
| `trades` | 读村民报价（`buy_a`/`buy_b`/`sell`、`uses`/`max_uses`），每条带 `payable` 与 `missing` | `target` |

### 3.2 动作（18 个，写操作用白名单把关）

| 工具 | 用途 | 必填 |
|---|---|---|
| `move_to` | A* 走到某个位置，可按权限挖路或架桥 | `at` 或 `xz`（二选一） |
| `dig` | 挖掉指定位置的方块 | `at` |
| `place` | 对着某个面放手上的方块；`auto:true` 自己选点 | `at` 或 `auto` |
| `climb_out` | 从坑里爬回某个高度（挖侧壁、跳、等落地、重复） | —（`surface_y` 可选） |
| `attack` | 按 runtime id / 种类 / `nearest` 打，走近再挥直到目标死亡 | `target` |
| `container` | 开容器并 `list` / `take` / `put` / `close` | `op`（还没开窗时加 `at`） |
| `craft` | 合成：默认用配方书，`mode:"grid"` 手动摆料 | `item` |
| `smelt` | 熔炉里烧炼 | `at`、`input` |
| `eat` | 吃东西回饥饿（指定的，或背包里最好的） | —（`food` 可选） |
| `equip` | 穿护甲，或把物品移到手/副手；`from` 与 `item` 等价 | `item` |
| `look` | 视角转向 yaw/pitch，或对着某个方块 | `yaw` 或 `at`（`pitch` 可选） |
| `chat` | 发一条聊天（纯文本，不能带斜杠命令）——默认被禁用 | `text` |
| `hold` | 把背包里的物品拿到手上并选中 | `item` |
| `sleep` | 去附近（或指定位置）的床睡觉 | —（`at`、`radius` 可选） |
| `pickup` | 走到掉落物旁等服务器交货；**带 `entities` 给的 `id` 最可靠** | —（建议给 `id`） |
| `stop` | 停掉一切正在做的动作，回到空闲 | — |
| `trade` | 与村民成交；`offer` 是 `trades` 里的下标或 `first_payable` | `target` |
| `drop` | 从主背包丢出一叠（热键栏塞满时的出口） | `slot` 或 `item` |

失败时工具回一个结构化 `reason`（如 `no_block` / `not_visible` / `out_of_reach` /
`missing_ingredient` / `rejected` / `timeout` / `position_desync`），外加一句 `hint` 告诉模型下一步
该干什么。完整清单与逐项含义见 [`agent-design.md`](agent-design.md) 的 reason 表——这里不重复份数，
免得两处数字各自漂移（那张表本身有测试盯着）。

`equip` 的答案分两半，都来自服务器回报而不是从 message 文本里切出来的：成功回 `action_used`
（`swap` / `clicks` / `use` / `none`，即最终搬动物品的那一种请求形状），失败则在 `details` 里回
`status_translated`（协议自己的名字，如 `55 CannotPlaceItem`）与 `attempts`（逐级试过什么）。
没有码时 `status_translated` 是 `null`（明确表示「这次不知道」），`attempts` 则在从未被试时整个省略。

## 4. 两种跑法

先构建：

```bash
cargo build --release -p acacia-agent
```

### 4.1 `chat`：一条任务、一个进程

```bash
export ACACIA_LLM_API_KEY=sk-...

./target/release/acacia-agent chat "砍 8 个橡木原木，做成木板" \
    --address 127.0.0.1:19132 --name AIBot --max-steps 60 --wall-secs 600
```

- 标准输出只有最后那句回答；过程打在标准错误：
  `acacia-agent: stop=answer steps=23/60 tool_calls=41 tokens=… compactions=0 elapsed=…`
  以及 `acacia-agent: trace=.acacia-agent/runs/<ts>-<id>.jsonl`。
- 退出码：`0` 有回答、`1` 协议/IO 出错、`2` 配置或参数错、`3` 没连上 / 模型调用失败、`4` 被取消、
  `5` 预算或墙钟用尽、`6` 跑到一半掉线。
- `--no-connect` 只跑模型不连服务器（用来检查提示词与工具表，工具会回 `disconnected`）。
- `--verbose` 在开头多打一行运行信息（模型名、工具数、trace 路径）。

### 4.2 `serve`：一套长连接、多场任务

```bash
./target/release/acacia-agent serve --address 127.0.0.1:19132 --name AIBot
```

标准输入输出是**一行一个 JSON 对象**的 JSON-RPC 2.0（日志走标准错误），启动时就开始连接。方法：

| 方法 | 作用 |
|---|---|
| `initialize` / `ping` | 握手与存活检查 |
| `tools/list` | 27 个工具的完整 schema |
| `tools/call` | 直接调一个工具：`{"name":"status","arguments":{}}` |
| `connect` | 重连：`{"address":…,"name":…,"timeout_s":60}` |
| `agent/run` | 起一场任务：`{"task":"…","limits":{"max_steps":60,"wall_secs":600}}`，立刻返回 run id，进度用 `agent/event` 通知推回 |
| `agent/status` / `agent/cancel` / `agent/trace` | 查进度、取消、取 trace 路径 |

一次最小交互：

```bash
{ echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status","arguments":{}}}'; sleep 2; } \
  | ./target/release/acacia-agent serve --address 127.0.0.1:19132
```

## 5. 一场真实任务长什么样

模型第一眼只能拿到 `status`（一行摘要：坐标、生命/饥饿、正在做什么）和一个工具表，然后自己决定：

1. **`status`** —— 我在哪、血量饥饿多少、有没有卡在某个动作里。
2. **`observe`**（需要细节时；`delta:true` 只看变化）—— 脚下与身旁的方块、几格内实体、背包摘要、
   扫描到的木头/矿石；这一份是 token 有界的，不是整个世界。
3. **动作** —— 例如 `find {block:"oak_log", radius:24}` 拿到坐标 → `move_to {at:[…], allow_dig:true}`
   → `dig {at:[…]}`。`dig` 与 `place` 的成功和失败结果里都带着自己的位置同步字段
   （`position_desync_blocks`、`server_reference`），`pickup` 带 `collected` / `position`，
   所以模型不必为了确认「刚才那下生效了吗」再补一次 `status`。
4. **复核** —— `inventory` 看原木数量、`find` 看目标方块还在不在、`block {at:[…]}` 回读刚放下的方块是不是
   真的实心。工具失败时给的是 `reason` + `hint`（例如「先 dig [x,y,z] 把挡路的那格挖开，再 pickup {id}」），
   而不是一句含糊的报错。

好的任务描述是「结果 + 数量 + 约束」：`砍 8 个原木做成木板，别走太远`，而不是「帮我玩」。预算用尽或
墙钟到点时，模型必须给出一句当前状态与没做完的部分。

## 6. 已知未验证 / 限制

**不要在别处另做一份清单。** 权威清单是仓库外验收账本（`.memory/`）的
**§四「尚未闭合」**：`eat` 与 `equip` 实现有但真机无样本、`sleep` 跨夜、`place` 慢回显、
`move_to` 长距（>100 格）分段问题、`allow_dig:true` 上「高一格台阶」的正对照缺包级证据。

同一份文件的 **§一** 是逐能力的真机状态（✅ / 🟡），**§五** 是环境约束（二进制 mtime、端口独占、
`Reason=55` 等），**§六** 解释了为什么 `position_desync` 这类信号不能当作「没同步」的证据。

另外两条属于 agent 自身、与服务器无关的限制：

- 连接路径只管离线登录（见 §1）；在线登录要用 `acacia-bot` 的 API 自己搭。
- `tools/call` 与 `chat` 走同一个 shell，写操作一条一条来；需要并行的场合请开多个进程。
