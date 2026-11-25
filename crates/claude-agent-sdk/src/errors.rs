//! Error types for the Claude Agent SDK.
//!
//! The variants mirror the exception hierarchy of the Python SDK
//! (`ClaudeSDKError`, `CLIConnectionError`, `CLINotFoundError`, `ProcessError`,
//! `ResultError`, `CLIJSONDecodeError`, `MessageParseError`).

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// All errors produced by this SDK.
///
/// The enum is `Clone` so that one failure can be delivered to every pending
/// control request at once (the reader task does this when the CLI dies).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    /// Unable to connect to, or communicate with, Claude Code
    /// (Python: `CLIConnectionError`).
    CliConnection(String),
    /// The Claude Code CLI executable was not found (Python: `CLINotFoundError`).
    CliNotFound {
        /// Human readable message; includes the path when one was tried.
        message: String,
        /// The path that was tried, if any.
        cli_path: Option<String>,
    },
    /// The CLI process exited abnormally (Python: `ProcessError`).
    Process {
        /// Formatted message, including the exit code and stderr tail.
        message: String,
        /// Exit code of the process, when known.
        exit_code: Option<i32>,
        /// Captured stderr, when available.
        stderr: Option<String>,
    },
    /// The CLI exited after reporting a terminal error `result` message
    /// (Python: `ResultError`, a subclass of `ProcessError`).
    Result(Box<ResultError>),
    /// A line of CLI output was not valid JSON (Python: `CLIJSONDecodeError`).
    JsonDecode {
        /// The offending line (possibly very long).
        line: String,
        /// The underlying JSON error.
        source: Arc<serde_json::Error>,
    },
    /// A JSON message from the CLI could not be mapped to a typed message
    /// (Python: `MessageParseError`).
    MessageParse {
        /// What went wrong.
        message: String,
        /// The raw message, when available.
        data: Option<Value>,
    },
    /// The CLI answered a control request with an error, or the request
    /// could not be delivered.
    ControlProtocol(String),
    /// A control request was not answered within its timeout. The payload is
    /// the request subtype (e.g. `initialize`).
    ControlTimeout(String),
    /// The supplied options are invalid or mutually exclusive
    /// (Python raises `ValueError` / `TypeError` for these).
    InvalidOptions(String),
    /// A user supplied callback (permission, hook or tool handler) failed.
    Callback(String),
    /// An I/O error while talking to the subprocess.
    Io(Arc<std::io::Error>),
    /// A JSON (de)serialization error outside of CLI output decoding.
    Json(Arc<serde_json::Error>),
}

impl Error {
    /// Build a [`Error::CliConnection`].
    pub fn cli_connection(message: impl Into<String>) -> Self {
        Error::CliConnection(message.into())
    }

    /// Build a [`Error::CliNotFound`]; the path is appended to the message.
    pub fn cli_not_found(message: impl Into<String>, cli_path: Option<String>) -> Self {
        let mut message = message.into();
        if let Some(path) = &cli_path {
            message = format!("{message}: {path}");
        }
        Error::CliNotFound { message, cli_path }
    }

    /// Build a [`Error::Process`], formatting the exit code and stderr into the
    /// message the same way the Python SDK does.
    pub fn process(
        message: impl Into<String>,
        exit_code: Option<i32>,
        stderr: Option<String>,
    ) -> Self {
        let mut message = message.into();
        if let Some(code) = exit_code {
            message = format!("{message} (exit code: {code})");
        }
        if let Some(err) = stderr.as_deref().filter(|s| !s.is_empty()) {
            message = format!("{message}\nError output: {err}");
        }
        Error::Process {
            message,
            exit_code,
            stderr,
        }
    }

    /// Build a [`Error::JsonDecode`].
    pub fn json_decode(line: impl Into<String>, source: serde_json::Error) -> Self {
        Error::JsonDecode {
            line: line.into(),
            source: Arc::new(source),
        }
    }

    /// Build a [`Error::MessageParse`].
    pub fn message_parse(message: impl Into<String>, data: Option<Value>) -> Self {
        Error::MessageParse {
            message: message.into(),
            data,
        }
    }

    /// Build a [`Error::ControlProtocol`].
    pub fn control(message: impl Into<String>) -> Self {
        Error::ControlProtocol(message.into())
    }

    /// Build a [`Error::InvalidOptions`].
    pub fn invalid_options(message: impl Into<String>) -> Self {
        Error::InvalidOptions(message.into())
    }

    /// Build a [`Error::Callback`]. Use this from permission, hook and tool
    /// callbacks to report a failure; the SDK forwards the text to the CLI.
    pub fn callback(message: impl Into<String>) -> Self {
        Error::Callback(message.into())
    }

