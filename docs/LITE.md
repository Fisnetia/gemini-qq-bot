# gemini-qq-bot-lite（基座版）

基座版是与完整版同仓的第二个二进制：只保留 QQ 联系层、交互层
与基座必需件，定位是**已经验证过的生产基座**——它复用完整版在
生产里验证过的机制（攒批、交付屏障、提交口令、outbox 状态机、
崩溃恢复），砍掉的是功能面，不是正确性机制。适合想要一个能
长期稳定挂着的群聊机器人、暂时不需要记忆/表情/额度管理的
部署。

## 与完整版的差异

| 能力 | 完整版 | 基座版 |
|---|---|---|
| 攒批（静默窗 + 硬顶切分） | 有（另有节奏闸门） | 有（只有时间口径，无闸门） |
| 应答触发 | @/引用/主人/称呼 + 相关性判定 + 群档位 | @/引用/主人/称呼（可配全量） |
| 交付通道 | MCP submit_reply + 屏障五条件 | 同左（同一套代码） |
| 发送 | outbox 先落库后发送，unknown 不重发 | 同左 |
| 崩溃恢复 | reconcile 全套 | 同左（同一套调用） |
| 长期记忆 | 有（检索/折叠/压缩/交接） | **无**：会话轮换后无交接，靠后续消息重建上下文 |
| 表情 | 库、打标、候选、发送 | **无** |
| 图片/语音 | 下载 + 辅助模型打标后给主模型 | **只占位**：`[图片]` / `[语音]`，模型看不见内容 |
| 额度管理 | 双桶 pacing 分档 T0–T3 | 只有全局小时保险丝（只熔断） |
| 辅助层（节奏/相关性/打标等） | 有，双模式 | **无** |
| 管理面 | HTTP 管理 API + Web 管理台 | **无**：只有 `--status` 自查与审计表 |
| 配置 | TOML 文件 + env + 热生效 | 只有 env，改配置即重启 |
| 探针/ canary | 每日自检轮 | **无** |
| 群接入 | 面板发现 + 开关 | 配置白名单（`LITE_GROUPS`） |

## 耦合纪律

基座版与 agy（Antigravity CLI）的耦合——步流解析、PTY 投递、
进程监督——只存在于共享的 `agent/` 适配层内；lite 的薄 app
（`src/lite/`）只依赖 agent 层的公开接口（ResidentSession、
barrier、step_stream 的公开类型），不解析步流原始字段。
薄 app 唯一引用的完整版 app 层符号是纯函数
`app::turn::build_prompt_batch`（提示渲染与校验作用域组装，
无运行时子系统），保证消息渲染格式与生产逐字一致。

## 多模态占位通则

基座版不下载、不识别任何媒体。图片与语音在消息里只呈现为
`[图片]` / `[语音]` 占位；随附的契约明确告诉模型：看不见就
不许编造内容、不许评价画质。将来若要做「图片路径直传 agy」，
那是独立实验，不在本版范围。

## 运行

```bash
# 必填
export LITE_BOT_ID=机器人QQ号
export LITE_OWNER_ID=主人QQ号
export ONEBOT_WS_URL=ws://127.0.0.1:3001
export LITE_GROUPS=群号1,群号2
# 建议独立库文件（与完整版共用同一套 schema，但别共用同一个文件同时跑）
export LITE_DB_PATH=/data/lite.sqlite3

gemini-qq-bot-lite            # 服务
gemini-qq-bot-lite --status   # 只读自查（逐群状态 + 阻塞项）
gemini-qq-bot-lite --health-probe  # 心跳 90 秒内退出 0
```

会话侧与完整版相同：每群一个 agy 常驻会话，工作区在
`<数据目录>/projects/<群号>/`（人设与契约写在 AGENTS.md，
改人设见 `LITE_PERSONA_FILE`）。

### MCP 配置（必须）

