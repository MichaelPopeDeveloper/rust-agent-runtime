//! [`ClaudeAgentOptions`] and its builder.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use super::common::{
    EffortLevel, OutputFormat, SandboxSettings, SdkPluginConfig, SettingSource, Skills,
    SystemPrompt, TaskBudget, ThinkingConfig, ToolsConfig,
};
use super::hooks::{HookEvent, HookMatcher};
use super::mcp_config::McpServers;
use super::permissions::CanUseTool;
use super::{AgentDefinition, PermissionMode};

/// Callback for a line of the CLI subprocess's stderr. Useful for logging.
#[derive(Clone)]
pub struct StderrCallback(Arc<dyn Fn(&str) + Send + Sync>);

impl StderrCallback {
    /// Wrap a function `(line) -> ()`.
    pub fn new<F: Fn(&str) + Send + Sync + 'static>(f: F) -> Self {
        StderrCallback(Arc::new(f))
    }

    pub(crate) fn call(&self, line: &str) {
        (self.0)(line)
    }
}

impl fmt::Debug for StderrCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StderrCallback(<callback>)")
    }
}

/// Beta features that can be enabled for a session.
pub type SdkBeta = String;

/// Options for a `query()` or [`ClaudeSDKClient`](crate::ClaudeSDKClient)
/// session.
///
/// Construct with [`ClaudeAgentOptions::builder`] and set only what you need;
/// every field has a sensible default. The field semantics match the Python
/// SDK's `ClaudeAgentOptions`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeAgentOptions {
    /// Base set of built-in tools. `None` leaves the CLI default.
    pub tools: Option<ToolsConfig>,
    /// Tool names auto-allowed without prompting.
    pub allowed_tools: Vec<String>,
    /// Tool names that are disallowed.
    pub disallowed_tools: Vec<String>,
    /// System prompt. `None` means an *empty* prompt, not the CLI default; use
    /// [`SystemPrompt::claude_code`] for the default.
    pub system_prompt: Option<SystemPrompt>,
    /// MCP server configurations.
    pub mcp_servers: Option<McpServers>,
    /// Only use MCP servers from `mcp_servers`, ignoring other configs.
    pub strict_mcp_config: bool,
    /// Permission mode.
    pub permission_mode: Option<PermissionMode>,
    /// Continue the most recent conversation in `cwd`.
    pub continue_conversation: bool,
    /// Session ID to resume.
    pub resume: Option<String>,
    /// Use a specific session ID (must be a UUID).
    pub session_id: Option<String>,
    /// Maximum number of conversation turns.
    pub max_turns: Option<u32>,
    /// Maximum budget in USD.
    pub max_budget_usd: Option<f64>,
    /// Claude model to use.
    pub model: Option<String>,
    /// Fallback model if the primary fails or is unavailable.
    pub fallback_model: Option<String>,
    /// Beta features to enable.
    pub betas: Vec<SdkBeta>,
    /// MCP tool name to route permission prompts through (mutually exclusive
    /// with [`can_use_tool`](Self::can_use_tool)).
    pub permission_prompt_tool_name: Option<String>,
    /// Working directory for the session.
    pub cwd: Option<PathBuf>,
    /// Path to the Claude Code CLI executable (otherwise auto-discovered).
    pub cli_path: Option<PathBuf>,
    /// Additional settings JSON file path or inline JSON string.
    pub settings: Option<String>,
    /// Extra directories Claude can access.
    pub add_dirs: Vec<PathBuf>,
    /// Environment variables for the subprocess (merged over the inherited
    /// environment).
    pub env: HashMap<String, String>,
    /// Extra CLI arguments (name without `--`; `None` value = boolean flag).
    pub extra_args: HashMap<String, Option<String>>,
    /// Max bytes to buffer per line of CLI stdout/stderr.
    pub max_buffer_size: Option<usize>,
    /// Callback for subprocess stderr lines.
    pub stderr: Option<StderrCallback>,
    /// Custom permission handler. Setting this routes prompts over stdio.
    pub can_use_tool: Option<CanUseTool>,
    /// Hook callbacks, by event.
    pub hooks: Option<HashMap<HookEvent, Vec<HookMatcher>>>,
    /// Optional user identifier for the session.
    pub user: Option<String>,
    /// Emit partial-message [`StreamEvent`](crate::StreamEvent)s.
    pub include_partial_messages: bool,
    /// Emit hook lifecycle events as system messages.
    pub include_hook_events: bool,
    /// Forward subagent text/thinking blocks, not just tool_use/tool_result.
    pub forward_subagent_text: bool,
    /// Deliver every prompt to Claude verbatim (no `@path` expansion or slash
    /// command dispatch). Requires Claude Code 2.1.248+.
    pub verbatim_prompts: bool,
    /// Resumed sessions fork to a new session ID.
    pub fork_session: bool,
    /// When resuming, load only up to this message UUID.
    pub resume_session_at: Option<String>,
    /// With `resume_session_at`, the prompt UUID whose turn is dropped.
    pub resume_drops_turn: Option<String>,
    /// Programmatically defined subagents, by name.
    pub agents: Option<HashMap<String, AgentDefinition>>,
    /// Which filesystem settings to load. `None` loads all; `Some(vec![])`
    /// disables filesystem settings.
    pub setting_sources: Option<Vec<SettingSource>>,
    /// Skills to enable for the main session.
    pub skills: Option<Skills>,
    /// Sandbox settings for command execution isolation.
    pub sandbox: Option<SandboxSettings>,
    /// Plugins to load.
    pub plugins: Vec<SdkPluginConfig>,
    /// Deprecated: maximum thinking tokens. Prefer [`thinking`](Self::thinking).
    pub max_thinking_tokens: Option<u64>,
    /// Thinking configuration.
    pub thinking: Option<ThinkingConfig>,
    /// Effort level.
    pub effort: Option<EffortLevel>,
    /// Structured output format.
    pub output_format: Option<OutputFormat>,
    /// Enable file checkpointing (for `rewind_files`).
    pub enable_file_checkpointing: bool,
    /// API-side task budget in tokens.
    pub task_budget: Option<TaskBudget>,
    /// Your app/library identifier (e.g. `my-app/1.0.0`), sent on API requests.
    pub client_app: Option<String>,
}

