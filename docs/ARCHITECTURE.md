# 架构说明（基线 8539f0a）

本文描述 gemini-qq-bot 在提交 `8539f0a`（「lane halt 接上运维面」）时的实际结构与
行为。所有结论都以该提交的代码为准，行号与常量值可直接对照源码；文档不描述计划、
只描述现状。

代码规模：`src/` 下 71 个 `.rs` 文件共 45 756 行（含测试），`tests/p4_replay.rs`
另有 401 行。11 个 migration、16 张表。

---

## 一、系统总览

### 1.1 进程与外部依赖

一个进程（`src/main.rs`）里跑着五条独立的异步循环，全部由 `App::build` +
`App::serve` 组装（`src/app/mod.rs:76-340` 装配，`src/app/mod.rs:381-620` 起循环）：

```text
QQ 客户端
  └─ snowluma 容器（OneBot v11 实现）
       └─ ws://snowluma:3001  ← bot 主动连出（forward WS）
            └─ gemini-qq-bot 进程
                 ├─ agy 子进程       每群一个常驻 PTY（唯一执行路径）
                 ├─ mihomo 子进程    仅美国出口组，Agy 可达性的前置条件
                 ├─ Aux 网关   hy3：节奏判定 / 图像打标 / 折叠 / 记忆小脑
                 └─ bot.sqlite3      全部状态（16 张表）
```

`main()` 只有三种模式：

| 参数 | 行为 | 源码 |
|---|---|---|
| （无） | 正常服务 | `src/main.rs:107-136` |
| `--health-probe` | 只对 `ADMIN_BIND` 做一次 TCP 连接（不认证、不读 token） | `src/main.rs:59-84` |
| `--mcp-memory` | 只为 MCP stdio 起一个只读记忆库服务 | `src/main.rs:31-56` |

### 1.2 模块职责

| 模块 | 行数 | 职责 |
|---|---:|---|
| `src/onebot/` | 1 684 | WS 连接与重连、事件解析、normalize（不可信输入的安全边界）、action 构造 |
| `src/pipeline/` | 1 478 | 攒批规则（batcher）、去重（dedupe）、每群串行且全局并发的调度（scheduler）、出站落库（outbox） |
| `src/app/` | 15 508 | 入站与 flush tick（intake）、一轮的全部步骤（turn）、节奏闸门（preflight）、额度分档（pacing）、降温器（damper）、插话策略（insert）、常驻会话管理（resident）、表情闸门（sticker_gate）、探针（probes） |
| `src/agent/` | 8 725 | agy 适配（argv/进程边界）、输出契约校验（contract）、抢救梯（salvage）、分句与凭据过滤（segments）、步流解析（step_stream）、交付屏障（barrier）、常驻会话监督（resident）、提示词（prompt） |
| `src/memory/` | 5 438 | 召回加权（retrieval）、fold 与提取（chunker/extract）、读侧消化（digest）、二次压缩遗忘（consolidate）、摘要压缩（compaction）、表情库（stickers）、交接纸条（handover） |
| `src/store/` | 4 234 | 全部 SQL 与 migration；枚举、常量、行结构体 |
| `src/admin/` | 1 643 | bearer 认证（auth）与 `/internal/v1/*` JSON API（routes） |
| `src/mcp/` | 1 235 | MCP stdio：`memory_search`、`sticker_search`、`sticker_keep`，群范围靠 roots + `scope.json` |
| `src/media/` | 1 270 | 下载（含 SSRF 防线）、缩放、JPEG 压缩、每批沙箱目录 |
| `src/integrations/` | 1 887 | Aux 网关客户端、额度 snapshot 抓取 |
| `src/process/` | 504 | mihomo 子进程监督与健康探测 |
| `src/state/` | 741 | 会话指针（conversation）、记忆写入（memory）、情绪与话题（mood） |
| `src/config.rs` | 631 | 环境变量 + `bot.toml` 解析与启动期校验 |
| `src/health.rs` | 388 | 自检快照与 blockers |
| `src/error.rs` | 226 | `BotError` / `ErrorClass` / `BotResult` |

