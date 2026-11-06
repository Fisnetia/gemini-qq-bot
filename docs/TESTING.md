# 测试指南（基线 8539f0a）

本文说明 gemini-qq-bot 的测试怎么跑、现在有多少、这次补了什么、还缺什么。

---

## 一、怎么跑

### 1.1 基本命令

```bash
cargo test --offline
```

**必须带 `--offline`**：`vendor/` 目录里只有一份 README（依赖不走 vendored 路径），
所有依赖都来自本地 cargo registry 缓存，联网拉取会引入不确定性。

进一步收窄：

```bash
cargo test --offline --lib                       # 只跑 src/ 里的单元测试（714+57 个）
cargo test --offline --lib app::damper           # 只跑一个模块
cargo test --offline --lib mood -- --nocapture   # 保留 stdout，调试时用
cargo test --offline -- --test-threads=1         # 串行（排查顺序相关问题时）
```

`cargo test --offline` 会依次跑三个 target：

| target | 数量 | 说明 |
|---|---:|---|
| `unittests src/lib.rs` | **771** | 全部单元测试，20.5 秒 |
| `unittests src/main.rs` | 0 | `main.rs` 没有测试（入口逻辑全部在 `app::`） |
| `tests/p4_replay.rs` | 6，全 **ignored** | 需要 `/tmp/insert-lab` 夹具，仓库里没有 |

### 1.2 为什么 `tests/p4_replay.rs` 全被忽略

那 6 个测试是「运行中插入消息」离线回放的验收（P4.3-a），夹具是生产库里
抽出来的会话 DB 副本，体积与内容都不适合进仓库。它们的 `#[ignore]` 原因文本
统一是「需要 /tmp/insert-lab 夹具」：

```text
lab_dbs_parse_without_drift_and_expected_shapes
production_copies_match_the_scan_table
production_turns_all_settle
replay_e0_blocks_on_unpaired_task
replay_e2_delivers_merged_and_keeps_draft
replay_e4_three_turns
```

要跑它们得自己准备夹具，本文不涉及。**因此「`cargo test` 全绿」的实际含义是
771 个单元测试全过、6 个集成测试被显式忽略。**

### 1.3 格式检查

```bash
cargo fmt --check
```

`deploy.sh` 第一道就是这个，写完代码先跑一遍，别等到部署时才被拦下。

### 1.4 测试的确定性约定

本项目测试全部遵守四条，新增测试必须照办：

1. **不联网。** 所有 HTTP 相关测试只测纯函数（`check_url`、`host_is_forbidden`、
   `origin_of`）或注入假的 transport，从不发真实请求。
2. **不依赖真实时间。** 涉及时间的函数一律显式传 `now: i64`，测试里用固定的
   假时间戳（如 `T0 = 1_700_000_000`）推进。
3. **不用未固定随机数、不用外部进程。** 需要子进程的路径（agy、mihomo）测试时
   都走桩或在函数边界外被切断。
4. **需要库的测试用临时库。** 模式固定为
   `tempfile::tempdir()` + `dir.path().join("bot.sqlite3")` + 
   `crate::store::migrations::open_and_migrate(&path).await`，测完自动清理。
   不碰 `runtime/data`，不碰任何真实 SQLite 库。

### 1.5 一条已知的偶发

`src/app/intake.rs` 的 `bounds_concurrent_turns_per_group` 系列依赖真实的批量
超时窗口，在本机负载高时会偶发失败（一次实测在 20 秒超时前没等到 claim）。
它在单跑和常态全量跑里都是绿的，重跑即恢复。**这是夹具性质带来的抖动，不是
产品 bug**，但如果 CI 上要跑，建议对 `app::intake` 单独串行或加大超时。

---

## 二、当前测试总数与分布

`src/` 共 71 个文件 45 756 行，`#[test]` / `#[tokio::test]` 标记 771 个
（与实跑完全一致）。

