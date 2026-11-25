#![allow(missing_docs)] // self-describing DTO fields; types are documented above each definition

//! Shared enums and small configuration structs.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::Error;

/// Permission mode for a session.
///
/// - `Default`: standard behavior, prompts for dangerous operations.
/// - `AcceptEdits`: auto-accept file edits.
/// - `Plan`: planning mode, no tool execution.
/// - `BypassPermissions`: allow every tool call (use with caution).
/// - `DontAsk`: deny anything not pre-approved by allow rules.
/// - `Auto`: a model classifier approves or denies each tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionMode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "auto")]
    Auto,
}

impl PermissionMode {
    /// The value passed to the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Auto => "auto",
        }
    }

    /// Parse a wire value.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "default" => PermissionMode::Default,
            "acceptEdits" => PermissionMode::AcceptEdits,
            "plan" => PermissionMode::Plan,
            "bypassPermissions" => PermissionMode::BypassPermissions,
            "dontAsk" => PermissionMode::DontAsk,
            "auto" => PermissionMode::Auto,
            _ => return None,
        })
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PermissionMode::parse(s)
            .ok_or_else(|| Error::invalid_options(format!("unknown permission mode: {s:?}")))
    }
}

/// How much effort Claude puts into a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl EffortLevel {
    /// The value passed to the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::Xhigh => "xhigh",
            EffortLevel::Max => "max",
        }
    }
}

impl std::fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which filesystem settings the CLI loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    /// Global user settings (`~/.claude/settings.json`).
    User,
    /// Project settings (`.claude/settings.json`).
    Project,
    /// Local settings (`.claude/settings.local.json`).
    Local,
}

impl SettingSource {
    /// The value passed to the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            SettingSource::User => "user",
            SettingSource::Project => "project",
            SettingSource::Local => "local",
        }
    }
}

/// System prompt configuration.
///
/// - [`SystemPrompt::Text`]: a custom system prompt.
/// - [`SystemPrompt::Preset`]: Claude Code's default system prompt, optionally
///   with appended instructions.
/// - [`SystemPrompt::Custom`]: same as `Text`, but this form can also set
///   `snapshot`.
/// - [`SystemPrompt::File`]: load the prompt from a file.
///
/// `None` in [`ClaudeAgentOptions::system_prompt`](crate::ClaudeAgentOptions)
/// means an *empty* system prompt, not Claude Code's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPrompt {
    /// A custom system prompt.
    Text(String),
    /// Claude Code's default (`claude_code`) system prompt.
    Preset {
        /// Text appended to the default prompt.
        append: Option<String>,
        /// Strip per-user dynamic sections (working directory, auto-memory, git
        /// status) so the prompt stays cacheable across users.
        exclude_dynamic_sections: Option<bool>,
        /// Whether the session keeps the system prompt recorded on its first
        /// request (see the Python SDK's `SystemPromptPreset.snapshot`).
        snapshot: Option<bool>,
    },
    /// A custom prompt that can also set `snapshot`.
    Custom {
        /// The prompt text.
        prompt: String,
        /// Same as `Preset::snapshot`, applied to `prompt`.
        snapshot: Option<bool>,
    },
    /// Read the system prompt from a file (`--system-prompt-file`).
    File(PathBuf),
}

impl SystemPrompt {
    /// Claude Code's default system prompt.
    pub fn claude_code() -> Self {
        SystemPrompt::Preset {
            append: None,
            exclude_dynamic_sections: None,
            snapshot: None,
        }
    }

    /// Claude Code's default system prompt with extra instructions appended.
    pub fn claude_code_with_append(append: impl Into<String>) -> Self {
        SystemPrompt::Preset {
            append: Some(append.into()),
            exclude_dynamic_sections: None,
            snapshot: None,
        }
    }

    /// A custom system prompt.
    pub fn text(prompt: impl Into<String>) -> Self {
        SystemPrompt::Text(prompt.into())
    }
}

impl From<&str> for SystemPrompt {
    fn from(s: &str) -> Self {
        SystemPrompt::Text(s.to_string())
    }
}

impl From<String> for SystemPrompt {
    fn from(s: String) -> Self {
        SystemPrompt::Text(s)
    }
}

/// The base set of built-in tools available to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolsConfig {
    /// Specific tool names (an empty list disables every built-in tool).
    List(Vec<String>),
    /// A named preset.
    Preset(ToolsPreset),
}

impl ToolsConfig {
    /// Every default Claude Code tool.
    pub fn claude_code() -> Self {
        ToolsConfig::Preset(ToolsPreset::ClaudeCode)
    }

    /// Only the named tools.
    pub fn list<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ToolsConfig::List(tools.into_iter().map(Into::into).collect())
    }
}

/// Tool presets understood by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolsPreset {
    /// The default Claude Code tool set.
    ClaudeCode,
}

/// API-side task budget in tokens.
///
/// When set, the model is made aware of its remaining token budget so it can
/// pace tool use and wrap up before the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudget {
    /// Total token budget.
    pub total: u64,
}

/// Where a programmatic agent keeps its memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMemoryScope {
    User,
    Project,
    Local,
}