`src/lib.rs` 只做 `pub mod`，`src/main.rs`、`src/error.rs`、`src/config.rs`、
`src/health.rs` 直接挂在 crate 根。各子目录的 `mod.rs`（`src/app/mod.rs` 除外）
都只是 `pub mod` + `pub use` 的胶水，没有逻辑。

---

## 二、一轮消息的完整生命周期

### 2.1 入站与入库

`src/app/intake.rs:200-330`，`Intake::handle`：

1. `serde_json::from_value::<RawEvent>` 失败 → `Skipped("missing_field")`，不碰库。
2. `rewrite_bot_poke`（`src/app/intake.rs:1414`）把「戳一戳机器人」的 `notice/notify`
   帧改写成等价 message 事件，让它走同一条路。
3. `post_type != "message"` → `not_message`；`message_type != "group"` →
   `private_message`。
4. `message_id` 缺失或空 → `missing_field`。
5. 进程内去重 `Dedupe::observe(dedupe_key(bot, message_id))`，`Duplicate`
   直接返回。键是 `format!("{bot_id}:{message_id}")`（`src/pipeline/dedupe.rs:120`），
   容量默认 `DEFAULT_CAPACITY = 20_000` 条，超了按插入顺序淘汰最旧。
6. **持久去重** `store.mark_seen`：重启后不能重放同一条消息。返回 `false` 即
   `Duplicate`。
7. `ensure_group` 建群（首次出现登记为**关闭**状态并写 `group.discovered` 审计），
   然后 `get_group`、`ensure_member` 分配群内别名（`u1`、`u2` …）。
8. `normalize::normalize_event` 产出 `Normalized::{Message,Command,Skipped}`。
9. 命令走 `run_command`（同步执行，永不进 prompt）；普通消息先检查
   `group.enabled && !group.paused`，否则 `group_inactive`，然后 `store_message`：
   同时写 `group_messages`（长期原始日志，永不删）和 `pending_events`（待攒批）。

**不变量**：关闭或暂停的群**什么都不积**——flush tick 里对此类群直接
`drop_pending`（`src/app/intake.rs:495-499`）。

### 2.2 flush tick 与攒批

`Intake::flush_tick`（`src/app/intake.rs:467`）每 `TICK_INTERVAL`
= 500 ms 跑一次（`src/app/mod.rs:54`）。每群的判定顺序是刻意排过的：

1. **额度档位**：先 `pacing.observe(now, live_groups)` 观察一次全局档位（一个账号
   服务所有群，所以只观察一次），`PacingReport` 通过 `cap_for` 下发给每个群。
2. **年龄护栏** `apply_age_guardrail`：超过 `STALE_ROW_SECONDS = 600` 的行
   在花任何预算之前就消费掉（否则回复会落在几分钟后，即「陈旧批次错位」）。
3. **特权地板** `pacing::privilege_floor`：T2 只放行「有 @ 或主人发言」的批次，
   T3 只放行主人。被筛掉的行直接删并审计 `pacing.drop.g<群>.n<数>.tier<t>`。
4. **小时预算熔断** `budget_gate`：窗口 `BUDGET_WINDOW_SECONDS = 3_600`；只为
   @ 批次保留 `BUDGET_PRIORITY_RESERVE = 15` 次，即环境批次最多花
   `budget − 15`。熔断已断时仍在 `drain_pool_if_flooded` 里排空池子。
5. **两层取严** `max_claims = fuse_claims.min(cap − calls_in_window)`，额度分档只能
   收紧不能放宽。`max_claims == 0` 同样走泄洪。
6. **对话形状** `observe_shape`：把本批每行解析成 `Observation` 交给
   `Damper::observe`，得到 `Verdict` 与的事件列表。同一批行在每个 tick 会被反复看到，
   所以降温器按 row id 去重（`rows_are_counted_once_however_often_the_tick_sees_them`）。
   「被点名」有三个来源，全部是结构信号：`@`、回复了机器人写过的东西（
   `quote_targets_bot`，每个被引 id 每 tick 只查一次）、主人身份。
