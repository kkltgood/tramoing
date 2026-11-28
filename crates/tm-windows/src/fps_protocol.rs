//! Bounded FPS control messages. No executable paths or arbitrary ETW providers.
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", deny_unknown_fields)]
pub enum Request {
    Read { pid: u32 },
    Stop {},
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Ready,
    NoFrames,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    pub pid: u32,
    /// Successful application Present submissions, not displayed/generated frames.
    pub fps: Option<f64>,
    pub status: Status,
    pub error: Option<String>,
}

impl Reading {
    pub(crate) fn empty(pid: u32, status: Status) -> Self {
        Self {
            pid,
            fps: None,
            status,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_cannot_choose_providers_paths_or_commands() {
        assert!(serde_json::from_str::<Request>(r#"{"command":"Read","pid":42}"#).is_ok());
        for text in [
            r#"{"command":"Read","pid":42,"provider":"anything"}"#,
            r#"{"command":"Read","pid":-1}"#,
            r#"{"command":"Launch","path":"app.exe"}"#,
        ] {
            assert!(serde_json::from_str::<Request>(text).is_err());
        }
    }
}
