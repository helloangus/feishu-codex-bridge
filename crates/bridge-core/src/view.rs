//! Platform-independent presentation intent; renderers own platform schemas.
use crate::command::Command;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Info,
    Success,
    Warning,
    Error,
    Muted,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ButtonStyle {
    #[default]
    Default,
    Primary,
    Destructive,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonAction {
    Command(Command),
    /// Opaque local token; the application validates ownership and allowed choice.
    Interaction {
        token: String,
        choice: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub label: String,
    pub description: Option<String>,
    pub section: Option<String>,
    pub group: Option<String>,
    pub separate: bool,
    pub style: ButtonStyle,
    pub action: ButtonAction,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panel {
    pub title: String,
    pub body: String,
    pub tone: Tone,
    pub buttons: Vec<Button>,
}
impl Panel {
    pub fn text(title: impl Into<String>, body: impl Into<String>, tone: Tone) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            tone,
            buttons: vec![],
        }
    }
}
