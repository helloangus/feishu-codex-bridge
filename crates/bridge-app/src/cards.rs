//! Cards carry opaque, one-use actions bound to the message actually delivered.
use bridge_core::view::{Button, ButtonAction, ButtonStyle, Panel, Tone};
use std::{collections::BTreeMap, path::PathBuf};
use tokio::time::Instant;

pub struct Click {
    pub token: String,
    pub source: String,
}
pub struct Action {
    pub generation: u64,
    pub user: String,
    pub chat: String,
    pub directory: PathBuf,
    pub source: String,
    pub deadline: Instant,
    pub command: String,
    pub stop_snapshot: Option<(u64, Option<String>)>,
}
pub struct Owner {
    pub user: String,
    pub chat: String,
    pub directory: PathBuf,
    pub generation: u64,
    pub stop_snapshot: (u64, Option<String>),
}
#[derive(Default)]
pub struct Actions(BTreeMap<String, Action>);

/// Keep delivered views until all their actions have disappeared. Updates are
/// serialized by the runtime, so an older snapshot cannot restore a button.
#[derive(Default)]
pub struct Views(BTreeMap<String, Panel>);
impl Views {
    pub fn note(&mut self, source: &str, text: &str) {
        if let Some(panel) = self.0.get_mut(source) {
            panel.body.push_str(&format!("\n\n{text}"));
        }
    }
    pub fn is_list(&self, source: &str) -> bool {
        self.0.get(source).is_some_and(|panel| {
            matches!(panel.title.as_str(), "最近会话" | "已归档会话" | "可用模型")
        })
    }
    pub fn remove(&mut self, source: &str) {
        self.0.remove(source);
    }
    pub fn insert(&mut self, source: String, panel: Panel) {
        if !panel.buttons.is_empty() && self.0.len() < 1000 {
            self.0.insert(source, panel);
        }
    }

    pub fn next_update(
        &mut self,
        actions: &Actions,
        now: Instant,
        snapshot: &(u64, Option<String>),
    ) -> Option<(String, Panel)> {
        let mut update = None;
        for (source, panel) in &mut self.0 {
            let before = panel.buttons.len();
            panel.buttons.retain(|button| match &button.action {
                ButtonAction::Interaction { token, .. } => actions.0.get(token).is_some_and(|a| {
                    a.source == *source
                        && now < a.deadline
                        && a.stop_snapshot.as_ref().is_none_or(|s| s == snapshot)
                }),
                _ => false,
            });
            if before != panel.buttons.len() {
                let mut updated = panel.clone();
                updated.body.push_str(if panel.buttons.is_empty() {
                    "\n\n本卡片按钮均已使用或失效。请重新发送对应列表命令或 /help。"
                } else {
                    "\n\n已移除使用过或失效的按钮；操作结果请查看单独回复。"
                });
                update = Some((source.clone(), updated));
                break;
            }
        }
        if let Some((source, panel)) = &update {
            if panel.buttons.is_empty() {
                self.0.remove(source);
            }
        }
        update
    }
}
impl Actions {
    pub fn retain_plan(&mut self, token: Option<&str>) {
        self.0.retain(|_, action| {
            !action.command.starts_with("/plan-action ")
                || token.is_some_and(|token| {
                    action
                        .command
                        .starts_with(&format!("/plan-action {token} "))
                })
        });
    }
    pub fn invalidate_approval(&mut self, token: &str) {
        let allow = format!("/approve {token}");
        let deny = format!("/deny {token}");
        let choice = format!("/choice {token} ");
        self.0.retain(|_, action| {
            action.command != allow
                && action.command != deny
                && !action.command.starts_with(&choice)
        });
    }
    pub fn invalidate_source(&mut self, source: &str) {
        self.0.retain(|_, a| a.source != source);
    }
    pub fn insert(&mut self, entries: Vec<(String, Action)>, now: Instant) -> bool {
        self.0.retain(|_, a| now < a.deadline);
        if self.0.len() + entries.len() > 1000 {
            return false;
        }
        self.0.extend(entries);
        true
    }
    pub fn take(
        &mut self,
        click: &Click,
        user: &str,
        chat: &str,
        directory: &std::path::Path,
        now: Instant,
        stop_snapshot: &(u64, Option<String>),
    ) -> Option<String> {
        let entry = self.0.get(&click.token)?;
        if entry.user != user
            || entry.chat != chat
            || entry.directory != directory
            || entry.source != click.source
            || now >= entry.deadline
            || entry
                .stop_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot != stop_snapshot)
        {
            return None;
        }
        self.0.remove(&click.token).map(|a| a.command)
    }
    pub fn invalidate(&mut self, user: &str) {
        self.0.retain(|_, a| a.user != user);
    }
}

