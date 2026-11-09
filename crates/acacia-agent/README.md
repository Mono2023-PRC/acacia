# acacia-agent

An LLM-driven survival agent on top of [`acacia-bot`](../acacia-bot): a tool registry with JSON
Schemas, a token-bounded perception snapshot with deltas, a step loop over any OpenAI-compatible
provider, task memory with compaction, guardrails, and a per-run JSONL trace.

It is the top layer of the stack described in [`docs/agent-design.md`](../../docs/agent-design.md):

```
LLM  ⇄  acacia-agent (tools + loop + trace)  ⇄  acacia-bot  ⇄  acacia-client  ⇄  Shuttler  ⇄  网易租赁服
```

The design (including *why* each decision was made) is [`docs/agent-design.md`](../../docs/agent-design.md).

```
crates/acacia-agent/
  src/main.rs        CLI: serve | chat | tools | runs
  src/config.rs      config.json + environment, key redaction
  src/shell.rs       the task that owns Bot: commands, snapshot refresh, tool execution
  src/perceive.rs    Perception/Delta, build from GameState, render inside a token budget
  src/tools/         registry, JSON Schemas, argument reader, failure taxonomy
  src/llm/           Llm trait, OpenAI-compatible client, mock server, system prompt
  src/agent/         step machine, memory, compaction, guardrails, trace
  src/rpc.rs         stdio JSON-RPC (serve)
```

## Install and build

Requirements: a Rust toolchain that supports edition 2024, and a reachable Bedrock endpoint
(this crate links `acacia-bot` directly — no MCP bridge needed).

```bash
source env.sh                    # team machines: toolchain switches (points TMP* at a writable temp)
export TEMP=../../.tmp   # aws-lc-sys/gcc need a writable temp...
export TMP=$TEMP TMPDIR=$TEMP             # ...set these on their own line: one `export A=… B=$A`
                                          # expands $A before A exists and the build still fails
mkdir -p "$TEMP"
cd ../..                                  # repo root
cargo build -p acacia-agent --release            # → target/release/acacia-agent.exe
cargo test  -p acacia-agent                      # registry, reason table, mock loop, stdio RPC
```

> ⚠️ **The binary goes stale.** Anything built before your last change silently runs the old behaviour
> (2026-11-08: a verification run used a 02:36 `acacia-netease.exe` against a 03:03 fix). Compare
> `git log -1 --date=iso` with the exe's mtime before trusting a live run.

## The endpoint: Shuttler (NetEase rental server)

`acacia-agent` dials a normal Bedrock server. For the NetEase rental server that is the Go bridge
[`Shuttler`](../../../Shuttler), which listens on `127.0.0.1:19132` and translates to 网易 898:

```bash
cd ../Shuttler
go build -o Shuttler.exe .

./Shuttler.exe     # asks for cookie / rental code / passcode on first run
# wait for 「基岩版服务器创建完成」, then point the agent at 127.0.0.1:19132
```

`Shuttler/config.json` (written by the prompts, relative to the process cwd):

```json
{
  "cookie": "<sauth JSON, one line>",
  "rental_server_code": "YOUR_CODE",
  "rental_server_passcode": "",
  "listen_address": "127.0.0.1:19132"
}
```

Rules learned the hard way: **one Shuttler process = one login** (it exits when the client
disconnects); **port 19132 is exclusive** (`taskkill //F //IM Shuttler.exe` before starting);
the rental server kicks frequent re-logins (`Reason=55`, wait 1–3 minutes); add `SHUT_TRACE=1` for
the relay's packet traces.

## Configuration

`./config.json` (or `--config FILE`) overrides the defaults; environment wins over the file.