7. **放行判定** `(ready, ceiling)`：
   - 无闸门（Aux 未配置）时用 `should_flush` 的固定窗口：
     静默 5 s（`quiet_window_seconds`）/ 硬年龄 30 s（`max_batch_age_seconds`）/
     批满（50 条、8 000 字、4 图，`BatchLimits` 默认值见
     `src/pipeline/batcher.rs:26-34`）/ 特权立即。
   - 有闸门时先卡 `MIN_BURST_SECONDS = 2`（两秒内连发的第二条必须能进同一批），
     再查**硬天花板**——年龄、条数、字数、图数四个维度任一越界就 `ceiling = true`，
     永不咨询模型；只有都不越界才让模型评分（`burst_may_hand_over`）。
   - `FlushTrigger::{Idle,Immediate,Quiet,MaxAge,Full}`，`as_str` 分别为
     `idle/immediate/quiet/max_age/full`（`src/pipeline/batcher.rs:59-80`）。
8. **降温 hold**：`shape.verdict.hold(primary, addressed, waited, pending)`。
   硬 hold（L3、per-sender cap）扛得住天花板，软 hold（quiet、thin）不扛——
   天花板的职责就是保证没有批次能被永远饿死。hold 期间 `record_damper_wait`
   以 `DAMPER_WAIT_AUDIT_INTERVAL_SECONDS = 60` 节流，避免每 500 ms 一行。
9. 放行后 `split_batches` 按 `BatchLimits` 切成顺序批次；若 `max_claims` 少于批数，
   **只服务最新的几段**（`claim_from = chunks.len() - max_claims`），更老的段
   被消费并审计——只剩一次调用时应该回答最新的话题。

### 2.3 调度

`src/pipeline/scheduler.rs` 两条硬约束：

- **每群一个 worker**：同一个群的两个批次永不并发，Agy 会话严格按到达顺序推进。
- **全局并发上限** `agent_concurrency`（默认 2）。

lane 被 halt 时它什么都不接（`enqueue` 返回 `false`）。flush 循环对此每个 halt
episode 只留一行审计 `scheduler.lane.halted.g<群>`（`src/app/mod.rs:470-495`），
因为 2026-10-28 出现过「群排了一小时队而没有任何可见原因」。

### 2.4 一轮的执行：单一常驻路径

`TurnExecutor::run_batch`（`src/app/turn.rs:1856`）是唯一的入口，顺序：

1. 群可能已被停用 → `SkippedInactive`。
2. **再次检查熔断** `hourly_budget_remaining`：claim 与真正开始是不同时刻，
   一个批次可能卡在调度许可上很久（实测一个窗口内 64 次开始 vs 预算 60），
   所以这里拒绝就 `release_events` + `scheduler.budget.exec_denied.g<群>.age<秒>`，
   返回 `BudgetDeferred`。
3. **代理闸门** `mihomo.ready()` 不通过 → `DeferredProxy`，审计
   `agent.waiting_for_us_proxy`。
4. **重生退避闸门**：在退避期 → 审计
   `resident.turn.deferred_backoff.g<群>.until<秒>` 并 `release_events`，返回
   `DeferredProxy`，批次留 pending 等下一次 tick。
5. 否则 → `run_resident_batch`。

**常驻是唯一的执行路径**（2026-10-28 决策点 D 的终局形态）：`agy --print`
那条一次一批的子进程通道已整路删除，`resident_enabled` 这个群级开关也随之消失
（migration `0019`）。因此**引擎故障即全群哑火**，这是刻意的产品定案：
一张能悄悄降级到 print 的部署，等于让故障只在审计里可见。

唯一的防抖是**常驻重生退避**（内存态，`src/app/resident.rs`）：每次退役 tier+1，
等待 `[60, 300, 900, 1800]` 秒中对应一档，1800 封顶；10 个干净轮清零。
进程重启即忘记梯子，这是刻意语义。

**resident 路径**（`src/app/turn.rs:1987` 起）：每群一个常驻 PTY 会话，一轮的真相
不在 stdout 而在 agy 自己的会话 DB（`<home>/.gemini/antigravity-cli/conversations/<id>.db`
的 `steps` 表）。`step_stream` 把表读成 `StepEvent` 序列：