模型的交付通道是 MCP 工具 `submit_reply`，服务进程就是本
二进制自己的 `--mcp-lite` 模式。在 agy 的 `mcp_config.json`
里把服务器指向 lite 二进制，例如：

```json
{
  "mcpServers": {
    "gemini-qq-lite": {
      "command": "/usr/local/bin/gemini-qq-bot-lite",
      "args": ["--mcp-lite"],
      "env": { "LITE_DB_PATH": "/data/lite.sqlite3" }
    }
  }
}
```

`--mcp-lite` 模式只提供 submit_reply 一个工具。serve 与 MCP
进程靠同一个 SQLite 库通信（武装口令 + 提交记录都在库里），
所以两边的库路径必须一致。

## 配置全表

| 变量 | 默认 | 说明 |
|---|---|---|
| `LITE_BOT_ID` | （必填） | 机器人 QQ 号，纯数字 |
| `LITE_OWNER_ID` | （必填） | 主人 QQ 号，纯数字；其发言视为触发 |
| `ONEBOT_WS_URL` | （必填） | OneBot 正向 WebSocket 地址 |
| `ONEBOT_ACCESS_TOKEN` | 空 | OneBot 访问令牌；同时进出站凭据过滤名单 |
| `LITE_GROUPS` | （必填） | 群白名单，逗号分隔的纯数字群号 |
| `LITE_RESPOND_ALL` | 关 | `1` = 白名单群内全量触发（默认只应答触发） |
| `LITE_CALL_NAMES` | `Gemini` | 呼唤词，逗号分隔，不区分大小写 |
| `LITE_DB_PATH` | `./lite.sqlite3` | SQLite 库路径 |
| `LITE_DATA_DIR` | 库所在目录 | 心跳文件与每群工作区根 |
| `AGY_BIN` | `agy` | agy 二进制路径 |
| `AGY_HOME` | `$HOME` | agy 的 HOME（OAuth 与设置所在） |
| `AGY_MODEL` | `gemini-3.8-flash` | 模型 id，原样透传 |
| `LITE_PERSONA_FILE` | 内置人设 | 人设文本文件路径 |
| `LITE_QUIET_S` | `5` | 攒批静默窗（秒） |
| `LITE_MAX_AGE_S` | `180` | 攒批最大年龄（秒） |
| `LITE_TURN_LIMIT` | `35` | 会话轮换轮次 |
| `LITE_FUSE_PER_HOUR` | `60` | 全局小时保险丝（轮次/小时） |
| `LITE_RESPAWN_BACKOFF` | 开 | `0` = 关闭重生退避（梯 60/300/900/1800 秒） |
| `LITE_SECRETS` | 空 | 额外的已知秘密值（逗号分隔），出站命中即打码 |

## 行为口径（与完整版一致的部分）

- 只应答触发：@/引用机器人/主人/称呼命中才开轮；未触发的
  消息留在池里作上下文，等触发批次一并带上。
- 攒批：静默窗 5 秒 + 条数 50 / 字数 8000 / 图片 4 硬顶。
- 迟到提交按轮口令拒绝；发送结果 unknown 绝不自动重发；
  本批最新消息超过 300 秒整轮不发（发送年龄门）；pending
  超过 600 秒未认领直接丢弃（陈旧护栏），都留审计。
- 审计事件前缀 `lite.`：`lite.turn.completed`、
  `lite.turn.failed`、`lite.session.rotated`、
  `lite.send.unknown`、`lite.stale_drop`、`lite.fuse_tripped`、
  `lite.backoff`，写在与完整版同一张 audit 表里。

## 已知限制

- 无记忆：会话轮换（默认 35 轮）后是全新会话，没有交接
  纸条；模型只能靠 AGENTS.md 的人设与新批次重新进入状态。
- 多模态只占位：图片内容、语音内容一律不可见。
- 与 agy 的内部步流格式耦合：agy 大版本升级可能需要同步
  适配层（与完整版同一条约束）。
- 无管理面：改白名单、开关群都要改 env 重启；运行状态靠
  `--status` 与审计表。
