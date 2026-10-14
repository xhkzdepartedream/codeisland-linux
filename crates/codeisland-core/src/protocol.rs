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

/// The same envelope, carrying a rewritten tool input.
///
/// `updatedInput` is how a question gets answered: measured on 2026-10-14, the CLI
/// runs `AskUserQuestion` with exactly the input handed back here, and the tool's
/// `PostToolUse` shows it. Upstream refuses a non-object (`"an allow decision whose
/// updatedInput is not an object"`), so a non-object is dropped rather than emitted --
/// a reply the CLI would reject is indistinguishable from a broken bridge.
pub fn permission_answer(
    event_name: &str,
    behavior: Behavior,
    updated_input: Option<&serde_json::Value>,
) -> String {
    let Some(input) = updated_input.filter(|value| value.is_object()) else {
        return permission_response(event_name, behavior);
    };
    let behavior = match behavior {
        Behavior::Allow => "allow",
        Behavior::Deny => "deny",
    };
    // serde_json's Display is valid JSON by construction, so interpolating the value
    // cannot break the envelope's quoting.
    format!(
        r#"{{"hookSpecificOutput":{{"hookEventName":"{}","decision":{{"behavior":"{}","updatedInput":{}}}}}}}"#,
        event_name, behavior, input
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
    /// Which terminal the hook process was running in, filled in by the bridge from
    /// its own process ancestry and environment. The agent's payload never says
    /// "window", so this is the only route back to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<crate::terminal::TerminalContext>,
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
        let asks_user = self.tool_name.as_deref() == Some(crate::question::ASK_TOOL);
        crate::event::EventNormalizer::route_kind(name, self.question.is_some(), asks_user)
    }

    /// The one line the island card shows for this event.
    ///
    /// Preference order is what a human scanning cards needs: a question the
    /// agent asked outranks everything, then the tool summary, then any message
    /// the CLI attached.
    pub fn detail_line(&self) -> Option<String> {
        if let Some(question) = self.question.as_deref().filter(|s| !s.trim().is_empty()) {
            return Some(question.to_string());
        }
        if let Some(summary) = self.summary.as_deref().filter(|s| !s.trim().is_empty()) {
            return Some(summary.to_string());
        }
        self.extra
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_string)
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
    fn an_answer_carries_the_rewritten_input() {
        // The exact shape the CLI accepted on 2026-10-14, minus the whitespace.
        let input = serde_json::json!({"answers": {"Red or blue?": "Blue"}, "annotations": {}});
        let out = permission_answer("PermissionRequest", Behavior::Allow, Some(&input));
        assert_eq!(
            out,
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"annotations":{},"answers":{"Red or blue?":"Blue"}}}}}"#
        );
        // It must stay parseable, or the CLI silently ignores the decision.
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Red or blue?"],
            "Blue"
        );
    }

    #[test]
    fn a_non_object_input_is_dropped_instead_of_sent() {
        // Upstream rejects an allow whose updatedInput is not an object; emitting one
        // would look like a broken bridge, so fall back to the plain envelope.
        let scalar = serde_json::json!("Blue");
        let out = permission_answer("PermissionRequest", Behavior::Allow, Some(&scalar));
        assert_eq!(
            out,
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
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