pub fn panel(
    title: &str,
    body: String,
    buttons: Vec<(String, String)>,
    prefix: &str,
) -> (Panel, Vec<(String, String)>) {
    let mut actions = Vec::new();
    let grouped = buttons.len() > 1;
    let buttons = buttons
        .into_iter()
        .enumerate()
        .map(|(index, (label, command))| {
            let token = format!("{prefix}-{index}");
            actions.push((token.clone(), command.clone()));
            Button {
                label,
                description: None,
                section: None,
                group: if !grouped || command == "/stop" {
                    None
                } else {
                    Some(
                        ["导航", "会话", "设置", "任务"]
                            .get(index / 2)
                            .unwrap_or(&"更多")
                            .to_string(),
                    )
                },
                separate: command == "/stop",
                style: if command == "/stop" {
                    ButtonStyle::Destructive
                } else {
                    ButtonStyle::Default
                },
                action: ButtonAction::Interaction {
                    token,
                    choice: "run".into(),
                },
            }
        })
        .collect();
    (
        Panel {
            title: title.into(),
            body,
            tone: Tone::Info,
            buttons,
        },
        actions,
    )
}

pub fn help(prefix: &str) -> (Panel, Vec<(String, String)>) {
    let commands = [
        ("状态 /status", "/status"),
        ("目录 /cd", "/cd"),
        ("恢复 /resume", "/resume"),
        ("新建 /new", "/new"),
        ("模型 /models", "/models"),
        ("Plan /plan", "/plan"),
        ("压缩 /compact", "/compact"),
        ("刷新 /help", "/help"),
        ("停止 /stop", "/stop"),
    ];
    panel("Codex 控制面板","发送文本开始任务。按钮限本人在当前聊天和目录使用，10 分钟内有效，每个按钮可执行一次；失效后重新发送 /help。\n/cd <路径> 切换目录，不存在时请求创建确认。\n/cd-confirm <编号> 确认创建。\n/archive <ID>、/archived、/unarchive <ID> 管理归档。\n/model [ID|default] 设置模型；/plan [on|off] 设置计划模式。\n目录和设置修改需全局空闲。".into(),commands.into_iter().map(|(label,command)|(label.into(),command.into())).collect(),prefix)
}

pub fn threads(
    entries: &[crate::sessions::ListedThread],
    archived: bool,
    prefix: &str,
) -> (Panel, Vec<(String, String)>) {
    let mut body =
        String::from("最多 8 项；按钮限本人在当前聊天和目录使用，10 分钟有效，每个按钮一次。\n");
    let mut buttons = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let title = if entry.title.is_empty() {
            "未命名会话"
        } else {
            &entry.title
        };
        let number = index + 1;
        body.push_str(&format!(
            "\n{number}. {title}{}\n{}\n",
            if entry.active { "（执行中）" } else { "" },
            entry.id
        ));
        let action = if archived { "/unarchive" } else { "/resume" };
        body.push_str(&format!("{action} {}\n", entry.id));
        buttons.push((
            format!(
                "{number}. {}",
                if archived {
                    "取消归档"
                } else {
                    "恢复会话"
                }
            ),
            format!("{action} {}", entry.id),
        ));
        if !archived {
            body.push_str(&format!("/archive {}\n", entry.id));
            buttons.push((
                format!("{number}. 归档会话"),
                format!("/archive {}", entry.id),
            ));
        }
    }
    if entries.is_empty() {
        body.push_str(if archived {
            "当前目录没有已归档会话。"
        } else {
            "当前目录没有可恢复的会话。"
        });
    }
    buttons.push((
        "刷新列表".into(),
        if archived { "/archived" } else { "/resume" }.into(),
    ));
    buttons.push((
        if archived {
            "查看最近会话"
        } else {
            "查看已归档会话"
        }
        .into(),
        if archived { "/resume" } else { "/archived" }.into(),
    ));
    let (mut card, commands) = panel(
        if archived {
            "已归档会话"
        } else {
            "最近会话"
        },
        body,
        buttons,
        prefix,
    );
    for button in &mut card.buttons {
        button.group = None;
    }
    (card, commands)
}