| 模块 | 行数 | 测试数 | 行/测试 | 备注 |
|---|---:|---:|---:|---|
| `src/app/` | 15 508 | 215 | 72 | `turn.rs` 一文件 68 个；`resident.rs` 仅 1 个 |
| `src/agent/` | 8 725 | 183 | 48 | `contract.rs` 38、`agy.rs` 32、`segments.rs` 22 |
| `src/memory/` | 5 438 | 117 | 46 | `extract.rs` 25、`retrieval.rs` 22 |
| `src/onebot/` | 1 684 | 40 | 42 | `normalize.rs` 19（不可信输入的主战场） |
| `src/store/` | 4 234 | 36 | 118 | `sqlite.rs` 3 590 行只有 24 个 |
| `src/pipeline/` | 1 478 | 30 | 49 | `outbox.rs` 7、`scheduler.rs` 6 |
| `src/integrations/` | 1 887 | 28 | 67 | `aux_gateway.rs` 14、`quota.rs` 14 |
| 根（`lib/main/config/error/health`） | 1 409 | 28 | 50 | `config.rs` 11、`health.rs` 9、`error.rs` 8 |
| `src/media/` | 1 270 | 27 | 47 | 缩放/拼图 8+8 |
| `src/mcp/` | 1 235 | 22 | 56 | `memory.rs` 13 |
| `src/state/` | 741 | 21 | 35 | `mood.rs` 15 |
| `src/admin/` | 1 643 | 15 | 110 | `routes.rs` 1 483 行只有 9 个 |
| `src/process/` | 504 | 9 | 56 | mihomo 子进程监督 |
| **合计** | **45 756** | **771** | **59** | 加 `tests/p4_replay.rs` 6 个 ignored |

**零测试的文件**（14 个）：`src/lib.rs`、`src/main.rs`、`src/pipeline/mod.rs`、
`src/media/mod.rs`、`src/state/mod.rs`、`src/mcp/mod.rs`、`src/memory/mod.rs`、
`src/agent/mod.rs`、`src/process/mod.rs`、`src/admin/mod.rs`、
`src/integrations/mod.rs`、`src/onebot/mod.rs`。其中除了 `lib.rs`/`main.rs` 之外的
11 个都是只含 `pub mod` + `pub use` 的胶水文件，没有可断言对象。

---

## 三、本次新增的 57 个测试

全部追加在原有 `#[cfg(test)] mod tests` 末尾（`src/error.rs` 与 `src/store/mod.rs`
原本没有测试模块，新建）；**生产逻辑零改动**。选模块的标准是
「行/测试 密度高 + 逻辑纯 + 断言对象明确」，避开需要重夹具的路径。

### 3.1 `src/error.rs`（8 个，新增覆盖）

原本 226 行零测试，是错误分类的唯一真相——`ErrorClass` 的 slug 直接进审计与
告警，错了会让 外部告警系统 分不清「代理断」和「模型崩」。

| 测试 | 场景 |
|---|---|
| `every_error_class_serialises_to_its_stable_slug` | 12 个变体逐个对 slug 字符串 |
| `error_class_slugs_are_unique` | 12 个 slug 互不相同（防复制粘贴漏改） |
| `config_error_carries_the_field_it_rejected` | `配置错误: ` 前缀 + 字段名在消息里 |
| `store_and_io_errors_are_prefixed` | `存储错误: ` / `IO 错误: ` 前缀 |
| `an_archived_memory_names_the_exact_row` | `记忆已归档，拒绝更新: ` + 行标识 |
| `a_plain_message_is_never_reworded` | `Message` 变体不加任何前缀 |
| `every_variant_is_usable_as_an_error_object` | 全部变体可 `&dyn Error`、`to_string()` 非空 |
| `bot_result_holds_the_bare_value_on_the_ok_path` | `BotResult<T>` 的 Ok 路径不变形 |

### 3.2 `src/store/mod.rs`（9 个，新增覆盖）

407 行零测试，里面全是写进数据库的字符串常量。写错一个字符，`OutboxState::parse`
就会在读回时把 `sent` 当成未知。

| 测试 | 场景 |
|---|---|
| `origin_constants_are_the_stored_slugs` | `AGY="agy"`/`PANEL="panel"`/`HY3="hy3"` |
| `only_extraction_gets_its_own_audit_actor` | `audit_actor("hy3")=="hy3_extract"`，其余原样 |
| `kept_values_are_distinct_and_ordered` | `KEPT_YES=1 / UNJUDGED=0 / REFUSED=-1` 且大小序 |
| `job_state_slugs_round_trip_through_serde` | 4 变体 serde snake_case 往返 |
| `call_state_slugs_are_lowercase_and_unique` | 4 个 slug 小写且不重复 |
| `actor_kind_slugs_are_the_audit_actors` | `Owner`/`Panel` 的审计名 |
| `outbox_slugs_round_trip_and_unknown_parses_back_to_unknown` | `parse` 各值 + `unknown` 往返 |
| `outbox_parse_refuses_anything_else` | 大小写错、空串、`sent ` 带空格 → `None` |
| `a_record_is_archived_only_after_the_flag_lands` | `is_archived` 只看 `archived_at` |

### 3.3 `src/state/mood.rs`（9 个，从 6 增到 15）

情绪与话题是每轮 prompt 都要读的字段，长度预算（32/200/40 字）超了会污染
prompt 或让 `MoodState::new` 静默返回 `None`（机器人突然「没有情绪」）。

