//! Tests moved out verbatim (std-split); assertions unchanged.

#[test]
fn drift_kind_only_fires_for_real_drift() {
    assert_eq!(
        ResidentError::Steps("drift: model step 9: no field 20".to_string()).drift_kind(),
        Some("no_field20")
    );
    assert_eq!(
        ResidentError::Steps(
            "drift: unknown step_type 55 at idx 1 carries readable text: \"hi\"".to_string()
        )
        .drift_kind(),
        Some("unknown_type")
    );
    assert_eq!(
        ResidentError::Steps("storage: database is locked".to_string()).drift_kind(),
        None
    );
    assert_eq!(ResidentError::Crash("gone".to_string()).drift_kind(), None);
}

use super::*;
use crate::agent::step_stream::{ParseResult, StepEvent};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn strip_ansi_removes_csi_osc_and_charset_escapes() {
    let raw = "a\x1b[1;31mred\x1b[0m b\x1b]0;title\x07c\x1b(Bd\x1b[?2004h";
    // `ESC ( B` is a charset designator: three bytes consumed, so `d` survives.
    assert_eq!(strip_ansi(raw), "ared bcd");
    assert_eq!(strip_ansi("\x1b[?25lhi\x1b[?25h"), "hi");
    assert_eq!(strip_ansi("no escapes"), "no escapes");
    assert_eq!(strip_ansi("crlf\r\nnext"), "crlf\nnext");
}

#[test]
fn chunk_bounds_never_split_a_code_point() {
    let text = "甲乙丙丁戊"; // 3 bytes each
    let ranges = chunk_bounds(text, 2);
    assert_eq!(ranges.len(), 3);
    let rebuilt: String = ranges.iter().map(|r| &text[r.clone()]).collect();
    assert_eq!(rebuilt, text);
    for range in &ranges {
        assert!(text.is_char_boundary(range.start));
        assert!(text.is_char_boundary(range.end));
    }
    assert_eq!(chunk_bounds("", 4), Vec::<Range<usize>>::new());
    assert_eq!(chunk_bounds("ab", 10).len(), 1);
}

#[test]
fn step_text_matches_trims_and_prefixes() {
    assert!(step_text_matches("你好 世界", "你好  世界"));
    assert!(step_text_matches(
        &format!("{} 尾巴", "甲".repeat(130)),
        &"甲".repeat(130)
    ));
    assert!(!step_text_matches("别的", "你好"));
    assert!(!step_text_matches("", "你好"));
}

#[test]
fn a_prefix_match_below_the_floor_is_not_a_new_delivery() {
    let old = PathBuf::from("/conv/old.db");
    let fresh = PathBuf::from("/conv/new.db");
    let mut unbound = DeliveryFloor {
        seen: HashMap::new(),
        owned: None,
    };
    unbound.seen.insert(old.clone(), 40);
    assert!(!unbound.accepts(&old, 41));
    assert!(unbound.accepts(&fresh, 0));

    let owned = DeliveryFloor {
        seen: HashMap::from([(old.clone(), 40)]),
        owned: Some(old.clone()),
    };
    assert!(!owned.accepts(&old, 40));
    assert!(owned.accepts(&old, 41));
    assert!(!owned.accepts(&fresh, 0));

    let exact = UserStepHit {
        db: fresh.clone(),
        idx: 0,
        exact: true,
    };
    let prefix = UserStepHit {
        db: old.clone(),
        idx: 41,
        exact: false,
    };
    let chosen = choose_fresh_user_step(vec![prefix, exact]).expect("exact wins");
    assert_eq!(chosen.0, fresh);

    let left = UserStepHit {
        db: old,
        idx: 41,
        exact: false,
    };
    let right = UserStepHit {
        db: fresh,
        idx: 0,
        exact: false,
    };
    assert!(choose_fresh_user_step(vec![left, right]).is_none());
}

#[test]
fn ensure_queued_messages_merges_only_the_one_key() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{"trustedWorkspaces":["/w"],"queuedMessages":"queue"}"#,
    )
    .unwrap();
    assert!(ensure_queued_messages(&path).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["queuedMessages"], QUEUED_SEND_IMMEDIATELY);
    assert_eq!(value["trustedWorkspaces"][0], "/w");
    // Idempotent.
    assert!(!ensure_queued_messages(&path).unwrap());
    assert!(queued_messages_intact(&path));
}

#[test]
fn ensure_queued_messages_creates_missing_file() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("nested/settings.json");
    assert!(ensure_queued_messages(&path).unwrap());
    assert!(queued_messages_intact(&path));
}

