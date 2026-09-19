//! Task lifecycle completions: plan implementation, durable admission,
//! preparation, turn start and backend control replies.
use super::super::RuntimeError;
use super::super::flow::{can_spawn, spawn_interrupt, tell};
use super::super::limits;
use super::super::state::{Done, Runtime, TaskDone};
use crate::execution::Execution;
use bridge_core::task::{TaskId, TaskSpec};
use tokio::task::JoinSet;

impl Runtime {
    pub(crate) async fn handle_task_done(
        &mut self,
        done: TaskDone,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match done {
            TaskDone::PlanAction {
                input,
                task,
                result,
            } => self.plan_action(input, task, result),
            TaskDone::Admission {
                ticket,
                input,
                result,
            } => self.admission(ticket, input, result),
            TaskDone::Prepared { id, result } => self.prepared(id, result, jobs).await,
            TaskDone::Started { id, result } => self.started(id, result, jobs).await,
            TaskDone::Control { result } => {
                result.map_err(|_| RuntimeError::Backend("Codex 控制请求失败，需重启连接"))?;
                Ok(())
            }
        }
    }

    /// A plan confirmation finished persisting. A successful "implement" or
    /// "fresh" choice enters the confirmed task into the scheduler directly.
    fn plan_action(
        &mut self,
        mut input: super::super::Input,
        task: Option<TaskSpec>,
        result: Result<bool, String>,
    ) -> Result<(), RuntimeError> {
        self.tasks.scheduler.end_session_mutation();
        input.ack.settle(true);
        match result {
            Ok(true) => {
                if let Some(task) = task {
                    let ticket = self
                        .tasks
                        .scheduler
                        .reserve(input.id, task)
                        .map_err(|_| RuntimeError::Interaction("计划实施入队失败"))?;
                    self.tasks.scheduler.commit_admission(ticket, true);
                    tell(
                        &self.delivery,
                        &input.chat,
                        "已关闭 Plan 模式，确认的计划已加入实施队列。",
                    )?;
                } else {
                    tell(
                        &self.delivery,
                        &input.chat,
                        "保持 Plan 模式，可继续发送消息讨论或修改计划。",
                    )?;
                }
            }
            Ok(false) => {
                tell(
                    &self.delivery,
                    &input.chat,
                    "此计划选择已处理，不会重复执行。",
                )?;
            }
            Err(error) => tell(
                &self.delivery,
                &input.chat,
                format!(
                    "计划选择处理失败：{error}。未启动实施，设置可能已部分保存；请检查 /plan 和当前会话后重新生成计划，不会自动重试。"
                ),
            )?,
        }
        Ok(())
    }

    /// The durable claim for a queued task finished. A failed claim releases
    /// the reservation; nothing was admitted and nothing is replayed.
    fn admission(
        &mut self,
        ticket: crate::AdmissionTicket,
        mut input: super::super::Input,
        result: Result<bool, ()>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(new) => {
                let queued = self.tasks.scheduler.commit_admission(ticket, new);
                input.ack.settle(true);
                if queued {
                    tell(&self.delivery, &input.chat, "请求已接收。")?;
                }
            }
            Err(_) => {
                self.tasks.scheduler.abort_admission(ticket);
                input.ack.settle(false);
                tell(
                    &self.delivery,
                    &input.chat,
                    "接收状态保存失败，未启动任务。",
                )?;
            }
        }
        Ok(())
    }

    /// File staging and session preparation finished; the task is ready to
    /// start a turn on the expected thread. The gate buffers early protocol
    /// events until the start request binds the turn identity.
    async fn prepared(
        &mut self,
        id: TaskId,
        result: Result<crate::ports::TurnInput, String>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        self.diagnostics.emit(
            crate::diagnostics::Event::TaskPrepared,
            if result.is_ok() {
                crate::diagnostics::Status::Ok
            } else {
                crate::diagnostics::Status::Failed
            },
            Some(id.as_str()),
            0,
        );
        if !can_spawn(jobs, false)
            && self
                .tasks
                .active
                .as_ref()
                .is_some_and(|active| active.spec.id == id)
        {
            super::super::flow::finish(
                &self.diagnostics,
                &mut self.tasks.active,
                &mut self.tasks.scheduler,
                &self.delivery,
                "系统繁忙，任务未启动；请稍后重试。".into(),
            )?;
            return Ok(());
        }
        let preparing = self
            .tasks
            .active
            .as_mut()
            .filter(|active| active.spec.id == id);
        if let Some(active) = preparing {
            if active.stopping {
                super::super::flow::finish(
                    &self.diagnostics,
                    &mut self.tasks.active,
                    &mut self.tasks.scheduler,
                    &self.delivery,
                    "准备阶段已停止，未启动任务".into(),
                )?;
                return Ok(());
            }
            match result {
                Ok(input) => {
                    active.spec.mode = input.mode;
                    active.gate = Some(Execution::starting(
                        self.settings.epoch,
                        input.thread_id.clone(),
                        limits::EARLY_PROTOCOL_EVENTS,
                    ));
                    let backend = self.backend.clone();
                    let id = active.spec.id.clone();
                    jobs.spawn(async move {
                        Done::Task(TaskDone::Started {
                            id,
                            result: backend.start_turn(input).await,
                        })
                    });
                }
                Err(error) => super::super::flow::finish(
                    &self.diagnostics,
                    &mut self.tasks.active,
                    &mut self.tasks.scheduler,
                    &self.delivery,
                    format!("准备失败：{error}"),
                )?,
            }
        }
        Ok(())
    }

    /// The start request answered. Binding the turn to the gate replays any
    /// buffered early events; an unknown start result stops the run because
    /// the turn may actually be executing.
    async fn started(
        &mut self,
        id: TaskId,
        result: Result<crate::ports::TurnRef, crate::ports::BackendError>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        self.diagnostics.emit(
            crate::diagnostics::Event::TaskStarted,
            if result.is_ok() {
                crate::diagnostics::Status::Ok
            } else {
                crate::diagnostics::Status::Failed
            },
            Some(id.as_str()),
            0,
        );
        let starting = self
            .tasks
            .active
            .as_mut()
            .filter(|active| active.spec.id == id);
        if let Some(active) = starting {
            match result {
                Ok(turn) => {
                    active.turn = Some(turn.clone());
                    let early = active
                        .gate
                        .as_mut()
                        .ok_or(RuntimeError::Internal("缺少执行状态"))?
                        .bind(turn.clone())
                        .map_err(|_| RuntimeError::Interaction("执行身份不匹配"))?;
                    if active.stopping {
                        spawn_interrupt(jobs, self.backend.clone(), turn.clone())?;
                    }
                    for item in early {
                        if let Some(offer) = super::super::flow::event(
                            &self.diagnostics,
                            &mut self.tasks.active,
                            &mut self.tasks.scheduler,
                            &self.delivery,
                            item,
                        )? {
                            self.cards.plan_offer = Some(offer);
                        }
                    }
                }
                Err(error) => {
                    super::super::flow::finish(
                        &self.diagnostics,
                        &mut self.tasks.active,
                        &mut self.tasks.scheduler,
                        &self.delivery,
                        format!("启动结果未知或失败：{error}；不会自动重试。"),
                    )?;
                    return Err(RuntimeError::Backend(
                        "Codex 启动失败，已停止运行以避免重复执行",
                    ));
                }
            }
        }
        Ok(())
    }
}
