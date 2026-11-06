//! Session launch helpers: interactive argv and the queued-messages settings file.
//! Moved out of resident.rs verbatim (std-split-resident); logic unchanged.

use super::*;

/// Interactive argv for the TUI: no `--print` and no `--output-format` (the TUI
/// renders, it does not emit an envelope), and no prompt argument.
pub fn build_interactive_argv(config: &ResidentConfig) -> Vec<String> {
    let mut argv = vec![
        "agy".to_string(),
        "--add-dir".to_string(),
        config.workspace.display().to_string(),
        "--model".to_string(),
        config.model.clone(),
    ];
    // Not every model takes the flag; Agy refuses to start if it gets one anyway.
    if !config.effort.is_empty() {
        argv.push("--effort".to_string());
        argv.push(config.effort.clone());
    }
    if let Some(id) = &config.conversation_id {
        argv.push("--conversation".to_string());
        argv.push(id.clone());
    }
    // The TUI has no terminal-less prompt to auto-answer, but tool approvals would
    // otherwise block the session.
    argv.push("--dangerously-skip-permissions".to_string());
    argv
}

/// Force `queuedMessages: "send-immediately"` into a settings file.
///
/// Returns `Ok(true)` when the file had to change. The merge is minimal on purpose:
/// the CLI owns every other key and rewrites the file on each launch.
pub fn ensure_queued_messages(path: &Path) -> Result<bool, String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
    };
    let mut value: serde_json::Value = if existing.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(&existing).map_err(|e| format!("解析 {} 失败: {e}", path.display()))?
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("{} 不是 JSON 对象", path.display()))?;
    let unchanged = object
        .get(QUEUED_MESSAGES_KEY)
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == QUEUED_SEND_IMMEDIATELY);
    if unchanged {
        return Ok(false);
    }
    object.insert(
        QUEUED_MESSAGES_KEY.to_string(),
        serde_json::Value::String(QUEUED_SEND_IMMEDIATELY.to_string()),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败: {e}", parent.display()))?;
    }
    let rendered =
        serde_json::to_string_pretty(&value).map_err(|e| format!("序列化 settings 失败: {e}"))?;
    std::fs::write(path, rendered).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    Ok(true)
}

/// Whether the queue policy survived the CLI's own settings rewrite.
pub fn queued_messages_intact(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| {
            v.get(QUEUED_MESSAGES_KEY)
                .and_then(|x| x.as_str())
                .map(|x| x == QUEUED_SEND_IMMEDIATELY)
        })
        .unwrap_or(false)
}

/// The conversations directory for a given `$HOME`.
pub fn conversations_dir(home: &Path) -> PathBuf {
    home.join(CONVERSATIONS_RELATIVE_PATH)
}