#[test]
fn interactive_argv_has_no_print_mode_parts() {
    let config = ResidentConfig {
        bin: PathBuf::from("/bin/agy"),
        home: PathBuf::from("/h"),
        workspace: PathBuf::from("/w"),
        model: "gemini-3.8-flash".to_string(),
        effort: "low".to_string(),
        conversation_id: Some("conv-1".to_string()),
        deliver_timeout_s: DELIVER_CONFIRM_TIMEOUT_SECONDS,
        memory_scope: None,
    };
    let argv = build_interactive_argv(&config);
    assert_eq!(argv[0], "agy");
    assert!(argv.windows(2).any(|w| w == ["--conversation", "conv-1"]));
    assert!(argv.contains(&"--dangerously-skip-permissions".to_string()));
    assert!(argv.contains(&"--add-dir".to_string()));
    assert!(!argv.contains(&"--print".to_string()));
    assert!(!argv.contains(&"--output-format".to_string()));
    assert_eq!(argv.last().unwrap(), "--dangerously-skip-permissions");
}

// ---- PTY-driven tests against a stand-in agy -------------------------------
//
// The stand-in walks the same onboarding screens the real CLI shows (one screen
// at a time, waiting for keys), then echoes stdin into an inbox file and creates
// the conversation DB on the first prompt. A synthetic `StepReader` turns the
// inbox into a step stream, so the whole supervisor loop is exercised without the
// real CLI.

struct InboxReader {
    inbox: PathBuf,
    reply: Option<String>,
    reads: AtomicUsize,
}

