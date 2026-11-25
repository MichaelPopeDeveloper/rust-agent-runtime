//! Subprocess transport: runs the `claude` CLI in streaming mode and frames
//! its NDJSON stdout.
//!
//! Ports the Python SDK's `SubprocessCLITransport`: CLI discovery, building the
//! argument list from [`ClaudeAgentOptions`], environment setup, line framing
//! with a buffer cap, the stderr callback, and graceful shutdown with a
//! terminate/kill escalation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::framing::{decode_frame, LineFramer};
use super::{MessageStream, Transport};
use crate::errors::{Error, Result};
use crate::types::{
    ClaudeAgentOptions, McpServers, Skills, SystemPrompt, ThinkingConfig, ToolsConfig,
};

/// Entrypoint label sent to the CLI in `CLAUDE_CODE_ENTRYPOINT`.
///
/// The bundled CLI recognizes `sdk-py`, `sdk-ts` and `sdk-cli` as SDK hosts;
/// this Rust SDK identifies as `sdk-cli`, the generic SDK entrypoint, since the
/// CLI has no Rust-specific value.
const SDK_ENTRYPOINT: &str = "sdk-cli";

/// Default per-message buffer cap (1 MiB), matching the Python SDK.
const DEFAULT_MAX_BUFFER_SIZE: usize = 1024 * 1024;

