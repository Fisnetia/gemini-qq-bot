//! Prompt delivery for ResidentSession: paste, confirm the user step
//! landed, and locate the session-owned step DB. Moved out of
//! resident.rs verbatim (std-split-resident); logic unchanged.

use super::*;

impl ResidentSession {
    /// Deliver one prompt and confirm it landed as a `user` step.
    ///
    /// Two attempts, each waiting [`Self::deliver_timeout`] (15s by default).
    /// The main prompt uses this. An insert must not: a 30s confirm blocks the
    /// loop that is supposed to notice the answer already on screen.
    pub async fn deliver(&self, text: &str) -> Result<DeliverReceipt, ResidentError> {
        self.deliver_with(text, self.deliver_timeout, 2).await
    }

    /// Press ESC to stop the in-flight generation and wait for the idle
    /// prompt, keeping the session in service for an immediate follow-up
    /// paste. Not to be confused with `interrupt()` (SIGINT, used when the
    /// turn is being abandoned): this is the TUI's own key. Steps already
    /// completed stay in the conversation; only the unfinished generation
    /// is cut (agy 1.4.9, verified 2026-10-29: while a turn runs the TUI
    /// footer offers "esc to cancel", ESC leaves an "Interrupted" line and
    /// the ready marker returns in under a second; the cut turn leaves no
    /// partial step in the conversation DB).
    pub async fn interrupt_generation(&self, timeout: Duration) -> Result<(), ResidentError> {
        self.write_raw(b"\x1b").await?;
        let deadline = Instant::now() + timeout;
        loop {
            if self.screen_text().contains(AGY_READY_MARKER) {
                push_audit(
                    &self.audit,
                    format!("resident.turn.interrupted.g{}", self.group_id),
                );
                return Ok(());
            }
            if let Some(reason) = self.child_exit().await {
                return Err(ResidentError::Crash(reason));
            }
            if Instant::now() >= deadline {
                return Err(ResidentError::DeliverUnconfirmed(format!(
                    "中断后 {}ms 内未回到空闲界面",
                    timeout.as_millis()
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// One paste, one confirmation window. Used by inserts, which only run when
    /// the screen is already idle and must not stall the turn that is reading it.
    pub async fn deliver_once(
        &self,
        text: &str,
        timeout: Duration,
    ) -> Result<DeliverReceipt, ResidentError> {
        self.deliver_with(text, timeout, 1).await
    }

    /// Confirm a paste and record the delivery audit on success.
    async fn confirm_and_audit(
        &self,
        text: &str,
        timeout: Duration,
        floor: &DeliveryFloor,
    ) -> Result<DeliverReceipt, ResidentError> {
        let receipt = self.confirm_delivery(text, timeout, floor).await?;
        push_audit(
            &self.audit,
            format!(
                "resident.turn.delivered.g{}.step{}",
                self.group_id, receipt.step_idx
            ),
        );
        Ok(receipt)
    }

    /// One paste with a two-phase confirmation, for inserts. The first
    /// window matches [`Self::deliver_once`]; if no user step is visible
    /// when it closes, wait `recheck` and confirm once more against the
    /// SAME pre-paste floor before reporting failure. The step DB's
    /// visibility lag occasionally outlives the first window while the
    /// paste did land; the insert caller treats "unconfirmed" as "never
    /// pasted" and releases its rows, so the next poll pasted the same
    /// lines a second time and the model received the insert twice
    /// (2026-11-04, a production group). The second look costs one sleep
    /// and no extra paste.
    pub async fn deliver_insert(
        &self,
        text: &str,
        timeout: Duration,
        recheck: Duration,
    ) -> Result<DeliverReceipt, ResidentError> {
        if normalized(text).is_empty() {
            return Err(ResidentError::DeliverUnconfirmed("空投递".to_string()));
        }
        {
            let inner = self.inner.lock().await;
            if !inner.state.is_ready() {
                return Err(ResidentError::Blocked(format!(
                    "state={}",
                    inner.state.as_str()
                )));
            }
        }
        let floor = self.delivery_floor().await;
        self.write_prompt(text).await?;
        match self.confirm_and_audit(text, timeout, &floor).await {
            Ok(receipt) => Ok(receipt),
            Err(first) => {
                tokio::time::sleep(recheck).await;
                self.confirm_and_audit(text, timeout, &floor)
                    .await
                    .map_err(|_| first)
            }
        }
    }

    pub(super) async fn deliver_with(
        &self,
        text: &str,
        timeout: Duration,
        attempts: u32,
    ) -> Result<DeliverReceipt, ResidentError> {
        if normalized(text).is_empty() {
            return Err(ResidentError::DeliverUnconfirmed("空投递".to_string()));
        }
        {
            let inner = self.inner.lock().await;
            if !inner.state.is_ready() {
                return Err(ResidentError::Blocked(format!(
                    "state={}",
                    inner.state.as_str()
                )));
            }
        }
        let floor = self.delivery_floor().await;
        let mut last = String::new();
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            self.write_prompt(text).await?;
            match self.confirm_and_audit(text, timeout, &floor).await {
                Ok(receipt) => return Ok(receipt),
                Err(e) => {
                    last = e.to_string();
                    if attempt + 1 < attempts {
                        // agy sometimes swallows the first paste while still painting
                        // the previous frame; one resend, then give up rather than
                        // looping into a duplicate prompt.
                        push_audit(
                            &self.audit,
                            format!("resident.turn.retry.g{}.{}", self.group_id, sanitize(&last)),
                        );
                    }
                }
            }
        }
        Err(ResidentError::DeliverUnconfirmed(last))
    }

    /// Write the prompt as a single bracketed paste, chunked at char boundaries.
    pub(super) async fn write_prompt(&self, text: &str) -> Result<(), ResidentError> {
        self.write_raw(PASTE_OPEN).await?;
        for range in chunk_bounds(text, PASTE_CHUNK_CHARS) {
            self.write_raw(text[range].as_bytes()).await?;
            tokio::time::sleep(PASTE_CHUNK_GAP).await;
        }
        self.write_raw(PASTE_CLOSE).await?;
        tokio::time::sleep(PASTE_CHUNK_GAP).await;
        self.write_raw(b"\r").await
    }

    pub(super) async fn confirm_delivery(
        &self,
        text: &str,
        timeout: Duration,
        floor: &DeliveryFloor,
    ) -> Result<DeliverReceipt, ResidentError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((db_path, step_idx)) = self.find_user_step(text, floor).await? {
                let conversation_id = db_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut inner = self.inner.lock().await;
                inner.db_path = Some(db_path.clone());
                inner.conversation_id = Some(conversation_id.clone());
                drop(inner);
                return Ok(DeliverReceipt {
                    db_path,
                    conversation_id,
                    step_idx,
                });
            }
            if Instant::now() >= deadline {
                return Err(ResidentError::DeliverUnconfirmed(format!(
                    "{}ms 内未见匹配 user 步（{}字）",
                    timeout.as_millis(),
                    text.chars().count()
                )));
            }
            tokio::time::sleep(DELIVER_CONFIRM_POLL).await;
        }
    }

    /// Candidate conversation DBs, the known one first, then newest first.
    pub(super) async fn candidate_dbs(&self) -> Vec<PathBuf> {
        let known = self.inner.lock().await.db_path.clone();
        let mut out = Vec::new();
        if let Some(path) = known
            && path.exists()
        {
            out.push(path);
        }
        let dir = conversations_dir(&self.home);
        let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("db") {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                found.push((modified, path));
            }
        }
        found.sort_by_key(|a| std::cmp::Reverse(a.0));
        for (_, path) in found.into_iter().take(DB_SCAN_LIMIT) {
            if !out.contains(&path) {
                out.push(path);
            }
        }
        out
    }

    pub(super) async fn owned_db(&self) -> Option<PathBuf> {
        let inner = self.inner.lock().await;
        if let Some(path) = &inner.db_path {
            return Some(path.clone());
        }
        inner
            .conversation_id
            .as_ref()
            .map(|id| conversations_dir(&self.home).join(format!("{id}.db")))
    }

    /// Snapshot taken before the paste. Read errors still mark the file as
    /// pre-existing so a history we failed to see cannot pass as a new step.
    pub(super) async fn delivery_floor(&self) -> DeliveryFloor {
        let owned = self.owned_db().await;
        let mut seen = HashMap::new();
        for db in self.candidate_dbs().await {
            let max = match self.reader.read(&db).await {
                Ok(parsed) => parsed
                    .events
                    .iter()
                    .map(|event| event.idx)
                    .max()
                    .unwrap_or(-1),
                Err(_) => -1,
            };
            seen.insert(db, max);
        }
        if let Some(path) = &owned
            && path.exists()
            && !seen.contains_key(path)
        {
            let max = match self.reader.read(path).await {
                Ok(parsed) => parsed
                    .events
                    .iter()
                    .map(|event| event.idx)
                    .max()
                    .unwrap_or(-1),
                Err(_) => -1,
            };
            seen.insert(path.clone(), max);
        }
        DeliveryFloor { seen, owned }
    }

    pub(super) async fn find_user_step(
        &self,
        text: &str,
        floor: &DeliveryFloor,
    ) -> Result<Option<(PathBuf, i64)>, ResidentError> {
        let dbs = match &floor.owned {
            Some(owned) => vec![owned.clone()],
            None => self.candidate_dbs().await,
        };
        let mut hits = Vec::new();
        for db in dbs {
            let parsed = match self.reader.read(&db).await {
                Ok(parsed) => parsed,
                // A DB that was just created has no `steps` table yet.
                Err(_) => continue,
            };
            for step in parsed.events.iter().filter(|event| event.is_kind("user")) {
                if !floor.accepts(&db, step.idx) {
                    continue;
                }
                let Some(exact) = step_match_kind(&step.text, text) else {
                    continue;
                };
                hits.push(UserStepHit {
                    db: db.clone(),
                    idx: step.idx,
                    exact,
                });
            }
        }
        Ok(choose_fresh_user_step(hits))
    }
}
