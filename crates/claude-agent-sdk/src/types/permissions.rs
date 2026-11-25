#![allow(missing_docs)] // self-describing DTO fields; types are documented above each definition

//! Tool permission types: permission updates, the `can_use_tool` callback and
//! its context and result.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::common::PermissionMode;
use super::BoxFuture;
use crate::errors::{Error, Result};

/// Where a permission update applies and is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    /// In memory until the session ends (the default).
    Session,
}

/// Behavior of a permission rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

/// A permission rule: a tool, optionally narrowed to matching invocations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionRuleValue {
    /// The tool the rule applies to (e.g. `Bash`).
    pub tool_name: String,
    /// What the rule matches within the tool, e.g. `npm test` for Bash.
    /// `None` makes a rule that covers every use of the tool.
    pub rule_content: Option<String>,
}

impl PermissionRuleValue {
    /// A rule covering every use of `tool_name`.
    pub fn new(tool_name: impl Into<String>) -> Self {
        PermissionRuleValue {
            tool_name: tool_name.into(),
            rule_content: None,
        }
    }

    /// A rule covering uses of `tool_name` matching `rule_content`.
    pub fn with_content(tool_name: impl Into<String>, rule_content: impl Into<String>) -> Self {
        PermissionRuleValue {
            tool_name: tool_name.into(),
            rule_content: Some(rule_content.into()),
        }
    }
}

string_enum! {
    /// The kind of a [`PermissionUpdate`].
    PermissionUpdateType {
        AddRules => "addRules",
        ReplaceRules => "replaceRules",
        RemoveRules => "removeRules",
        SetMode => "setMode",
        AddDirectories => "addDirectories",
        RemoveDirectories => "removeDirectories",
    }
}

/// A permission change, either suggested by the CLI alongside a permission
/// request or applied by an allow result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionUpdate {
    /// What kind of update this is.
    pub kind: PermissionUpdateType,
    /// Rules, for the `*Rules` kinds.
    pub rules: Option<Vec<PermissionRuleValue>>,
    /// Behavior, for the `*Rules` kinds.
    pub behavior: Option<PermissionBehavior>,
    /// Mode, for `setMode`.
    pub mode: Option<PermissionMode>,
    /// Directories, for the `*Directories` kinds.
    pub directories: Option<Vec<String>>,
    /// Where the update applies; `None` is sent as `session`.
    pub destination: Option<PermissionUpdateDestination>,
}

impl PermissionUpdate {
    fn base(kind: PermissionUpdateType) -> Self {
        PermissionUpdate {
            kind,
            rules: None,
            behavior: None,
            mode: None,
            directories: None,
            destination: None,
        }
    }

    /// Add permission rules with the given behavior.
    pub fn add_rules(rules: Vec<PermissionRuleValue>, behavior: PermissionBehavior) -> Self {
        let mut update = Self::base(PermissionUpdateType::AddRules);
        update.rules = Some(rules);
        update.behavior = Some(behavior);
        update
    }

    /// Replace the rules with the given behavior.
    pub fn replace_rules(rules: Vec<PermissionRuleValue>, behavior: PermissionBehavior) -> Self {
        let mut update = Self::base(PermissionUpdateType::ReplaceRules);
        update.rules = Some(rules);
        update.behavior = Some(behavior);
        update
    }

    /// Remove rules with the given behavior.
    pub fn remove_rules(rules: Vec<PermissionRuleValue>, behavior: PermissionBehavior) -> Self {
        let mut update = Self::base(PermissionUpdateType::RemoveRules);
        update.rules = Some(rules);
        update.behavior = Some(behavior);
        update
    }

    /// Switch the permission mode.
    pub fn set_mode(mode: PermissionMode) -> Self {
        let mut update = Self::base(PermissionUpdateType::SetMode);
        update.mode = Some(mode);
        update
    }

    /// Add working directories.
    pub fn add_directories(directories: Vec<String>) -> Self {
        let mut update = Self::base(PermissionUpdateType::AddDirectories);
        update.directories = Some(directories);
        update
    }

