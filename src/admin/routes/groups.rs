//! Admin panel group and group-state endpoints.
//! Moved out of routes.rs verbatim (std-split-routes); logic unchanged.

use super::*;

/// Group list with mood/topic and pending counts.
pub(super) async fn list_groups(State(state): State<Arc<AdminState>>) -> ApiResult {
    let groups = state
        .store
        .list_groups(&state.bot_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let mut out = Vec::with_capacity(groups.len());
    for group in groups {
        let group_state = state
            .store
            .get_state(&group.bot_id, &group.group_id)
            .await
            .ok()
            .flatten();
        let pending = state
            .store
            .pending_count(&group.bot_id, &group.group_id)
            .await
            .unwrap_or(0);
        let (memory_count, memory_chars) = state
            .store
            .memory_stats(&group.bot_id, &group.group_id)
            .await
            .unwrap_or((0, 0));
        let summary = state
            .store
            .get_summary(&group.bot_id, &group.group_id)
            .await
            .ok()
            .flatten();
        let mood = group_state
            .as_ref()
            .and_then(|s| s.mood_json.as_deref())
            .and_then(MoodState::from_json);
        let backoff = match &state.resident {
            Some(resident) => {
                resident
                    .backoff_status(&group.group_id, crate::now_unix())
                    .await
            }
            None => None,
        };
        out.push(json!({
            "group_id": group.group_id,
            "enabled": group.enabled,
            "paused": group.paused,
            // The one remaining per-group opt-in. The resident session has no switch
            // any more — it is the only execution path — so this flag is the only
            // resident knob the panel still reads, and it defaults to false.
            "resident_insert_enabled": group.resident_insert_enabled,
            // The ids are shown so an operator can correlate a group with the Agy
            // conversation, the gate session and the memory scope. They are handles,
            // not credentials, and this endpoint is already token-gated.
            "agy_conversation_id": group.agy_conversation_id,
            "has_conversation": group.agy_conversation_id.is_some(),
            // Rounds since the last session reset, so the panel can show how close the
            // group is to the hard cap instead of only saying "has a conversation".
            "agy_turn_count": group.agy_turn_count,
            "ambient_pace": group.ambient_pace,
            // Discovery metadata (stage 3b): name/member count fetched from
            // OneBot when the group was discovered; `departed_at` marks a
            // group the bot was removed from (data kept, never deleted).
            "name": group.name,
            "member_count": group.member_count,
            "departed_at": group.departed_at,
            // `null` means the cap is off (Agy compacts its own transcript), so the
            // panel renders "不限制" rather than a fake denominator.
            "conversation_turn_limit": crate::app::turn::turn_limit_value(
                &state.conversation_turn_limit
            ),
            // The gate scores how unfinished a burst looks; this is the longest it can
            // ever hold one back. Shown so an operator reading a slow reply can tell
            // "the gate was patient" from "the agent was slow".
            "cadence_hold_max_seconds": crate::app::preflight::HOLD_MAX_SECONDS,
            "cadence_context_messages": crate::app::preflight::PREFLIGHT_CONTEXT_MESSAGES,
            "preflight_session_id": crate::app::preflight::Preflight::session_id(
                &group.bot_id,
                &group.group_id,
            ),
            // The gate uses the base id plus a 24h bucket, so show what is actually in
            // use rather than the stable prefix alone.
            "aux_session_id": crate::integrations::aux_gateway::timestamped_session_id(
                &crate::app::preflight::Preflight::session_id(&group.bot_id, &group.group_id),
                crate::now_unix(),
            ),
            "memory_scope": format!("{}:{}", group.bot_id, group.group_id),
            "memory_revision": group.memory_revision,
            "mood": mood.map(|m| json!({"label": m.label, "reason": m.reason})),
            "topic_state": group_state.as_ref().and_then(|s| s.topic_state.clone()),
            "pending": pending,
            "memory_count": memory_count,
            "memory_chars": memory_chars,
            "summary_updated_at": summary.map(|s| s.updated_at),
            // In-memory respawn ladder. Null when the group is free to run; the
            // panel must not reconstruct this from the audit log (a restart
            // drops the map, so an old until in the log would be a lie).
            "backoff_until": backoff.map(|(_, until)| until),
            "backoff_tier": backoff.map(|(tier, _)| tier),
        }));
    }
    ok_json(json!({ "groups": out }))
}

#[derive(Debug, Deserialize)]
pub(super) struct GroupPatch {
    pub(super) enabled: Option<bool>,
    pub(super) paused: Option<bool>,
    /// P4 opt-in: let a running resident turn absorb newly claimed batches.
    ///
    /// The retired per-group resident switch is deliberately *not* here. The resident
    /// session is the only execution path, so a group cannot be switched off it; a
    /// PATCH body that still carries that retired key is ignored, which is how this
    /// file already treats unknown fields (serde drops them, and a body of only that
    /// key lands on `empty_patch`).
    pub(super) resident_insert_enabled: Option<bool>,
    /// Ambient pace tier: addressed_only | quiet | normal | lively.
    pub(super) ambient_pace: Option<String>,
}

impl GroupPatch {
    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.paused.is_none()
            && self.resident_insert_enabled.is_none()
            && self.ambient_pace.is_none()
    }
}

