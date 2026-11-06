//! MCP stdio server exposing one read-only tool: `memory_search`.
//!
//! Shape of the contract, and why it is this small:
//!
//! * **One scope, injected.** The group is fixed by MCP roots + `scope.json` (see
//!   `session.rs`), never by a tool argument. A model that could pass a group id could
//!   read another group; it cannot pass one at all.
//! * **Read-only.** The SQLite handle is opened through the same store the bot uses, but
//!   this module only ever calls read methods. Writing memory still happens exclusively
//!   through the per-turn `memory_ops` contract.
//! * **No ids leak.** Hits are returned as store keys (`mem_{hex}`), `chunk:N` and
//!   `log:{rowid}`; raw sender ids and QQ message ids are never printed.
//!
//! The server speaks line-delimited JSON-RPC 2.0 over stdin/stdout, which is what the
//! MCP stdio transport is: no framing beyond one JSON object per line, and stdout must
//! carry protocol frames only — diagnostics go to stderr.

use serde_json::{Value, json};

use crate::store::SqliteStore;

/// MCP protocol revision this server answers with.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Tool name the model sees.
pub const TOOL_NAME: &str = "memory_search";

/// Default and maximum number of hits.
pub const DEFAULT_LIMIT: i64 = 5;
pub const MAX_LIMIT: i64 = 20;

/// The fixed scope read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub bot_id: String,
    pub group_id: String,
}

impl Scope {
    /// Parse `bot_id:group_id`. Both halves must be non-empty; a malformed scope is a
    /// hard error, because guessing a group would break isolation.
    pub fn parse(raw: &str) -> Option<Self> {
        let (bot, group) = raw.split_once(':')?;
        if bot.is_empty() || group.is_empty() {
            return None;
        }
        Some(Self {
            bot_id: bot.to_string(),
            group_id: group.to_string(),
        })
    }
}

/// One search hit, already detached from raw identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Store key for facts, `chunk:<seq>` for folds, `log:<rowid>` for unarchived lines.
    pub label: String,
    pub text: String,
}

/// The tool's JSON schema, advertised on `tools/list`.
pub fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Search this group's memory: reviewed facts, folded windows, and the \
                        unarchived recent log. Ranked by overlap, not newest first. Use it \
                        when you need a detail that is not in the current context.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look for. Plain words, not a command."
                },
                "limit": {
                    "type": "integer",
                    "description": "How many notes to return (1-20, default 5)."
                }
            },
            "required": ["query"]
        }
    })
}

/// Score a candidate against the query: character bigram overlap, so Chinese matches
/// without tokenisation and no extra dependency is needed.
pub fn relevance(query: &str, text: &str) -> f64 {
    let q = bigrams(query);
    if q.is_empty() {
        return 0.0;
    }
    let t = bigrams(text);
    if t.is_empty() {
        return 0.0;
    }
    let shared: f64 = q.iter().filter(|g| t.contains(g)).count() as f64;
    // Dice coefficient: symmetric, bounded, and independent of text length.
    2.0 * shared / (q.len() + t.len()) as f64
}

fn bigrams(text: &str) -> Vec<Bigram> {
    let chars: Vec<char> = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    if chars.len() == 1 {
        out.push(Bigram(chars[0], chars[0]));
        return out;
    }
    for pair in chars.windows(2) {
        out.push(Bigram(pair[0], pair[1]));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bigram(char, char);

/// Rank reviewed facts, folded chunks and unarchived log lines together.
/// Higher overlap first; on a tie the newer item wins.
pub fn rank<'a>(
    query: &str,
    memories: impl IntoIterator<Item = (&'a str, &'a str, i64)>,
    chunks: impl IntoIterator<Item = (i64, &'a str)>,
    logs: impl IntoIterator<Item = (i64, &'a str, i64)>,
    limit: i64,
) -> Vec<Hit> {
    let limit = limit.clamp(1, MAX_LIMIT) as usize;
    let mut scored: Vec<(f64, i64, Hit)> = Vec::new();
    for (memory_ref, content, updated_at) in memories {
        let score = relevance(query, content);
        if score > 0.0 {
            scored.push((
                score,
                updated_at,
                Hit {
                    label: memory_ref.to_string(),
                    text: content.to_string(),
                },
            ));
        }
    }
    for (seq, summary) in chunks {
        let score = relevance(query, summary);
        if score > 0.0 {
            scored.push((
                score,
                seq,
                Hit {
                    label: format!("chunk:{seq}"),
                    text: summary.to_string(),
                },
            ));
        }
    }
    for (rowid, text, created_at) in logs {
        let score = relevance(query, text);
        if score > 0.0 {
            scored.push((
                score,
                created_at,
                Hit {
                    label: format!("log:{rowid}"),
                    text: text.to_string(),
                },
            ));
        }
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.cmp(&a.1))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, hit)| hit)
        .collect()
}

/// Render hits the way the model should read them.
pub fn render_hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "没有找到相关记忆。".to_string();
    }
    let mut out = String::new();
    for hit in hits {
        out.push('[');
        out.push_str(&hit.label);
        out.push_str("] ");
        out.push_str(hit.text.trim());
        out.push('\n');
    }
    out
}