/// How long [`close`](SubprocessTransport::close) waits at each escalation step.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Subprocess transport using the Claude Code CLI.
pub struct SubprocessTransport {
    options: Arc<ClaudeAgentOptions>,
    cli_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
    max_buffer_size: usize,
    ready: Arc<AtomicBool>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Option<Child>>>,
    // The child's stdout, held until `take_message_stream` turns it into the
    // pull-based reader stream. A std mutex keeps the transport `Sync`.
    stdout: std::sync::Mutex<Option<ChildStdout>>,
    // The last lines of the child's stderr, for error reports.
    stderr_tail: StderrTail,
    // The stderr reader task, awaited at stdout EOF so the tail is complete
    // before a process failure is reported.
    stderr_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SubprocessTransport {
    /// Build a transport for these options. The process is not started until
    /// [`connect`](Transport::connect).
    pub fn new(options: Arc<ClaudeAgentOptions>) -> Self {
        let cli_path = options.cli_path.clone();
        let cwd = options.cwd.clone();
        let max_buffer_size = options.max_buffer_size.unwrap_or(DEFAULT_MAX_BUFFER_SIZE);
        SubprocessTransport {
            options,
            cli_path,
            cwd,
            max_buffer_size,
            ready: Arc::new(AtomicBool::new(false)),
            stdin: Arc::new(Mutex::new(None)),
            child: Arc::new(Mutex::new(None)),
            stdout: std::sync::Mutex::new(None),
            stderr_tail: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            stderr_task: std::sync::Mutex::new(None),
        }
    }

    /// Locate the `claude` executable, mirroring the Python search order.
    ///
    /// A successful discovery is cached process-wide: the install location does
    /// not change while the process runs, so a server spawning many sessions
    /// should not rescan `PATH` and stat candidate files on every spawn. A
    /// failure is not cached, so a CLI installed later is still found.
    fn find_cli() -> Result<PathBuf> {
        static DISCOVERED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        if let Some(path) = DISCOVERED.get() {
            return Ok(path.clone());
        }
        let path = Self::discover_cli()?;
        let _ = DISCOVERED.set(path.clone());
        Ok(path)
    }

    fn discover_cli() -> Result<PathBuf> {
        if let Ok(path) = which("claude") {
            return Ok(path);
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut candidates = vec![
            PathBuf::from("/usr/local/bin/claude"),
            PathBuf::from("/opt/claude-code/bin/claude"),
        ];
        if let Some(home) = home {
            candidates.extend([
                home.join(".npm-global/bin/claude"),
                home.join(".local/bin/claude"),
                home.join("node_modules/.bin/claude"),
                home.join(".yarn/bin/claude"),
                home.join(".claude/local/claude"),
            ]);
        }
        for path in candidates {
            if path.is_file() {
                return Ok(path);
            }
        }
        Err(Error::cli_not_found(
            "Claude Code not found. Install with:\n  npm install -g @anthropic-ai/claude-code\n\
             \nIf already installed locally, try:\n  export PATH=\"$HOME/node_modules/.bin:$PATH\"\n\
             \nOr set ClaudeAgentOptions.cli_path to the claude executable.",
            None,
        ))
    }

    /// Build the CLI argument list from the options (everything after argv[0]).
    ///
    /// Public within the crate so command-building can be unit tested without
    /// spawning anything.
    pub(crate) fn build_args(options: &ClaudeAgentOptions) -> Vec<String> {
        let mut cmd: Vec<String> = vec![
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];

        // System prompt.
        match &options.system_prompt {
            None => cmd.extend(["--system-prompt".into(), String::new()]),
            Some(SystemPrompt::Text(text)) => cmd.extend(["--system-prompt".into(), text.clone()]),
            Some(SystemPrompt::Custom { prompt, .. }) => {
                cmd.extend(["--system-prompt".into(), prompt.clone()])
            }
            Some(SystemPrompt::File(path)) => cmd.extend([
                "--system-prompt-file".into(),
                path.to_string_lossy().into_owned(),
            ]),
            Some(SystemPrompt::Preset { append, .. }) => {
                if let Some(append) = append {
                    cmd.extend(["--append-system-prompt".into(), append.clone()]);
                }
            }
        }

        // Base tool set.
        if let Some(tools) = &options.tools {
            match tools {
                ToolsConfig::List(list) => cmd.extend(["--tools".into(), list.join(",")]),
                ToolsConfig::Preset(_) => cmd.extend(["--tools".into(), "default".into()]),
            }
        }

        // allowed_tools, extended with skill rules when `skills` is set.
        let (allowed_tools, setting_sources) = Self::apply_skills_defaults(options);
        if !allowed_tools.is_empty() {
            cmd.extend(["--allowedTools".into(), allowed_tools.join(",")]);
        }

        if let Some(max_turns) = options.max_turns {
            cmd.extend(["--max-turns".into(), max_turns.to_string()]);
        }
        if let Some(budget) = options.max_budget_usd {
            cmd.extend(["--max-budget-usd".into(), budget.to_string()]);
        }
        if !options.disallowed_tools.is_empty() {
            cmd.extend([
                "--disallowedTools".into(),
                options.disallowed_tools.join(","),
            ]);
        }
        if let Some(budget) = &options.task_budget {
            cmd.extend(["--task-budget".into(), budget.total.to_string()]);
        }
        if let Some(model) = &options.model {
            cmd.extend(["--model".into(), model.clone()]);
        }
        if let Some(model) = &options.fallback_model {
            cmd.extend(["--fallback-model".into(), model.clone()]);
        }
        if !options.betas.is_empty() {
            cmd.extend(["--betas".into(), options.betas.join(",")]);
        }
        if let Some(name) = &options.permission_prompt_tool_name {
            cmd.extend(["--permission-prompt-tool".into(), name.clone()]);
        }
        if let Some(mode) = options.permission_mode {
            cmd.extend(["--permission-mode".into(), mode.as_str().into()]);
        }
        if options.continue_conversation {
            cmd.push("--continue".into());
        }
        // `--flag=value` form so a dash-leading value can't be parsed as its
        // own flag (matches the Python SDK's injection guard).
        if let Some(resume) = &options.resume {
            cmd.push(format!("--resume={resume}"));
        }
        if let Some(session_id) = &options.session_id {
            cmd.push(format!("--session-id={session_id}"));
        }

        if let Some(settings) = Self::build_settings_value(options) {
            cmd.extend(["--settings".into(), settings]);
        }

        for dir in &options.add_dirs {
            cmd.extend(["--add-dir".into(), dir.to_string_lossy().into_owned()]);
        }

        // MCP servers.
        if let Some(servers) = &options.mcp_servers {
            if !servers.is_empty() {
                match servers {
                    McpServers::Map(map) => {
                        let mut for_cli = serde_json::Map::new();
                        for (name, config) in map {
                            for_cli.insert(name.clone(), config.to_cli_json());
                        }
                        let wrapper = serde_json::json!({ "mcpServers": for_cli });
                        cmd.extend(["--mcp-config".into(), wrapper.to_string()]);
                    }
                    McpServers::Path(path) => {
                        cmd.extend(["--mcp-config".into(), path.to_string_lossy().into_owned()])
                    }
                    McpServers::Json(json) => cmd.extend(["--mcp-config".into(), json.clone()]),
                }
            }
        }

        if options.include_partial_messages {
            cmd.push("--include-partial-messages".into());
        }
        if options.include_hook_events {
            cmd.push("--include-hook-events".into());
        }
        if options.strict_mcp_config {
            cmd.push("--strict-mcp-config".into());
        }
        if options.fork_session {
            cmd.push("--fork-session".into());
        }
        if let Some(at) = &options.resume_session_at {
            cmd.push(format!("--resume-session-at={at}"));
        }
        if let Some(drops) = &options.resume_drops_turn {
            cmd.push(format!("--resume-drops-turn={drops}"));
        }

        if let Some(sources) = setting_sources {
            let joined = sources
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",");
            cmd.push(format!("--setting-sources={joined}"));
        }

        for plugin in &options.plugins {
            let crate::types::SdkPluginConfig::Local { path } = plugin;
            cmd.extend(["--plugin-dir".into(), path.to_string_lossy().into_owned()]);
        }

        // Extra args (forward-compat escape hatch).
        for (flag, value) in &options.extra_args {
            match value {
                None => cmd.push(format!("--{flag}")),
                Some(value) if value.starts_with('-') => cmd.push(format!("--{flag}={value}")),
                Some(value) => cmd.extend([format!("--{flag}"), value.clone()]),
            }
        }

        // Thinking / effort.
        match &options.thinking {
            Some(ThinkingConfig::Adaptive { display }) => {
                cmd.extend(["--thinking".into(), "adaptive".into()]);
                if let Some(display) = display {
                    cmd.extend(["--thinking-display".into(), display.as_str().into()]);
                }
            }
            Some(ThinkingConfig::Enabled {
                budget_tokens,
                display,
            }) => {
                cmd.extend(["--max-thinking-tokens".into(), budget_tokens.to_string()]);
                if let Some(display) = display {
                    cmd.extend(["--thinking-display".into(), display.as_str().into()]);
                }
            }
            Some(ThinkingConfig::Disabled) => cmd.extend(["--thinking".into(), "disabled".into()]),
            None => {
                if let Some(tokens) = options.max_thinking_tokens {
                    cmd.extend(["--max-thinking-tokens".into(), tokens.to_string()]);
                }
            }
        }

        if let Some(effort) = options.effort {
            cmd.extend(["--effort".into(), effort.as_str().into()]);
        }

        if let Some(crate::types::OutputFormat::JsonSchema { schema }) = &options.output_format {
            cmd.extend(["--json-schema".into(), schema.to_string()]);
        }

        // Always streaming stdin: lets agents and large configs go via the
        // initialize request.
        cmd.extend(["--input-format".into(), "stream-json".into()]);

        cmd
    }

    /// Compute the effective `allowed_tools` and `setting_sources` for skills.
    ///
    /// `skills = "all"` injects the bare `Skill` tool; a list injects
    /// `Skill(name)` for each entry; either defaults `setting_sources` to
    /// `["user", "project"]` when unset so the CLI discovers skills.
    fn apply_skills_defaults(
        options: &ClaudeAgentOptions,
    ) -> (Vec<String>, Option<Vec<crate::types::SettingSource>>) {
        let mut allowed = options.allowed_tools.clone();
        let mut sources = options.setting_sources.clone();
        match &options.skills {
            None => {}
            Some(Skills::All) => {
                if !allowed.iter().any(|t| t == "Skill") {
                    allowed.push("Skill".into());
                }
                if sources.is_none() {
                    sources = Some(vec![
                        crate::types::SettingSource::User,
                        crate::types::SettingSource::Project,
                    ]);
                }
            }
            Some(Skills::List(names)) => {
                for name in names {
                    let rule = format!("Skill({name})");
                    if !allowed.contains(&rule) {
                        allowed.push(rule);
                    }
                }
                if sources.is_none() {
                    sources = Some(vec![
                        crate::types::SettingSource::User,
                        crate::types::SettingSource::Project,
                    ]);
                }
            }
        }
        (allowed, sources)
    }

    /// Merge `sandbox` into the `--settings` JSON, if present.
    fn build_settings_value(options: &ClaudeAgentOptions) -> Option<String> {
        let settings = options.settings.clone();
        let Some(sandbox) = &options.sandbox else {
            return settings;
        };
        let mut obj: serde_json::Map<String, Value> = match &settings {
            Some(text) => {
                let trimmed = text.trim();
                if trimmed.starts_with('{') && trimmed.ends_with('}') {
                    serde_json::from_str(trimmed).unwrap_or_default()
                } else {
                    // A path: read it relative to cwd, like the CLI does.
                    let path = match &options.cwd {
                        Some(cwd) => cwd.join(trimmed),
                        None => PathBuf::from(trimmed),
                    };
                    std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default()
                }
            }
            None => serde_json::Map::new(),
        };
        obj.insert(
            "sandbox".into(),
            serde_json::to_value(sandbox).unwrap_or(Value::Null),
        );
        Some(Value::Object(obj).to_string())
    }

    /// The SDK-controlled environment variables to set on the child, over the
    /// inherited environment.
    ///
    /// This returns *only* the overrides, not a copy of the whole environment:
    /// the child inherits the parent's env from `Command` by default, so there
    /// is no reason to clone every inherited variable on each spawn just to
    /// change a handful (the old code did, which was wasteful for a process
    /// that spawns many sessions). `CLAUDECODE` is dropped separately with
    /// `Command::env_remove` in `connect`.
    fn env_overrides(options: &ClaudeAgentOptions) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = Vec::new();
        let set = |env: &mut Vec<(String, String)>, key: &str, value: String| {
            env.retain(|(k, _)| k != key);
            env.push((key.to_string(), value));
        };
        set(&mut env, "CLAUDE_CODE_ENTRYPOINT", SDK_ENTRYPOINT.into());
        for (key, value) in &options.env {
            set(&mut env, key, value.clone());
        }
        set(
            &mut env,
            "CLAUDE_AGENT_SDK_VERSION",
            env!("CARGO_PKG_VERSION").into(),
        );
        if let Some(app) = &options.client_app {
            set(&mut env, "CLAUDE_AGENT_SDK_CLIENT_APP", app.clone());
        }
        if options.enable_file_checkpointing {
            set(
                &mut env,
                "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
                "true".into(),
            );
        }
        if let Some(cwd) = &options.cwd {
            set(&mut env, "PWD", cwd.to_string_lossy().into_owned());
        }
        env
    }
}