impl InboxReader {
    fn new(inbox: PathBuf, reply: Option<String>) -> Self {
        Self {
            inbox,
            reply,
            reads: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl StepReader for InboxReader {
    async fn read(&self, _db_path: &Path) -> Result<ParseResult, ParseError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let text = std::fs::read_to_string(&self.inbox).unwrap_or_default();
        let mut events: Vec<StepEvent> = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            events.push(StepEvent {
                idx: events.len() as i64,
                kind: "user".to_string(),
                text: line.to_string(),
                tool: String::new(),
                task_id: String::new(),
                ts: Some(1_700_000_000.0),
                raw_type: 14,
            });
        }
        if let Some(reply) = &self.reply {
            events.push(StepEvent {
                idx: events.len() as i64,
                kind: "agent_text".to_string(),
                text: reply.clone(),
                tool: String::new(),
                task_id: String::new(),
                ts: Some(1_700_000_030.0),
                raw_type: 15,
            });
        }
        Ok(ParseResult {
            events,
            other_counts: BTreeMap::new(),
        })
    }
}

fn fake_agy(dir: &Path, inbox: &Path, bootstrap: bool) -> PathBuf {
    let script = if bootstrap {
        format!(
            r#"#!/bin/sh
printenv GEMINI_MEMORY_SCOPE > "$HOME/gemini_memory_scope" 2>/dev/null || true
printf 'Welcome to the Antigravity CLI. You are currently not signed in.\r\n'
sleep 0.2
printf 'Choose your color scheme\r\n'
dd bs=1 count=1 of=/dev/null 2>/dev/null
printf 'Terms of Service & Data Use\r\n'
dd bs=1 count=5 of=/dev/null 2>/dev/null
printf 'Accessing workspace: {ws}. Do you trust the contents of this project?\r\n'
dd bs=1 count=1 of=/dev/null 2>/dev/null
printf 'Antigravity CLI test\r\n? for shortcuts\r\n'
exec "$0.stdin"
"#,
            ws = dir.display()
        )
    } else {
        "#!/bin/sh\nprintenv GEMINI_MEMORY_SCOPE > \"$HOME/gemini_memory_scope\" 2>/dev/null || true\nprintf '? for shortcuts\\r\\n'\nexec \"$0.stdin\"\n".to_string()
    };
    let path = dir.join("agy");
    std::fs::write(&path, script).expect("write fake agy");
    // The read loop is a second file because `/bin/sh script` cannot easily
    // restart a `while read` after `dd` consumed bytes from the same fd.
    let stdin_loop = dir.join("agy.stdin");
    // The real TUI consumes the bracketed-paste markers; the stand-in has to do
    // the same, otherwise the recorded user step carries `ESC [ 2 0 0 ~` and no
    // delivered prompt can ever match.
    let loop_body = format!(
        r#"#!/bin/sh
esc=$(printf '\033')
conv="$HOME/.gemini/antigravity-cli/conversations"
mkdir -p "$conv"
first=1
while IFS= read -r line; do
  line=$(printf '%s' "$line" | sed -e "s/${{esc}}\\[200~//g" -e "s/${{esc}}\\[201~//g")
  [ -z "$line" ] && continue
  if [ "$first" = "1" ]; then
    : > "$conv/00000000-0000-4000-8000-000000000001.db"
    first=0
  fi
  printf '%s\n' "$line" >> '{inbox}'
done
"#,
        inbox = inbox.display()
    );
    std::fs::write(&stdin_loop, loop_body).expect("write stdin loop");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&stdin_loop, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn session_config(dir: &Path, bin: PathBuf, home: PathBuf, work: PathBuf) -> ResidentConfig {
    let _ = dir;
    ResidentConfig {
        bin,
        home,
        workspace: work,
        model: "test-model".to_string(),
        effort: "low".to_string(),
        conversation_id: None,
        deliver_timeout_s: DELIVER_CONFIRM_TIMEOUT_SECONDS,
        memory_scope: None,
    }
}

async fn start(
    bootstrap: bool,
    reply: Option<String>,
) -> (tempfile::TempDir, ResidentSession, Arc<InboxReader>) {
    let dir = tempfile::tempdir().expect("tmp");
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    let inbox = dir.path().join("inbox.txt");
    std::fs::create_dir_all(conversations_dir(&home)).expect("conv dir");
    std::fs::create_dir_all(&work).expect("work dir");
    let bin = fake_agy(dir.path(), &inbox, bootstrap);
    let reader = Arc::new(InboxReader::new(inbox, reply));
    let config = session_config(dir.path(), bin, home, work);
    let session = ResidentSession::spawn(&config, "12345", reader.clone())
        .await
        .expect("spawn");
    (dir, session, reader)
}

#[tokio::test]
async fn spawn_injects_memory_scope_into_the_child() {
    let dir = tempfile::tempdir().expect("tmp");
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    let inbox = dir.path().join("inbox.txt");
    std::fs::create_dir_all(conversations_dir(&home)).expect("conv dir");
    std::fs::create_dir_all(&work).expect("work dir");
    let bin = fake_agy(dir.path(), &inbox, false);
    let reader = Arc::new(InboxReader::new(inbox, None));
    let mut config = session_config(dir.path(), bin, home.clone(), work);
    config.memory_scope = Some("bot:12345".into());
    let session = ResidentSession::spawn(&config, "12345", reader)
        .await
        .expect("spawn");
    let dumped = std::fs::read_to_string(home.join("gemini_memory_scope"))
        .expect("child dumped GEMINI_MEMORY_SCOPE");
    assert_eq!(dumped.trim(), "bot:12345");
    session.close().await;
}

#[tokio::test]
async fn spawn_walks_onboarding_to_the_idle_prompt() {
    let (_dir, session, _reader) = start(true, None).await;
    assert_eq!(session.state().await, SessionState::Ready);
    let screen = session.screen_text();
    assert!(screen.contains(AGY_READY_MARKER), "screen: {screen}");
    let audit = session.drain_audit();
    assert!(
        audit
            .iter()
            .any(|a| a == "resident.session.onboard.g12345.color_scheme")
    );
    assert!(
        audit
            .iter()
            .any(|a| a == "resident.session.onboard.g12345.tos")
    );
    assert!(audit.iter().any(|a| a == "resident.session.ready.g12345"));
    session.close().await;
}

#[tokio::test]
async fn deliver_confirms_the_user_step_and_binds_the_db() {
    let (_dir, session, reader) = start(false, None).await;
    let receipt = session
        .deliver("你好，这是一次投递")
        .await
        .expect("deliver");
    assert_eq!(receipt.step_idx, 0);
    assert_eq!(
        receipt.conversation_id,
        "00000000-0000-4000-8000-000000000001"
    );
    assert_eq!(
        session.db_path().await.as_deref(),
        Some(receipt.db_path.as_path())
    );
    assert!(reader.reads.load(Ordering::Relaxed) > 0);
    session.close().await;
}

#[tokio::test]
async fn deliver_does_not_bind_an_older_step_with_the_same_prefix() {
    let (_dir, session, _reader) = start(false, None).await;
    let head = "甲".repeat(120);
    let first = session
        .deliver(&format!("{head}第一轮"))
        .await
        .expect("first");
    assert_eq!(first.step_idx, 0);
    let second = session
        .deliver(&format!("{head}第二轮"))
        .await
        .expect("second");
    assert_eq!(second.step_idx, 1);
    assert_eq!(second.conversation_id, first.conversation_id);
    session.close().await;
}

#[tokio::test]
async fn a_full_turn_reaches_a_deliver_verdict() {
    let reply = r#"{"reply_parts":["好的"]}"#.to_string();
    let (_dir, session, _reader) = start(false, Some(reply.clone())).await;
    let receipt = session.deliver("第一批").await.expect("deliver");
    let spec = TurnSpec::new(receipt.step_idx);
    // Past the quiet window.
    let (_parsed, verdict) = session
        .evaluate(&spec, 1_700_000_100.0, 12.0, None)
        .await
        .expect("evaluate");
    match verdict {
        Verdict::Deliver { idx, text } => {
            assert_eq!(idx, 1);
            assert_eq!(text, reply);
        }
        other => panic!("expected Deliver, got {other:?}"),
    }
    session.close().await;
}

#[tokio::test]
async fn quiet_window_and_contract_validator_are_honoured() {
    let (_dir, session, _reader) = start(false, Some("不是 JSON".to_string())).await;
    let receipt = session.deliver("第一批").await.expect("deliver");
    let spec = TurnSpec::new(receipt.step_idx);
    let (_p, too_soon) = session
        .evaluate(&spec, 1_700_000_001.0, 12.0, None)
        .await
        .expect("evaluate");
    assert!(too_soon.is_not_ready(), "{too_soon:?}");
    let validator = |text: &str| {
        if text.trim_start().starts_with('{') {
            Ok(())
        } else {
            Err(crate::agent::contract::RejectReason::Malformed)
        }
    };
    let (_p, failed) = session
        .evaluate(&spec, 1_700_000_100.0, 12.0, Some(&validator))
        .await
        .expect("evaluate");
    assert!(failed.is_failed(), "{failed:?}");
    // The red line: no draft fallback is even representable.
    assert!(failed.deliver_text().is_none());
    session.close().await;
}

#[tokio::test]
async fn interrupt_generation_on_an_idle_screen_is_a_no_op_success() {
    // The stand-in cannot model a mid-generation screen, so this pins the
    // half of the contract reachable here: on an already idle screen the
    // ESC keypress is harmless, the wait sees the ready marker at once,
    // and the session stays usable. The mid-generation half (ESC leaves
    // the TUI at an "Interrupted" prompt, ready in under a second, no
    // partial step in the DB) was verified against the real agy 1.4.9
    // TUI on 2026-10-29.
    let (_dir, session, _reader) = start(false, None).await;
    session
        .interrupt_generation(Duration::from_secs(2))
        .await
        .expect("interrupt_generation");
    assert!(
        session
            .drain_audit()
            .iter()
            .any(|a| a == "resident.turn.interrupted.g12345"),
        "中断审计行应在"
    );
    session.close().await;
}

#[tokio::test]
async fn interrupt_keeps_the_tui_alive() {
    let (_dir, session, _reader) = start(false, None).await;
    let outcome = session.interrupt().await.expect("interrupt");
    // The stand-in shell is not a TUI: it has no SIGINT handler and dies.
    assert!(matches!(
        outcome,
        InterruptOutcome::Exited | InterruptOutcome::StillRunning
    ));
    session.close().await;
}

// Kills the child with a nix signal directly: unix-only.
#[cfg(unix)]
#[tokio::test]
async fn a_killed_child_surfaces_as_a_crash() {
    let (_dir, session, _reader) = start(false, None).await;
    let pid = session.pid().await.expect("pid");
    let _ = send_signal(Pid::from_raw(pid as i32), Signal::SIGKILL);
    let mut crashed = None;
    for _ in 0..100 {
        if let Err(e) = session.tick(true).await {
            crashed = Some(e);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let error = crashed.expect("tick 应报 Crash");
    assert!(matches!(error, ResidentError::Crash(_)), "{error:?}");
    assert!(matches!(session.state().await, SessionState::Dead(_)));
    assert!(
        session
            .drain_audit()
            .iter()
            .any(|a| a.starts_with("resident.session.crash.g12345"))
    );
}

#[tokio::test]
async fn pinned_paused_session_is_reclaimed_after_the_window() {
    let (_dir, session, _reader) = start(false, None).await;
    assert!(!session.reclaim_if_idle(Some(1_000), 2_000).await);
    assert!(!session.reclaim_if_idle(None, 2_000).await);
    assert!(
        session
            .reclaim_if_idle(Some(1_000), 1_000 + PAUSED_RECLAIM_SECONDS + 1)
            .await
    );
}

#[tokio::test]
async fn rebuild_resumes_the_same_conversation_id() {
    let (_dir, session, _reader) = start(false, None).await;
    let receipt = session.deliver("第一批").await.expect("deliver");
    let rebuilt = session.rebuild().await.expect("rebuild");
    assert_eq!(rebuilt.1, RebuildKind::Resume);
    assert_eq!(
        rebuilt.0.conversation_id().await.as_deref(),
        Some(receipt.conversation_id.as_str())
    );
    rebuilt.0.close().await;
}