    /// Exit code of the CLI process for [`Error::Process`] and
    /// [`Error::Result`], `None` otherwise.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Error::Process { exit_code, .. } => *exit_code,
            Error::Result(result) => result.exit_code,
            _ => None,
        }
    }

    /// The structured result payload when this is an [`Error::Result`].
    pub fn as_result_error(&self) -> Option<&ResultError> {
        match self {
            Error::Result(result) => Some(result),
            _ => None,
        }
    }

    /// Whether this error means the CLI process itself failed
    /// ([`Error::Process`] or [`Error::Result`]).
    pub fn is_process_error(&self) -> bool {
        matches!(self, Error::Process { .. } | Error::Result(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::CliConnection(message) => f.write_str(message),
            Error::CliNotFound { message, .. } => f.write_str(message),
            Error::Process { message, .. } => f.write_str(message),
            Error::Result(result) => fmt::Display::fmt(result, f),
            Error::JsonDecode { line, .. } => {
                let preview: String = line.chars().take(100).collect();
                write!(f, "Failed to decode JSON: {preview}...")
            }
            Error::MessageParse { message, .. } => f.write_str(message),
            Error::ControlProtocol(message) => f.write_str(message),
            Error::ControlTimeout(subtype) => write!(f, "Control request timeout: {subtype}"),
            Error::InvalidOptions(message) => f.write_str(message),
            Error::Callback(message) => f.write_str(message),
            Error::Io(err) => fmt::Display::fmt(err, f),
            Error::Json(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::JsonDecode { source, .. } => Some(source.as_ref()),
            Error::Io(err) => Some(err.as_ref()),
            Error::Json(err) => Some(err.as_ref()),
            Error::Result(result) => Some(result.as_ref()),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(Arc::new(err))
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(Arc::new(err))
    }
}

/// Details of a terminal error `result` message, carried by [`Error::Result`].
///
/// The CLI ends a failed run by emitting a `result` message with
/// `is_error: true` (also yielded to you as a `ResultMessage`) and then exiting
/// non-zero. Instead of a bare "exit code 1" process error, the SDK raises this
/// so callers can branch on *why* the run failed without string matching.
#[derive(Debug, Clone)]
pub struct ResultError {
    /// Formatted message (includes the exit code when known).
    pub message: String,
    /// The result subtype (`error_max_turns`, `error_during_execution`, ... or
    /// `success` when the agent loop completed but the last turn was an API
    /// error).
    pub subtype: Option<String>,
    /// Error strings reported by the CLI (may be empty).
    pub errors: Vec<String>,
    /// The result text, if any. For API failures this holds the
    /// `"API Error: ..."` prose.
    pub result: Option<String>,
    /// HTTP status of the failing API call, if any.
    pub api_error_status: Option<i64>,
    /// Why the run ended (e.g. `api_error`, `max_turns`), if reported.
    pub terminal_reason: Option<String>,
    /// Session the result belongs to, if reported.
    pub session_id: Option<String>,
    /// The raw `result` message payload as emitted by the CLI.
    pub data: Value,
    /// Exit code of the CLI process.
    pub exit_code: Option<i32>,
}

impl ResultError {
    /// Build a `ResultError` from the raw result payload.
    pub fn new(message: impl Into<String>, data: Value, exit_code: Option<i32>) -> Self {
        let mut message = message.into();
        if let Some(code) = exit_code {
            message = format!("{message} (exit code: {code})");
        }
        let obj = data.as_object();
        let get_str = |key: &str| -> Option<String> {
            obj.and_then(|o| o.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        ResultError {
            message,
            subtype: get_str("subtype"),
            errors: normalize_result_errors(obj.and_then(|o| o.get("errors"))),
            result: get_str("result"),
            api_error_status: obj
                .and_then(|o| o.get("api_error_status"))
                .and_then(Value::as_i64),
            terminal_reason: get_str("terminal_reason"),
            session_id: get_str("session_id"),
            data,
            exit_code,
        }
    }
}

impl fmt::Display for ResultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ResultError {}

/// Normalize the `errors` field of a `result` frame to clean strings.
///
/// The CLI emits a list of strings; tolerate a bare string and drop non-string
/// or blank entries.
pub(crate) fn normalize_result_errors(raw: Option<&Value>) -> Vec<String> {
    match raw {
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                vec![]
            } else {
                vec![s.to_string()]
            }
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn process_error_message_includes_exit_code_and_stderr() {
        let err = Error::process("Command failed", Some(2), Some("boom".into()));
        assert_eq!(
            err.to_string(),
            "Command failed (exit code: 2)\nError output: boom"
        );
        assert_eq!(err.exit_code(), Some(2));
    }

    #[test]
    fn result_error_extracts_fields() {
        let data = json!({
            "subtype": "error_max_turns",
            "errors": ["too many turns", "  ", 5],
            "result": "stopped",
            "api_error_status": 529,
            "terminal_reason": "max_turns",
            "session_id": "abc",
        });
        let err = ResultError::new("Claude Code returned an error result", data, Some(1));
        assert_eq!(err.subtype.as_deref(), Some("error_max_turns"));
        assert_eq!(err.errors, vec!["too many turns".to_string()]);
        assert_eq!(err.api_error_status, Some(529));
        assert_eq!(err.terminal_reason.as_deref(), Some("max_turns"));
        assert_eq!(err.session_id.as_deref(), Some("abc"));
        assert!(err.message.ends_with("(exit code: 1)"));
    }

    #[test]
    fn normalize_errors_tolerates_bare_string() {
        assert_eq!(
            normalize_result_errors(Some(&json!(" oops "))),
            vec!["oops".to_string()]
        );
        assert!(normalize_result_errors(Some(&json!(42))).is_empty());
        assert!(normalize_result_errors(None).is_empty());
    }

    #[test]
    fn json_decode_display_truncates() {
        let long = "{".repeat(500);
        let source = serde_json::from_str::<Value>(&long).unwrap_err();
        let err = Error::json_decode(long, source);
        let text = err.to_string();
        assert!(text.starts_with("Failed to decode JSON: {{{{"));
        assert!(text.len() < 150);
    }
}