/// State for the pull-based stdout reader stream.
struct ReadState {
    reader: ChildStdout,
    framer: LineFramer,
    chunk: Vec<u8>,
    child: Arc<Mutex<Option<Child>>>,
    stderr_tail: StderrTail,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    eof: bool,
    tail_flushed: bool,
    done: bool,
}

/// The last lines of the child's stderr, shared between the stderr reader and
/// the stdout stream (which attaches them to a process failure).
type StderrTail = Arc<std::sync::Mutex<std::collections::VecDeque<String>>>;

/// How many trailing stderr lines are kept for error reports.
const STDERR_TAIL_LINES: usize = 64;

fn push_tail(tail: &StderrTail, line: &str) {
    if line.is_empty() {
        return;
    }
    let mut tail = tail.lock().unwrap();
    if tail.len() == STDERR_TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line.to_string());
}

fn stderr_text(tail: &StderrTail) -> String {
    let tail = tail.lock().unwrap();
    if tail.is_empty() {
        "no stderr output captured".to_string()
    } else {
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// Run the child as `user` (a numeric uid or a user name).
#[cfg(unix)]
fn apply_user(command: &mut Command, user: &str) -> Result<()> {
    command.uid(resolve_uid(user)?);
    Ok(())
}

#[cfg(not(unix))]
fn apply_user(_command: &mut Command, _user: &str) -> Result<()> {
    Err(Error::invalid_options(
        "the `user` option is only supported on Unix",
    ))
}

/// Resolve a numeric uid or a user name (via `getpwnam`) to a uid.
#[cfg(unix)]
fn resolve_uid(user: &str) -> Result<u32> {
    if let Ok(uid) = user.parse::<u32>() {
        return Ok(uid);
    }
    let name = std::ffi::CString::new(user)
        .map_err(|_| Error::invalid_options(format!("invalid user name: {user:?}")))?;
    // SAFETY: `getpwnam` takes a valid NUL-terminated string and returns null or
    // a pointer to a passwd record valid until the next such call; `pw_uid` is
    // read from it immediately and nothing else is retained.
    let record = unsafe { libc::getpwnam(name.as_ptr()) };
    if record.is_null() {
        return Err(Error::invalid_options(format!("unknown user: {user}")));
    }
    Ok(unsafe { (*record).pw_uid })
}

#[async_trait]
impl Transport for SubprocessTransport {
    async fn connect(&mut self) -> Result<()> {
        if self.child.lock().await.is_some() {
            return Ok(());
        }
        let cli_path = match &self.cli_path {
            Some(path) => path.clone(),
            None => {
                let path = Self::find_cli()?;
                self.cli_path = Some(path.clone());
                path
            }
        };

        let args = Self::build_args(&self.options);

        let mut command = Command::new(&cli_path);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Always capture stderr: its last lines are attached to a process
            // failure so it is diagnosable even without a stderr callback.
            .stderr(Stdio::piped())
            // Inherit the parent environment, drop CLAUDECODE so the child
            // doesn't think it is nested, and apply only the SDK overrides.
            .env_remove("CLAUDECODE")
            .envs(Self::env_overrides(&self.options))
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if let Some(user) = &self.options.user {
            apply_user(&mut command, user)?;
        }

        let mut child = command.spawn().map_err(|e| {
            if self.cwd.as_ref().is_some_and(|c| !c.exists()) {
                Error::cli_connection(format!(
                    "Working directory does not exist: {}",
                    self.cwd.as_ref().unwrap().display()
                ))
            } else if e.kind() == std::io::ErrorKind::NotFound {
                Error::cli_not_found(
                    "Claude Code not found",
                    Some(cli_path.to_string_lossy().into_owned()),
                )
            } else {
                Error::cli_connection(format!("Failed to start Claude Code: {e}"))
            }
        })?;

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::cli_connection("child stdout was not piped"))?;
        let stderr = child.stderr.take();

        *self.stdin.lock().await = stdin;

        // Stderr reader: always on. Each line goes to the callback (if any) and
        // into a bounded tail that is attached to a process failure. Draining
        // the pipe also keeps a chatty child from blocking on a full stderr.
        if let Some(stderr) = stderr {
            let callback = self.options.stderr.clone();
            let stderr_tail = self.stderr_tail.clone();
            let max = self.max_buffer_size;
            let stderr_task = tokio::spawn(async move {
                let mut reader = stderr;
                let mut framer = LineFramer::new(max);
                let mut chunk = [0u8; 8192];
                let emit = |line: &str| {
                    if let Some(callback) = &callback {
                        callback.call(line);
                    }
                    push_tail(&stderr_tail, line);
                };
                loop {
                    match reader.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            framer.push(&chunk[..n]);
                            while let Some(line) = framer.next_line() {
                                let text = String::from_utf8_lossy(line);
                                emit(text.trim_end());
                            }
                            if framer.overflowed() {
                                let text = String::from_utf8_lossy(framer.tail());
                                emit(text.trim_end());
                                framer.reset_tail();
                            }
                            framer.compact();
                        }
                    }
                }
                let tail = framer.tail();
                if !tail.is_empty() {
                    let text = String::from_utf8_lossy(tail);
                    emit(text.trim_end());
                }
            });
            *self.stderr_task.lock().unwrap() = Some(stderr_task);
        }

        // No reader task and no intermediate channel: `take_message_stream`
        // returns a pull-based stream that reads the pipe, frames and decodes
        // lazily as the engine polls it. That is one task and one channel hop
        // fewer per frame than an eager reader feeding a queue.
        *self.stdout.lock().unwrap() = Some(stdout);

        *self.child.lock().await = Some(child);
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn take_message_stream(&mut self) -> Option<MessageStream> {
        let stdout = self.stdout.lock().unwrap().take()?;
        let state = ReadState {
            reader: stdout,
            framer: LineFramer::new(self.max_buffer_size),
            chunk: vec![0u8; 16 * 1024],
            child: self.child.clone(),
            stderr_tail: self.stderr_tail.clone(),
            stderr_task: self.stderr_task.lock().unwrap().take(),
            eof: false,
            tail_flushed: false,
            done: false,
        };
        let stream = futures::stream::unfold(state, |mut st| async move {
            loop {
                if st.done {
                    return None;
                }
                // Hand out any complete line already buffered. The borrow of
                // the framer ends at the `;`, so `st` can be moved below.
                let next = st.framer.next_line().map(decode_frame);
                match next {
                    Some(Some(Ok(value))) => return Some((Ok(value), st)),
                    Some(Some(Err(err))) => {
                        st.done = true;
                        return Some((Err(err), st));
                    }
                    // Blank or non-JSON diagnostic line: skip it.
                    Some(None) => continue,
                    None => {}
                }
                if st.eof {
                    if !st.tail_flushed {
                        // A trailing line with no newline.
                        st.tail_flushed = true;
                        if let Some(Ok(value)) = decode_frame(st.framer.tail()) {
                            return Some((Ok(value), st));
                        }
                    }
                    // stdout closed: report a non-zero exit as an error.
                    st.done = true;
                    let status = {
                        let mut guard = st.child.lock().await;
                        match guard.as_mut() {
                            Some(child) => child.wait().await.ok(),
                            None => None,
                        }
                    };
                    if let Some(status) = status {
                        if !status.success() {
                            // Let the stderr reader finish draining so the
                            // tail attached below is complete (bounded wait).
                            if let Some(task) = st.stderr_task.take() {
                                let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
                            }
                            let code = status.code();
                            let err = Error::process(
                                format!(
                                    "Command failed with exit code {}",
                                    code.map(|c| c.to_string())
                                        .unwrap_or_else(|| "unknown".into())
                                ),
                                code,
                                Some(stderr_text(&st.stderr_tail)),
                            );
                            return Some((Err(err), st));
                        }
                    }
                    return None;
                }
                // Nothing complete is buffered: compact (resets when fully
                // consumed) and read more from the pipe.
                st.framer.compact();
                match st.reader.read(&mut st.chunk).await {
                    Ok(0) | Err(_) => st.eof = true,
                    Ok(n) => {
                        st.framer.push(&st.chunk[..n]);
                        // Enforce the per-message cap before handing out a
                        // line, so an oversized frame is never parsed.
                        if st.framer.overflowed() {
                            st.done = true;
                            let err = Error::json_decode(
                                String::from_utf8_lossy(st.framer.tail()).into_owned(),
                                de_error("JSON message exceeded the maximum buffer size"),
                            );
                            return Some((Err(err), st));
                        }
                    }
                }
            }
        });
        Some(Box::pin(stream))
    }

    async fn write(&self, data: String) -> Result<()> {
        if !self.ready.load(Ordering::SeqCst) {
            return Err(Error::cli_connection("transport is not ready for writing"));
        }
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| Error::cli_connection("transport stdin is closed"))?;
        stdin
            .write_all(data.as_bytes())
            .await
            .map_err(|e| Error::cli_connection(format!("Failed to write to process stdin: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| Error::cli_connection(format!("Failed to flush process stdin: {e}")))?;
        Ok(())
    }

    async fn end_input(&self) -> Result<()> {
        // Dropping stdin closes it (EOF to the CLI).
        self.stdin.lock().await.take();
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.ready.store(false, Ordering::SeqCst);
        self.stdin.lock().await.take();

        let mut guard = self.child.lock().await;
        let Some(mut child) = guard.take() else {
            return Ok(());
        };
        // Graceful: wait, then SIGTERM, then SIGKILL, each bounded.
        if tokio::time::timeout(CLOSE_GRACE, child.wait())
            .await
            .is_ok()
        {
            return Ok(());
        }
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            // SIGTERM first (tokio's kill() sends SIGKILL).
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            if tokio::time::timeout(CLOSE_GRACE, child.wait())
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
        let _ = child.start_kill();
        let _ = tokio::time::timeout(CLOSE_GRACE, child.wait()).await;
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

fn de_error(message: &str) -> serde_json::Error {
    serde::de::Error::custom(message)
}

/// Minimal `which`: find an executable named `name` on `PATH`.
fn which(name: &str) -> Result<PathBuf> {
    let path_var =
        std::env::var_os("PATH").ok_or_else(|| Error::cli_not_found("PATH is not set", None))?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(Error::cli_not_found(
        format!("{name} not found on PATH"),
        None,
    ))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClaudeAgentOptions, EffortLevel, PermissionMode, SystemPrompt};

    fn args_for(options: ClaudeAgentOptions) -> Vec<String> {
        SubprocessTransport::build_args(&options)
    }

    #[test]
    fn base_args_are_streaming() {
        let args = args_for(ClaudeAgentOptions::default());
        assert!(args
            .windows(2)
            .any(|w| w == ["--output-format", "stream-json"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--input-format", "stream-json"]));
        assert!(args.iter().any(|a| a == "--verbose"));
        // No system prompt => empty --system-prompt.
        assert!(args.windows(2).any(|w| w == ["--system-prompt", ""]));
    }

    #[test]
    fn builds_common_options() {
        let options = ClaudeAgentOptions::builder()
            .model("claude-opus-5-5")
            .permission_mode(PermissionMode::AcceptEdits)
            .allowed_tools(["Read", "Bash"])
            .disallowed_tools(["WebFetch"])
            .max_turns(3)
            .effort(EffortLevel::High)
            .system_prompt(SystemPrompt::text("be terse"))
            .build();
        let args = args_for(options);
        assert!(args.windows(2).any(|w| w == ["--model", "claude-opus-5-5"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--allowedTools", "Read,Bash"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--disallowedTools", "WebFetch"]));
        assert!(args.windows(2).any(|w| w == ["--max-turns", "3"]));
        assert!(args.windows(2).any(|w| w == ["--effort", "high"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--system-prompt", "be terse"]));
    }

    #[test]
    fn preset_system_prompt_appends() {
        let options = ClaudeAgentOptions::builder()
            .system_prompt(SystemPrompt::claude_code_with_append("extra"))
            .build();
        let args = args_for(options);
        assert!(args
            .windows(2)
            .any(|w| w == ["--append-system-prompt", "extra"]));
        assert!(!args.iter().any(|a| a == "--system-prompt"));
    }

    #[test]
    fn resume_uses_equals_form() {
        let options = ClaudeAgentOptions::builder().resume("-rm -rf").build();
        let args = args_for(options);
        assert!(args.iter().any(|a| a == "--resume=-rm -rf"));
    }

    #[test]
    fn skills_all_injects_skill_tool_and_sources() {
        let options = ClaudeAgentOptions::builder().skills(Skills::All).build();
        let args = args_for(options);
        let allowed = args
            .windows(2)
            .find(|w| w[0] == "--allowedTools")
            .map(|w| w[1].clone())
            .unwrap();
        assert!(allowed.split(',').any(|t| t == "Skill"));
        assert!(args.iter().any(|a| a.starts_with("--setting-sources=")));
    }

    #[test]
    fn skills_list_injects_named_rules() {
        let options = ClaudeAgentOptions::builder()
            .skills(Skills::List(vec!["pdf".into(), "xlsx".into()]))
            .build();
        let args = args_for(options);
        let allowed = args
            .windows(2)
            .find(|w| w[0] == "--allowedTools")
            .map(|w| w[1].clone())
            .unwrap();
        assert!(allowed.contains("Skill(pdf)"));
        assert!(allowed.contains("Skill(xlsx)"));
    }

    #[test]
    fn thinking_and_output_format() {
        let options = ClaudeAgentOptions::builder()
            .thinking(ThinkingConfig::adaptive())
            .output_format(crate::types::OutputFormat::json_schema(
                serde_json::json!({"type": "object"}),
            ))
            .build();
        let args = args_for(options);
        assert!(args.windows(2).any(|w| w == ["--thinking", "adaptive"]));
        assert!(args.iter().any(|a| a == "--json-schema"));
    }

    #[test]
    fn extra_args_dash_value_uses_equals() {
        let options = ClaudeAgentOptions::builder()
            .extra_arg("flag", Some("-x".into()))
            .extra_arg("bare", None)
            .build();
        let args = args_for(options);
        assert!(args.iter().any(|a| a == "--flag=-x"));
        assert!(args.iter().any(|a| a == "--bare"));
    }

    #[test]
    fn env_overrides_set_entrypoint_and_version() {
        let env = SubprocessTransport::env_overrides(&ClaudeAgentOptions::default());
        assert!(env
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_ENTRYPOINT" && v == "sdk-cli"));
        assert!(env.iter().any(|(k, _)| k == "CLAUDE_AGENT_SDK_VERSION"));
        // Overrides carry only SDK vars, not a copy of the inherited env.
        assert!(!env.iter().any(|(k, _)| k == "CLAUDECODE"));
        assert!(env.len() < 10);
    }

    #[test]
    fn can_use_tool_routes_permission_prompts_over_stdio() {
        // The flag the CLI needs in order to send `can_use_tool` control
        // requests at all; without it the callback is never consulted.
        let options = ClaudeAgentOptions::builder()
            .can_use_tool(crate::CanUseTool::new(|_, _, _| async {
                Ok(crate::PermissionResult::allow())
            }))
            .build()
            .configured_for_can_use_tool()
            .unwrap();
        let args = args_for(options);
        assert!(args
            .windows(2)
            .any(|w| w == ["--permission-prompt-tool", "stdio"]));
    }

    #[test]
    fn options_env_can_override_entrypoint_but_not_version() {
        let options = ClaudeAgentOptions::builder()
            .env("CLAUDE_CODE_ENTRYPOINT", "custom")
            .build();
        let env = SubprocessTransport::env_overrides(&options);
        let entrypoint: Vec<_> = env
            .iter()
            .filter(|(k, _)| k == "CLAUDE_CODE_ENTRYPOINT")
            .collect();
        assert_eq!(entrypoint.len(), 1);
        assert_eq!(entrypoint[0].1, "custom");
    }

    #[test]
    fn sandbox_merges_into_settings() {
        let sandbox = crate::types::SandboxSettings {
            enabled: Some(true),
            ..Default::default()
        };
        let options = ClaudeAgentOptions::builder().sandbox(sandbox).build();
        let args = args_for(options);
        let settings = args
            .windows(2)
            .find(|w| w[0] == "--settings")
            .map(|w| w[1].clone())
            .unwrap();
        let parsed: Value = serde_json::from_str(&settings).unwrap();
        assert_eq!(parsed["sandbox"]["enabled"], true);
    }
}