| 测试 | 场景 |
|---|---|
| `mood_label_at_the_budget_boundary_is_accepted` | label 恰好 32 字符 → `Some` |
| `mood_reason_at_the_budget_boundary_is_kept_intact` | reason 恰好 200 字符原样保留 |
| `a_blank_label_is_never_a_mood` | 空串、纯空格、纯 tab → `None` |
| `stored_timestamp_survives_the_round_trip_and_defaults_to_zero` | `updated_at` 保留，缺省为 0 |
| `label_or_none_matches_what_the_prompt_would_see` | `label_or_none` 与 prompt 视角一致 |
| `a_bare_label_longer_than_the_budget_degrades_to_none` | 裸字符串超 32 → `None` |
| `to_json_never_fails_and_stays_valid_json` | `to_json()` 输出可被 `serde_json` 解析 |
| `topic_exactly_at_the_budget_is_kept` | topic 恰好 40 字符 → `Some` |
| `topic_is_trimmed_and_blank_is_empty` | trim 后 `as_str`、空白 `is_empty()` |

### 3.4 `src/pipeline/dedupe.rs`（5 个，从 3 增到 8）

去重窗口是内存里唯一会无界增长的结构，容量语义错了就是慢速内存泄漏。

| 测试 | 场景 |
|---|---|
| `a_zero_capacity_is_clamped_rather_than_emptying_the_window` | `new(0)` 被夹到 1，第一条仍是 `New` |
| `a_duplicate_does_not_grow_the_window_or_refresh_its_position` | Duplicate 不改 `len`、不延寿 |
| `the_window_never_exceeds_its_capacity` | 反复 observe 后 `len() <= capacity` 恒成立 |
| `the_default_capacity_is_the_documented_one` | `DEFAULT_CAPACITY == 20_000` |
| `the_key_namespaces_both_ids_and_keeps_them_distinguishable` | `bot:msg` 拼接，含冒号输入不串号 |

### 3.5 `src/agent/segments.rs`（14 个，从 8 增到 22）

出站文本的最后一道闸：这里漏一个控制字符，群里就是方块；漏一个凭据，就是泄露。

| 测试 | 场景 |
|---|---|
| `an_empty_reply_produces_no_messages` | 空回复 + 无 parts → 空 `Vec`（不是 `[""]`） |
| `whitespace_only_parts_fall_back_to_the_raw_text` | 全空白 parts → 回退到原文切分 |
| `control_and_invisible_characters_never_reach_the_group` | `\u{feff}`、`\u{2028}`、`\u{2029}` 被剥离 |
| `every_sentence_terminator_splits_the_fallback` | `。！？!?；;` 与换行都切 |
| `a_quote_lands_on_the_first_line_a_part_produces` | 引用只挂在该 part 的首行 |
| `a_quote_index_points_at_its_own_part_not_the_first_line` | 多段引用索引不串位 |
| `a_quote_on_a_blank_part_is_dropped_with_the_part` | 空 part 被丢，引用也不留 |
| `the_fallback_path_never_carries_a_quote` | 回退路径没有任何 quote |
| `short_known_values_are_never_masked` | known 值 <16 字符不做精确替换 |
| `a_secret_shaped_value_without_a_marker_is_left_alone` | 无 `:`/`=` 标记的值不动 |
| `gaps_are_computed_from_the_previous_part_only` | `gaps[i]` 只看 part i 的长度 |
| `a_gap_is_never_below_the_floor_for_a_one_char_part` | 1 字符 part → `800+12=812`，不低于 `MIN_GAP_MS` |
| `gaps_scale_linearly_between_the_bounds` | 60 字顶格 1400、中间线性 |
| `masking_merges_overlapping_marker_values` | 重叠区间合并后从右往左替换 |

### 3.6 `src/app/damper.rs`（12 个，从 12 增到 24）

降温器决定「是不是同一人刷屏」，误判会让整个群闭嘴，所以每个档位、每条
hold 的优先级都必须钉住。