| env | meaning |
|---|---|
| `ACACIA_LLM_API_KEY` (or `DEEPSEEK_API_KEY`, or `llm.api_key`) | the model key — never pass it on the command line |
| `ACACIA_LLM_BASE_URL` | default `https://api.deepseek.com/v1` (any OpenAI-compatible endpoint) |
| `ACACIA_LLM_MODEL` | default `deepseek-chat` |
| `ACACIA_LLM_API_KEY_ENV` | the variable `llm.api_key_env` reads, default `ACACIA_LLM_API_KEY` |
| `ACACIA_ADDRESS` | the listener to dial, default `127.0.0.1:19132` |
| `ACACIA_NAME` | offline login name, default `AIBot` |
| `ACACIA_AGENT_RUNS_DIR` | trace directory, default `.acacia-agent/runs` |

```json
{
  "address": "127.0.0.1:19132",
  "name": "AIBot",
  "llm": {"base_url": "https://api.deepseek.com/v1", "model": "deepseek-chat",
          "max_tokens": 4096, "context_window": 128000, "timeout_s": 120, "temperature": 0.2},
  "agent": {"max_steps": 80, "wall_secs": 900, "compact_threshold": 0.8,
            "compact_keep_recent": 6, "max_total_tokens": 1500000,
            "post_action_observe": "brief"},
  "tools": {"allow": ["perceive", "act"], "deny": ["chat"]},
  "chat": {"enabled": false, "prefix": "[Acacia]", "max_chars": 200,
           "min_interval_ms": 3000, "auto_reply_cooldown_ms": 10000},
  "safety": {"confirm": false, "allow_protected": false,
             "protected_regions": [[0,0,0,0,0,0]],
             "keep_blocks": ["minecraft:bed", "minecraft:chest", "minecraft:furnace"]},
  "runs_dir": ".acacia-agent/runs"
}
```

- `tools.allow` takes the families `perceive` / `act` or single tool names; `deny` wins.
- `chat` is **denied by default**: player chat is untrusted input, enable it deliberately
  (`--allow perceive,act,chat` or `tools.allow`).
- `safety.keep_blocks` are never dug without confirmation; `protected_regions` are boxes
  `[x0,y0,z0,x1,y1,z1]` where no write happens (`safety.allow_protected` bypasses in `serve`).

## Running

```bash
export ACACIA_LLM_API_KEY=sk-...

# one task, final answer on stdout, stop=… steps=… tokens=… trace=… on stderr
acacia-agent chat "砍三根云杉原木，做一张工作台放下，再用 3x3 做一把木镐" --verbose

# tighter budgets / a smaller tool face
acacia-agent chat "把背包里的原木都做成木板" --max-steps 30 --wall-secs 300 --allow perceive,act

# the registry the model sees, as JSON (schemas included)
acacia-agent tools

# recent traces, newest first
acacia-agent runs --last 5
```

`chat` flags: `--address HOST:PORT`, `--name NAME`, `--config FILE`, `--max-steps N` (default 80; a reminder is injected when 10 steps are left),
`--wall-secs N`, `--allow a,b,c`, `--verbose`, `--no-connect` (tool registry only).
Global flags may come before or after the command.

Exit codes: `0` answered, `2` usage/config, `3` bot or LLM unreachable, `4` cancelled,
`5` a budget ran out (step, wall clock or tokens), `6` the connection dropped and the reconnect
attempt failed.

### `serve` (stdio JSON-RPC)

`acacia-agent serve` speaks line-delimited JSON-RPC 2.0 on stdin/stdout (logs on stderr), connects
at startup, and is what the verification runner drives:

| method | meaning |
|---|---|
| `initialize` | handshake, returns the server info |
| `tools/list` | the registry (name, description, JSON Schema) |
| `tools/call` | `{name, arguments}` → `{ok, data}` or `{ok:false, reason, message, hint}` |
| `connect` | connect/reconnect the bot |
| `agent/run` | run a task (asynchronous; progress via `agent/event` notifications) |
| `agent/status` | current step, tokens, elapsed, running flag |
| `agent/cancel` | cancel the run (`CancelHandle`; keys and pending actions are released) |
| `agent/trace` | the JSONL trace of the run |

## Tools