impl ClaudeAgentOptions {
    /// A fresh builder.
    pub fn builder() -> ClaudeAgentOptionsBuilder {
        ClaudeAgentOptionsBuilder::default()
    }

    /// Whether the control channel may still receive requests needing a reply
    /// (SDK MCP servers, hooks, or `can_use_tool`). Mirrors the Python SDK's
    /// `_has_bidirectional_needs`.
    pub(crate) fn has_bidirectional_needs(&self) -> bool {
        let has_sdk_mcp = self
            .mcp_servers
            .as_ref()
            .map(|servers| !servers.sdk_servers().is_empty())
            .unwrap_or(false);
        let has_hooks = self
            .hooks
            .as_ref()
            .map(|hooks| !hooks.is_empty())
            .unwrap_or(false);
        has_sdk_mcp || has_hooks || self.can_use_tool.is_some()
    }

    /// Validate `can_use_tool` and route permission prompts over stdio.
    ///
    /// Mirrors the Python SDK's `_configure_can_use_tool`, applied by `query()`
    /// and the client before the options become CLI flags: with a callback set,
    /// the CLI must be told `--permission-prompt-tool stdio` so it sends
    /// `can_use_tool` requests over the control protocol — without that the
    /// callback is never invoked. An explicit, different
    /// `permission_prompt_tool_name` is an error (one or the other); `"stdio"`
    /// is accepted so this is idempotent. Also warns (via `tracing`) when other
    /// options visibly shadow the callback.
    pub(crate) fn configured_for_can_use_tool(mut self) -> crate::Result<Self> {
        if self.can_use_tool.is_none() {
            return Ok(self);
        }
        match self.permission_prompt_tool_name.as_deref() {
            None | Some("stdio") => {}
            Some(_) => {
                return Err(crate::Error::invalid_options(
                    "can_use_tool cannot be combined with permission_prompt_tool_name; \
                     use one or the other.",
                ))
            }
        }
        if self.permission_mode == Some(PermissionMode::BypassPermissions) {
            tracing::warn!(
                "can_use_tool will not be invoked: permission_mode bypassPermissions \
                 auto-approves every tool call (except explicit deny rules) before the \
                 callback is consulted. To gate every tool call, use a PreToolUse hook instead."
            );
        } else {
            let mut shadowed: Vec<&str> = Vec::new();
            for entry in &self.allowed_tools {
                if let Some(tool) = whole_tool_allowed(entry) {
                    if !shadowed.contains(&tool) {
                        shadowed.push(tool);
                    }
                }
            }
            if matches!(self.skills, Some(Skills::All)) && !shadowed.contains(&"Skill") {
                shadowed.push("Skill");
            }
            if !shadowed.is_empty() {
                tracing::warn!(
                    "can_use_tool will not be invoked for: {}. An allowed_tools entry that \
                     allows a whole tool auto-approves it before the callback is consulted. To \
                     gate every tool call, use a PreToolUse hook; or narrow the entry so calls \
                     fall through to can_use_tool.",
                    shadowed.join(", ")
                );
            }
        }
        self.permission_prompt_tool_name = Some("stdio".into());
        Ok(self)
    }
}

