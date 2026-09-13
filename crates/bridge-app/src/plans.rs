//! A completed, fully retained plan is the only source of an implementation offer.
use bridge_core::task::TaskSpec;

pub struct Offer {
    pub task: TaskSpec,
    pub thread: String,
    pub text: String,
    pub token: String,
    pub sent: bool,
    pub deadline: tokio::time::Instant,
}

pub fn panel(offer: &Offer, prefix: &str) -> (bridge_core::view::Panel, Vec<(String, String)>) {
    crate::cards::panel(
        "Plan 已完成，请确认下一步",
        format!(
            "{}\n\n请选择直接实施、清空上下文后实施，或继续讨论。实施会关闭 Plan 模式；继续讨论保持 Plan 模式。仅本人在原聊天、目录和会话中操作，10 分钟有效。新任务或会话设置变更会使本计划失效。",
            offer.text
        ),
        vec![
            (
                "确认并实施".into(),
                format!("/plan-action {} implement", offer.token),
            ),
            (
                "清空上下文后实施".into(),
                format!("/plan-action {} fresh", offer.token),
            ),
            (
                "继续讨论计划".into(),
                format!("/plan-action {} stay", offer.token),
            ),
        ],
        prefix,
    )
}
