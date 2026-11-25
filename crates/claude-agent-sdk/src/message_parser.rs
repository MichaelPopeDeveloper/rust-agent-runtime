//! Parse raw CLI JSON frames into typed [`Message`] values.
//!
//! Mirrors the Python SDK's `parse_message`, with one deliberate difference:
//! this takes the frame [`Value`] **by value** and *moves* fields out of it
//! rather than cloning each one. For a message carrying a large payload (a big
//! tool result, a long assistant turn), that avoids duplicating the strings and
//! sub-trees — the frame is parsed once and projected into typed structs with
//! no second allocation. Unknown `type`s are returned as [`Message::Other`]
//! rather than dropped, so a newer CLI never breaks an older SDK.

use serde_json::{Map, Value};

use crate::errors::{Error, Result};
use crate::types::*;

type Object = Map<String, Value>;

fn missing(kind: &str, field: &str) -> Error {
    Error::message_parse(
        format!("Missing required field in {kind} message: '{field}'"),
        None,
    )
}

/// Remove `key` and return it if it is a string.
fn take_string(map: &mut Object, key: &str) -> Option<String> {
    match map.remove(key) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

fn take_i64(map: &mut Object, key: &str) -> Option<i64> {
    map.remove(key).as_ref().and_then(Value::as_i64)
}

fn take_f64(map: &mut Object, key: &str) -> Option<f64> {
    map.remove(key).as_ref().and_then(Value::as_f64)
}

fn take_bool(map: &mut Object, key: &str) -> Option<bool> {
    map.remove(key).as_ref().and_then(Value::as_bool)
}

/// Remove `key` and return it as a list of strings (dropping non-string items),
/// or `None` when absent or not an array.
fn take_string_list(map: &mut Object, key: &str) -> Option<Vec<String>> {
    match map.remove(key) {
        Some(Value::Array(items)) => Some(
            items
                .into_iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Remove and return `origin` if it is an object with a string `kind`.
fn take_origin(map: &mut Object) -> Option<MessageOrigin> {
    match map.remove("origin") {
        Some(Value::Object(fields)) => {
            let kind = fields.get("kind").and_then(Value::as_str)?.to_string();
            Some(MessageOrigin { kind, fields })
        }
        _ => None,
    }
}

fn parse_content_block(block: Value) -> Result<ContentBlock> {
    let mut b = match block {
        Value::Object(map) => map,
        other => return Ok(ContentBlock::Unknown(other)),
    };
    let block_type = match b.get("type").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => return Ok(ContentBlock::Unknown(Value::Object(b))),
    };
    Ok(match block_type.as_str() {
        "text" => ContentBlock::Text(TextBlock {
            text: take_string(&mut b, "text").ok_or_else(|| missing("content", "text"))?,
        }),
        "thinking" => ContentBlock::Thinking(ThinkingBlock {
            thinking: take_string(&mut b, "thinking")
                .ok_or_else(|| missing("content", "thinking"))?,
            signature: take_string(&mut b, "signature").unwrap_or_default(),
        }),
        "tool_use" => ContentBlock::ToolUse(ToolUseBlock {
            id: take_string(&mut b, "id").ok_or_else(|| missing("content", "id"))?,
            name: take_string(&mut b, "name").ok_or_else(|| missing("content", "name"))?,
            input: b.remove("input").unwrap_or(Value::Null),
        }),
        "tool_result" => ContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: take_string(&mut b, "tool_use_id")
                .ok_or_else(|| missing("content", "tool_use_id"))?,
            content: b.remove("content"),
            is_error: take_bool(&mut b, "is_error"),
        }),
        "server_tool_use" => ContentBlock::ServerToolUse(ServerToolUseBlock {
            id: take_string(&mut b, "id").ok_or_else(|| missing("content", "id"))?,
            name: take_string(&mut b, "name").ok_or_else(|| missing("content", "name"))?,
            input: b.remove("input").unwrap_or(Value::Null),
        }),
        "advisor_tool_result" => ContentBlock::ServerToolResult(ServerToolResultBlock {
            tool_use_id: take_string(&mut b, "tool_use_id")
                .ok_or_else(|| missing("content", "tool_use_id"))?,
            content: b.remove("content").unwrap_or(Value::Null),
        }),
        _ => ContentBlock::Unknown(Value::Object(b)),
    })
}

fn parse_blocks(content: Value) -> Result<Vec<ContentBlock>> {
    match content {
        Value::Array(blocks) => {
            let mut out = Vec::with_capacity(blocks.len());
            for block in blocks {
                out.push(parse_content_block(block)?);
            }
            Ok(out)
        }
        _ => Err(Error::message_parse(
            "Invalid assistant content (expected list)",
            None,
        )),
    }
}

/// Parse a raw CLI JSON frame into a typed [`Message`], consuming the value.
pub fn parse_message(data: Value) -> Result<Message> {
    let mut obj = match data {
        Value::Object(map) => map,
        other => {
            return Err(Error::message_parse(
                "Invalid message data type (expected object)",
                Some(other),
            ))
        }
    };

    let message_type = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::message_parse("Message missing 'type' field", None))?
        .to_string();

    match message_type.as_str() {
        "user" => {
            let origin = take_origin(&mut obj);
            let uuid = take_string(&mut obj, "uuid");
            let parent_tool_use_id = take_string(&mut obj, "parent_tool_use_id");
            let tool_use_result = obj.remove("tool_use_result");
            let session_id = take_string(&mut obj, "session_id");
            let mut message = match obj.remove("message") {
                Some(Value::Object(m)) => m,
                _ => return Err(missing("user", "message")),
            };
            let content = match message.remove("content") {
                Some(Value::Array(blocks)) => {
                    let mut out = Vec::with_capacity(blocks.len());
                    for block in blocks {
                        out.push(parse_content_block(block)?);
                    }
                    UserContent::Blocks(out)
                }
                Some(Value::String(s)) => UserContent::Text(s),
                Some(other) => UserContent::Text(other.to_string()),
                None => return Err(missing("user", "content")),
            };
            Ok(Message::User(UserMessage {
                content,
                uuid,
                parent_tool_use_id,
                tool_use_result,
                origin,
                session_id,
            }))
        }
        "assistant" => {
            let parent_tool_use_id = take_string(&mut obj, "parent_tool_use_id");
            let error = take_string(&mut obj, "error");
            let session_id = take_string(&mut obj, "session_id");
            let uuid = take_string(&mut obj, "uuid");
            let user_message_uuid = take_string(&mut obj, "user_message_uuid");
            let user_message_uuids = take_string_list(&mut obj, "user_message_uuids");
            let mut message = match obj.remove("message") {
                Some(Value::Object(m)) => m,
                _ => return Err(missing("assistant", "message")),
            };
            let model =
                take_string(&mut message, "model").ok_or_else(|| missing("assistant", "model"))?;
            let content = parse_blocks(
                message
                    .remove("content")
                    .ok_or_else(|| missing("assistant", "content"))?,
            )?;
            let usage = message.remove("usage");
            let message_id = take_string(&mut message, "id");
            let stop_reason = take_string(&mut message, "stop_reason");
            Ok(Message::Assistant(AssistantMessage {
                content,
                model,
                parent_tool_use_id,
                error,
                usage,
                message_id,
                stop_reason,
                session_id,
                uuid,
                user_message_uuid,
                user_message_uuids,
            }))
        }
        "system" => {
            let subtype = obj
                .get("subtype")
                .and_then(Value::as_str)
                .ok_or_else(|| missing("system", "subtype"))?
                .to_string();
            Ok(Message::System(SystemMessage {
                subtype,
                data: Value::Object(obj),
            }))
        }
        "result" => {
            let deferred_tool_use = match obj.remove("deferred_tool_use") {
                Some(Value::Object(mut d)) => Some(DeferredToolUse {
                    id: take_string(&mut d, "id")
                        .ok_or_else(|| missing("result", "deferred id"))?,
                    name: take_string(&mut d, "name")
                        .ok_or_else(|| missing("result", "deferred name"))?,
                    input: d.remove("input").unwrap_or(Value::Null),
                }),
                _ => None,
            };
            let permission_denials = match obj.remove("permission_denials") {
                Some(Value::Array(items)) => Some(items),
                _ => None,
            };
            Ok(Message::Result(ResultMessage {
                subtype: take_string(&mut obj, "subtype")
                    .ok_or_else(|| missing("result", "subtype"))?,
                duration_ms: take_i64(&mut obj, "duration_ms")
                    .ok_or_else(|| missing("result", "duration_ms"))?,
                duration_api_ms: take_i64(&mut obj, "duration_api_ms")
                    .ok_or_else(|| missing("result", "duration_api_ms"))?,
                is_error: take_bool(&mut obj, "is_error")
                    .ok_or_else(|| missing("result", "is_error"))?,
                num_turns: take_i64(&mut obj, "num_turns")
                    .ok_or_else(|| missing("result", "num_turns"))?,
                session_id: take_string(&mut obj, "session_id")
                    .ok_or_else(|| missing("result", "session_id"))?,
                stop_reason: take_string(&mut obj, "stop_reason"),
                total_cost_usd: take_f64(&mut obj, "total_cost_usd"),
                usage: obj.remove("usage"),
                result: take_string(&mut obj, "result"),
                structured_output: obj.remove("structured_output"),
                model_usage: obj.remove("modelUsage"),
                permission_denials,
                deferred_tool_use,
                errors: take_string_list(&mut obj, "errors"),
                api_error_status: take_i64(&mut obj, "api_error_status"),
                uuid: take_string(&mut obj, "uuid"),
                terminal_reason: take_string(&mut obj, "terminal_reason"),
                origin: take_origin(&mut obj),
                user_message_uuid: take_string(&mut obj, "user_message_uuid"),
                user_message_uuids: take_string_list(&mut obj, "user_message_uuids"),
            }))
        }
        "stream_event" => Ok(Message::StreamEvent(StreamEvent {
            uuid: take_string(&mut obj, "uuid").ok_or_else(|| missing("stream_event", "uuid"))?,
            session_id: take_string(&mut obj, "session_id")
                .ok_or_else(|| missing("stream_event", "session_id"))?,
            event: obj
                .remove("event")
                .ok_or_else(|| missing("stream_event", "event"))?,
            parent_tool_use_id: take_string(&mut obj, "parent_tool_use_id"),
            user_message_uuid: take_string(&mut obj, "user_message_uuid"),
            user_message_uuids: take_string_list(&mut obj, "user_message_uuids"),
        })),
        "rate_limit_event" => {
            let uuid =
                take_string(&mut obj, "uuid").ok_or_else(|| missing("rate_limit_event", "uuid"))?;
            let session_id = take_string(&mut obj, "session_id")
                .ok_or_else(|| missing("rate_limit_event", "session_id"))?;
            let mut info = match obj.remove("rate_limit_info") {
                Some(Value::Object(m)) => m,
                _ => return Err(missing("rate_limit_event", "rate_limit_info")),
            };
            let status = take_string(&mut info, "status")
                .ok_or_else(|| missing("rate_limit_event", "status"))?;
            let resets_at = take_i64(&mut info, "resetsAt");
            let rate_limit_type = take_string(&mut info, "rateLimitType");
            let utilization = take_f64(&mut info, "utilization");
            let overage_status = take_string(&mut info, "overageStatus");
            let overage_resets_at = take_i64(&mut info, "overageResetsAt");
            let overage_disabled_reason = take_string(&mut info, "overageDisabledReason");
            Ok(Message::RateLimit(RateLimitEvent {
                rate_limit_info: RateLimitInfo {
                    status,
                    resets_at,
                    rate_limit_type,
                    utilization,
                    overage_status,
                    overage_resets_at,
                    overage_disabled_reason,
                    raw: Value::Object(info),
                },
                uuid,
                session_id,
            }))
        }
        "conversation_reset" => Ok(Message::ConversationReset(ConversationResetMessage {
            new_conversation_id: take_string(&mut obj, "new_conversation_id")
                .ok_or_else(|| missing("conversation_reset", "new_conversation_id"))?,
            uuid: take_string(&mut obj, "uuid")
                .ok_or_else(|| missing("conversation_reset", "uuid"))?,
            session_id: take_string(&mut obj, "session_id")
                .ok_or_else(|| missing("conversation_reset", "session_id"))?,
        })),
        _ => Ok(Message::Other(Value::Object(obj))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_assistant_with_blocks() {
        let data = json!({
            "type": "assistant",
            "message": {
                "model": "claude-opus-5-5",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
                ],
            },
            "session_id": "s1",
            "uuid": "u1",
        });
        let message = parse_message(data).unwrap();
        let assistant = message.as_assistant().unwrap();
        assert_eq!(assistant.model, "claude-opus-5-5");
        assert_eq!(assistant.content.len(), 3);
        assert_eq!(assistant.content[0].as_text(), Some("hi"));
        assert_eq!(assistant.content[2].as_tool_use().unwrap().name, "Bash");
    }

    #[test]
    fn parses_result() {
        let data = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 10,
            "duration_api_ms": 8,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s1",
            "result": "done",
            "total_cost_usd": 0.01,
        });
        let result = parse_message(data).unwrap();
        let result = result.as_result().unwrap();
        assert_eq!(result.result.as_deref(), Some("done"));
        assert_eq!(result.total_cost_usd, Some(0.01));
        assert!(!result.is_error);
    }

    #[test]
    fn unknown_type_is_other() {
        let data = json!({"type": "brand_new_thing", "foo": 1});
        assert!(matches!(parse_message(data).unwrap(), Message::Other(_)));
    }

    #[test]
    fn user_string_and_blocks() {
        let text = parse_message(json!({
            "type": "user",
            "message": {"role": "user", "content": "hello"},
        }))
        .unwrap();
        match text {
            Message::User(UserMessage {
                content: UserContent::Text(t),
                ..
            }) => {
                assert_eq!(t, "hello")
            }
            other => panic!("unexpected: {other:?}"),
        }
        let blocks = parse_message(json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
        }))
        .unwrap();
        match blocks {
            Message::User(UserMessage {
                content: UserContent::Blocks(b),
                ..
            }) => {
                assert_eq!(b.len(), 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_required_field_errors() {
        let err = parse_message(json!({"type": "result", "subtype": "success"})).unwrap_err();
        assert!(matches!(err, Error::MessageParse { .. }));
    }

    #[test]
    fn system_init_exposed() {
        let msg = parse_message(json!({
            "type": "system",
            "subtype": "init",
            "model": "claude-opus-5-5",
            "session_id": "s1",
        }))
        .unwrap();
        match msg {
            Message::System(system) => {
                assert_eq!(system.subtype, "init");
                assert_eq!(system.init_data().unwrap()["model"], "claude-opus-5-5");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn origin_preserved_on_user() {
        let msg = parse_message(json!({
            "type": "user",
            "message": {"role": "user", "content": "hi"},
            "origin": {"kind": "peer", "from": "agent-7"},
        }))
        .unwrap();
        match msg {
            Message::User(user) => {
                let origin = user.origin.unwrap();
                assert_eq!(origin.kind, "peer");
                assert_eq!(origin.fields["from"], "agent-7");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