/// Answer one `tools/call` for `memory_search`.
pub async fn call_memory_search(store: &SqliteStore, scope: &Scope, args: &Value) -> Value {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_LIMIT);
    if query.is_empty() {
        return tool_error("query 不能为空");
    }
    let memories = match store
        .list_memories(&scope.bot_id, &scope.group_id, 200)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return tool_error("读取记忆失败"),
    };
    let chunks = match store
        .recent_chunks(&scope.bot_id, &scope.group_id, 200)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return tool_error("读取记忆块失败"),
    };
    let logs = match store
        .recent_unarchived_messages(&scope.bot_id, &scope.group_id, 100)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return tool_error("读取近期对话失败"),
    };
    let members = match store.list_members(&scope.bot_id, &scope.group_id).await {
        Ok(rows) => rows,
        Err(_) => return tool_error("读取成员失败"),
    };
    let alias_of = |sender_id: &str, speaker_role: &str| -> String {
        if speaker_role == "bot" {
            return "me".to_string();
        }
        members
            .iter()
            .find(|m| m.user_id == sender_id)
            .map(|m| m.speaker_ref.clone())
            .unwrap_or_else(|| "群".to_string())
    };
    let log_texts: Vec<(i64, String, i64)> = logs
        .iter()
        .map(|m| {
            let who = alias_of(&m.sender_id, &m.speaker_role);
            // The tagger's description lives in `image_tags`; `text` only ever holds the
            // `[图片]` placeholder. Prefer the description so a hit about a picture says
            // what the picture was instead of that something was there.
            let described = m
                .image_tags
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|tags| {
                    let lines: Vec<String> = tags
                        .as_array()?
                        .iter()
                        .filter_map(|t| t.get("text").and_then(Value::as_str))
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    (!lines.is_empty()).then(|| lines.join("；"))
                });
            let body = match described {
                Some(text) => format!("{text}（图）"),
                None if m.text.is_empty() && m.has_image => "[图]".to_string(),
                None => m.text.clone(),
            };
            (m.id, format!("[{who}] {body}"), m.created_at)
        })
        .collect();
    let hits = rank(
        &query,
        memories
            .iter()
            .map(|m| (m.memory_ref.as_str(), m.content.as_str(), m.updated_at)),
        chunks.iter().map(|c| (c.seq, c.summary.as_str())),
        log_texts
            .iter()
            .map(|(id, text, at)| (*id, text.as_str(), *at)),
        limit,
    );
    json!({
        "content": [{"type": "text", "text": render_hits(&hits)}],
        "isError": false
    })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true
    })
}

