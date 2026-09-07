//! Text and card adapters both produce these commands.
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Status,
    ChangeDirectory(Option<String>),
    New,
    Resume(Option<String>),
    Archive(String),
    Archived,
    Unarchive(String),
    Model(Option<String>),
    Models,
    Plan(Option<bool>),
    Compact,
    Stop(Option<String>),
    Approve { token: String, allow: bool },
    Pair(String),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("不是文本命令")]
    NotCommand,
    #[error("未知命令")]
    Unknown,
    #[error("命令参数无效或缺失")]
    InvalidArgument,
}

impl Command {
    /// Parse without invoking a shell or logging user-supplied arguments.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let text = text.trim();
        if !text.starts_with('/') {
            return Err(ParseError::NotCommand);
        }
        let mut parts = text.splitn(2, char::is_whitespace);
        let name = parts.next().ok_or(ParseError::NotCommand)?.to_lowercase();
        let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
        let required = || arg.map(str::to_owned).ok_or(ParseError::InvalidArgument);
        let optional = || arg.map(str::to_owned);
        let no_arg = |cmd| {
            if arg.is_none() {
                Ok(cmd)
            } else {
                Err(ParseError::InvalidArgument)
            }
        };
        match name.as_str() {
            "/help" => no_arg(Self::Help),
            "/status" => no_arg(Self::Status),
            "/cd" => Ok(Self::ChangeDirectory(optional())),
            "/new" => no_arg(Self::New),
            "/resume" => Ok(Self::Resume(optional())),
            "/archive" => Ok(Self::Archive(required()?)),
            "/archived" => no_arg(Self::Archived),
            "/unarchive" => Ok(Self::Unarchive(required()?)),
            "/model" => Ok(Self::Model(optional())),
            "/models" => no_arg(Self::Models),
            "/plan" => Ok(Self::Plan(match arg.map(str::to_lowercase).as_deref() {
                None => None,
                Some("on" | "开启" | "打开") => Some(true),
                Some("off" | "关闭" | "退出") => Some(false),
                _ => return Err(ParseError::InvalidArgument),
            })),
            "/compact" => no_arg(Self::Compact),
            "/stop" => Ok(Self::Stop(optional())),
            "/approve" | "/deny" => Ok(Self::Approve {
                token: required()?,
                allow: name == "/approve",
            }),
            "/pair" => Ok(Self::Pair(required()?)),
            _ => Err(ParseError::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_preserve_paths_and_require_explicit_modes() {
        assert_eq!(
            Command::parse(" /CD dir with spaces "),
            Ok(Command::ChangeDirectory(Some("dir with spaces".into())))
        );
        assert_eq!(Command::parse("/plan off"), Ok(Command::Plan(Some(false))));
        assert_eq!(
            Command::parse("/plan maybe"),
            Err(ParseError::InvalidArgument)
        );
        assert_eq!(Command::parse("/archive"), Err(ParseError::InvalidArgument));
        assert_eq!(
            Command::parse("/stop token"),
            Ok(Command::Stop(Some("token".into())))
        );
    }
}
