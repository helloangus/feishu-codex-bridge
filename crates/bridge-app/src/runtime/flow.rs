//! Shared flow helpers: delivery of text and panels, task completion and
//! protocol event folding, plus the bounded background-job policy.
use super::RuntimeError;
use super::limits;
use super::state::{Active, ActiveKind, Done};
use crate::diagnostics::Diagnostics;
use crate::{
    Scheduler,
    events::{AgentEvent, TurnOutcome},
    messaging::{DeliveryError, MessageId, Messenger},
    ports::TurnRef,
    presentation::Request as DeliveryRequest,
};
use bridge_core::{ExecutionMode, view::Panel};
use std::sync::Arc;
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, timeout},
};

pub(crate) fn tell(
    tx: &mpsc::Sender<DeliveryRequest>,
    chat: &str,
    text: impl Into<String>,
) -> Result<(), RuntimeError> {
    tx.try_send(DeliveryRequest::Text(chat.into(), text.into()))
        .map_err(|_| RuntimeError::Capacity("回复队列已满或发送器已退出"))
}

/// User-triggered spawns keep the control reserve free; control spawns may use
/// it. Beyond the hard limit nothing is spawned and the run stops instead.
pub(crate) fn can_spawn(jobs: &JoinSet<Done>, control: bool) -> bool {
    if control {
        jobs.len() < limits::BACKGROUND_JOBS
    } else {
        jobs.len() + limits::CONTROL_JOB_RESERVE < limits::BACKGROUND_JOBS
    }
}

/// Queue one interaction reply on the reserved control capacity. A full
/// reserve means replies could be lost silently; the run must stop instead.
pub(crate) fn spawn_reply(
    diagnostics: &Diagnostics,
    jobs: &mut JoinSet<Done>,
    pending: crate::interactions::Pending,
    allow: bool,
) -> Result<(), RuntimeError> {
    if jobs.len() >= limits::BACKGROUND_JOBS {
        return Err(RuntimeError::Capacity("控制回传容量耗尽，停止运行"));
    }
    let handle = diagnostics.clone();
    jobs.spawn(async move {
        let outcome = crate::interactions::deliver_reply(&handle, pending, allow).await;
        Done::ApprovalReplied { outcome }
    });
    Ok(())
}

