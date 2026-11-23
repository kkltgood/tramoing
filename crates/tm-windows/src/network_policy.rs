//! 网络控制协议。规则身份为完整 exe 路径，不能使用 PID 或展示名称。

use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", deny_unknown_fields)]
pub enum Request {
    List {},
    Set { path: String, blocked: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub path: String,
    /// 只有四个 ALE 层的持久规则全部存在且启用才报告已断网。
    pub complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub rules: Vec<Rule>,
    pub error: Option<String>,
}

/// Windows 路径在两端都校验。拒绝未知 PID、聚合行、设备路径与网络共享。
/// 恢复联网不要求文件仍然存在，应用升级/卸载后仍可移除旧路径规则。
pub fn validate_path(path: &str) -> Result<(), String> {
    let b = path.as_bytes();
    if path.encode_utf16().count() > 32760
        || b.len() < 7
        || !b[0].is_ascii_alphabetic()
        || b[1] != b':'
        || b[2] != b'\\'
        || !path.to_ascii_lowercase().ends_with(".exe")
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, '*' | '?' | '"' | '/' | '<' | '>' | '|'))
        || path[2..].contains(':')
        || path[3..]
            .split('\\')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.ends_with([' ', '.']))
    {
        return Err("无法控制此条目：需要本机程序的完整 .exe 路径".into());
    }
    Ok(())
}

pub fn same_path(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_local_executables_can_be_blocked() {
        for path in [r"C:\Program Files\抖音\Douyin.exe", r"D:\App.EXE"] {
            assert!(validate_path(path).is_ok(), "{path}");
        }
        for path in [
            "",
            "pid 123",
            "其他应用（采集容量上限）",
            "app.exe",
            r"C:app.exe",
            r"\\host\share\app.exe",
            r"\\?\C:\app.exe",
            r"C:\*\app.exe",
            r"C:\folder\..\app.exe",
            r"C:\app.exe:other.exe",
            r"C:\app.exe\",
            "C:\\bad\nname.exe",
            r"C:\folder.\app.exe",
            r"C:\folder\\app.exe",
        ] {
            assert!(validate_path(path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn protocol_preserves_full_path_and_rejects_unknown_commands() {
        let path = r"C:\Program Files\抖音\app.exe";
        let request = Request::Set {
            path: path.into(),
            blocked: true,
        };
        let decoded: Request =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert!(matches!(decoded, Request::Set { path: p, blocked: true } if p == path));
        assert!(
            serde_json::from_str::<Request>(r#"{"command":"Execute","path":"cmd.exe"}"#).is_err()
        );
        assert!(serde_json::from_str::<Request>(r#"{"command":"List","execute":"x"}"#).is_err());
        assert!(!same_path(r"C:\One\app.exe", r"C:\Two\app.exe"));
        assert!(same_path(r"C:\App.EXE", r"c:\app.exe"));
    }
}