    /// Remove working directories.
    pub fn remove_directories(directories: Vec<String>) -> Self {
        let mut update = Self::base(PermissionUpdateType::RemoveDirectories);
        update.directories = Some(directories);
        update
    }

    /// Set where the update is kept.
    pub fn with_destination(mut self, destination: PermissionUpdateDestination) -> Self {
        self.destination = Some(destination);
        self
    }

    /// Convert to the control protocol wire format.
    ///
    /// Claude Code rejects an update that lacks a field its type needs, and
    /// then ignores every update in the same response, so this fails with
    /// [`Error::InvalidOptions`] when a required field is missing, leaves
    /// `ruleContent` out for whole-tool rules, and sends `session` for a
    /// missing destination.
    pub fn to_wire(&self) -> Result<Value> {
        let destination = self
            .destination
            .unwrap_or(PermissionUpdateDestination::Session);
        let mut result = json!({
            "type": self.kind.as_str(),
            "destination": serde_json::to_value(destination)?,
        });
        let obj = result.as_object_mut().expect("object");
        match self.kind {
            PermissionUpdateType::AddRules
            | PermissionUpdateType::ReplaceRules
            | PermissionUpdateType::RemoveRules => {
                let rules = self.rules.as_ref().ok_or_else(|| {
                    Error::invalid_options(format!(
                        "PermissionUpdate(type={:?}) has no rules. Set rules to a list of PermissionRuleValue.",
                        self.kind.as_str()
                    ))
                })?;
                let behavior = self.behavior.ok_or_else(|| {
                    Error::invalid_options(format!(
                        "PermissionUpdate(type={:?}) has no behavior. Set behavior to allow, deny or ask.",
                        self.kind.as_str()
                    ))
                })?;
                let wire_rules: Vec<Value> = rules
                    .iter()
                    .map(|rule| {
                        let mut wire = json!({ "toolName": rule.tool_name });
                        if let Some(content) = &rule.rule_content {
                            wire["ruleContent"] = Value::String(content.clone());
                        }
                        wire
                    })
                    .collect();
                obj.insert("rules".into(), Value::Array(wire_rules));
                obj.insert("behavior".into(), serde_json::to_value(behavior)?);
            }
            PermissionUpdateType::SetMode => {
                let mode = self.mode.ok_or_else(|| {
                    Error::invalid_options(
                        "PermissionUpdate(type=\"setMode\") has no mode. Set mode to a permission mode such as acceptEdits.",
                    )
                })?;
                obj.insert("mode".into(), Value::String(mode.as_str().to_string()));
            }
            PermissionUpdateType::AddDirectories | PermissionUpdateType::RemoveDirectories => {
                let directories = self.directories.as_ref().ok_or_else(|| {
                    Error::invalid_options(format!(
                        "PermissionUpdate(type={:?}) has no directories. Set directories to a list of paths.",
                        self.kind.as_str()
                    ))
                })?;
                obj.insert("directories".into(), json!(directories));
            }
            PermissionUpdateType::Other(ref other) => {
                return Err(Error::invalid_options(format!(
                    "PermissionUpdate has type {other:?}, which this version of the SDK cannot send. \
                     Use one of addRules, replaceRules, removeRules, setMode, addDirectories or \
                     removeDirectories, or upgrade the SDK if this update came from Claude Code."
                )));
            }
        }
        Ok(result)
    }

