#![allow(missing_docs)] // self-describing DTO fields; types are documented above each definition

//! Hook types: events, matchers, callbacks, typed inputs and outputs.
//!
//! Hooks are delivered by the CLI over the control protocol as
//! `hook_callback` requests; the SDK routes each one to the matching
//! [`HookCallback`] and sends its [`HookJsonOutput`] back.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::BoxFuture;
use crate::errors::Result;

string_enum! {
    /// A hook event name.
    ///
    /// The first ten are the events the Python SDK documents as supported over
    /// the SDK control protocol; the rest are events the CLI defines that the
    /// TypeScript SDK also exposes. Unknown names round-trip through
    /// [`HookEvent::Other`].
    HookEvent {
        PreToolUse => "PreToolUse",
        PostToolUse => "PostToolUse",
        PostToolUseFailure => "PostToolUseFailure",
        UserPromptSubmit => "UserPromptSubmit",
        Stop => "Stop",
        SubagentStop => "SubagentStop",
        PreCompact => "PreCompact",
        Notification => "Notification",
        SubagentStart => "SubagentStart",
        PermissionRequest => "PermissionRequest",
        PostToolBatch => "PostToolBatch",
        UserPromptExpansion => "UserPromptExpansion",
        SessionStart => "SessionStart",
        SessionEnd => "SessionEnd",
        StopFailure => "StopFailure",
        PostCompact => "PostCompact",
        PreModelSwitch => "PreModelSwitch",
        PostModelSwitch => "PostModelSwitch",
        PermissionDenied => "PermissionDenied",
        Setup => "Setup",
        TeammateIdle => "TeammateIdle",
        TaskCreated => "TaskCreated",
        TaskCompleted => "TaskCompleted",
        Elicitation => "Elicitation",
        ElicitationResult => "ElicitationResult",
        ConfigChange => "ConfigChange",
        WorktreeCreate => "WorktreeCreate",
        WorktreeRemove => "WorktreeRemove",
        InstructionsLoaded => "InstructionsLoaded",
        CwdChanged => "CwdChanged",
        FileChanged => "FileChanged",
        DirectoryAdded => "DirectoryAdded",
        MessageDisplay => "MessageDisplay",
        PreSkillShellCommand => "PreSkillShellCommand",
    }
}

/// What happens when a callback in a [`HookMatcher`] fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookOnFailure {
    /// A failure counts as no decision (the default).
    Continue,
    /// A callback that fails, returns nothing usable, or returns a value the
    /// CLI rejects blocks the action.
    Block,
}

impl HookOnFailure {
    /// The wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            HookOnFailure::Continue => "continue",
            HookOnFailure::Block => "block",
        }
    }
}

/// Context passed to hook callbacks. Reserved for future use (abort signals);
/// currently carries nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookContext {
    _private: (),
}

type HookCallbackFn = dyn Fn(Value, Option<String>, HookContext) -> BoxFuture<'static, Result<HookJsonOutput>>
    + Send
    + Sync;

/// An async hook callback: `(input, tool_use_id, context) -> Result<HookJsonOutput>`.
///
/// `input` is the raw hook input object from the CLI; use
/// [`HookInput::parse`] for a typed view of the common events.
///
/// ```no_run
/// use claude_agent_sdk::{HookCallback, HookJsonOutput, HookInput};
///
/// let log_tools = HookCallback::new(|input, _tool_use_id, _ctx| async move {
///     if let Some(HookInput::PreToolUse(pre)) = HookInput::parse(&input) {
///         println!("about to run {}", pre.tool_name);
///     }
///     Ok(HookJsonOutput::default())
/// });
/// ```
#[derive(Clone)]
pub struct HookCallback(Arc<HookCallbackFn>);

impl HookCallback {
    /// Wrap an async function.
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn(Value, Option<String>, HookContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HookJsonOutput>> + Send + 'static,
    {
        HookCallback(Arc::new(move |input, tool_use_id, context| {
            Box::pin(f(input, tool_use_id, context))
        }))
    }

    /// Invoke the callback.
    pub fn call(
        &self,
        input: Value,
        tool_use_id: Option<String>,
        context: HookContext,
    ) -> BoxFuture<'static, Result<HookJsonOutput>> {
        (self.0)(input, tool_use_id, context)
    }
}

impl fmt::Debug for HookCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HookCallback(<callback>)")
    }
}

