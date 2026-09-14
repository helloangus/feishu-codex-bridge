//! Shared flow helpers: delivery of text and panels, task completion and
//! protocol event folding, plus the bounded background-job policy.
use super::state::{BACKGROUND_LIMIT, CONTROL_RESERVE, Done, Active};
use crate::{
    Scheduler,
    events::{AgentEvent, TurnOutcome},
    messaging::{DeliveryError, MessageId, Messenger},
    ports::TurnRef,
    presentation::Request as DeliveryRequest,
};
use bridge_core::{view::Panel, ExecutionMode};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, timeout},
};

pub(crate) fn tell(
    tx: &mpsc::Sender<DeliveryRequest>,
    chat: &str,
    text: impl Into<String>,
) -> Result<(), String> {
    tx.try_send(DeliveryRequest::Text(chat.into(), text.into()))
        .map_err(|_| "回复队列已满或发送器已退出".into())
}

/// User-triggered spawns keep the control reserve free; control spawns may use
/// it. Beyond the hard limit nothing is spawned and the run stops instead.
pub(crate) fn can_spawn(jobs: &JoinSet<Done>, control: bool) -> bool {
    if control {
        jobs.len() < BACKGROUND_LIMIT
    } else {
        jobs.len() + CONTROL_RESERVE <= BACKGROUND_LIMIT
    }
}

/// Queue one interaction reply on the reserved control capacity. A full
/// reserve means replies could be lost silently; the run must stop instead.
pub(crate) fn spawn_reply(
    jobs: &mut JoinSet<Done>,
    pending: crate::interactions::Pending,
    allow: bool,
) -> Result<(), String> {
    if jobs.len() >= BACKGROUND_LIMIT {
        return Err("控制回传容量耗尽，停止运行".into());
    }
    jobs.spawn(async move {
        let outcome = crate::interactions::deliver_reply(pending, allow).await;
        Done::ApprovalReplied { outcome }
    });
    Ok(())
}

pub(crate) fn send_panel(
    source: Option<String>,
    jobs: &mut JoinSet<Done>,
    messenger: Arc<dyn Messenger>,
    owner: crate::cards::Owner,
    panel: Panel,
    commands: Vec<(String, String)>,
) -> bool {
    if !can_spawn(jobs, false) {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(600);
    jobs.spawn(async move {
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
        let result = match timeout(Duration::from_secs(45), async {
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
        if result.is_err()
            && !matches!(
                timeout(
                    Duration::from_secs(45),
                    messenger.send_text(owner.chat.clone(), fallback)
                )
                .await,
                Ok(Ok(()))
            )
        {
            eprintln!("{{\"event\":\"delivery_failed\"}}");
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
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<DeliveryRequest>,
    outcome: String,
) -> Result<(), String> {
    if let Some(active) = active.take() {
        crate::diagnostics::emit(
            crate::diagnostics::Event::TaskFinished,
            if outcome.starts_with("执行完成") {
                crate::diagnostics::Status::Ok
            } else {
                crate::diagnostics::Status::Failed
            },
            Some(&active.spec.id),
            active.output.len(),
        );
        if active.compact {
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
                task: active.spec.id,
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
            .map_err(|_| "回复队列已满".to_owned())?;
    }
    Ok(())
}

pub(crate) fn event(
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<DeliveryRequest>,
    event: AgentEvent,
) -> Result<Option<crate::plans::Offer>, String> {
    let offer = if matches!(
        &event,
        AgentEvent::Finished {
            outcome: TurnOutcome::Completed,
            ..
        }
    ) {
        active
            .as_ref()
            .filter(|active| !active.compact && !active.stopping && active.spec.mode == ExecutionMode::Plan)
            .and_then(|active| {
                let (text, truncated) = active.plan.as_ref()?;
                if *truncated || text.trim().is_empty() || text.len() > 16000 {
                    return None;
                }
                Some(crate::plans::Offer {
                    task: active.spec.clone(),
                    thread: active.turn.as_ref()?.thread_id.clone(),
                    text: text.clone(),
                    token: format!("plan-{}", active.spec.id),
                    sent: false,
                    deadline: Instant::now() + Duration::from_secs(600),
                })
            })
    } else {
        None
    };
    match event {
        AgentEvent::Plan { text, .. } => {
            if let Some(active) = active {
                let mut end = text.len().min(32 * 1024);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                active.plan = Some((text[..end].to_owned(), end < text.len()));
            }
        }
        AgentEvent::Output { delta, .. } => {
            if let Some(active) = active {
                let available = (32 * 1024_usize).saturating_sub(active.output.len());
                let mut end = delta.len().min(available);
                while !delta.is_char_boundary(end) {
                    end -= 1;
                }
                active.output.push_str(&delta[..end]);
                active.truncated |= end < delta.len();
            }
        }
        AgentEvent::Finished { outcome, .. } => {
            if let Some(current) = active.as_mut().filter(|active| active.compact) {
                let label = match outcome {
                    TurnOutcome::Completed => "上下文压缩完成。".into(),
                    TurnOutcome::Interrupted => "上下文压缩已停止。".into(),
                    TurnOutcome::Failed { message, .. } => format!(
                        "上下文压缩失败：{}",
                        message
                            .unwrap_or_else(|| "Codex 未返回原因".into())
                            .chars()
                            .take(1000)
                            .collect::<String>()
                    ),
                };
                if !current.compact_ack {
                    current.compact_outcome = Some(label);
                    return Ok(None);
                }
                return finish(active, scheduler, delivery, label).map(|_| None);
            }
            let label = match outcome {
                TurnOutcome::Completed => "执行完成".into(),
                TurnOutcome::Interrupted => "任务已停止".into(),
                TurnOutcome::Failed { message, .. } => format!(
                    "执行失败：{}",
                    message
                        .unwrap_or_else(|| "Codex 未返回原因".into())
                        .chars()
                        .take(1000)
                        .collect::<String>()
                ),
            };
            finish(active, scheduler, delivery, label)?;
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
) -> Result<(), String> {
    if jobs.len() >= BACKGROUND_LIMIT {
        return Err("控制容量耗尽，无法回传停止请求".into());
    }
    jobs.spawn(async move { Done::Control {
        result: backend.interrupt(turn).await,
    } });
    Ok(())
}