    /// Parse the control protocol wire format (inverse of [`Self::to_wire`]).
    pub fn from_wire(value: &Value) -> Result<Self> {
        let obj = value.as_object().ok_or_else(|| {
            Error::message_parse("permission update is not an object", Some(value.clone()))
        })?;
        let kind = obj.get("type").and_then(Value::as_str).ok_or_else(|| {
            Error::message_parse("permission update has no type", Some(value.clone()))
        })?;
        let rules = match obj.get("rules") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|item| {
                        let tool_name =
                            item.get("toolName")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    Error::message_parse(
                                        "permission rule has no toolName",
                                        Some(value.clone()),
                                    )
                                })?;
                        Ok(PermissionRuleValue {
                            tool_name: tool_name.to_string(),
                            rule_content: item
                                .get("ruleContent")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            _ => None,
        };
        let behavior = match obj.get("behavior") {
            Some(v) if !v.is_null() => Some(serde_json::from_value(v.clone())?),
            _ => None,
        };
        let mode = obj
            .get("mode")
            .and_then(Value::as_str)
            .and_then(PermissionMode::parse);
        let directories = match obj.get("directories") {
            Some(v) if !v.is_null() => Some(serde_json::from_value(v.clone())?),
            _ => None,
        };
        let destination = match obj.get("destination") {
            Some(v) if !v.is_null() => Some(serde_json::from_value(v.clone())?),
            _ => None,
        };
        Ok(PermissionUpdate {
            kind: PermissionUpdateType::parse(kind),
            rules,
            behavior,
            mode,
            directories,
            destination,
        })
    }
}

impl Serialize for PermissionUpdate {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.to_wire()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PermissionUpdate {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        PermissionUpdate::from_wire(&value).map_err(serde::de::Error::custom)
    }
}

/// Context passed to the [`CanUseTool`] callback.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolPermissionContext {
    /// Permission suggestions from the CLI (e.g. "always allow this").
    pub suggestions: Vec<PermissionUpdate>,
    /// Identifier of this tool call within the assistant message. Always
    /// present when delivered to a callback.
    pub tool_use_id: Option<String>,
    /// If running within a sub-agent, the sub-agent's ID.
    pub agent_id: Option<String>,
    /// The file path that triggered the permission request, if applicable.
    pub blocked_path: Option<String>,
    /// Why this permission request was triggered (e.g. a PreToolUse hook
    /// returned `ask` with a reason).
    pub decision_reason: Option<String>,
    /// Full permission prompt sentence (e.g. "Claude wants to read foo.txt").
    pub title: Option<String>,
    /// Short noun phrase for the action (e.g. "Read file").
    pub display_name: Option<String>,
    /// Human-readable subtitle for a permission UI.
    pub description: Option<String>,
}

/// The decision returned by a [`CanUseTool`] callback.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResult {
    /// Allow the tool call.
    Allow {
        /// Replacement input for the tool; `None` keeps the original input.
        updated_input: Option<Value>,
        /// Permission updates to apply along with this allow. If any of them
        /// cannot be sent, none are and a warning is logged; the tool call is
        /// still allowed.
        updated_permissions: Option<Vec<PermissionUpdate>>,
    },
    /// Deny the tool call.
    Deny {
        /// Message shown to the model.
        message: String,
        /// Also interrupt the whole turn.
        interrupt: bool,
    },
}

impl PermissionResult {
    /// Allow with the original input.
    pub fn allow() -> Self {
        PermissionResult::Allow {
            updated_input: None,
            updated_permissions: None,
        }
    }

    /// Allow with a modified input.
    pub fn allow_with_input(updated_input: Value) -> Self {
        PermissionResult::Allow {
            updated_input: Some(updated_input),
            updated_permissions: None,
        }
    }

    /// Allow and apply permission updates (e.g. the CLI's suggestions).
    pub fn allow_with_permissions(updated_permissions: Vec<PermissionUpdate>) -> Self {
        PermissionResult::Allow {
            updated_input: None,
            updated_permissions: Some(updated_permissions),
        }
    }

    /// Deny with a message.
    pub fn deny(message: impl Into<String>) -> Self {
        PermissionResult::Deny {
            message: message.into(),
            interrupt: false,
        }
    }

    /// Deny and interrupt the turn.
    pub fn deny_and_interrupt(message: impl Into<String>) -> Self {
        PermissionResult::Deny {
            message: message.into(),
            interrupt: true,
        }
    }