/// Effort for a programmatic agent: a level, or a raw token count.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentEffort {
    Level(EffortLevel),
    Tokens(u64),
}

/// A subagent definition, invokable through the `Agent` tool.
///
/// Field names serialize exactly as the CLI expects them (camelCase).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// When the agent should be used.
    pub description: String,
    /// The agent's system prompt.
    pub prompt: String,
    /// Tools the agent may use (defaults to all tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Tools the agent may not use.
    #[serde(
        rename = "disallowedTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub disallowed_tools: Option<Vec<String>>,
    /// Model alias (`sonnet`, `opus`, `haiku`, `inherit`) or a full model ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Skills available to the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Memory scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<AgentMemoryScope>,
    /// Each entry is a server name (string) or an inline `{name: config}`
    /// object.
    #[serde(
        rename = "mcpServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mcp_servers: Option<Vec<Value>>,
    /// Prompt sent to the agent when it starts.
    #[serde(
        rename = "initialPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_prompt: Option<String>,
    /// Maximum number of turns.
    #[serde(rename = "maxTurns", default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Run the agent in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Effort level or token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<AgentEffort>,
    /// Permission mode for the agent.
    #[serde(
        rename = "permissionMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<PermissionMode>,
}

impl AgentDefinition {
    /// An agent with only a description and prompt; set other fields as needed.
    pub fn new(description: impl Into<String>, prompt: impl Into<String>) -> Self {
        AgentDefinition {
            description: description.into(),
            prompt: prompt.into(),
            tools: None,
            disallowed_tools: None,
            model: None,
            skills: None,
            memory: None,
            mcp_servers: None,
            initial_prompt: None,
            max_turns: None,
            background: None,
            effort: None,
            permission_mode: None,
        }
    }
}

/// How thinking is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

impl ThinkingDisplay {
    /// The value passed to the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingDisplay::Summarized => "summarized",
            ThinkingDisplay::Omitted => "omitted",
        }
    }
}

/// Controls Claude's thinking/reasoning behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ThinkingConfig {
    /// Claude decides when and how much to think.
    Adaptive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// A fixed thinking token budget (older models).
    Enabled {
        budget_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// No extended thinking.
    Disabled,
}

impl ThinkingConfig {
    /// Adaptive thinking with the default display mode.
    pub fn adaptive() -> Self {
        ThinkingConfig::Adaptive { display: None }
    }

    /// A fixed thinking budget.
    pub fn enabled(budget_tokens: u64) -> Self {
        ThinkingConfig::Enabled {
            budget_tokens,
            display: None,
        }
    }

    /// No extended thinking.
    pub fn disabled() -> Self {
        ThinkingConfig::Disabled
    }
}

/// Output format configuration for structured responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputFormat {
    /// The agent returns structured data matching this JSON Schema; read it
    /// from `ResultMessage::structured_output`.
    JsonSchema { schema: Value },
}

impl OutputFormat {
    /// Structured output validated against `schema`.
    pub fn json_schema(schema: Value) -> Self {
        OutputFormat::JsonSchema { schema }
    }
}

/// Skills to enable for the main session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skills {
    /// Enable every discovered skill.
    All,
    /// Enable only the listed skills (SKILL.md `name` / directory name, or
    /// `plugin:skill`).
    List(Vec<String>),
}

/// A plugin loaded for the session. Only local plugins are supported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SdkPluginConfig {
    /// A plugin directory (or `.zip`) on disk.
    Local { path: PathBuf },
}

impl SdkPluginConfig {
    /// A local plugin at `path`.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        SdkPluginConfig::Local { path: path.into() }
    }
}

/// Network configuration for the bash sandbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetworkConfig {
    /// Domain names that sandboxed processes can access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    /// Domains that are always blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_domains: Option<Vec<String>>,
    /// Only managed-settings allowed domains are respected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_managed_domains_only: Option<bool>,
    /// Unix socket paths accessible in the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unix_sockets: Option<Vec<String>>,
    /// Allow all Unix sockets (less secure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_unix_sockets: Option<bool>,
    /// Allow binding to localhost ports (macOS only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    /// macOS only: XPC/Mach service names to allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_mach_lookup: Option<Vec<String>>,
    /// HTTP proxy port if bringing your own proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy_port: Option<u16>,
    /// SOCKS5 proxy port if bringing your own proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socks_proxy_port: Option<u16>,
    /// Any other keys, passed through verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Sandbox settings for command execution isolation.
///
/// Merged into the `--settings` JSON under the `sandbox` key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// Enable bash sandboxing (macOS/Linux only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Auto-approve bash commands when sandboxed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    /// Commands that run outside the sandbox (e.g. `git`, `docker`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_commands: Option<Vec<String>>,
    /// Allow commands to bypass the sandbox via `dangerouslyDisableSandbox`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unsandboxed_commands: Option<bool>,
    /// Network isolation for sandboxed commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetworkConfig>,
    /// Violations to leave unreported: command pattern (`"*"` for every
    /// command) to substrings of the violation text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_violations: Option<HashMap<String, Vec<String>>>,
    /// Weaker sandbox for unprivileged Docker environments (Linux only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_weaker_nested_sandbox: Option<bool>,
    /// Any other keys, passed through verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