| type | 含义 | 正文位置 |
|---|---|---|
| 14 | user 输入 | payload 19.2 |
| 15 | model 步 | 发言 20.1、thought 20.3、工具调用 20.7 |
| 17 | 错误步 | 24.3.2（已知类别，带文本但**不是**漂移） |
| 101 | 任务通知 | `<conv>/task-N` 引用被关闭的任务 |
| 132 | 工具结果 | field 148.1 = 发起的 task id |

`KNOWN_TYPES = [14, 15, 17, 101, 132]`（`src/agent/step_stream.rs:40`）。未知类型
**不再无条件硬失败**：带可读文本的未知步才是 `ParseError::Drift`（退役该会话 +
审计 `resident.drift.rotate.g<群>`，走重生退避），不带文本的只
计数跳过。步是可变的——同一 idx 的 payload 会在生成中被原地
改写，所以每次调用都全量重读整表，调用方不得缓存增量尾巴。

### 2.5 交付屏障与插话

`src/agent/barrier.rs` 决定这一轮何时完成、哪条文本可以发。`TurnSpec` =
`{start_idx, insert_idxs: BTreeSet<i64>, pending_inserts, end_idx}`。五个条件全满足
才算完成：

1. 本轮发出的每个插入都已被 agy 接纳（它的 user 步已存在于步流）；
2. 最后一个被接纳的 user 步之后至少有一条 `agent_text`；
3. 本轮发起的后台任务没有未配对的（配对 = 之后出现引用它 id 的 101 通知）；
4. 静默期：最后一条步之后至少 `quiet_s` 秒（`DEFAULT_QUIET_S = 12.0`）；
5. 交付物是最后一个被接纳 user 步之后**最后一条** `agent_text`，且必须过契约校验。

`Verdict::{Deliver{idx,text}, NotReady{reasons}, Failed{idx,reason}}`。
**草稿红线**：出现在最后一个被接纳 user 步之前的每条 `agent_text` 都是草稿，
永不交付；最后一条校验不过则整轮 `Failed`，屏障**永不回退草稿**。

`pending_inserts` 是相对 Python 参考实现唯一的适配：Python 版回放时插入 idx
事后已知，生产里要等它落库才知道，所以屏障用「已投递未见到」的计数把「我投了但
agy 还没接纳」和「agy 接纳了」区分开，而不是靠超时猜。

插话的价值判定是纯函数 `insert::decide`（`src/app/insert.rs:150`），顺序即契约：

```text
未开启            → Hold(disabled)
插话预算已用完     → Hold(insert_limit)
turn_age >= soft_cap → Hold(soft_cap)
档次不是 T0/T1     → Hold(pacing_tier)
含图片             → Hold(image_deferred)   ← 图片要等下一轮全轮，插入路径不描述图
mentions_bot       → Accept(mention)
from_owner         → Accept(owner)
same_speaker       → Accept(same_speaker)
same_topic         → Accept(same_topic)
其它               → Hold(uninteresting)
```

`CONTINUATION_WINDOW_SECONDS = 60`：本批距上一轮最后一条消息在 60 s 内算「延续」。

### 2.6 量级与耗时：关键常量

