//! Configuration-aware credential collection and validation.
//!
//! The configured environment variable names decide which credentials are
//! read; no default name is forced on an existing configuration. Interactive
//! prompts read one line with terminal echo disabled and never print values.
//! `--print` emits safely quoted `export` lines for shell callers (`eval`) so
//! a prompted value reaches the service started afterwards.
use crate::{Config, valid_env_name};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, BufRead, Write},
};

const PAIRING_MIN_BYTES: usize = 16;
const PAIRING_MAX_BYTES: usize = 256;

/// Byte length and Unicode whitespace rules in one place; the shell no longer
/// duplicates them and the runtime check in bootstrap reuses this predicate.
pub fn valid_pairing_code(code: &str) -> bool {
    (PAIRING_MIN_BYTES..=PAIRING_MAX_BYTES).contains(&code.len())
        && !code.chars().any(char::is_whitespace)
}

/// Emit a single-quoted POSIX assignment; the value cannot leave quoting.
fn quoted(name: &str, value: &str) -> String {
    format!("export {name}='{}'", value.replace('\'', r"'\''"))
}

/// Hide input while a line is read, restoring terminal state even on error.
/// Without a terminal (piped input, non-interactive runs) the line is read as
/// plain text so automation can still supply values on stdin.
fn read_hidden(prompt: &str) -> io::Result<String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let original = tcgetattr(&handle).ok();
    if let Some(original) = &original {
        let mut muted = original.clone();
        muted.local_modes = original.local_modes & !LocalModes::ECHO;
        let _ = tcsetattr(&handle, OptionalActions::Drain, &muted);
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    let result = handle.read_line(&mut line);
    if let Some(original) = &original {
        let _ = tcsetattr(&handle, OptionalActions::Drain, original);
    }
    result?;
    while line.ends_with(['\n', '\r']) {
        line.pop();
    }
    eprintln!();
    Ok(line)
}

fn environment(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Paired users already recorded in the durable state keep pairing optional.
fn state_allows_pairing_skip(state_dir: &std::path::Path) -> bool {
    #[derive(serde::Deserialize)]
    struct Peek {
        #[serde(default)]
        allowed_open_ids: BTreeSet<String>,
    }
    let Ok(text) = fs::read_to_string(state_dir.join("state.json")) else {
        return false;
    };
    serde_json::from_str::<Peek>(&text)
        .map(|peek| !peek.allowed_open_ids.is_empty())
        .unwrap_or(false)
}

/// Validate required credentials for `config` and prompt for missing ones.
/// Returns the prompted values as `export` lines; empty output means every
/// credential was already present in the environment.
pub fn ensure(config: &Config, print_exports: bool) -> Result<Vec<String>, String> {
    let feishu = config.feishu.as_ref().ok_or("缺少 [feishu] 配置段")?;
    for name in [&feishu.app_id_env, &feishu.app_secret_env] {
        if !valid_env_name(name) {
            return Err("飞书凭据环境变量名无效；请修改配置后重试".into());
        }
    }
    if let Some(name) = &feishu.proxy_env {
        if !valid_env_name(name) {
            return Err("飞书代理环境变量名无效；请修改配置后重试".into());
        }
    }
    let mut exports = Vec::new();
    let mut pairing_present = false;
    for (label, name) in [
        ("飞书 App ID", &feishu.app_id_env),
        ("飞书 App Secret", &feishu.app_secret_env),
    ] {
        if environment(name).is_some() {
            continue;
        }
        let value = read_hidden(&format!("{label}（{name}）："))
            .map_err(|e| format!("无法读取{label}输入：{e}"))?;
        if value.is_empty() {
            return Err(format!("缺少{name}；非交互运行请预先设置该环境变量"));
        }
        if print_exports {
            exports.push(quoted(name, &value));
        }
    }
    if let Some(name) = &config.access.pairing_code_env {
        if !valid_env_name(name) {
            return Err("配对码环境变量名无效；请修改配置后重试".into());
        }
        let provided = environment(name);
        let value = match &provided {
            Some(value) => value.clone(),
            None => {
                let paired = !config.access.allowed_open_ids.is_empty()
                    || state_allows_pairing_skip(&config.workspace.state_dir);
                let value = read_hidden(&format!(
                    "配对码（可先在另一终端执行 openssl rand -hex 16 生成；{}；留空{}）：",
                    if paired {
                        "已有白名单或已配对用户"
                    } else {
                        "首次使用或无白名单时必填"
                    },
                    if paired { "跳过" } else { "则取消" }
                ))
                .map_err(|e| format!("无法读取配对码输入：{e}"))?;
                if value.is_empty() && !paired {
                    return Err("未设置白名单且尚无已配对用户时，配对码不能为空".into());
                }
                value
            }
        };
        if !value.is_empty() {
            if !valid_pairing_code(&value) {
                return Err(format!(
                    "配对码无效：必须为 {PAIRING_MIN_BYTES}–{PAIRING_MAX_BYTES} 字节且不能包含空白"
                ));
            }
            pairing_present = true;
            if print_exports && provided.is_none() {
                exports.push(quoted(name, &value));
            }
        }
    }
    if print_exports && pairing_present {
        // Lets the start script mention /pair without parsing the configuration.
        exports.push("export BRIDGE_RUST_PAIRING_PRESENT=1".into());
    }
    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairing_rules_reject_short_long_and_whitespace() {
        assert!(valid_pairing_code(&"a".repeat(16)));
        assert!(valid_pairing_code(&"a".repeat(256)));
        assert!(!valid_pairing_code(&"a".repeat(15)));
        assert!(!valid_pairing_code(&"a".repeat(257)));
        assert!(!valid_pairing_code("0123456789abcdef\t"));
        assert!(!valid_pairing_code("0123456789abcdef\u{00a0}"));
        assert!(!valid_pairing_code("0123456789abcdef "));
    }
    #[test]
    fn quoting_survives_embedded_single_quotes() {
        let line = quoted("VAR", "it's");
        assert_eq!(line, r"export VAR='it'\''s'");
    }
}
