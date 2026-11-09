# acacia-netease

给 LLM 用的 MCP（Model Context Protocol）服务端：把一个 acacia 物理机器人接到本机的
Shuttler 代理上，再用 `connect / status / move_to / dig / chat` 这几个工具操作它。

链路：`acacia-netease` → Shuttler（`127.0.0.1:19132`，标准协议 2193，关闭认证）→ 网易租赁服。

## 构建

```bash
source env.sh                       # GNU toolchain + mingw（aws-lc-sys 需要 C 编译器）
export TEMP=../../.tmp              # 必须可写，否则 cc-rs 报 Cannot create temporary file
export TMP=$TEMP TMPDIR=$TEMP       # 分行写：一行写 `export A=… B=$A` 会在 A 赋值前展开 $A
mkdir -p "$TEMP"
cargo build -p acacia-netease
# 产物：target/debug/acacia-netease.exe
```

## 在 MCP 客户端里注册

```json
{
  "mcpServers": {
    "acacia-netease": {
      "command": "./target/debug/acacia-netease.exe",
      "args": []
    }
  }
}
```

传输是 stdio，一行一个 JSON-RPC 对象（不是 Content-Length 分帧）；日志走 stderr，stdout 只有协议消息。

## 工具

| 工具 | 参数 | 说明 |
|---|---|---|
| `netease_connect` | `address`(默认 127.0.0.1:19132) `name`(默认 AIBot) `timeout_s`(默认 60) | 连 Shuttler、等 spawn，返回位置/维度/血量/饥饿/游戏模式 |
| `netease_status` | – | 读共享快照，长任务执行中也能秒回；未连接返回 `{connected:false}` |
| `netease_move_to` | `x y z` `exact`(默认 true，否则只对齐 XZ) `timeout_s` `allow_dig` `allow_bridge` | 用 acacia 的寻路走过去，返回 `{arrived, elapsed_s, position, detail}` |
| `netease_dig` | `x y z` | 挖一个方块（需要物理机器人且在触及范围内），返回方块名 |
| `netease_chat` | `text` | 以玩家身份发聊天（不是命令） |
| `netease_look` | `yaw` `pitch` | 设置视角，随下一个 PlayerAuthInput 发出 |
| `netease_disconnect` | – | 断开；之后再 `connect` 会新建 bot |

## 实现要点

- `Bot` 只归一个 owner 任务所有（acacia 的 `Bot` 不能跨任务共享），命令走 `mpsc` + `oneshot` 回执；
  状态写进 `Arc<Mutex<Snapshot>>`，所以 `netease_status` 不会被 `move_to` 堵住。
- 每条 stdin 请求单独一个任务、单独一个写 stdout 的任务，响应顺序不严格要求与请求一致（JSON-RPC 允许）。
- 连接掉线时快照标记断开、挂起命令全部回 `not connected`，进程不退出；`connect` 可以重来。

## 前置条件

Shuttler 必须先登录到租赁服并在 `127.0.0.1:19132` 监听（见 Shuttler 仓库）。它一次只服务一个客户端，
客户端断开后进程会退出，需要重新启动。

## 已知限制

- `netease_look` 依赖 acacia 的 `controls()`（物理 bot 才有），非物理 bot 会返回 `ok:false`。
- 寻路受地形限制：目标在墙后/水里时返回 `no path`，不是连接问题。
- Shuttler 会把网易的数字 ID 配方整体丢掉，配方书为空（不影响行走/挖掘）。