/// Toggle `enabled`/`paused`, or set the ambient pace tier. Nothing
/// else is writable.
pub(super) async fn patch_group(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let patch: GroupPatch =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    if patch.is_empty() {
        return Err(ApiError::bad_request("empty_patch"));
    }
    if let Some(raw) = &patch.ambient_pace
        && crate::store::AmbientPace::parse(raw).is_none()
    {
        return Err(ApiError::bad_request("invalid_ambient_pace"));
    }
    let now = crate::now_unix();
    let exists = state
        .store
        .set_group_flags(
            &state.bot_id,
            &group_id,
            patch.enabled,
            patch.paused,
            patch.resident_insert_enabled,
            now,
        )
        .await
        .map_err(|_| ApiError::internal())?;
    if !exists {
        return Err(ApiError::not_found("not_found"));
    }
    if let Some(raw) = &patch.ambient_pace {
        state
            .store
            .set_group_ambient_pace(&state.bot_id, &group_id, raw, now)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    // Pausing drops queued bodies; the design says a paused group must not accumulate
    // message content, and resuming only processes new messages.
    if patch.paused == Some(true) {
        let _ = state.store.drop_pending(&state.bot_id, &group_id).await;
    }
    // Unpausing is the operator's "resolved" signal, so it must also clear a
    // halted lane: a timeout-unknown result stops the lane in memory, and the
    // DB flag alone never reached it — the group looked live while its
    // batches queued silently forever (2026-10-28, group 900000003, whose
    // lane stayed halted through a session reset and an unpause). Dropped
    // jobs are not replayed; their rows return to the pending pool by event
    // id, so intake re-claims them as a fresh batch for the current session
    // instead of stranding them under a dead claim.
    if patch.paused == Some(false) {
        let dropped = state.scheduler.resume_lane(&group_id).await;
        if !dropped.is_empty() {
            let ids: Vec<i64> = dropped
                .iter()
                .flat_map(|job| job.event_ids.iter().copied())
                .collect();
            let _ = state.store.release_events(&ids).await;
            let _ = state
                .store
                .audit(
                    ActorKind::Panel.as_str(),
                    &format!(
                        "scheduler.lane.resumed.g{group_id}.dropped{}",
                        dropped.len()
                    ),
                    Some(&state.bot_id),
                    Some(&group_id),
                    now,
                )
                .await;
        }
    }
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "group.flags",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}

/// Current mood/topic for one group.
pub(super) async fn get_state(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let record = state
        .store
        .get_state(&state.bot_id, &group_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let mood = record
        .as_ref()
        .and_then(|s| s.mood_json.as_deref())
        .and_then(MoodState::from_json);
    ok_json(json!({
        "group_id": group_id,
        "mood": mood.map(|m| json!({"label": m.label, "reason": m.reason})),
        "topic_state": record.and_then(|s| s.topic_state),
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct StateUpdate {
    pub(super) topic_state: Option<String>,
    pub(super) mood_label: Option<String>,
    pub(super) mood_reason: Option<String>,
}

impl StateUpdate {
    /// Accept a bare string for `topic_state` as well as an object.
    pub(super) fn from_value(value: Value) -> Result<Self, ApiError> {
        if let Some(s) = value.as_str() {
            return Ok(Self {
                topic_state: Some(s.to_string()),
                mood_label: None,
                mood_reason: None,
            });
        }
        serde_json::from_value(value).map_err(|_| ApiError::bad_request("invalid_body"))
    }
}

/// Operator edit of mood/topic.
pub(super) async fn put_state(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let update = StateUpdate::from_value(body)?;
    let now = crate::now_unix();

    let existing = state
        .store
        .get_state(&state.bot_id, &group_id)
        .await
        .map_err(|_| ApiError::internal())?;

    let mood_json = match update.mood_label.as_deref() {
        Some(label) => {
            let mood = MoodState::new(label, update.mood_reason.as_deref().unwrap_or(""), now)
                .ok_or_else(|| ApiError::bad_request("invalid_mood"))?;
            Some(mood.to_json())
        }
        // Keep the previous mood when the request does not mention one.
        None => existing.as_ref().and_then(|s| s.mood_json.clone()),
    };
    let topic_state = match update.topic_state.as_deref() {
        Some(raw) => {
            let topic =
                TopicState::new(raw).ok_or_else(|| ApiError::bad_request("invalid_topic"))?;
            Some(topic.0)
        }
        None => existing.as_ref().and_then(|s| s.topic_state.clone()),
    };

    state
        .store
        .put_state(
            &state.bot_id,
            &group_id,
            mood_json.as_deref(),
            topic_state.as_deref(),
            now,
        )
        .await
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "group.state",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}