Perception (`Table::Perceive`, never writes):

| tool | arguments | what it answers |
|---|---|---|
| `status` | — | connection, position, health/food, the running action (`idle`/`walking`/`mining`/…), last error |
| `observe` | `radius` (2–24), `detail` (`brief`/`full`), `delta` | self, inventory, entities, resources, scan; **full by default**, `delta:true` for changes only |
| `find` | `block` (exact, wildcard `*_bed` / `minecraft:*_bed` / `*bed*`, or an alias `bed`/`wool`/`planks`/`log`/`door`/`carpet`; case-insensitive), `radius`, `limit` | the nearest matching blocks; the answer carries `matched_selector`, and a miss explains which selector ran and suggests a wildcard |
| `inventory` | `detail` | stacks carried, armour, free slots |
| `block` | `at` or `offset` | name, solidity, break time with the held tool, chunk loaded? |
| `entities` | `radius`, `kinds` | nearby entities by distance, with a `hostile` flag |
| `recipes` | `item`, `limit` | server-sent recipes, inputs, 2x2 fit |
| `capabilities` | `keyword` | tools held, craftable items, stations in reach, food |

Actions (`Table::Act`; `write:true` ones are rate-limited to one per 250 ms and never retried on
timeout):

| tool | arguments | what it does |
|---|---|---|
| `move_to` | `at` or `xz`, `exact`, `allow_dig`, `allow_bridge`, `timeout_s` | A* walk (may dig/bridge when allowed); climbs out of a pit it is standing in first, and returns `climbed_out` |
| `dig` | `at`, `face` | break a block, waiting for the server's echo |
| `place` | `at` **or** `auto`+`radius`, `face`, `item` | place against a face; `auto:true` picks an air cell with a solid floor within `radius`, tries the three nearest, and reports what each attempt said |
| `attack` | `target` (`nearest`, an entity kind, or a runtime id), `radius`=16, `once`=false, `times` (with `once`), `allow_dig`, `timeout_s`=45 | fight through `Bot::fight` — walks into reach and swings until it dies; `once:true` swings `times` times without approaching |
| `container` | `at`, `op` (`list`/`take`/`put`/`close`), `item`, `count`, `slot`, `keep_open` | open a container and move stacks |
| `craft` | `item`, `count`, `mode` (`book` default / `grid`), `table` | craft from the recipe book, or lay the grid out by hand |
| `smelt` | `at`, `input`, `count`, `fuel` | furnace in/out |
| `eat` | `food` | eat the named or the best carried food |
| `equip` | `item`, `slot` (`hand`/`head`/`chest`/`legs`/`feet`/`offhand`) | armour or hand/offhand |
| `look` | `yaw`, `pitch`, `at` | point the view |
| `chat` | `text`, `recipient` | one chat line (disabled unless configured) |
| `hold` | `item`, `slot` | put an item in the hand and select it |
| `sleep` | `at`, `radius`, `timeout_s` | walk to a bed, lie down, report `slept`/`woke_reason` |
| `pickup` | `id?` (entity runtime_id — preferred), `item`, `radius`, `timeout_s` | walk to a drop and wait for the hand-over; with `id` it goes to exactly that drop; returns gained stacks. Two empty runs in a row answer "this server puts broken blocks straight into the inventory" instead of sending the model to hunt drops that will never exist |
| `stop` | — | drop everything (walk, mine, use, sleep, ride) and return to idle |
| `trade` | `target` (runtime_id \| `nearest` \| a kind from `entities`, e.g. `minecraft:villager_v2`), `offer`=0, `times`=1, `radius`=16 | trade with a villager: the server does the trade, `got`/`paid` come from the inventory diff, the offer block repeats the prices |
| `trades` | `target` (runtime_id \| `nearest` \| kind), `radius`=16 | read the villager's offers (buy_a/buy_b/sell, uses/max_uses) before trading; opens the trade screen, reads it, closes it |
| `drop` | `slot?` (0–35, 0–8 hotbar) \| `item?`, `count`=1 | drop a stack from the main inventory — the way out when the hotbar is full of junk; returns `{dropped:{item,count}, slot, remaining}` |
| `climb_out` | `surface_y?` | climb back to a surface height (digs a side wall, jumps, waits to land, repeats); returns `surface_y`/`from_y`/`to_y`/`steps`/`landed`; `already_out` when the bot is not in a pit |

