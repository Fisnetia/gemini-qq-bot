//! Admin panel operations endpoints: compaction, session reset, outbox retry, pending overview.
//! Moved out of routes.rs verbatim (std-split-routes); logic unchanged.

use super::*;

/// Compaction job history for one group.
pub(super) async fn compaction_status(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let jobs = state
        .store
        .jobs_for_group(&state.bot_id, &group_id, 20)
        .await
        .map_err(|_| ApiError::internal())?;
    let (count, chars) = state
        .store
        .memory_stats(&state.bot_id, &group_id)
        .await
        .unwrap_or((0, 0));
    let aux = state.health.snapshot().await.aux;
    ok_json(json!({
        "group_id": group_id,
        "memory_count": count,
        "memory_chars": chars,
        "thresholds": {
            "max_memories": 50,
            "max_chars": 8000,
            "cooldown_seconds": 86_400,
            "daily_limit": 10,
        },
        "aux_ready": aux.as_ref().map(|w| w.ready).unwrap_or(false),
        "jobs": jobs
            .into_iter()
            .map(|j| json!({
                "id": j.id,
                "state": j.state,
                "source_revision": j.source_revision,
                "attempt_count": j.attempt_count,
                "requested_by": j.requested_by,
                "created_at": j.created_at,
                "finished_at": j.finished_at,
                "error_class": j.error_class,
            }))
            .collect::<Vec<_>>(),
    }))
}

/// Queue a compaction. Returns 202 with a `job_id` when accepted, 429 when the
/// cooldown/limit refuses, 409 when the compactor is not ready.
///
/// The body carries **no** model, prompt or content — only the group in the path.
pub(super) async fn start_compaction(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
    body: Option<Json<Value>>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    // A body is only tolerated if it is empty; anything else means a caller is trying to
    // pass a prompt or model name.
    if let Some(Json(value)) = body
        && !value.is_null()
        && value.as_object().map(|o| !o.is_empty()).unwrap_or(true)
    {
        return Err(ApiError::bad_request("body_not_allowed"));
    }
    let aux = state.health.snapshot().await.aux;
    if !aux.as_ref().map(|w| w.ready).unwrap_or(false) {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error": "compactor_not_ready"})),
        )
            .into_response());
    }
    let now = crate::now_unix();
    match state
        .compactor
        .maybe_queue(&state.bot_id, &group_id, ActorKind::Panel, now, true)
        .await
    {
        CompactionDecision::Queued(job) => {
            let _ = state
                .store
                .audit(
                    ActorKind::Panel.as_str(),
                    "memory.compaction",
                    Some(&state.bot_id),
                    Some(&group_id),
                    now,
                )
                .await;
            let aliases = state
                .store
                .list_members(&state.bot_id, &group_id)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|m| (m.user_id, m.speaker_ref))
                .collect::<Vec<_>>();
            let model = Arc::clone(&state.summary_model);
            let compactor = Arc::clone(&state.compactor);
            let job_for_run = job.clone();
            tokio::spawn(async move {
                let _ = compactor
                    .run_job(&job_for_run, model, aliases, crate::now_unix())
                    .await;
            });
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({ "job_id": job.id, "source_revision": job.source_revision })),
            )
                .into_response())
        }
        CompactionDecision::Skipped(reason) => Ok((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "skipped", "reason": reason.as_str() })),
        )
            .into_response()),
    }
}

/// Clear the stored Agy conversation id for one group.
pub(super) async fn reset_session(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let now = crate::now_unix();
    state
        .store
        .set_conversation_id(&state.bot_id, &group_id, None, now)
        .await
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "session.reset",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}

/// Operator-confirmed retry of an `unknown` outbox row.
pub(super) async fn retry_outbox(
    State(state): State<Arc<AdminState>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult {
    let now = crate::now_unix();
    match state.store.retry_unknown(&id, now).await {
        Ok(true) => {
            let _ = state
                .store
                .audit(ActorKind::Panel.as_str(), "outbox.retry", None, None, now)
                .await;
            ok_json(json!({ "ok": true }))
        }
        // A row that is not `unknown` must not be re-sent.
        Ok(false) => Err(ApiError::bad_request("not_unknown")),
        Err(_) => Err(ApiError::internal()),
    }
}

/// Pending and unknown counts, plus recent errors, for the panel.
pub(super) async fn pending_overview(State(state): State<Arc<AdminState>>) -> ApiResult {
    let pending = state
        .store
        .pending_total(&state.bot_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let outbox = state
        .store
        .outbox_counts(&state.bot_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let unknown = state
        .store
        .count_calls_with_error(&state.bot_id, "agent_result_unknown")
        .await
        .unwrap_or(0);
    let calls = state
        .store
        .recent_calls_all(&state.bot_id, 50)
        .await
        .unwrap_or_default();
    ok_json(json!({
        "pending_messages": pending,
        "unknown_calls": unknown,
        "outbox": outbox
            .into_iter()
            .map(|(s, c)| json!({"state": s, "count": c}))
            .collect::<Vec<_>>(),
        "calls": calls
            .into_iter()
            .filter(|c| panel_lists_call(&c.state, c.error_class.as_deref()))
            .map(|c| json!({
                "id": c.id,
                "group_id": c.group_id,
                "state": c.state,
                "error_class": c.error_class,
                "finished_at": c.finished_at,
            }))
            .collect::<Vec<_>>(),
    }))
}

/// The calls list hides ordinary successes. A deliberate non-reply is still
/// `succeeded`, with `error_class = silent`, and has to stay on the list or
/// the panel looks like the batch never happened.
pub(super) fn panel_lists_call(state: &str, error_class: Option<&str>) -> bool {
    state != "succeeded" || error_class == Some("silent")
}