| 测试 | 场景 |
|---|---|
| `tiers_are_strictly_ordered_and_level_matches_the_order` | `L3>L2>L1>L0`，`level()` 对应 3/2/1/0 |
| `from_streak_changes_level_exactly_at_the_documented_counts` | 0/5/6/9/10/13/14/999 的边界 |
| `quiet_windows_and_pending_floors_are_monotonic_in_the_level` | `quiet_seconds`/`min_pending` 随档单调 |
| `reset_reasons_have_stable_audit_slugs` | `third_party`/`owner`/`silence` |
| `the_neutral_verdict_never_holds_anything` | `Verdict::neutral()` 字段值与「永不 hold」 |
| `an_addressed_batch_is_never_held_whatever_the_level` | 被点名就直接放行，压过一切 |
| `the_l3_hold_is_reported_before_the_soft_cap` | l3 硬 hold 优先于 cap |
| `the_soft_cap_outranks_the_quiet_window` | cap 优先于 quiet |
| `quiet_and_thin_holds_are_soft_and_named_after_their_level` | 软 hold 的 reason 与 `hard=false` |
| `an_unknown_group_is_undamped_and_has_no_hint` | 没见过的群 → `L0`、`hint()` 为 `None` |
| `messages_older_than_the_window_leave_it` | 600 秒滑窗淘汰后 `window_messages` 下降 |
| `the_owner_ends_an_episode_without_joining_the_window_count_as_a_partner` | 主人终止计入 `Owner` 而非 `ThirdParty` |

### 3.7 结果

```text
cargo test --offline --lib   →  771 passed; 0 failed; 0 ignored; 20.50s
cargo test --offline         →  lib 771 全绿；tests/p4_replay.rs 6 ignored；EXIT=0
```

基线是 714，净增 57。没有测试被标 `#[ignore]`，因为**没有测试暴露 bug**
（本次未新增「疑似 bug」条目，详见报告）。

---

## 四、没覆盖的空白区

按「值不值得补」排序，写清原因，方便下次接着干。

### 4.1 数据库层（`src/store/sqlite.rs`，3 590 行 / 24 测试 / 150 行每测试）

**最大的空白**。它是唯一有真实 SQL 的文件，一条 `WHERE` 写漏就可能跨群泄露或
永久饿死一批消息。为什么这次没碰：它的公开面有 80+ 个方法，每个都需要
`open_and_migrate` 建库，补 20 个测试要写 20 套夹具，单位时间产出最低；而且
现有 24 个测试已经钉住了最关键的三条不变量（`mark_seen` 持久去重、
`release_orphaned_claims` 释放孤儿、`commit_chunk` 的 seq 连续性）。

优先补的具体点：`reconcile_calls`/`reconcile_outbox`/`reconcile_jobs` 三个崩溃
恢复函数各自的「已 running → unknown」与「已 sending → unknown」迁移；
`prune_seen` 的 7 天边界；`ARCHIVED_MESSAGES_KEEP = 1000` 的归档裁剪。

### 4.2 `src/app/resident.rs`（544 行 / 1 测试）

常驻会话的 spawn/rotate/kill 状态机，只有 1 个测试。原因是它几乎所有路径都要
真的起 PTY 子进程，测它等于测 agy。可行的补法是把「何时轮换」（`RESIDENT_STEP_
ROTATE_LIMIT = 40`）、「何时回收」（`turn_age >= hard_cap_s`）抽成纯判定函数
再测——但这需要改生产代码的结构，超出本次「只加测试」的授权范围。

### 4.3 `src/app/mod.rs`（822 行 / 3 测试）与 `src/admin/routes.rs`（1 483 行 / 9 测试）

都是「组装 + 路由」，逻辑被大量委托出去。`routes.rs` 的 handler 需要构造
`AppState`（内含 `SqliteStore`、`Mihomo`、`AuxLlm`、各种 `Arc`），而
`http-body-util` 与 `tower` 的 `util` 已在 dev-dependencies 里——说明原作者的
意图是能测的，只是没测完。可行补法：给每个 GET 路由写「未带 token → 401」、
「带错 token → 401」、「带对 token → 200 且 JSON 形状固定」，不需要真实业务数据。

### 4.4 `src/integrations/`（`aux_gateway.rs` / 14、`quota.rs` 1 115 行 / 14）

fail-open 分支（超时、5xx、JSON 畸形 → 返回 `None` 让主流程继续）只测了一部分。
`quota.rs` 的 `SNAPSHOT_MAX_AGE_SECONDS = 1200` 与 `STALE_ESCALATION_SECONDS = 1800`
的边界值得表驱动。

### 4.5 `src/process/mihomo.rs`（499 行 / 9）

现在是唯一会把「代理没起来」和「代理起来了但出口不通」混淆的地方，前者已有
测试覆盖，后者的超时/重试序列没测（需要假时钟）。

### 4.6 刻意不补的

- **11 个纯胶水 `mod.rs`**：只有 `pub mod` + `pub use`，没有可断言对象。
- **`src/main.rs`**：三种模式的入口，`--health-probe` 要连真实 `ADMIN_BIND`。
- **`tests/p4_replay.rs`**：夹具不进仓库，保持 ignored 是正确状态。