| 常量 | 值 | 位置 |
|---|---:|---|
| `TICK_INTERVAL` | 500 ms | `src/app/mod.rs:54` |
| `HEALTH_INTERVAL` / `HEALTH_CONNECT_INTERVAL` | 30 s / 1 s | `src/app/mod.rs:56,68` |
| `FOLD_INTERVAL` | 600 s | `src/app/mod.rs:652` |
| `resident quiet_s` / `soft_cap_s` / **`hard_cap_s`** | 5.0 / 45 / **90** 秒 | `src/app/turn.rs:137-163` |
| `resident max_inserts` / `poll_ms` / `deliver_timeout_s` | 3 / 500 / 15 | 同上 |
| `RESIDENT_STEP_ROTATE_LIMIT` | 40 步 | `src/app/turn.rs:132` |
| `INSERT_BOOST_PENDING` / `_MAX_INSERTS` / `_SOFT_CAP_S` | 10 / 6 / 60 s | `src/app/turn.rs:167-171` |
| `CONVERSATION_TURN_LIMIT`（默认关闭） | 0，配置默认 35 | `src/app/turn.rs:269`、`src/config.rs:126` |
| `SEND_GATE_MAX_AGE_SECONDS` / `_MAX_NEWER_MESSAGES` | 300 / 8 | `src/app/turn.rs:283,286` |
| `QUOTE_ANCHOR_MAX_NEWER` | 5 | `src/app/turn.rs:289` |
| `MIN_BURST_SECONDS` / `HOLD_MAX_SECONDS` / `GATE_TIMEOUT` | 2 / 6 s / 15 s | `src/app/preflight.rs:85,75,97` |
| `STALE_ROW_SECONDS` | 600 | `src/app/intake.rs:1384` |
| `FLOOD_POOL_THRESHOLD` / `FLOOD_LIVE_MAX` | 20 / 15 | `src/app/intake.rs:1393,1399` |
| `TRIGGER_MESSAGES` / `KEEP_RECENT` / `MAX_FOLD_MESSAGES` / `TRIGGER_AGE_SECONDS` | 300 / 100 / 200 / 2 700 | `src/memory/chunker.rs:22,26,30,82` |
| `SNAPSHOT_MAX_AGE_SECONDS` / `STALE_ESCALATION_SECONDS` | 1 200 / 1 800 | `src/app/pacing.rs:76,78` |

**90 秒硬顶**是常驻轮的上限：`turn_age >= hard_cap_s` 时该轮被中断并按失败收场；
45 秒软顶之后不再接受新插话。`boosted_limits`（`src/app/turn.rs:183`）在待处理
行数 ≥ 10 时把插话席位放宽到 6、软顶放宽到 60 s——**只放宽插入侧**，因为插话骑的是
已经付过费的调用，而提前交批会开更多轮、更快烧掉小时预算。

### 2.7 额度分档（pacing）

`src/app/pacing.rs`，全是纯函数：

```text
tier_for_weekly:  >50 → T0, >25 → T1, >10 → T2, 否则 T3
apply_five_hour:  <10 → 强制 T3, <25 → 收紧一档 (只收不放)
escalate_stale:   last_fresh.escalate(1 + overdue/1800)，overdue = max(0, age−1200)
decide:           weekly_pct 或 age 缺失 / age>1200 / 抓取出错 → 不信快照，走 stale 分支
```

`Tier::{T0,T1,T2,T3}` 的 `Ord` 顺序即松紧顺序，`stricter()` 饱和到 T3。
`group_cap` 取三者最严：熔断、档位天花板（T1/T2 用 `T1_CAP = 30`，T3 用
`T3_CAP = 3`）、按权重的全局分摊（`T1_FLOOR_PER_GROUP = 8` 的地板先于天花板生效，
所以地板永远抬不破天花板）。`projected_global_hourly` 用
`COST_PER_CALL_PCT = 0.03`、`BOT_WEEKLY_SHARE = 0.5` 把剩余额度摊到重置前的
小时数上。

### 2.8 发送侧：outbox、新鲜度门、引用兜底

`src/pipeline/outbox.rs` 的核心语义在 `classify` 与 `DeliveryResult`：

| OneBot 结果 | `DeliveryResult` | 落库状态 | `ErrorClass` |
|---|---|---|---|
| `Sent{message_id}` | `Sent` | `sent` | 无 |
| `Rejected{retcode,message}` | `Rejected` | `unknown` | `onebot_action_failed` |
| `Unknown{reason}` | `Unknown` | `unknown` | `onebot_disconnected` |

**结果不确定时状态是 `unknown`，绝不自动重发**——只有运维能在面板上把 `unknown`
改回 `pending`。拒绝也记成 `unknown`，因为盲目重发被拒的 payload 会死循环。

发送前还有一道新鲜度门 `send_gate_drop(batch_age_seconds, newer_messages)`
（`src/app/turn.rs:506`）：批内最新消息距今超过 300 s，或之后群里又来了 8 条以上，
整轮丢弃并审计。注释里的实测很有说服力：90 s 时一个安静的群里 agent 只是想了
两分钟就丢了每条回复（age 188 / new 0）。

