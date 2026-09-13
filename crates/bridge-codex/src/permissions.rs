//! Version-local permission overlays converted to complete literal display text.
use bridge_app::ports::BackendError;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Profile {
    file_system: Option<FileSystem>,
    network: Option<Network>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Network {
    enabled: Option<bool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileSystem {
    read: Option<Vec<String>>,
    write: Option<Vec<String>>,
    entries: Option<Vec<Entry>>,
    glob_scan_max_depth: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    access: Access,
    path: Target,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Access {
    Read,
    Write,
    Deny,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Path { path: String },
    GlobPattern { pattern: String },
    Special { value: Special },
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Special {
    Root,
    Minimal,
    ProjectRoots {
        subpath: Option<String>,
    },
    Tmpdir,
    SlashTmp,
    Unknown {
        path: String,
        subpath: Option<String>,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    host: String,
    protocol: Protocol,
}
#[derive(Deserialize)]
enum Protocol {
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "https")]
    Https,
    #[serde(rename = "socks5Tcp")]
    Socks5Tcp,
    #[serde(rename = "socks5Udp")]
    Socks5Udp,
}

fn nonempty(value: &str) -> Result<&str, BackendError> {
    if value.trim().is_empty() {
        Err(BackendError::Incompatible)
    } else {
        Ok(value)
    }
}

// Quote path/host strings so newlines or Markdown cannot masquerade as labels.
fn literal(value: &str) -> Result<String, BackendError> {
    nonempty(value)?;
    serde_json::to_string(value).map_err(|_| BackendError::Incompatible)
}

fn target(value: Target) -> Result<(String, bool), BackendError> {
    Ok(match value {
        Target::Path { path } => (format!("路径 {}", literal(&path)?), true),
        Target::GlobPattern { pattern } => (format!("通配模式 {}", literal(&pattern)?), true),
        Target::Special { value } => match value {
            Special::Root => ("整个文件系统（root）".into(), true),
            Special::Minimal => ("Codex 最小运行路径集合（minimal）".into(), true),
            Special::Tmpdir => ("Codex 环境临时目录（tmpdir）".into(), true),
            Special::SlashTmp => ("/tmp（slash_tmp）".into(), true),
            Special::ProjectRoots { subpath } => (
                format!(
                    "Codex 项目根目录集合（project_roots），子路径 {}",
                    subpath
                        .as_deref()
                        .map(literal)
                        .transpose()?
                        .unwrap_or_else(|| "无".into())
                ),
                true,
            ),
            Special::Unknown { path, subpath } => (
                format!(
                    "未识别的特殊路径 {}，子路径 {}",
                    literal(&path)?,
                    subpath
                        .as_deref()
                        .map(literal)
                        .transpose()?
                        .unwrap_or_else(|| "无".into())
                ),
                false,
            ),
        },
    })
}

/// A well-formed but semantically unknown special path is visible, decline-only.
pub fn profile(value: Option<&Value>) -> Result<(Option<String>, bool), BackendError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok((None, true));
    };
    let profile: Profile =
        serde_json::from_value(value.clone()).map_err(|_| BackendError::Incompatible)?;
    let mut lines = vec!["以下为本次命令额外申请的权限，不保存为长期规则。".to_owned()];
    let mut supported = true;
    if let Some(network) = profile.network {
        lines.push(format!(
            "网络权限：{}",
            match network.enabled {
                Some(true) => "启用网络访问（此开关本身不限定主机）",
                Some(false) => "不启用网络访问",
                None => "未指定，沿用原设置",
            }
        ));
    }
    if let Some(fs) = profile.file_system {
        if let Some(depth) = fs.glob_scan_max_depth {
            if depth == 0 {
                return Err(BackendError::Incompatible);
            }
            lines.push(format!("通配路径最大扫描深度：{depth}"));
        }
        for (label, paths) in [
            ("额外读取路径（read）", fs.read),
            ("额外写入路径（write）", fs.write),
        ] {
            if let Some(paths) = paths {
                if paths.len() > 100 {
                    return Err(BackendError::Incompatible);
                }
                if paths.is_empty() {
                    lines.push(format!("{label}：空列表"));
                }
                for path in paths {
                    lines.push(format!("{label}：{}", literal(&path)?));
                }
            }
        }
        if let Some(entries) = fs.entries {
            if entries.len() > 100 {
                return Err(BackendError::Incompatible);
            }
            if entries.is_empty() {
                lines.push("文件系统规则（entries）：空列表".into());
            }
            for entry in entries {
                let (path, known) = target(entry.path)?;
                supported &= known;
                let access = match entry.access {
                    Access::Read => "读取",
                    Access::Write => "写入",
                    Access::Deny => "拒绝访问",
                };
                lines.push(format!("文件系统规则：{access}；{path}"));
            }
        }
    }
    if lines.len() == 1 {
        lines.push("未指定额外权限值。".into());
    }
    let result = lines.join("\n");
    supported &= result.len() <= 4096;
    Ok((Some(result), supported))
}

pub fn context(value: Option<&Value>) -> Result<Option<String>, BackendError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let context: Context =
        serde_json::from_value(value.clone()).map_err(|_| BackendError::Incompatible)?;
    let protocol = match context.protocol {
        Protocol::Http => "HTTP",
        Protocol::Https => "HTTPS",
        Protocol::Socks5Tcp => "SOCKS5 TCP",
        Protocol::Socks5Udp => "SOCKS5 UDP",
    };
    Ok(Some(format!(
        "目标主机：{}\n协议：{protocol}\n仅处理本次访问请求，不添加永久网络规则。",
        literal(&context.host)?
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn overlays_preserve_all_ranges_and_special_path_semantics() -> Result<(), BackendError> {
        let value = json!({"network":{"enabled":true},"fileSystem":{
        "read":["/read"],"write":["/write"],"globScanMaxDepth":3,
        "entries":[
            {"access":"read","path":{"type":"path","path":"/literal"}},
            {"access":"write","path":{"type":"glob_pattern","pattern":"/output/**"}},
            {"access":"deny","path":{"type":"special","value":{"kind":"root"}}},
            {"access":"read","path":{"type":"special","value":{"kind":"minimal"}}},
            {"access":"write","path":{"type":"special","value":{"kind":"project_roots","subpath":"build"}}},
            {"access":"write","path":{"type":"special","value":{"kind":"tmpdir"}}},
            {"access":"read","path":{"type":"special","value":{"kind":"slash_tmp"}}}
        ]}});
        let (body, supported) = profile(Some(&value))?;
        assert!(supported);
        let body = body.ok_or(BackendError::Incompatible)?;
        for text in [
            "不限定主机",
            "/read",
            "/write",
            "深度：3",
            "/literal",
            "/output/**",
            "拒绝访问",
            "root",
            "minimal",
            "project_roots",
            "build",
            "tmpdir",
            "slash_tmp",
        ] {
            assert!(body.contains(text), "missing {text}");
        }
        Ok(())
    }

    #[test]
    fn unknown_fields_types_and_empty_targets_never_become_approval() {
        for value in [
            json!({"future":true}),
            json!({"network":{"enabled":"yes"}}),
            json!({"network":{"enabled":true,"hosts":["hidden"]}}),
            json!({"fileSystem":{"read":[""]}}),
            json!({"fileSystem":{"globScanMaxDepth":0}}),
            json!({"fileSystem":{"entries":[{"access":"execute","path":{"type":"path","path":"/a"}}]}}),
            json!({"fileSystem":{"entries":[{"access":"read","path":{"type":"path","path":"/a","hidden":"/b"}}]}}),
        ] {
            assert!(profile(Some(&value)).is_err());
        }
        for value in [
            json!({"host":"example.test","protocol":"ftp"}),
            json!({"host":"","protocol":"https"}),
            json!({"host":"example.test"}),
            json!({"host":"example.test","protocol":"https","extra":true}),
        ] {
            assert!(context(Some(&value)).is_err());
        }
    }

    #[test]
    fn unknown_special_and_oversized_details_are_decline_only() -> Result<(), BackendError> {
        let value = json!({"fileSystem":{"entries":[{"access":"write","path":{
            "type":"special","value":{"kind":"unknown","path":"future-root","subpath":"nested"}
        }}]}});
        let (body, supported) = profile(Some(&value))?;
        assert!(!supported);
        assert!(
            body.ok_or(BackendError::Incompatible)?
                .contains("future-root")
        );
        assert!(!profile(Some(&json!({"fileSystem":{"read":["x".repeat(4096)]}})))?.1);
        for protocol in ["http", "https", "socks5Tcp", "socks5Udp"] {
            let text = context(Some(&json!({"host":"example.test","protocol":protocol})))?
                .ok_or(BackendError::Incompatible)?;
            assert!(text.contains("example.test"));
        }
        assert_eq!(profile(None)?, (None, true));
        assert_eq!(context(Some(&Value::Null))?, None);
        Ok(())
    }
}