/// Hook matcher configuration: which tool (or other target) the callbacks
/// apply to, and the callbacks themselves.
#[derive(Debug, Clone, Default)]
pub struct HookMatcher {
    /// Matcher string, e.g. a tool name like `Bash` or a combination like
    /// `Write|MultiEdit|Edit`. `None` matches everything.
    pub matcher: Option<String>,
    /// Callbacks to run when the matcher matches.
    pub hooks: Vec<HookCallback>,
    /// Timeout in seconds for all hooks in this matcher (CLI default: 60).
    pub timeout: Option<f64>,
    /// What happens when a callback fails. With [`HookOnFailure::Block`],
    /// connecting to a CLI too old to honor it fails.
    pub on_failure: Option<HookOnFailure>,
}

impl HookMatcher {
    /// A matcher for `matcher` (or everything when `None`) running `hooks`.
    pub fn new(matcher: Option<&str>, hooks: Vec<HookCallback>) -> Self {
        HookMatcher {
            matcher: matcher.map(str::to_string),
            hooks,
            timeout: None,
            on_failure: None,
        }
    }

    /// A matcher that fires for every target.
    pub fn all(hooks: Vec<HookCallback>) -> Self {
        Self::new(None, hooks)
    }

    /// Set the timeout in seconds.
    pub fn with_timeout(mut self, seconds: f64) -> Self {
        self.timeout = Some(seconds);
        self
    }

    /// Set the failure behavior.
    pub fn with_on_failure(mut self, on_failure: HookOnFailure) -> Self {
        self.on_failure = Some(on_failure);
        self
    }
}

// ---------------------------------------------------------------------------
// Hook inputs
// ---------------------------------------------------------------------------

/// Reasoning effort in effect when a hook fires.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookEffort {
    /// Effort level for the current turn (e.g. `low`, `high`, `max`).
    #[serde(default)]
    pub level: String,
}

/// In-flight background task, as listed on Stop and SubagentStop inputs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTaskSummary {
    #[serde(default)]
    pub id: String,
    /// Task-type label (e.g. `shell`, `subagent`, `monitor`, `workflow`).
    #[serde(rename = "type", default)]
    pub task_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub description: String,
    /// Shell command line (shell tasks only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Subagent type name (subagent tasks only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// MCP server name (monitor tasks only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// MCP tool name (monitor tasks only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Workflow name (workflow tasks only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Session-scoped scheduled wakeup, as listed on Stop and SubagentStop inputs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionCronSummary {
    #[serde(default)]
    pub id: String,
    /// Cron expression, e.g. `0 9 * * 1-5`.
    #[serde(default)]
    pub schedule: String,
    /// False for one-shot wakeups.
    #[serde(default)]
    pub recurring: bool,
    /// Prompt submitted when the cron fires.
    #[serde(default)]
    pub prompt: String,
}