pub(crate) fn send_panel(
    diagnostics: &Diagnostics,
    source: Option<String>,
    jobs: &mut JoinSet<Done>,
    messenger: Arc<dyn Messenger>,
    owner: crate::cards::Owner,
    panel: Panel,
    commands: Vec<(crate::cards::CardToken, String)>,
) -> bool {
    if !can_spawn(jobs, false) {
        return false;
    }
    let deadline = Instant::now() + limits::INTERACTION_TIMEOUT;
    let handle = diagnostics.clone();
    jobs.spawn(async move {
        let diagnostics = &handle;
        let fallback = if commands
            .iter()
            .any(|(_, command)| command.starts_with("/plan-action "))
        {
            format!(
                "{}\n{}\n\n计划确认卡片发送失败，未开放实施；请继续讨论并重新生成计划。",
                panel.title, panel.body
            )
        } else {
            format!(
                "{}\n{}\n{}",
                panel.title,
                panel.body,
                commands
                    .iter()
                    .map(|(_, command)| command.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let result = match timeout(limits::MESSAGE_TIMEOUT, async {
            if let Some(source) = &source {
                let id = MessageId(source.clone());
                messenger.update_panel(id.clone(), panel.clone()).await?;
                Ok(id)
            } else {
                messenger
                    .send_panel(owner.chat.clone(), panel.clone())
                    .await
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(DeliveryError::Transport),
        };
        if result.is_err() {
            diagnostics.emit(
                crate::diagnostics::Event::CardFailed,
                crate::diagnostics::Status::Failed,
                None,
                0,
            );
        }
        if result.is_err()
            && !matches!(
                timeout(
                    limits::MESSAGE_TIMEOUT,
                    messenger.send_text(owner.chat.clone(), fallback)
                )
                .await,
                Ok(Ok(()))
            )
        {
            diagnostics.emit(
                crate::diagnostics::Event::DeliveryFailed,
                crate::diagnostics::Status::Failed,
                None,
                0,
            );
        }
        let entries = commands
            .into_iter()
            .map(|(token, command)| {
                let stop_snapshot = if command == "/stop" || command.starts_with("/plan-action ") {
                    Some(owner.stop_snapshot.clone())
                } else {
                    None
                };
                (
                    token,
                    crate::cards::Action {
                        generation: owner.generation,
                        user: owner.user.clone(),
                        chat: owner.chat.clone(),
                        directory: owner.directory.clone(),
                        source: String::new(),
                        deadline,
                        command,
                        stop_snapshot,
                    },
                )
            })
            .collect();
        Done::PanelSent {
            refreshed: source.is_some(),
            panel,
            entries,
            result,
        }
    });
    true
}

pub(crate) fn finish(
    diagnostics: &Diagnostics,
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<DeliveryRequest>,
    outcome: String,
) -> Result<(), RuntimeError> {
    if let Some(active) = active.take() {
        diagnostics.emit(
            crate::diagnostics::Event::TaskFinished,
            if outcome.starts_with("执行完成") {
                crate::diagnostics::Status::Ok
            } else {
                crate::diagnostics::Status::Failed
            },
            Some(active.spec.id.as_str()),
            active.output.len(),
        );
        if active.is_compact() {
            scheduler.end_session_mutation();
            return tell(delivery, &active.spec.chat, outcome);
        }
        scheduler.finish(&active.spec.id);
        let (text, truncated) = active
            .plan
            .as_ref()
            .map(|(text, truncated)| (text.as_str(), *truncated))
            .unwrap_or((&active.output, active.truncated));
        let output = if text.is_empty() {
            "（无文本输出）"
        } else {
            text
        };
        delivery
            .try_send(DeliveryRequest::Answer {
                task: active.spec.id.as_str().to_owned(),
                chat: active.spec.chat,
                text: format!(
                    "{outcome}\n\n{output}{}",
                    if truncated {
                        "\n\n输出超过 32 KiB 上限，已截断。"
                    } else {
                        ""
                    }
                ),
            })
            .map_err(|_| RuntimeError::Capacity("回复队列已满"))?;
    }
    Ok(())
}

pub(crate) fn event(
    diagnostics: &Diagnostics,
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<DeliveryRequest>,
    event: AgentEvent,
) -> Result<Option<crate::plans::Offer>, RuntimeError> {
    let offer = if matches!(
        &event,
        AgentEvent::Finished {
            outcome: TurnOutcome::Completed,
            ..
        }
    ) {
        active
            .as_ref()
            .filter(|active| {
                !active.is_compact() && !active.stopping && active.spec.mode == ExecutionMode::Plan
            })
            .and_then(|active| {
                let (text, truncated) = active.plan.as_ref()?;
                if *truncated || text.trim().is_empty() || text.len() > limits::PLAN_BYTES {
                    return None;
                }
                Some(crate::plans::Offer {
                    task: active.spec.clone(),
                    thread: active.turn.as_ref()?.thread_id.clone(),
                    text: text.clone(),
                    token: crate::cards::CardToken::new(format!("plan-{}", active.spec.id)),
                    sent: false,
                    deadline: Instant::now() + limits::INTERACTION_TIMEOUT,
                })
            })
    } else {
        None
    };
    match event {
        AgentEvent::Plan { text, .. } => {
            if let Some(active) = active {
                let mut end = text.len().min(limits::OUTPUT_BYTES);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                active.plan = Some((text[..end].to_owned(), end < text.len()));
            }
        }
        AgentEvent::Output { delta, .. } => {
            if let Some(active) = active {
                let available = limits::OUTPUT_BYTES.saturating_sub(active.output.len());
                let mut end = delta.len().min(available);
                while !delta.is_char_boundary(end) {
                    end -= 1;
                }
                active.output.push_str(&delta[..end]);
                active.truncated |= end < delta.len();
            }
        }
        AgentEvent::Finished { outcome, .. } => {
            if let Some(current) = active.as_mut().filter(|active| active.is_compact()) {
                let label = match outcome {
                    TurnOutcome::Completed => "上下文压缩完成。".into(),
                    TurnOutcome::Interrupted => "上下文压缩已停止。".into(),
                    TurnOutcome::Failed { message, .. } => format!(
                        "上下文压缩失败：{}",
                        message
                            .unwrap_or_else(|| "Codex 未返回原因".into())
                            .chars()
                            .take(limits::PREVIEW_CHARS)
                            .collect::<String>()
                    ),
                };
                if let ActiveKind::Compact {
                    acknowledged,
                    terminal,
                    ..
                } = &mut current.kind
                {
                    if !*acknowledged {
                        *terminal = Some(label);
                        return Ok(None);
                    }
                }
                return finish(diagnostics, active, scheduler, delivery, label).map(|_| None);
            }
            let label = match outcome {
                TurnOutcome::Completed => "执行完成".into(),
                TurnOutcome::Interrupted => "任务已停止".into(),
                TurnOutcome::Failed { message, .. } => format!(
                    "执行失败：{}",
                    message
                        .unwrap_or_else(|| "Codex 未返回原因".into())
                        .chars()
                        .take(limits::PREVIEW_CHARS)
                        .collect::<String>()
                ),
            };
            finish(diagnostics, active, scheduler, delivery, label)?;
        }
        _ => {}
    }
    Ok(offer)
}

/// Interrupt a turn on the reserved control capacity.
pub(crate) fn spawn_interrupt(
    jobs: &mut JoinSet<Done>,
    backend: Arc<dyn crate::ports::AgentBackend>,
    turn: TurnRef,
) -> Result<(), RuntimeError> {
    if jobs.len() >= limits::BACKGROUND_JOBS {
        return Err(RuntimeError::Capacity("控制容量耗尽，无法回传停止请求"));
    }
    jobs.spawn(async move {
        Done::Control {
            result: backend.interrupt(turn).await,
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_core::task::TaskSpec;
    use bridge_core::{ExecutionMode, SessionKey};
    use tokio::sync::mpsc;

    fn compact_active(turn: TurnRef) -> Active {
        Active {
            kind: ActiveKind::Compact {
                acknowledged: false,
                terminal: None,
                thread: Some(turn.thread_id.clone()),
            },
            spec: TaskSpec {
                id: bridge_core::task::TaskId::new(1, 1),
                session: SessionKey::new("user", "/project"),
                chat: "chat".into(),
                prompt: String::new(),
                model: None,
                mode: ExecutionMode::Execute,
            },
            gate: None,
            turn: Some(turn.clone()),
            stopping: false,
            output: String::new(),
            plan: None,
            truncated: false,
            started: Instant::now(),
        }
    }

    #[tokio::test]
    async fn slow_user_jobs_leave_the_control_reserve_available() {
        let mut jobs = JoinSet::new();
        for _ in 0..(limits::BACKGROUND_JOBS - limits::CONTROL_JOB_RESERVE) {
            jobs.spawn(std::future::pending::<Done>());
        }
        assert!(!can_spawn(&jobs, false));
        assert!(can_spawn(&jobs, true));

        for _ in 0..limits::CONTROL_JOB_RESERVE {
            assert!(can_spawn(&jobs, true));
            jobs.spawn(std::future::pending::<Done>());
        }
        assert!(!can_spawn(&jobs, false));
        assert!(!can_spawn(&jobs, true));

        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
    }

    #[test]
    fn compact_terminal_before_ack_keeps_mutation_gate_and_sends_no_success()
    -> Result<(), RuntimeError> {
        let mut scheduler = Scheduler::new(1);
        assert!(scheduler.begin_session_mutation());
        let (delivery, mut messages) = mpsc::channel(4);
        let turn = TurnRef {
            epoch: 1,
            thread_id: "thread".into(),
            turn_id: "compact".into(),
        };
        let mut active = Some(compact_active(turn.clone()));
        event(
            &Diagnostics::noop(),
            &mut active,
            &mut scheduler,
            &delivery,
            AgentEvent::Finished {
                turn,
                outcome: TurnOutcome::Completed,
            },
        )?;
        assert!(messages.try_recv().is_err());
        assert!(!scheduler.begin_session_mutation());
        let label = match active.as_mut().map(|active| &mut active.kind) {
            Some(ActiveKind::Compact { terminal, .. }) => terminal
                .take()
                .ok_or(RuntimeError::Internal("missing terminal"))?,
            _ => return Err(RuntimeError::Internal("missing compact state")),
        };
        finish(
            &Diagnostics::noop(),
            &mut active,
            &mut scheduler,
            &delivery,
            label,
        )?;
        assert!(active.is_none());
        assert!(scheduler.begin_session_mutation());
        assert!(matches!(
            messages.try_recv().map_err(|_| RuntimeError::Internal("closed"))?,
            DeliveryRequest::Text(_, text) if text == "上下文压缩完成。"
        ));
        Ok(())
    }
}