/// The tool an `allowed_tools` entry allows outright, else `None`.
///
/// Mirrors the CLI's rule parser: an entry allows a whole tool when it has no
/// `(...)` specifier (`Read`), or when the specifier is empty or a lone
/// wildcard (`Read()`, `Read(*)`). A real specifier (`Bash(ls:*)`) only allows
/// matching invocations.
fn whole_tool_allowed(entry: &str) -> Option<&str> {
    if entry.trim().is_empty() {
        return None;
    }
    match entry.find('(') {
        None => Some(entry),
        Some(0) => None,
        Some(open) => {
            if !entry.ends_with(')') {
                return None;
            }
            let spec = &entry[open + 1..entry.len() - 1];
            if spec.is_empty() || spec == "*" {
                Some(&entry[..open])
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_use_tool_routes_prompts_over_stdio() {
        let options = ClaudeAgentOptions::builder()
            .can_use_tool(CanUseTool::new(|_, _, _| async {
                Ok(crate::PermissionResult::allow())
            }))
            .build();
        let configured = options.configured_for_can_use_tool().unwrap();
        assert_eq!(
            configured.permission_prompt_tool_name.as_deref(),
            Some("stdio")
        );
        // Idempotent: configuring again is fine.
        let again = configured.configured_for_can_use_tool().unwrap();
        assert_eq!(again.permission_prompt_tool_name.as_deref(), Some("stdio"));
    }

    #[test]
    fn can_use_tool_conflicts_with_other_prompt_tool() {
        let options = ClaudeAgentOptions::builder()
            .can_use_tool(CanUseTool::new(|_, _, _| async {
                Ok(crate::PermissionResult::allow())
            }))
            .permission_prompt_tool_name("mcp__approver__ask")
            .build();
        assert!(matches!(
            options.configured_for_can_use_tool(),
            Err(crate::Error::InvalidOptions(_))
        ));
    }

    #[test]
    fn no_callback_means_no_change() {
        let options = ClaudeAgentOptions::default();
        let configured = options.configured_for_can_use_tool().unwrap();
        assert!(configured.permission_prompt_tool_name.is_none());
    }

    #[test]
    fn whole_tool_detection() {
        assert_eq!(whole_tool_allowed("Read"), Some("Read"));
        assert_eq!(whole_tool_allowed("Read()"), Some("Read"));
        assert_eq!(whole_tool_allowed("Read(*)"), Some("Read"));
        assert_eq!(whole_tool_allowed("Bash(ls:*)"), None);
        assert_eq!(whole_tool_allowed("(oops)"), None);
        assert_eq!(whole_tool_allowed(""), None);
    }
}

/// Builder for [`ClaudeAgentOptions`].
#[derive(Debug, Clone, Default)]
pub struct ClaudeAgentOptionsBuilder {
    options: ClaudeAgentOptions,
}

macro_rules! setter {
    ($(#[$meta:meta])* $name:ident, $field:ident, $ty:ty) => {
        $(#[$meta])*
        pub fn $name(mut self, value: $ty) -> Self {
            self.options.$field = value;
            self
        }
    };
    (opt $(#[$meta:meta])* $name:ident, $field:ident, $ty:ty) => {
        $(#[$meta])*
        pub fn $name(mut self, value: $ty) -> Self {
            self.options.$field = Some(value);
            self
        }
    };
    (str $(#[$meta:meta])* $name:ident, $field:ident) => {
        $(#[$meta])*
        pub fn $name(mut self, value: impl Into<String>) -> Self {
            self.options.$field = Some(value.into());
            self
        }
    };
    (path $(#[$meta:meta])* $name:ident, $field:ident) => {
        $(#[$meta])*
        pub fn $name(mut self, value: impl Into<std::path::PathBuf>) -> Self {
            self.options.$field = Some(value.into());
            self
        }
    };
    (into $(#[$meta:meta])* $name:ident, $field:ident, $ty:ty) => {
        $(#[$meta])*
        pub fn $name(mut self, value: impl Into<$ty>) -> Self {
            self.options.$field = Some(value.into());
            self
        }
    };
}

impl ClaudeAgentOptionsBuilder {
    setter!(opt
        /// Set the base built-in tool set.
        tools, tools, ToolsConfig);

    /// Set the tools auto-allowed without prompting.
    pub fn allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.allowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    /// Set the disallowed tools.
    pub fn disallowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.disallowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    setter!(into
        /// Set the system prompt (accepts `&str`, `String`, or [`SystemPrompt`]).
        system_prompt, system_prompt, SystemPrompt);
    setter!(opt
        /// Set MCP servers.
        mcp_servers, mcp_servers, McpServers);
    setter!(
        /// Only use MCP servers from `mcp_servers`.
        strict_mcp_config, strict_mcp_config, bool);
    setter!(opt
        /// Set the permission mode.
        permission_mode, permission_mode, PermissionMode);
    setter!(
        /// Continue the most recent conversation in `cwd`.
        continue_conversation, continue_conversation, bool);
    setter!(str
        /// Resume a session by ID.
        resume, resume);
    setter!(str
        /// Use a specific session ID (UUID).
        session_id, session_id);
    setter!(opt
        /// Cap the number of turns.
        max_turns, max_turns, u32);
    setter!(opt
        /// Cap the spend in USD.
        max_budget_usd, max_budget_usd, f64);
    setter!(str
        /// Choose the model.
        model, model);
    setter!(str
        /// Set a fallback model.
        fallback_model, fallback_model);
    setter!(str
        /// Route permission prompts through an MCP tool.
        permission_prompt_tool_name, permission_prompt_tool_name);
    setter!(path
        /// Set the working directory.
        cwd, cwd);
    setter!(path
        /// Point at a specific CLI executable.
        cli_path, cli_path);
    setter!(str
        /// Load additional settings (file path or inline JSON).
        settings, settings);
    setter!(opt
        /// Set the custom permission handler (routes prompts over stdio).
        can_use_tool, can_use_tool, CanUseTool);
    setter!(opt
        /// Set hook callbacks.
        hooks, hooks, HashMap<HookEvent, Vec<HookMatcher>>);
    setter!(str
        /// Set a user identifier.
        user, user);
    setter!(
        /// Emit partial-message stream events.
        include_partial_messages, include_partial_messages, bool);
    setter!(
        /// Emit hook lifecycle events as system messages.
        include_hook_events, include_hook_events, bool);
    setter!(
        /// Forward subagent text/thinking blocks.
        forward_subagent_text, forward_subagent_text, bool);
    setter!(
        /// Deliver prompts verbatim.
        verbatim_prompts, verbatim_prompts, bool);
    setter!(
        /// Fork resumed sessions to a new ID.
        fork_session, fork_session, bool);
    setter!(str
        /// When resuming, load only up to this message UUID.
        resume_session_at, resume_session_at);
    setter!(str
        /// With `resume_session_at`, the prompt UUID whose turn is dropped.
        resume_drops_turn, resume_drops_turn);
    setter!(opt
        /// Define subagents.
        agents, agents, HashMap<String, AgentDefinition>);
    setter!(opt
        /// Choose which filesystem settings to load.
        setting_sources, setting_sources, Vec<SettingSource>);
    setter!(opt
        /// Enable skills.
        skills, skills, Skills);
    setter!(opt
        /// Configure the bash sandbox.
        sandbox, sandbox, SandboxSettings);
    setter!(opt
        /// Set the stderr callback.
        stderr, stderr, StderrCallback);
    setter!(opt
        /// Cap the per-line buffer size.
        max_buffer_size, max_buffer_size, usize);
    setter!(opt
        /// Deprecated: cap thinking tokens.
        max_thinking_tokens, max_thinking_tokens, u64);
    setter!(opt
        /// Configure thinking.
        thinking, thinking, ThinkingConfig);
    setter!(opt
        /// Set the effort level.
        effort, effort, EffortLevel);
    setter!(opt
        /// Request structured output.
        output_format, output_format, OutputFormat);
    setter!(
        /// Enable file checkpointing.
        enable_file_checkpointing, enable_file_checkpointing, bool);
    setter!(opt
        /// Set an API-side task budget.
        task_budget, task_budget, TaskBudget);
    setter!(str
        /// Identify your app/library in the User-Agent.
        client_app, client_app);

    /// Add one plugin.
    pub fn plugin(mut self, plugin: SdkPluginConfig) -> Self {
        self.options.plugins.push(plugin);
        self
    }

    /// Set all plugins.
    pub fn plugins(mut self, plugins: Vec<SdkPluginConfig>) -> Self {
        self.options.plugins = plugins;
        self
    }

    /// Add an additional accessible directory.
    pub fn add_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.options.add_dirs.push(dir.into());
        self
    }

    /// Set all beta flags.
    pub fn betas<I, S>(mut self, betas: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.betas = betas.into_iter().map(Into::into).collect();
        self
    }

    /// Set one environment variable for the subprocess.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.env.insert(key.into(), value.into());
        self
    }

    /// Set one extra CLI argument (value `None` = boolean flag).
    pub fn extra_arg(mut self, key: impl Into<String>, value: Option<String>) -> Self {
        self.options.extra_args.insert(key.into(), value);
        self
    }

    /// Add a hook matcher for `event`, appending to any already registered.
    pub fn hook(mut self, event: HookEvent, matcher: HookMatcher) -> Self {
        self.options
            .hooks
            .get_or_insert_with(HashMap::new)
            .entry(event)
            .or_default()
            .push(matcher);
        self
    }

    /// Finish building.
    pub fn build(self) -> ClaudeAgentOptions {
        self.options
    }
}