macro_rules! hook_input_struct {
    ($(#[$meta:meta])* $name:ident { $( $(#[$fmeta:meta])* $field:ident : $ty:ty ),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        pub struct $name {
            /// Session the hook fired in.
            #[serde(default)]
            pub session_id: String,
            /// Path of the session transcript.
            #[serde(default)]
            pub transcript_path: String,
            /// Working directory.
            #[serde(default)]
            pub cwd: String,
            /// Permission mode in effect.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub permission_mode: Option<String>,
            /// UUID of the user prompt being processed.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub prompt_id: Option<String>,
            /// Effort in effect for the current turn.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub effort: Option<HookEffort>,
            /// Sub-agent identifier, when the hook fired inside a sub-agent.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub agent_id: Option<String>,
            /// Agent type name, when known.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub agent_type: Option<String>,
            $( $(#[$fmeta])* pub $field: $ty, )*
            /// Any other keys, passed through verbatim.
            #[serde(flatten)]
            pub extra: serde_json::Map<String, Value>,
        }
    };
}

hook_input_struct! {
    /// Input for `PreToolUse` hooks.
    PreToolUseHookInput {
        #[serde(default)] tool_name: String,
        #[serde(default)] tool_input: Value,
        #[serde(default)] tool_use_id: String,
    }
}

hook_input_struct! {
    /// Input for `PostToolUse` hooks.
    PostToolUseHookInput {
        #[serde(default)] tool_name: String,
        #[serde(default)] tool_input: Value,
        #[serde(default)] tool_response: Value,
        #[serde(default)] tool_use_id: String,
        /// Tool execution time in milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")] duration_ms: Option<u64>,
    }
}

hook_input_struct! {
    /// Input for `PostToolUseFailure` hooks.
    PostToolUseFailureHookInput {
        #[serde(default)] tool_name: String,
        #[serde(default)] tool_input: Value,
        #[serde(default)] tool_use_id: String,
        #[serde(default)] error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] is_interrupt: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")] duration_ms: Option<u64>,
    }
}

hook_input_struct! {
    /// Input for `UserPromptSubmit` hooks.
    UserPromptSubmitHookInput {
        #[serde(default)] prompt: String,
    }
}

hook_input_struct! {
    /// Input for `Stop` hooks.
    StopHookInput {
        #[serde(default)] stop_hook_active: bool,
        /// Text of the last assistant message before stopping.
        #[serde(default, skip_serializing_if = "Option::is_none")] last_assistant_message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")] background_tasks: Option<Vec<BackgroundTaskSummary>>,
        #[serde(default, skip_serializing_if = "Option::is_none")] session_crons: Option<Vec<SessionCronSummary>>,
    }
}

hook_input_struct! {
    /// Input for `SubagentStop` hooks (`agent_id` / `agent_type` are in the
    /// common fields).
    SubagentStopHookInput {
        #[serde(default)] stop_hook_active: bool,
        #[serde(default)] agent_transcript_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] last_assistant_message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")] background_tasks: Option<Vec<BackgroundTaskSummary>>,
        #[serde(default, skip_serializing_if = "Option::is_none")] session_crons: Option<Vec<SessionCronSummary>>,
    }
}

hook_input_struct! {
    /// Input for `PreCompact` hooks.
    PreCompactHookInput {
        /// `manual` or `auto`.
        #[serde(default)] trigger: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] custom_instructions: Option<String>,
    }
}

hook_input_struct! {
    /// Input for `Notification` hooks.
    NotificationHookInput {
        #[serde(default)] message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] title: Option<String>,
        #[serde(default)] notification_type: String,
    }
}

hook_input_struct! {
    /// Input for `SubagentStart` hooks (`agent_id` / `agent_type` are in the
    /// common fields).
    SubagentStartHookInput {}
}

hook_input_struct! {
    /// Input for `PermissionRequest` hooks.
    PermissionRequestHookInput {
        #[serde(default)] tool_name: String,
        #[serde(default)] tool_input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")] permission_suggestions: Option<Vec<Value>>,
    }
}

/// Typed view of a hook input, discriminated by `hook_event_name`.
///
/// Build one with [`HookInput::parse`]; events this SDK does not model return
/// `None`, and the raw [`Value`] is still available to the callback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "hook_event_name")]
pub enum HookInput {
    PreToolUse(PreToolUseHookInput),
    PostToolUse(PostToolUseHookInput),
    PostToolUseFailure(PostToolUseFailureHookInput),
    UserPromptSubmit(UserPromptSubmitHookInput),
    Stop(StopHookInput),
    SubagentStop(SubagentStopHookInput),
    PreCompact(PreCompactHookInput),
    Notification(NotificationHookInput),
    SubagentStart(SubagentStartHookInput),
    PermissionRequest(PermissionRequestHookInput),
}

impl HookInput {
    /// Parse a raw hook input object. Returns `None` for events this SDK does
    /// not model, or when the payload is not an object.
    pub fn parse(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }

    /// The event name.
    pub fn event(&self) -> HookEvent {
        match self {
            HookInput::PreToolUse(_) => HookEvent::PreToolUse,
            HookInput::PostToolUse(_) => HookEvent::PostToolUse,
            HookInput::PostToolUseFailure(_) => HookEvent::PostToolUseFailure,
            HookInput::UserPromptSubmit(_) => HookEvent::UserPromptSubmit,
            HookInput::Stop(_) => HookEvent::Stop,
            HookInput::SubagentStop(_) => HookEvent::SubagentStop,
            HookInput::PreCompact(_) => HookEvent::PreCompact,
            HookInput::Notification(_) => HookEvent::Notification,
            HookInput::SubagentStart(_) => HookEvent::SubagentStart,
            HookInput::PermissionRequest(_) => HookEvent::PermissionRequest,
        }
    }
}

// ---------------------------------------------------------------------------
// Hook outputs
// ---------------------------------------------------------------------------

string_enum! {
    /// Top-level hook decision. Only `block` is meaningful for most hooks;
    /// `approve` is deprecated for PreToolUse (use `permissionDecision`).
    HookDecision {
        Block => "block",
        Approve => "approve",
    }
}

string_enum! {
    /// Permission decision returned by PreToolUse (and PreModelSwitch) hooks.
    HookPermissionDecision {
        Allow => "allow",
        Deny => "deny",
        Ask => "ask",
        /// Stop the run and surface the tool call as
        /// `ResultMessage::deferred_tool_use`.
        Defer => "defer",
    }
}

string_enum! {
    /// Action for MCP elicitation hooks.
    ElicitationAction {
        Accept => "accept",
        Decline => "decline",
        Cancel => "cancel",
    }
}

macro_rules! hook_output_struct {
    ($(#[$meta:meta])* $name:ident { $( $(#[$fmeta:meta])* $field:ident : $ty:ty ),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $name {
            $( $(#[$fmeta])* #[serde(default, skip_serializing_if = "Option::is_none")] pub $field: Option<$ty>, )*
        }
    };
}

hook_output_struct! {
    /// Hook-specific output for `PreToolUse`.
    PreToolUseHookSpecificOutput {
        permission_decision: HookPermissionDecision,
        permission_decision_reason: String,
        updated_input: Value,
        additional_context: String,
    }
}

hook_output_struct! {
    /// Hook-specific output for `PostToolUse`.
    PostToolUseHookSpecificOutput {
        additional_context: String,
        /// Replaces the tool output before it is sent to the model. For
        /// built-in tools the value must match the tool's output schema.
        updated_tool_output: Value,
        /// Replaces the output for MCP tools only; prefer `updated_tool_output`.
        #[serde(rename = "updatedMCPToolOutput")] updated_mcp_tool_output: Value,
    }
}

hook_output_struct! {
    /// Hook-specific output for `PostToolUseFailure`.
    PostToolUseFailureHookSpecificOutput {
        additional_context: String,
    }
}

hook_output_struct! {
    /// Hook-specific output for `UserPromptSubmit`.
    UserPromptSubmitHookSpecificOutput {
        additional_context: String,
        session_title: String,
        /// When `decision` is `block`, omit the original prompt from the block
        /// message.
        suppress_original_prompt: bool,
    }
}

hook_output_struct! {
    /// Hook-specific output for `UserPromptExpansion`.
    UserPromptExpansionHookSpecificOutput {
        additional_context: String,
        suppress_original_prompt: bool,
    }
}

hook_output_struct! {
    /// Hook-specific output for `SessionStart`.
    SessionStartHookSpecificOutput {
        additional_context: String,
        initial_user_message: String,
        session_title: String,
        watch_paths: Vec<String>,
        reload_skills: bool,
    }
}

hook_output_struct! {
    /// Hook-specific output for `Notification`.
    NotificationHookSpecificOutput { additional_context: String }
}

hook_output_struct! {
    /// Hook-specific output for `SubagentStart`.
    SubagentStartHookSpecificOutput { additional_context: String }
}

hook_output_struct! {
    /// Hook-specific output for `PermissionRequest`.
    PermissionRequestHookSpecificOutput {
        /// `{"behavior": "allow", "updatedInput"?, "updatedPermissions"?}` or
        /// `{"behavior": "deny", "message", "interrupt"?}`.
        decision: Value,
    }
}

hook_output_struct! {
    /// Hook-specific output for `PostToolBatch`.
    PostToolBatchHookSpecificOutput { additional_context: String }
}

hook_output_struct! {
    /// Hook-specific output for `Stop`. `additional_context` is non-error
    /// feedback delivered to the model; the conversation continues.
    StopHookSpecificOutput { additional_context: String }
}

hook_output_struct! {
    /// Hook-specific output for `SubagentStop`.
    SubagentStopHookSpecificOutput { additional_context: String }
}

hook_output_struct! {
    /// Hook-specific output for `PermissionDenied`.
    PermissionDeniedHookSpecificOutput {
        /// Tell the model it may retry the denied tool call.
        retry: bool,
    }
}

hook_output_struct! {
    /// Hook-specific output for `PreModelSwitch`.
    PreModelSwitchHookSpecificOutput {
        permission_decision: HookPermissionDecision,
        permission_decision_reason: String,
    }
}

hook_output_struct! {
    /// Hook-specific output for `PostModelSwitch`.
    PostModelSwitchHookSpecificOutput { additional_context: String }
}

hook_output_struct! {
    /// Hook-specific output for `Elicitation`.
    ElicitationHookSpecificOutput {
        action: ElicitationAction,
        /// Form field values to submit (only with `accept`).
        content: Value,
    }
}

hook_output_struct! {
    /// Hook-specific output for `ElicitationResult`.
    ElicitationResultHookSpecificOutput {
        action: ElicitationAction,
        content: Value,
    }
}

hook_output_struct! {
    /// Hook-specific output for `CwdChanged`.
    CwdChangedHookSpecificOutput { watch_paths: Vec<String> }
}

hook_output_struct! {
    /// Hook-specific output for `FileChanged`.
    FileChangedHookSpecificOutput { watch_paths: Vec<String> }
}

hook_output_struct! {
    /// Hook-specific output for `MessageDisplay` (display only).
    MessageDisplayHookSpecificOutput { display_content: String }
}

/// Hook-specific output for `WorktreeCreate`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCreateHookSpecificOutput {
    /// Absolute path to the created worktree directory.
    pub worktree_path: String,
}

/// Event-specific controls, tagged by `hookEventName` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "hookEventName")]
pub enum HookSpecificOutput {
    PreToolUse(PreToolUseHookSpecificOutput),
    PostToolUse(PostToolUseHookSpecificOutput),
    PostToolUseFailure(PostToolUseFailureHookSpecificOutput),
    UserPromptSubmit(UserPromptSubmitHookSpecificOutput),
    UserPromptExpansion(UserPromptExpansionHookSpecificOutput),
    SessionStart(SessionStartHookSpecificOutput),
    Notification(NotificationHookSpecificOutput),
    SubagentStart(SubagentStartHookSpecificOutput),
    PermissionRequest(PermissionRequestHookSpecificOutput),
    PostToolBatch(PostToolBatchHookSpecificOutput),
    Stop(StopHookSpecificOutput),
    SubagentStop(SubagentStopHookSpecificOutput),
    PermissionDenied(PermissionDeniedHookSpecificOutput),
    PreModelSwitch(PreModelSwitchHookSpecificOutput),
    PostModelSwitch(PostModelSwitchHookSpecificOutput),
    Elicitation(ElicitationHookSpecificOutput),
    ElicitationResult(ElicitationResultHookSpecificOutput),
    CwdChanged(CwdChangedHookSpecificOutput),
    FileChanged(FileChangedHookSpecificOutput),
    WorktreeCreate(WorktreeCreateHookSpecificOutput),
    MessageDisplay(MessageDisplayHookSpecificOutput),
}

/// The JSON a hook callback returns to the CLI.
///
/// An empty output (`HookJsonOutput::default()`) means "no opinion". Set
/// `async_` to defer the hook; set `continue_ = Some(false)` to stop Claude
/// (with `stop_reason`); set `decision = Some(HookDecision::Block)` with a
/// `reason` to block; put event-specific controls in `hook_specific_output`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookJsonOutput {
    /// Defer hook execution (`async` on the wire).
    #[serde(rename = "async", default, skip_serializing_if = "Option::is_none")]
    pub async_: Option<bool>,
    /// Timeout in milliseconds for an async hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_timeout: Option<u64>,
    /// Whether Claude should proceed after the hook (`continue` on the wire;
    /// default true).
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,
    /// Hide stdout from transcript mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
    /// Message shown when `continue` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// `block` to block the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<HookDecision>,
    /// Warning message shown to the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Feedback for Claude about the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Event-specific controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
    /// Any other keys, sent verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl HookJsonOutput {
    /// Block the action with a reason for Claude.
    pub fn block(reason: impl Into<String>) -> Self {
        HookJsonOutput {
            decision: Some(HookDecision::Block),
            reason: Some(reason.into()),
            ..Default::default()
        }
    }

    /// Stop Claude with a reason shown to the user.
    pub fn stop(stop_reason: impl Into<String>) -> Self {
        HookJsonOutput {
            continue_: Some(false),
            stop_reason: Some(stop_reason.into()),
            ..Default::default()
        }
    }

    /// Only event-specific output.
    pub fn with_specific(output: HookSpecificOutput) -> Self {
        HookJsonOutput {
            hook_specific_output: Some(output),
            ..Default::default()
        }
    }

    /// A `PreToolUse` permission decision.
    pub fn pre_tool_use_decision(decision: HookPermissionDecision, reason: Option<String>) -> Self {
        Self::with_specific(HookSpecificOutput::PreToolUse(
            PreToolUseHookSpecificOutput {
                permission_decision: Some(decision),
                permission_decision_reason: reason,
                ..Default::default()
            },
        ))
    }

    /// Add context for the model (`additionalContext`) for the given event.
    /// Returns `None` for events whose output has no such field.
    pub fn additional_context(event: &HookEvent, context: impl Into<String>) -> Option<Self> {
        let context = Some(context.into());
        let specific = match event {
            HookEvent::PreToolUse => HookSpecificOutput::PreToolUse(PreToolUseHookSpecificOutput {
                additional_context: context,
                ..Default::default()
            }),
            HookEvent::PostToolUse => {
                HookSpecificOutput::PostToolUse(PostToolUseHookSpecificOutput {
                    additional_context: context,
                    ..Default::default()
                })
            }
            HookEvent::PostToolUseFailure => {
                HookSpecificOutput::PostToolUseFailure(PostToolUseFailureHookSpecificOutput {
                    additional_context: context,
                })
            }
            HookEvent::UserPromptSubmit => {
                HookSpecificOutput::UserPromptSubmit(UserPromptSubmitHookSpecificOutput {
                    additional_context: context,
                    ..Default::default()
                })
            }
            HookEvent::UserPromptExpansion => {
                HookSpecificOutput::UserPromptExpansion(UserPromptExpansionHookSpecificOutput {
                    additional_context: context,
                    ..Default::default()
                })
            }
            HookEvent::SessionStart => {
                HookSpecificOutput::SessionStart(SessionStartHookSpecificOutput {
                    additional_context: context,
                    ..Default::default()
                })
            }
            HookEvent::Notification => {
                HookSpecificOutput::Notification(NotificationHookSpecificOutput {
                    additional_context: context,
                })
            }
            HookEvent::SubagentStart => {
                HookSpecificOutput::SubagentStart(SubagentStartHookSpecificOutput {
                    additional_context: context,
                })
            }
            HookEvent::PostToolBatch => {
                HookSpecificOutput::PostToolBatch(PostToolBatchHookSpecificOutput {
                    additional_context: context,
                })
            }
            HookEvent::Stop => HookSpecificOutput::Stop(StopHookSpecificOutput {
                additional_context: context,
            }),
            HookEvent::SubagentStop => {
                HookSpecificOutput::SubagentStop(SubagentStopHookSpecificOutput {
                    additional_context: context,
                })
            }
            HookEvent::PostModelSwitch => {
                HookSpecificOutput::PostModelSwitch(PostModelSwitchHookSpecificOutput {
                    additional_context: context,
                })
            }
            _ => return None,
        };
        Some(Self::with_specific(specific))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hook_event_round_trips() {
        assert_eq!(HookEvent::parse("PreToolUse"), HookEvent::PreToolUse);
        assert_eq!(
            HookEvent::parse("Brand New"),
            HookEvent::Other("Brand New".into())
        );
        assert_eq!(
            serde_json::to_value(HookEvent::SessionStart).unwrap(),
            json!("SessionStart")
        );
    }

    #[test]
    fn hook_output_serializes_wire_names() {
        let output = HookJsonOutput {
            continue_: Some(false),
            stop_reason: Some("done".into()),
            hook_specific_output: Some(HookSpecificOutput::PreToolUse(
                PreToolUseHookSpecificOutput {
                    permission_decision: Some(HookPermissionDecision::Deny),
                    permission_decision_reason: Some("nope".into()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!({
                "continue": false,
                "stopReason": "done",
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "nope",
                },
            })
        );
        let empty = serde_json::to_value(HookJsonOutput::default()).unwrap();
        assert_eq!(empty, json!({}));
        let asynchronous = serde_json::to_value(HookJsonOutput {
            async_: Some(true),
            async_timeout: Some(5000),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(asynchronous, json!({"async": true, "asyncTimeout": 5000}));
    }

    #[test]
    fn hook_input_parses_typed() {
        let raw = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/work",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_1",
            "permission_mode": "default",
            "something_new": 1,
        });
        match HookInput::parse(&raw) {
            Some(HookInput::PreToolUse(input)) => {
                assert_eq!(input.tool_name, "Bash");
                assert_eq!(input.tool_input["command"], "ls");
                assert_eq!(input.session_id, "s1");
                assert_eq!(input.extra["something_new"], 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(HookInput::parse(&json!({"hook_event_name": "Mystery"})).is_none());
        assert!(HookInput::parse(&json!("not an object")).is_none());
    }

    #[test]
    fn additional_context_helper() {
        let out = HookJsonOutput::additional_context(&HookEvent::PostToolUse, "ctx").unwrap();
        assert_eq!(
            serde_json::to_value(out).unwrap(),
            json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "ctx"}})
        );
        assert!(HookJsonOutput::additional_context(&HookEvent::PreCompact, "x").is_none());
    }
}
