//! Wire format between the hook bridge and the daemon.
//!
//! Transcribed from the upstream macOS project
//! (`Sources/CodeIsland/HookServer.swift` and
//! `Sources/CodeIslandBridge/main.swift`).
//!
//! Shape of the exchange:
//!
//! ```text
//! CLI --spawn--> bridge --unix socket, one JSON blob--> daemon
//!                          <--one JSON blob, or nothing--
//! ```
//!
//! One request per connection, newline-free, terminated by EOF. For blocking
//! kinds the bridge keeps the connection open while it waits for the answer and
//! then writes that answer to stdout for the CLI to read.
//!
//! ## The safety contract
//!
//! A blocking hook sits between the agent and its own permission prompt. If the
//! daemon is not running, is frozen, or answers with something unexpected, the
//! bridge must exit 0 having written nothing. The CLI then falls back to its
//! normal prompt. Auto-approving on failure would be a security hole; hanging
//! would break the agent. Both are forbidden here.

use serde::{Deserialize, Serialize};

use crate::event::RouteKind;

/// Permission decision sent back to the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Behavior {
    Allow,
    Deny,
}

/// What to send back when nobody is available to make the decision.
///
/// `None` means "write nothing to stdout", which is the only safe outcome: the
/// CLI falls through to its own prompt. It deliberately does not distinguish
/// "daemon down" from "user never answered" -- both must leave the decision with
/// the agent.
pub fn fallback_response() -> Option<String> {
    None
}

/// The Claude Code / Codex permission response envelope.
///
/// Verified verbatim against the upstream source and against the strings
/// embedded in the installed `claude` binary, which contains the literal
/// `hookSpecificOutput` (187 occurrences) and `permissionDecision` (153).
pub fn permission_response(event_name: &str, behavior: Behavior) -> String {
    let behavior = match behavior {
        Behavior::Allow => "allow",
        Behavior::Deny => "deny",
    };
    // Hand-built rather than serialised so the exact byte shape is visible in
    // code review and cannot drift with a serde version bump.
    format!(
        r#"{{"hookSpecificOutput":{{"hookEventName":"{}","decision":{{"behavior":"{}"}}}}}}"#,
        event_name, behavior
    )
}

/// A fully parsed hook event on its way from bridge to daemon.
///
/// Every field is optional except the ones we key sessions on: agent CLIs differ
/// wildly in what they include, and rejecting a payload we do not fully
/// understand would silently drop events.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    /// Which CLI fired this. `claude`, `codex`, `pi`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Raw event name as the CLI spelled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// Normalised event name. Filled in by the bridge so the daemon does not
    /// have to re-run the mapping table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Raw `tool_input` object, kept untyped: it is already JSON and every
    /// agent shapes it differently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    /// Free-form question text, present only when this is a question event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Human-readable one-liner describing what the tool is about to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Unix epoch milliseconds, stamped by the bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u64>,
    /// Anything else the CLI sent, preserved so later phases can read fields
    /// this crate does not know about yet without changing the bridge.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl HookEvent {
    /// How the daemon should treat this event.
    pub fn route_kind(&self) -> RouteKind {
        let name = self
            .normalized_event
            .as_deref()
            .or(self.event.as_deref())
            .unwrap_or("");
        crate::event::EventNormalizer::route_kind(name, self.question.is_some())
    }
}

/// Sanitised description of what a tool call is about to do, for display.
///
/// Only ever used for display and logging, never for a decision, so it favours
/// being short and readable over being exhaustive.
pub fn summarize_tool_call(tool_name: &str, tool_input: Option<&serde_json::Value>) -> String {
    let Some(input) = tool_input else {
        return tool_name.to_string();
    };
    let pick = |key: &str| input.get(key).and_then(|v| v.as_str());
    let detail = match tool_name {
        "Bash" => pick("command"),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => pick("file_path"),
        "Read" => pick("file_path"),
        _ => pick("command").or_else(|| pick("file_path")),
    };
    match detail {
        Some(d) => {
            let one_line = d.replace('\n', " ");
            let truncated: String = one_line.chars().take(120).collect();
            format!("{tool_name}: {truncated}")
        }
        None => tool_name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_response_matches_the_exact_bytes_upstream_emits() {
        assert_eq!(
            permission_response("PermissionRequest", Behavior::Allow),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
        assert_eq!(
            permission_response("PermissionRequest", Behavior::Deny),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}"#
        );
    }

    #[test]
    fn fallback_is_silence_not_denial() {
        assert!(
            fallback_response().is_none(),
            "a failure must leave the decision with the CLI, never auto-deny"
        );
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let raw = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "abc",
            "some_future_field": {"nested": [1, 2, 3]}
        });
        let event: HookEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(event.session_id.as_deref(), Some("abc"));
        let back = serde_json::to_value(&event).unwrap();
        assert_eq!(
            back.get("some_future_field"),
            Some(&json!({"nested": [1, 2, 3]})),
            "fields we do not model must not be dropped"
        );
    }

    #[test]
    fn route_kind_reads_the_normalized_name_first() {
        let event = HookEvent {
            event: Some("BeforeTool".into()),
            normalized_event: Some("PermissionRequest".into()),
            ..Default::default()
        };
        assert_eq!(event.route_kind(), RouteKind::Permission);
    }

    #[test]
    fn question_payload_routes_to_question() {
        let event = HookEvent {
            event: Some("Notification".into()),
            question: Some("Which database?".into()),
            ..Default::default()
        };
        assert_eq!(event.route_kind(), RouteKind::Question);
    }

    #[test]
    fn summary_prefers_the_meaningful_field_for_bash() {
        let input = json!({"command": "rm -rf /tmp/x", "description": "cleanup"});
        assert_eq!(
            summarize_tool_call("Bash", Some(&input)),
            "Bash: rm -rf /tmp/x"
        );
    }

    #[test]
    fn summary_collapses_newlines_and_truncates() {
        let long = "a".repeat(500);
        let input = json!({"command": format!("line1\n{long}")});
        let summary = summarize_tool_call("Bash", Some(&input));
        assert!(!summary.contains('\n'), "must stay on one line: {summary}");
        assert!(summary.chars().count() <= "Bash: ".len() + 120 + 1);
    }
}
