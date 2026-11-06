//! PTY screen plumbing: reader thread, snapshots and audit sink.
//! Moved out of resident.rs verbatim (std-split-resident); logic unchanged.

use super::*;

pub(super) fn spawn_reader(mut reader: Box<dyn Read + Send>, screen: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut buffer) = screen.lock() {
                        buffer.extend_from_slice(&chunk[..n]);
                        let len = buffer.len();
                        if len > SCREEN_BUFFER_MAX_BYTES {
                            let drop_from = len - SCREEN_BUFFER_MAX_BYTES;
                            buffer.drain(..drop_from);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
}

pub(super) fn screen_snapshot(screen: &Arc<Mutex<Vec<u8>>>) -> String {
    match screen.lock() {
        Ok(buffer) => strip_ansi(&String::from_utf8_lossy(&buffer)),
        Err(_) => String::new(),
    }
}

pub(super) fn screen_tail(screen: &Arc<Mutex<Vec<u8>>>) -> String {
    let text = screen_snapshot(screen);
    let tail: String = text
        .chars()
        .rev()
        .take(240)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    sanitize(&tail)
}

pub(super) fn digest_of(screen: &str) -> String {
    let tail: String = screen.chars().rev().take(2_000).collect();
    tail
}

pub(super) fn progress_signal(events: &[StepEvent]) -> (usize, String) {
    let last = events
        .last()
        .map(|e| format!("{}:{}:{}", e.idx, e.kind, e.text.len()))
        .unwrap_or_default();
    (events.len(), last)
}

pub(super) fn sanitize(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    out = out.split_whitespace().collect::<Vec<_>>().join("_");
    if out.chars().count() > 80 {
        out = out.chars().take(80).collect();
    }
    if out.is_empty() {
        out = "empty".to_string();
    }
    out
}

pub(super) fn push_audit(audit: &Arc<Mutex<Vec<String>>>, action: String) {
    tracing::info!(target: "resident", action = %action, "resident audit");
    if let Ok(mut guard) = audit.lock() {
        guard.push(action);
    }
}