    /// Whether this result allows the call.
    pub fn is_allow(&self) -> bool {
        matches!(self, PermissionResult::Allow { .. })
    }
}

type CanUseToolFn = dyn Fn(String, Value, ToolPermissionContext) -> BoxFuture<'static, Result<PermissionResult>>
    + Send
    + Sync;

/// Custom permission handler for tool calls that would otherwise prompt the
/// user.
///
/// Invoked when the CLI's permission rules evaluate to "ask" for a tool call.
/// It is *not* invoked for tool calls already permitted by `allowed_tools`,
/// `permission_mode` or settings allow rules. To observe or gate every tool
/// call, use a `PreToolUse` hook instead.
///
/// ```no_run
/// use claude_agent_sdk::{CanUseTool, PermissionResult};
///
/// let callback = CanUseTool::new(|tool_name, _input, _context| async move {
///     if tool_name == "Bash" {
///         Ok(PermissionResult::deny("no shell commands"))
///     } else {
///         Ok(PermissionResult::allow())
///     }
/// });
/// ```
#[derive(Clone)]
pub struct CanUseTool(Arc<CanUseToolFn>);

impl CanUseTool {
    /// Wrap an async function `(tool_name, input, context) -> Result<PermissionResult>`.
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn(String, Value, ToolPermissionContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<PermissionResult>> + Send + 'static,
    {
        CanUseTool(Arc::new(move |name, input, context| {
            Box::pin(f(name, input, context))
        }))
    }

    /// Invoke the callback.
    pub fn call(
        &self,
        tool_name: String,
        input: Value,
        context: ToolPermissionContext,
    ) -> BoxFuture<'static, Result<PermissionResult>> {
        (self.0)(tool_name, input, context)
    }
}

impl fmt::Debug for CanUseTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CanUseTool(<callback>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_wire_add_rules() {
        let update = PermissionUpdate::add_rules(
            vec![
                PermissionRuleValue::new("Read"),
                PermissionRuleValue::with_content("Bash", "npm test"),
            ],
            PermissionBehavior::Allow,
        );
        let wire = update.to_wire().unwrap();
        assert_eq!(
            wire,
            json!({
                "type": "addRules",
                "destination": "session",
                "rules": [
                    {"toolName": "Read"},
                    {"toolName": "Bash", "ruleContent": "npm test"},
                ],
                "behavior": "allow",
            })
        );
        // Round-trip: to_wire fills in the default "session" destination, so the
        // parsed value carries Some(Session) where the original had None.
        let back = PermissionUpdate::from_wire(&wire).unwrap();
        assert_eq!(back.kind, update.kind);
        assert_eq!(back.rules, update.rules);
        assert_eq!(back.behavior, update.behavior);
        assert_eq!(back.destination, Some(PermissionUpdateDestination::Session));
    }

    #[test]
    fn to_wire_requires_fields() {
        let mut update = PermissionUpdate::set_mode(PermissionMode::AcceptEdits);
        update.mode = None;
        assert!(matches!(update.to_wire(), Err(Error::InvalidOptions(_))));
        let other = PermissionUpdate {
            kind: PermissionUpdateType::parse("futureType"),
            rules: None,
            behavior: None,
            mode: None,
            directories: None,
            destination: None,
        };
        assert!(matches!(other.to_wire(), Err(Error::InvalidOptions(_))));
    }

    #[test]
    fn from_wire_keeps_unknown_type() {
        let update = PermissionUpdate::from_wire(&json!({"type": "newThing"})).unwrap();
        assert_eq!(update.kind, PermissionUpdateType::Other("newThing".into()));
    }

    #[test]
    fn set_mode_wire() {
        let wire = PermissionUpdate::set_mode(PermissionMode::Plan)
            .with_destination(PermissionUpdateDestination::LocalSettings)
            .to_wire()
            .unwrap();
        assert_eq!(
            wire,
            json!({"type": "setMode", "destination": "localSettings", "mode": "plan"})
        );
    }
}