引用兜底 `quote_drop_reason`（`src/app/turn.rs:638`）：锚点已经有 5 条以上更新的
消息时，评论必须带引用；不可验证则丢弃。

出站文本先过 `segments::filter_secrets`（`src/agent/segments.rs`）：启动时载入的
真实凭据值（`ADMIN_TOKEN`、Aux key，只取长度 ≥16 的）做精确替换，再用
命名值掩码兜住 `token/secret/password/passwd/api_key/apikey` 后跟 `:` 或 `=` 且
值 ≥8 字符的情况，命中替换为 `[Filtered]`。URL 不会被误判（
`a_long_url_is_not_mistaken_for_a_key`）。

---

## 三、关键不变量与产品原则

1. **宁可不回复，也不要错位。** 落到代码上至少有五处独立实现：陈旧行护栏
   （600 s）、发送前新鲜度门（300 s / 8 条）、引用锚点门（5 条）、
   「不可验证就丢弃」的引用兜底、以及 `unknown` 不自动重发。
2. **闸门不可达即放行。** `BurstScore::Unavailable.hold_seconds() == 0`：节奏闸门
   超时、报错、解析不出分数一律当「已经说完」，队列永不因闸门而停摆。这不是
   宽容，是设计——闸门只决定「何时交出去」，不决定「回不回」。
3. **闸门从不决定回不回。** `src/app/preflight.rs:113-118` 明说：分数里没有
   「沉默」这个变体，闸门对「她回不回」没有权限。
4. **禁止返回修（永不放宽严格路径）。** `contract::validate` 是全有或全无；`salvage`
   只在严格路径已经失败之后跑，是旁路，不是放宽。
   `delivery_was_salvaged`（`src/app/turn.rs:197`）判断一次交付是否只靠抢救梯通过
   （层不是 `None`），若是则它是可能半成品，需要稳定门。
5. **salvage 稳定门（be56e3d）。** `salvage_text_stable`（`src/app/turn.rs:207`）：
   抢救出来的步文本必须在**连续两次**屏障求值里都是「同一 idx 且同一文本」才算稳定；
   变了、或是没见过的新步，都不算。修复中的半截文本不会被当成最终答案发出去。
6. **hy3 产物一律 fail-open，且是派生视图。** 读侧摘要每轮现算、不落库
   （`src/memory/digest.rs`）；提取出来的记忆条目是唯一例外（它们是带溯源的新事实行）。
7. **提取只增不改。** `src/memory/extract.rs` 只能 `create`，never update/delete，
   `source_refs` 必须 ⊆ 本批 fold 覆盖的消息，置信度封顶
   `MAX_EXTRACTED_CONFIDENCE = 0.8`，每批 ≤ `MAX_CANDIDATES = 6` 条，近重
   （`NEAR_DUPLICATE_THRESHOLD = 0.55` bigram）丢弃。
8. **遗忘 fail-safe，记忆 fail-open。** 二次压缩（`src/memory/consolidate.rs`）
   任何一步失败整轮不动——绝不靠猜归档，因为丢掉的记忆是静默丢失。
9. **表情库的 `kept` 永不被 upsert 冲突分支改写。** 换提示词、重新打标都不会往
   她库里偷偷塞东西，也复活不了她拒绝过的图（`kept = 0/1/-1` 三态）。
10. **群范围只从 roots + `scope.json` 来，绝不从工具参数来。** 模型连传都传不了，
    所以读不到别的群。
11. **原始消息永不删。** `group_messages` 只归档（`archived` + `archived_at`），
    fold 之后连归档行也保留 1 000 条（`ARCHIVED_MESSAGES_KEEP`）。
12. **权限边界交给容器，不假装 argv 能隔离。** `--sandbox` 刻意不传（1.4.7 上与
    已登录的 agy-home 互斥，且在此环境里唯一作用是禁 `command`）；`run_command`
    一律拒绝的 hook 是必需的，因为 `permissions.deny` 只对读写生效。

---

## 四、配置与运行

### 4.1 配置来源