pub fn models(
    models: &[crate::ports::Model],
    current: Option<&str>,
    prefix: &str,
) -> (Panel, Vec<(String, String)>) {
    let mut buttons = models
        .iter()
        .take(20)
        .filter_map(|model| {
            if model.id.is_empty() || model.id.len() > 256 || model.id.chars().any(char::is_control)
            {
                return None;
            }
            let suffix = if current == Some(model.id.as_str()) {
                "（已选择）"
            } else if model.is_default {
                "（默认）"
            } else {
                ""
            };
            Some((
                format!("{}{}", model.id, suffix),
                format!("/model {}", model.id),
            ))
        })
        .collect::<Vec<_>>();
    buttons.push(("恢复 Codex 默认模型".into(), "/model default".into()));
    buttons.push(("刷新模型列表".into(), "/models".into()));
    let (mut card, commands) = panel(
        "可用模型",
        format!(
            "当前模型：{}。全局空闲时可修改，保存成功后回复确认。\n按钮限本人在当前聊天和目录使用，10 分钟有效，每个按钮一次。",
            current.unwrap_or("Codex 默认")
        ),
        buttons,
        prefix,
    );
    for button in &mut card.buttons {
        button.group = None;
    }
    (card, commands)
}

/// One question per card; opaque commands carry indices, never answer text.
pub fn question_supported(question: &crate::requests::Question) -> bool {
    question.options.len() <= 20
        && !question.text.trim().is_empty()
        && (question.secret || question.options.iter().all(|o| !o.label.trim().is_empty()))
        && question.text.len()
            + question.header.len()
            + question
                .options
                .iter()
                .map(|o| o.label.len() + o.description.len())
                .sum::<usize>()
            <= 16000
}

pub fn question(
    question: &crate::requests::Question,
    index: usize,
    total: usize,
    token: &str,
    prefix: &str,
) -> (Panel, Vec<(String, String)>) {
    let supported = question_supported(question);
    let mut body = format!(
        "第 {} / {} 题。请选择或自行回答。整组问答 10 分钟有效；超时停止本次桥接运行，不提交空答案。\n",
        index + 1,
        total
    );
    let mut buttons = Vec::new();
    if supported {
        body.push_str(&format!("{}\n{}\n", question.header, question.text));
        for (option_index, option) in question
            .options
            .iter()
            .enumerate()
            .filter(|_| !question.secret)
        {
            body.push_str(&format!(
                "\n{}. {}\n{}\n",
                option_index + 1,
                option.label,
                option.description
            ));
            buttons.push((
                format!("选择 {}", option_index + 1),
                format!("/choice {token} {index} {option_index}"),
            ));
        }
        if question.other || question.options.is_empty() || question.secret {
            body.push_str("\n点击自行回答后，按提示发送专用答案命令。答案不在确认回复中回显；飞书聊天仍保留输入。\n");
            buttons.push((
                "其他／自行回答".into(),
                format!("/choice {token} {index} other"),
            ));
        }
    } else {
        body.push_str("本题详情超过展示上限，无法完整展示，已停止问答。\n");
    }
    let (mut card, commands) = panel("Codex 问答", body, buttons, prefix);
    for button in &mut card.buttons {
        button.group = None;
    }
    (card, commands)
}

