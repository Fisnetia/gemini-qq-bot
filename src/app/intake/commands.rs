//! Owner commands for Intake: execution and acknowledgement delivery.
//! Moved out of intake.rs verbatim (std-split-intake); logic unchanged.

use super::*;

impl Intake {
    /// Execute an owner command and queue the acknowledgement.
    pub(super) async fn run_command(
        &self,
        group_id: &str,
        command: Command,
        group: &crate::store::GroupRecord,
    ) -> IntakeOutcome {
        let now = crate::now_unix();
        let reply = match command {
            Command::Status => format!(
                "gemini 状态：{}，{}。",
                if group.enabled {
                    "已启用"
                } else {
                    "已停用"
                },
                if group.paused {
                    "已暂停"
                } else {
                    "运行中"
                }
            ),
            Command::Pause => {
                let _ = self
                    .store
                    .set_group_flags(&self.bot_id, group_id, None, Some(true), None, now)
                    .await;
                // Pausing drops queued bodies: a paused group must not accumulate.
                let _ = self.store.drop_pending(&self.bot_id, group_id).await;
                "gemini 已暂停。".to_string()
            }
            Command::Resume => {
                // Resuming a group an operator has switched off would be a second,
                // hidden switch for the same thing; `enabled` is the only gate.
                let _ = self
                    .store
                    .set_group_flags(&self.bot_id, group_id, Some(true), Some(false), None, now)
                    .await;
                "gemini 已恢复。".to_string()
            }
        };
        let _ = self
            .store
            .audit(
                "owner",
                &format!(
                    "command.{}",
                    match command {
                        Command::Status => "status",
                        Command::Pause => "pause",
                        Command::Resume => "resume",
                    }
                ),
                Some(&self.bot_id),
                Some(group_id),
                now,
            )
            .await;
        IntakeOutcome::Command { reply }
    }

    /// Queue and deliver a command acknowledgement.
    pub async fn deliver_command_reply(
        &self,
        group_id: &str,
        text: &str,
        send: impl AsyncFn(&str) -> crate::onebot::action::SendOutcome,
    ) {
        let now = crate::now_unix();
        let id = match self
            .outbox
            .enqueue(&self.bot_id, group_id, "command", text, now)
            .await
        {
            Ok(id) => id,
            Err(_) => return,
        };
        if self.outbox.begin(&id, now).await.is_err() {
            return;
        }
        let outcome = send(text).await;
        let result = crate::pipeline::outbox::classify(outcome);
        // An unknown delivery stays in the outbox for an operator: it is never retried
        // automatically, because the action may have been executed.
        let _ = self.outbox.finish(&id, &result, crate::now_unix()).await;
    }
}