两层：环境变量（`src/config.rs:200-350`）+ 可选的 `bot.toml`
（`GEMINI_BOT_CONFIG` 指向，`FileConfig` 带 `deny_unknown_fields`）。校验在启动期
完成，任何安全问题都是**致命**的（`main.rs` 返回 `ExitCode::from(2)`）：

- `ONEBOT_BOT_ID`、`OWNER_QQ_ID` 必须是纯十进制；
- `ADMIN_TOKEN_FILE`（优先）或 `ADMIN_TOKEN`，弱 token 拒绝启动；
- `SNOWLUMA_MEDIA_ORIGINS` 必须是精确 origin（允许带端口，不许通配符、不许带路径）；
- `AUX_MODEL` 是普通配置（默认 `hy3`）：网关模式下作为请求里的模型名
  发送，可填网关支持的任何模型；agy 模式下辅助职责不经网关、与它无关。
- `EVENT_WEBHOOK_URL` 不设即关闭事件 webhook；`EVENT_WEBHOOK_SECRET`
  设置后投递带 HMAC-SHA256 签名。事件与格式契约见 `docs/EXTENDING.md`，
  实现在 `src/integrations/webhook.rs`（发送端、尽力而为、不重试）。

**未验证的外部契约不是致命的**：它们变成 `production_ready: false` 与具体的
blockers（`src/health.rs:100-151`），因为拒绝启动会藏起运维最需要看的诊断。

主要 `FileConfig` 默认值（`src/config.rs:109-136`）：`quiet_window_seconds=3`、
`max_batch_age_seconds=30`、`max_batch_messages=50`、`max_batch_chars=8000`、
`max_batch_images=4`、`agent_concurrency=2`、`agent_timeout_seconds=180`、
`memory_context_budget=4000`、`compaction_daily_limit=10`、`hourly_call_budget=60`、
`flood_enabled=true`、`conversation_turn_limit=35`、`reply_quote_enabled=true`、
`resident_quiet_s=5.0`、`resident_soft_cap_s=45`、`resident_hard_cap_s=90`、
`resident_max_inserts=3`、`resident_poll_ms=500`、`resident_deliver_timeout_s=15`、
`resident_insert_esc=false`（插入投递模式：false = 等空闲屏借 send-immediately 并入；
true = 先 ESC 中断当轮生成再投递。ESC 会把 TUI 停在「已取消、不就绪」，默认关闭；
改 bot.toml 后重启即切换）。
`no_submission_retry_groups=[]`（零步哑火重放：列出的群在 no_submission
退役后，由 intake 把释放的原批次作为独立 retry 任务在新会话优先重放一次；
空列表 = 关闭，改 bot.toml 后重启生效）。
`bot_call_names`（bot.toml，默认 `["Gemini"]`）：消息文本里出现这些称呼
即算 addressed（与 @/引用她的消息/主人发言同级），不走相关性门（只应答档不认此触发，见 ambient_pace 条）。
`ambient_pace`（群级档位、非 bot.toml 键：面板/管理 PATCH 写 groups.ambient_pace，
intake 每 tick 读、即时生效）：她加入未冲她说话的闲聊（ambient）的积极程度，
四档 `addressed_only`（只应答）/`quiet`（过相关性门＋冷却）/`normal`（过相关性门，
默认）/`lively`（不判相关性，回到旧行为）；addressed 批次不受此档位影响。
`addressed_only` 例外（2026-10-30 改定）：该档为脚本触发——只有 @ 她或引用她的消息算触发（主人发言、喊称呼在此档不算），放行不经 hy3 节奏门与相关性门（quiet 窗口＋硬顶的脚本判定）；成轮上下文取固定范围：触发消息加上方最多 10 条（`STRICT_CONTEXT_LOOKBACK`），更早的待处理行当场消费并审计（`intake.addressed_only.context_trim.*`），泄洪摘要不带入此档。
相关性门（审计 `intake.relevance.*`）：ambient 窗口成轮前由独立的
RelevanceJudge（Aux hy3、会话 `relevance:{bot}:{group}`）判相关性，
与 preflight 分工——preflight 只判「说完没」、无权决定回不回。判官不可达
按 relevant 放行（fail-open）；verdict 按窗口 widest 行缓存，新消息才重判；
quiet 档另有 10 分钟 ambient 冷却（`AMBIENT_QUIET_COOLDOWN_S`）。

