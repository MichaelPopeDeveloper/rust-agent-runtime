//! Runtime configuration, read from the environment (and `.env`).

use std::net::SocketAddr;
use std::path::PathBuf;

use claude_agent_sdk::PermissionMode;

/// Server configuration.
///
/// Built from the environment by [`Config::from_env`]; tests construct it
/// directly. Every field has a safe default: bind to loopback, no `cwd`
/// override from clients, and whatever permission mode the CLI defaults to
/// (which, non-interactively, denies anything that would prompt).
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on (`BIND`, default `127.0.0.1`, and `PORT`, default
    /// `3000`).
    pub bind: SocketAddr,
    /// If set, every route except `/health` requires `Authorization: Bearer
    /// <token>` (`AGENT_RUNTIME_TOKEN`).
    pub auth_token: Option<String>,
    /// API key for the direct Messages API routes (`ANTHROPIC_API_KEY`). Agent
    /// runs do not use it; they use the `claude` CLI's own login.
    pub anthropic_api_key: Option<String>,
    /// Base URL for the Messages API (`ANTHROPIC_BASE_URL`, default
    /// `https://api.anthropic.com`).
    pub anthropic_base_url: String,
    /// Explicit path to the `claude` executable (`CLAUDE_CLI_PATH`); otherwise
    /// it is discovered on `PATH`.
    pub cli_path: Option<PathBuf>,
    /// Permission mode for agent runs (`AGENT_PERMISSION_MODE`, e.g.
    /// `acceptEdits`). Clients cannot change it per request.
    pub permission_mode: Option<PermissionMode>,
    /// Tools allowed without prompting when a request doesn't specify any
    /// (`AGENT_ALLOWED_TOOLS`, comma-separated).
    pub default_allowed_tools: Vec<String>,
    /// Let clients set the agent's working directory (`AGENT_RUNTIME_ALLOW_CWD`
    /// = `1`/`true`). Off by default: it lets a caller point tools at any path.
    pub allow_cwd: bool,
    /// Default model for agent runs and completions (`AGENT_MODEL`).
    pub default_model: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: "127.0.0.1:3000".parse().expect("valid default address"),
            auth_token: None,
            anthropic_api_key: None,
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            cli_path: None,
            permission_mode: None,
            default_allowed_tools: Vec::new(),
            allow_cwd: false,
            default_model: None,
        }
    }
}

impl Config {
    /// Read the configuration from the process environment.
    pub fn from_env() -> Result<Self, String> {
        let mut config = Config::default();

        let host = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
        config.bind = format!("{host}:{port}")
            .parse()
            .map_err(|e| format!("invalid BIND/PORT ({host}:{port}): {e}"))?;

        config.auth_token = non_empty(std::env::var("AGENT_RUNTIME_TOKEN").ok());
        config.anthropic_api_key = non_empty(std::env::var("ANTHROPIC_API_KEY").ok());
        if let Some(url) = non_empty(std::env::var("ANTHROPIC_BASE_URL").ok()) {
            config.anthropic_base_url = url.trim_end_matches('/').to_string();
        }
        config.cli_path = non_empty(std::env::var("CLAUDE_CLI_PATH").ok()).map(PathBuf::from);
        if let Some(mode) = non_empty(std::env::var("AGENT_PERMISSION_MODE").ok()) {
            config.permission_mode = Some(
                PermissionMode::parse(&mode)
                    .ok_or_else(|| format!("invalid AGENT_PERMISSION_MODE: {mode:?}"))?,
            );
        }
        if let Some(tools) = non_empty(std::env::var("AGENT_ALLOWED_TOOLS").ok()) {
            config.default_allowed_tools = tools
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect();
        }
        config.allow_cwd = matches!(
            std::env::var("AGENT_RUNTIME_ALLOW_CWD").ok().as_deref(),
            Some("1") | Some("true") | Some("yes")
        );
        config.default_model = non_empty(std::env::var("AGENT_MODEL").ok());
        Ok(config)
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}