/// Approval details must remain complete before an allow action is offered.
pub fn approval(
    request: &crate::requests::Approval,
    token: &str,
    prefix: &str,
) -> (Panel, Vec<(String, String)>) {
    use crate::requests::ApprovalKind;
    let title = match request.kind {
        ApprovalKind::Command if request.network_context.is_some() => "网络访问审批",
        ApprovalKind::Command => "命令执行审批",
        ApprovalKind::WriteStdin => "进程输入审批",
        ApprovalKind::FileChange => "文件修改审批",
    };
    let mut body = String::from("请核对以下请求。仅本次请求有效；10 分钟内未答复将拒绝。\n");
    let file = request.kind == ApprovalKind::FileChange;
    let mut complete = if file {
        request
            .changes
            .as_ref()
            .is_some_and(|changes| !changes.is_empty() && changes.len() <= 20)
    } else if request.kind == ApprovalKind::Command
        && request.network_context.is_some()
        && request.permissions.is_none()
    {
        true
    } else {
        request
            .command
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
            && request
                .directory
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
    };
    let mut fields = vec![
        ("命令或输入", request.command.clone()),
        ("工作目录", request.directory.clone()),
        ("请求原因", request.reason.clone()),
        ("授权目录", request.grant_root.clone()),
    ];
    if request.permissions.is_some() {
        fields.push(("本次额外权限", request.permissions.clone()));
    }
    if request.network_context.is_some() {
        fields.push(("网络访问目标", request.network_context.clone()));
    }
    if file {
        if let Some(changes) = &request.changes {
            for change in changes.iter().take(20) {
                if change.path.trim().is_empty()
                    || change.diff.trim().is_empty()
                    || !matches!(change.operation.as_str(), "add" | "delete" | "update")
                {
                    complete = false;
                }
                fields.extend([
                    ("文件路径", Some(change.path.clone())),
                    ("操作类型", Some(change.operation.clone())),
                    ("移动目标", change.move_path.clone()),
                    ("文件差异", Some(change.diff.clone())),
                ]);
            }
        }
        if !complete {
            body.push_str("\n当前尚未提供逐文件修改详情，或详情不完整。\n");
        }
    }
    if request.grant_root.is_some() {
        complete = false;
        body =
            "本请求包含可能持续整个会话的目录写入授权，当前不支持同意。10 分钟内未答复将拒绝。\n"
                .into();
    }
    for (label, value) in fields {
        let value = value.as_deref().unwrap_or("（未提供）");
        // Do not silently truncate security-relevant details and still allow.
        if value.len() > 4096
            || value
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            complete = false;
            body.push_str(&format!("\n{label}：内容过长或包含无法显示的控制字符。\n"));
            continue;
        }
        // A longer fence preserves literal Markdown, including embedded fences.
        let fence = "`".repeat(
            value
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(2)
                + 1,
        );
        body.push_str(&format!("\n{label}：\n{fence}\n{value}\n{fence}\n"));
    }
    if body.len() > 24 * 1024 {
        complete = false;
        body = "审批详情超过卡片展示上限，无法完整展示。10 分钟内未答复将拒绝。\n".into();
    }
    let mut buttons = Vec::new();
    if complete && request.can_allow {
        buttons.push(("同意本次请求".into(), format!("/approve {token}")));
    } else {
        body.push_str("\n本请求暂不能通过卡片同意，请拒绝后让 Codex 调整请求。\n");
    }
    buttons.push(("拒绝".into(), format!("/deny {token}")));
    let (mut card, commands) = panel(title, body, buttons, prefix);
    card.tone = Tone::Warning;
    for button in &mut card.buttons {
        button.group = None;
        button.style = if button.label == "拒绝" {
            ButtonStyle::Destructive
        } else {
            ButtonStyle::Primary
        };
    }
    (card, commands)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_details_are_literal_and_incomplete_display_cannot_allow() {
        let mut request = crate::requests::Approval {
            permissions: None,
            network_context: None,
            changes: None,
            kind: crate::requests::ApprovalKind::Command,
            command: Some("echo ```\n**not a heading**".into()),
            directory: Some("/workspace".into()),
            reason: Some("读取测试结果".into()),
            grant_root: None,
            can_allow: true,
        };
        let (card, commands) = approval(&request, "request-1", "card-1");
        assert!(
            card.body
                .contains("````\necho ```\n**not a heading**\n````")
        );
        assert!(card.body.contains("/workspace"));
        assert_eq!(
            commands
                .iter()
                .map(|(_, command)| command.as_str())
                .collect::<Vec<_>>(),
            vec!["/approve request-1", "/deny request-1"]
        );
        assert!(card.buttons.iter().all(|button| button.group.is_none()));
        for value in ["x".repeat(4097), "hidden\u{1b}[0m".into()] {
            request.command = Some(value);
            let (card, commands) = approval(&request, "request-1", "card-2");
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].1, "/deny request-1");
            assert!(card.body.contains("暂不能通过卡片同意"));
        }
        request.command = Some("echo ok".into());
        request.can_allow = false;
        assert_eq!(approval(&request, "request-1", "card-3").1.len(), 1);
        request.can_allow = true;
        request.command = None;
        assert_eq!(
            approval(&request, "request-1", "missing-command").1.len(),
            1
        );
        request.command = Some("echo ok".into());
        request.directory = None;
        assert_eq!(
            approval(&request, "request-1", "missing-directory").1.len(),
            1
        );
        request.directory = Some("/workspace".into());
        request.can_allow = true;
        request.kind = crate::requests::ApprovalKind::FileChange;
        let (card, commands) = approval(&request, "request-1", "card-4");
        assert!(card.body.contains("尚未提供逐文件修改详情"));
        assert_eq!(commands.len(), 1);
        request.kind = crate::requests::ApprovalKind::WriteStdin;
        request.command = Some("`".repeat(4096));
        request.directory = request.command.clone();
        request.reason = request.command.clone();
        let (card, commands) = approval(&request, "request-1", "card-5");
        assert!(card.body.len() < 24 * 1024);
        assert_eq!(commands.len(), 1);
    }
    #[test]
    fn view_updates_remove_only_consumed_actions_and_expire_remaining_buttons()
    -> Result<(), &'static str> {
        let now = Instant::now();
        let deadline = now + std::time::Duration::from_secs(600);
        let (panel, commands) = help("view");
        let mut actions = Actions::default();
        actions.insert(
            commands
                .into_iter()
                .map(|(token, command)| {
                    (
                        token,
                        Action {
                            generation: 0,
                            user: "user".into(),
                            chat: "chat".into(),
                            directory: "/root".into(),
                            source: "source".into(),
                            deadline,
                            command,
                            stop_snapshot: None,
                        },
                    )
                })
                .collect(),
            now,
        );
        let mut views = Views::default();
        views.insert("source".into(), panel);
        assert!(views.next_update(&actions, now, &(0, None)).is_none());
        let click = Click {
            token: "view-0".into(),
            source: "source".into(),
        };
        assert!(
            actions
                .take(
                    &click,
                    "wrong",
                    "chat",
                    std::path::Path::new("/root"),
                    now,
                    &(0, None)
                )
                .is_none()
        );
        assert!(views.next_update(&actions, now, &(0, None)).is_none());
        assert_eq!(
            actions.take(
                &click,
                "user",
                "chat",
                std::path::Path::new("/root"),
                now,
                &(0, None)
            ),
            Some("/status".into())
        );
        let (_, updated) = views
            .next_update(&actions, now, &(0, None))
            .ok_or("consumed view")?;
        assert_eq!(updated.buttons.len(), 8);
        assert!(updated.body.contains("操作结果请查看单独回复"));
        assert!(views.next_update(&actions, now, &(0, None)).is_none());
        let (_, expired) = views
            .next_update(&actions, deadline, &(0, None))
            .ok_or("expired view")?;
        assert!(expired.buttons.is_empty());
        assert!(expired.body.contains("均已使用或失效"));
        assert!(views.0.is_empty());
        Ok(())
    }

    #[test]
    fn view_updates_follow_directory_invalidation_and_stale_stop() -> Result<(), &'static str> {
        let now = Instant::now();
        let (panel, commands) = help("view");
        let mut actions = Actions::default();
        actions.insert(
            commands
                .into_iter()
                .map(|(token, command)| {
                    let stop_snapshot = (command == "/stop").then_some((0, None));
                    (
                        token,
                        Action {
                            generation: 0,
                            user: "user".into(),
                            chat: "chat".into(),
                            directory: "/root".into(),
                            source: "source".into(),
                            deadline: now + std::time::Duration::from_secs(600),
                            command,
                            stop_snapshot,
                        },
                    )
                })
                .collect(),
            now,
        );
        let mut views = Views::default();
        views.insert("source".into(), panel);
        let (_, updated) = views
            .next_update(&actions, now, &(1, None))
            .ok_or("stale stop")?;
        assert_eq!(updated.buttons.len(), 8);
        assert!(!updated.buttons.iter().any(|b| b.label.contains("停止")));
        actions.invalidate("user");
        let (_, updated) = views
            .next_update(&actions, now, &(1, None))
            .ok_or("directory changed")?;
        assert!(updated.buttons.is_empty());
        Ok(())
    }
    #[test]
    fn stale_stop_and_expired_card_do_not_consume_valid_action() {
        let now = Instant::now();
        let deadline = now + std::time::Duration::from_secs(600);
        let mut registry = Actions::default();
        let snapshot = (2, Some("task".into()));
        assert!(registry.insert(
            vec![(
                "token".into(),
                Action {
                    generation: 0,
                    user: "user".into(),
                    chat: "chat".into(),
                    directory: "/root".into(),
                    source: "card".into(),
                    deadline,
                    command: "/stop".into(),
                    stop_snapshot: Some(snapshot.clone())
                }
            )],
            now
        ));
        let click = Click {
            token: "token".into(),
            source: "card".into(),
        };
        let root = std::path::Path::new("/root");
        assert!(
            registry
                .take(&click, "user", "chat", root, now, &(3, Some("new".into())))
                .is_none()
        );
        assert!(
            registry
                .take(&click, "user", "chat", root, deadline, &snapshot)
                .is_none()
        );
        assert_eq!(
            registry.take(&click, "user", "chat", root, now, &snapshot),
            Some("/stop".into())
        );
        assert!(
            registry
                .take(&click, "user", "chat", root, now, &snapshot)
                .is_none()
        );
    }

    #[test]
    fn plan_question_does_not_offer_skip_button() {
        let q = crate::requests::Question {
            id: "drink".into(),
            header: "饮料".into(),
            text: "请选择".into(),
            other: false,
            secret: false,
            options: vec![
                crate::requests::QuestionOption {
                    label: "咖啡".into(),
                    description: "A".into(),
                },
                crate::requests::QuestionOption {
                    label: "茶".into(),
                    description: "B".into(),
                },
            ],
        };
        let (panel, commands) = question(&q, 0, 1, "token", "panel");
        assert!(
            panel
                .buttons
                .iter()
                .all(|button| button.label != "跳过本题")
        );
        assert!(
            commands
                .iter()
                .all(|(_, command)| !command.ends_with(" skip"))
        );
    }

    #[test]
    fn lists_preserve_fallback_context_and_offer_full_width_navigation() {
        let entries = vec![crate::sessions::ListedThread {
            id: "thread-1".into(),
            title: "登录修复".repeat(30),
            active: true,
        }];
        let (card, commands) = threads(&entries, false, "recent");
        assert!(card.body.contains(&entries[0].title));
        assert!(card.body.contains("（执行中）"));
        assert!(card.body.contains("/resume thread-1"));
        assert!(card.body.contains("/archive thread-1"));
        assert!(
            card.buttons
                .iter()
                .all(|b| b.group.is_none() && b.label.chars().count() < 20)
        );
        assert_eq!(commands.last().map(|(_, c)| c.as_str()), Some("/archived"));
        let (empty, commands) = threads(&[], true, "empty");
        assert!(empty.body.contains("没有已归档会话"));
        assert_eq!(
            commands.iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            vec!["/archived", "/resume"]
        );
        let (_, archived) = threads(&entries, true, "archive");
        assert_eq!(archived[0].1, "/unarchive thread-1");
        assert!(!archived.iter().any(|(_, c)| c.starts_with("/archive ")));
        let (card, _) = models(
            &[crate::ports::Model {
                id: "long-model".repeat(20),
                is_default: false,
            }],
            None,
            "models",
        );
        assert!(card.buttons.iter().all(|b| b.group.is_none()));
        assert!(card.body.contains("全局空闲"));
    }

    #[test]
    fn list_cards_keep_thread_commands_and_model_selection() {
        let entries = vec![crate::sessions::ListedThread {
            id: "thread-1".into(),
            title: "修复登录".into(),
            active: false,
        }];
        let (_, thread_commands) = threads(&entries, false, "threads");
        assert_eq!(thread_commands[0].1, "/resume thread-1");
        assert_eq!(thread_commands[1].1, "/archive thread-1");

        let available_models = vec![crate::ports::Model {
            id: "gpt-test".into(),
            is_default: true,
        }];
        let (_, model_commands) = super::models(&available_models, Some("gpt-test"), "models");
        assert_eq!(model_commands[0].1, "/model gpt-test");
        assert_eq!(
            model_commands.last().map(|(_, command)| command.as_str()),
            Some("/models")
        );
        assert!(
            model_commands
                .iter()
                .any(|(_, command)| command == "/model default")
        );
    }
}