### 4.2 后台循环

`App::serve`（`src/app/mod.rs:381`）起五个循环：

| 循环 | 周期 | 职责 |
|---|---:|---|
| admin HTTP | — | `axum::serve`，bearer 层由 `admin_router` 加 |
| OneBot 监督 | 30 s / 连不上时 1 s | 重连 forever + 刷新 health |
| flush | 500 ms | `flush_tick` → `scheduler.enqueue` |
| health | 30 s | `mihomo.probe()` 与 `aux.probe()` 并行（串行时慢的那个会把快的结果挡在快照外） |
| fold | 600 s | fold → 提取 → trim 归档 → 二次压缩 → 交接纸条 |

L3 记忆摘要压缩是**面板专用**，没有自动循环：派生摘要不进 prompt，自动跑等于为
没人看的页面烧 Aux token。

### 4.3 管理面

同一批 handler 双挂 `/api/v1/*`（公开面，契约见 `docs/openapi.yaml`）与
`/internal/v1/*`（旧前缀，存量网关/面板过渡用）；`settings`、`settings/aux`、
`settings/aux/test`、`settings/runtime` 四条运行时设置路由只挂 `/api/v1`
（aux 配置与轮换/降级开关热生效，覆盖层落盘在数据库旁的
`runtime-settings.json`，见 `src/app/runtime_settings.rs`）。17 条共享路由：`status`、`quota`、
`groups`（列表 / PATCH 单群 / 单群 state）、`groups/{id}/memories`（增删改查）、
`persona`、`groups/{id}/memory/compaction`、`groups/{id}/session/reset`、
`stickers`（列表 / PATCH / DELETE / 原图）、`outbox/{id}/retry`、`pending`、`audit`。
认证是常量时间比较的 bearer token，从不记录、不回显。
Web 管理台（\`web/\`，React + Vite）构建后由同一监听托管：\`WEB_DIST\` 指向构建产物目录时，\`/console/\` 下提供静态页面（页面本身不含数据、不设鉴权，其调用的 API 仍走上面的 bearer 认证），构建与使用见 \`web/README.md\`。

### 4.4 deploy.sh 的角色

`deploy.sh`（158 行）是一键部署脚本，做：本地 `cargo fmt --check` + 
`cargo test --offline` → 安全校验 → 备份镜像 → 同步到部署机 → 构建 → 重启 → 验收，
支持 `--check-only` 只跑本地检查、`FORCE=1` 跳过 pending 非 0 的中止。**本文档只
描述它的职责，不执行它**；本文所有验证都只用 `cargo test --offline`。

---

## 五、存储

16 张表（`migrations/0001_init.sql` … `0016_call_priority.sql`，10 个索引）：
`groups`、`group_members`、`speaker_aliases`、`pending_events`、`seen_messages`、
`group_state`、`group_messages`、`memories`、`memory_chunks`、
`group_memory_summaries`、`memory_compaction_jobs`、`agent_calls`、`outbox`、
`stickers`、`audit_events`、`persona_config`。

启动时 `App::build` 做的恢复动作（`src/app/mod.rs:105-130`）值得单列，它们决定了
崩溃后的语义：

- `reconcile_calls`：被打断的调用记为 `unknown`，**永不重放**；
- `release_orphaned_claims`：上个进程 claim 了却从未执行的消息释放回池子，
  每群留一行审计（否则它们对 flush tick 永远不可见）；
- `reconcile_jobs`、`reconcile_outbox`；
- `prune_seen(now - 7 天)`。

`src/store/mod.rs` 里三组可读状态：`KEPT_YES=1 / KEPT_UNJUDGED=0 / KEPT_REFUSED=-1`；
`origin::{AGY="agy", PANEL="panel", HY3="hy3"}`，`audit_actor` 把 `hy3` 映射为
`hy3_extract`；`CallState::{Running,Succeeded,Failed,Unknown}`、
`OutboxState::{Pending,Sending,Sent,Unknown}`（有 `parse`）。