`craft` now answers `needs_table` instead of silently running a 3×3 recipe in the 2×2 grid: give it
`table:[x,y,z]`, or check `recipes {item}` for `fits_2x2` first.

`attack` reports `{target:{runtime_id,kind,distance}, hits, target_health_before, target_health_after, killed, approached, elapsed_s}`.
`Bot::fight` does not expose its swing count on a kill, so `hits` is `null` there — health before/after is the signal;
on a timeout the bot's own `(N swings, M retreats)` sentence is folded into the message. A target out of the arm's
reach is approached, not refused; `out_of_reach` is what `once:true` answers.

### Result shape and failure reasons

```json
{"ok": true,  "data": {...}}
{"ok": false, "reason": "no_block", "message": "…", "hint": "…"}
```

`reason` shares its vocabulary with `acacia-netease`'s `fail_reason`: `no_block`, `no_item`,
`not_visible`, `out_of_reach`, `unreachable_target` (a fight the bot cannot path to, e.g. a target a
pit's depth below), `position_desync` (the client's body is over a block from the server's position, so
clicks would be dropped), `no_effect_on_target` (five swings the server counted no hurt for),
`missing_ingredient`, `no_recipe`, `rejected`, `timeout`, `no_space`, `no_physics`, `game_mode`,
`disconnected`, `cancelled`, plus `busy`, `invalid_argument`, `unknown_tool` from the registry.

`rejected` carries the three dig/place verdicts in its message (`server_rejected:` /
`no_local_change:` / `no_effect:`, each followed by the last block seen and whether a drop was
near) — see the live-run notes (kept outside the repo, in `.memory/`) §9.7. Library-side these are covered by
`crates/acacia-bot/tests/dig_place_confirm.rs` (a 3 s late echo must still succeed); on the rental
server only the happy path has been hit so far, because every failure there stopped earlier
(`no_block`, `refused`).

## Example dialogue

A run on the rental server (coordinates from a real session; tool calls abbreviated):

```
user: 砍三根云杉原木，做一张工作台放下，再用 3x3 做一把木镐

→ status
  {"ok":true,"data":{"position":[150005,69,149851],"health":20,"food":20,"activity":"idle"}}

→ find {"block":"*_log","radius":48}
  {"ok":true,"data":{"blocks":[{"name":"minecraft:spruce_log","at":[150012,70,149848],"distance":7.6}]}}

→ move_to {"at":[150010,69,149848]}
  {"ok":true,"data":{"arrived":true,"elapsed_s":3.4}}

→ dig {"at":[150012,70,149848],"face":"north"}
  {"ok":true,"data":{"block":"minecraft:spruce_log"}}
→ pickup {"radius":4}
  {"ok":true,"data":{"gained":[{"item":"minecraft:spruce_log","count":1}]}}

→ craft {"item":"minecraft:spruce_planks","count":12,"mode":"book"}
  {"ok":true,"data":{"crafted":12,"mode":"book"}}

→ craft {"item":"minecraft:crafting_table","count":1,"mode":"book"}
  {"ok":true,"data":{"crafted":1,"mode":"book"}}

→ place {"at":[150006,69,149851],"face":"up"}
  {"ok":false,"reason":"rejected","message":"… the cell became minecraft:short_grass …"}
→ place {"at":[150007,69,149851],"face":"up"}
  {"ok":true,"data":{"at":[150007,69,149851],"block":"minecraft:crafting_table"}}

→ craft {"item":"minecraft:wooden_pickaxe","count":1,"mode":"book","table":[150007,69,149851]}
  {"ok":true,"data":{"crafted":1,"mode":"book"}}

assistant: 三根原木、一张工作台和一把木镐都做好了；工作台放在 [150007,69,149851]。
```