/// Handle one JSON-RPC message. Returns the response, or `None` for notifications
/// (which carry no `id` and must not be answered).
pub async fn handle(store: &SqliteStore, scope: &Scope, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = id?;
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "gemini-qq-memory", "version": env!("CARGO_PKG_VERSION")}
        })),
        "tools/list" => Ok(json!({"tools": [tool_definition()]})),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name == TOOL_NAME {
                let args = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": call_memory_search(store, scope, &args).await
                }));
            }
            Err((-32601, format!("unknown tool: {name}")))
        }
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("unknown method: {other}"))),
    };
    Some(match result {
        Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parse_requires_both_halves_and_keeps_them() {
        assert!(Scope::parse("2000000001:900000002").is_some());
        assert!(Scope::parse(":900000002").is_none());
        assert!(Scope::parse("2000000001:").is_none());
        assert!(Scope::parse("nocolon").is_none());
        let scope = Scope::parse("1:2").expect("scope");
        assert_eq!(scope.bot_id, "1");
        assert_eq!(scope.group_id, "2");
    }

    #[test]
    fn relevance_matches_chinese_without_tokenising() {
        assert!(relevance("明天开会", "他们说明天上午开会") > 0.0);
        assert_eq!(relevance("完全无关", "明天上午开会"), 0.0);
    }

    #[test]
    fn rank_prefers_the_better_match_and_labels_by_kind() {
        let hits = rank(
            "看电影",
            [("mem_1", "周末一起去看电影", 10)],
            [(3, "他们约好周末看电影")],
            [],
            5,
        );
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.label == "mem_1"));
        assert!(hits.iter().any(|h| h.label == "chunk:3"));
    }

    #[test]
    fn rank_includes_unarchived_log_hits() {
        let hits = rank("看电影", [], [], [(7, "[u1] 周末去看电影", 10)], 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label, "log:7");
        assert!(hits[0].text.contains("[u1]"));
        assert!(!hits[0].text.contains("message_id"));
    }

    #[test]
    fn rank_honours_the_limit_and_clamps_it() {
        let many: Vec<(i64, &str)> = (0..30).map(|i| (i, "开会讨论")).collect();
        let hits = rank("开会讨论", [], many, [], 3);
        assert_eq!(hits.len(), 3);
        let clamped = rank("开会讨论", [], (0..30).map(|i| (i, "开会讨论")), [], 999);
        assert_eq!(clamped.len(), MAX_LIMIT as usize);
    }

    #[test]
    fn empty_query_results_are_stated_plainly() {
        assert_eq!(render_hits(&[]), "没有找到相关记忆。");
    }

    #[test]
    fn hits_render_with_their_labels() {
        let hits = vec![Hit {
            label: "mem_7".into(),
            text: "主人喜欢在晚上聊天".into(),
        }];
        assert_eq!(render_hits(&hits), "[mem_7] 主人喜欢在晚上聊天\n");
    }

    #[tokio::test]
    async fn notifications_are_never_answered() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("m.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("open");
        let store = SqliteStore::new(pool);
        let scope = Scope::parse("1:2").expect("scope");
        let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle(&store, &scope, &notification).await.is_none());
    }

    #[tokio::test]
    async fn unknown_tools_and_methods_are_errors() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("m.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("open");
        let store = SqliteStore::new(pool);
        let scope = Scope::parse("1:2").expect("scope");
        let bad_tool = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "rm_rf", "arguments": {}}
        });
        let reply = handle(&store, &scope, &bad_tool).await.expect("响应");
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn initialize_advertises_the_tool_capability() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("m.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("open");
        let store = SqliteStore::new(pool);
        let scope = Scope::parse("1:2").expect("scope");
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let reply = handle(&store, &scope, &init).await.expect("响应");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(reply["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn search_reads_only_its_own_group() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("m.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("open");
        let store = SqliteStore::new(pool);
        store
            .ensure_group("1", "2", 1)
            .await
            .expect("create group 2");
        store
            .ensure_group("1", "3", 1)
            .await
            .expect("create group 3");
        for (group, content) in [("2", "这个群里讨论过看电影"), ("3", "别的群的秘密计划")]
        {
            store
                .insert_memory("1", group, None, content, "fact", "[]", 0.9, 1)
                .await
                .expect("写记忆");
        }
        let scope = Scope::parse("1:2").expect("scope");
        let args = json!({"query": "秘密计划"});
        let reply = call_memory_search(&store, &scope, &args).await;
        let text = reply["content"][0]["text"].as_str().unwrap_or_default();
        assert!(!text.contains("别的群"), "不得跨群泄漏，实际：{text}");
    }

    #[tokio::test]
    async fn search_never_returns_an_archived_memory() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("m.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("open");
        let store = SqliteStore::new(pool);
        store.ensure_group("1", "2", 1).await.expect("建群");
        let dead = store
            .insert_memory("1", "2", None, "周末去看电影", "fact", "[]", 0.9, 1)
            .await
            .expect("写记忆");
        store
            .insert_memory("1", "2", None, "周末去打羽毛球", "fact", "[]", 0.9, 2)
            .await
            .expect("写记忆");
        sqlx::query("UPDATE memories SET archived_at = ? WHERE memory_ref = ?")
            .bind(3_i64)
            .bind(&dead)
            .execute(&store.pool)
            .await
            .expect("归档");

        let scope = Scope::parse("1:2").expect("scope");
        let args = json!({"query": "周末去看电影"});
        let reply = call_memory_search(&store, &scope, &args).await;
        let text = reply["content"][0]["text"].as_str().unwrap_or_default();
        assert!(!text.contains("周末去看电影"), "归档行不得召回：{text}");
        assert!(text.contains("周末去打羽毛球"), "活跃行仍需可搜：{text}");
    }
}