Notes for prompting: the model is told to **look before it acts** (`status`/`observe`/`find`), and
`stop` is called once at the start of every run so a previous crash cannot leave keys held.

## Context budget

A snapshot is **current truth, not a log entry**: the per-step state message replaces the previous one,
and an older `observe`/`status`/`inventory` body is stubbed out (`{"superseded": …}`) as soon as a newer
one is sent. Lists are cut to what a decision needs — 10 inventory stacks, 8 entities, 8 resources —
with `inventory_more` / `entities_more` / `resources_more` counting the rest. `tests/loop_mock.rs`
prices the policy with a 10-step replay of the same script:

| policy | prompt characters (10 steps, same script) |
|---|---|
| append every snapshot (`legacy_history = true`) | 185,377 |
| replace + supersede + capped lists (default) | 88,941 (**−52%**) |

The real run that motivated this (t20: 36 steps, 206 s) spent 587,123 prompt tokens and reached
22,933 tokens in its last request — one small task away from the `0.8 × 128k` compaction threshold.
With the policy above the growth per step is the assistant message plus the tool result, not another
full world.

## Hints: what the model can do next

Every failure carries a `hint` that is one line, imperative, and free of paths and stack traces. Most
are static per reason; two are built from what the bot can see at that moment:

- `not_visible` names the block between the eye and the target
  (`被 minecraft:oak_log（[149998,74,149857]）挡住：换个面点目标，或先挖开它`), by sampling the eye →
  target segment; with no coordinate in the message it says so explicitly.
- `refused` / `no_space` look for a same-level neighbour that is air with something solid under it and
  offer it (`换个位置：[…] 是同层空位、下方有支撑`), falling back to "dig the blocking block first".

`attack` also judges a timeout by an external fact: `Bot::fight` can only end itself on a death it sees,
and on the rental server the entity often just stops being tracked — that is reported as
`killed:true` with `hits:<n>` and a note, not as `rejected`.

## What a good tool description buys (field notes)

From the first real-model run (the first real-model run notes, kept outside the repo in `.memory/`, §4), three corrections the model made **from the
observation alone** — they are the reference for how much a tool description is worth:

1. `move_to` refused → the model switched to `{xz:[…]}` and succeeded: the failure said *what* was
   refused, not *why*, and the model found the other argument shape itself.
2. `dig` `not_visible` → it dug away the leaves in front of the log, re-aimed, and at step 21 named the
   occluder itself ("y=74 的原木挡住了 73 那格"). Five `block` probes were spent doing what the hint now
   says in one line.
3. `pickup` empty twice → it checked the inventory and found the log already there ("服务器直接给了
   物品，所以地上没有可捡的掉落物"). That server trait is now in the tool result and in the system prompt.

## Traces

Each run appends one JSON object per record to `<runs_dir>/<timestamp>.jsonl`: the request, every
tool call and its result, the budgets left, compaction events, and the final answer. The API key is
redacted; `runs` lists them newest first.

## Troubleshooting

| symptom | fix |
|---|---|
| `aws-lc-sys: Cannot create temporary file` | export `TEMP`/`TMP`/`TMPDIR` to a writable dir (see build section) |
| `reason: "busy"` | an action is still running: call `stop` (every run does it first) |
| everything "succeeds" but the world does not change | the binary is stale — rebuild (see warning above) |
| `reason: "timeout"` on craft/container | NetEase sometimes answers after seconds; the bot waits 12 s and re-reads the inventory before failing |
| `chat` refused | `chat` is denied by default; allow it explicitly |
| run keeps ending at step 80 / 900 s / 1.5 M tokens | raise `agent.max_steps` / `agent.wall_secs` / `agent.max_total_tokens`, or split the task |
