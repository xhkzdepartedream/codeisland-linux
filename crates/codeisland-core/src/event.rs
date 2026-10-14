//! Event-name normalisation and routing.
//!
//! Transcribed from the upstream macOS project:
//! - `Sources/CodeIslandCore/EventNormalizer.swift` (the mapping table)
//! - `Sources/CodeIsland/HookServer.swift::routeKind(for:)` (the routing rule)
//!
//! Every agent CLI spells the same lifecycle moment differently. Claude Code
//! sends `PreToolUse`, Cursor sends `beforeShellExecution`, Gemini sends
//! `BeforeTool`, Hermes sends `pre_tool_call`. Normalising here means the rest
//! of the system only ever sees one vocabulary.
//!
//! Upstream keeps a rule file warning that routing logic is duplicated between
//! the bridge and the server and that the two have drifted apart before, which
//! caused one side to block while the other treated the event as
//! fire-and-forget. Here both call into this single function.

/// What the system should do with an event once it is normalised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// The agent is waiting for a yes/no decision. The bridge blocks on the
    /// socket and the decision travels back to the CLI on stdout.
    Permission,
    /// The agent asked the user a free-form question. Also blocking.
    Question,
    /// Fire-and-forget status update. Nobody waits for an answer.
    Event,
}

/// Normalise an event name from any supported CLI to the internal vocabulary.
///
/// Unknown names pass through unchanged, which keeps a new CLI usable (as a
/// plain status event) even before its mapping is added here.
pub struct EventNormalizer;

impl EventNormalizer {
    pub fn normalize(name: &str) -> &str {
        match name {
            // Cursor (camelCase)
            "beforeSubmitPrompt" => "UserPromptSubmit",
            "beforeShellExecution" => "PreToolUse",
            "afterShellExecution" => "PostToolUse",
            "beforeReadFile" => "PreToolUse",
            "afterFileEdit" => "PostToolUse",
            "beforeMCPExecution" => "PreToolUse",
            "afterMCPExecution" => "PostToolUse",
            "afterAgentThought" => "Notification",
            "afterAgentResponse" => "AfterAgentResponse",
            "stop" => "Stop",
            // Gemini
            "BeforeTool" => "PermissionRequest",
            "AfterTool" => "PostToolUse",
            "BeforeAgent" => "SubagentStart",
            "AfterAgent" => "SubagentStop",
            // GitHub Copilot CLI
            "sessionStart" => "SessionStart",
            "sessionEnd" => "SessionEnd",
            "userPromptSubmitted" => "UserPromptSubmit",
            "preToolUse" => "PreToolUse",
            "postToolUse" => "PostToolUse",
            "errorOccurred" => "Notification",
            // Kiro CLI (camelCase, agent-scoped)
            "agentSpawn" => "SessionStart",
            "userPromptSubmit" => "UserPromptSubmit",
            // Trae CLI (snake_case)
            "session_start" => "SessionStart",
            "session_end" => "SessionEnd",
            "user_prompt_submit" => "UserPromptSubmit",
            "pre_tool_use" => "PreToolUse",
            "post_tool_use" => "PostToolUse",
            "post_tool_use_failure" => "PostToolUseFailure",
            "permission_request" => "PermissionRequest",
            "permission_denied" => "PermissionDenied",
            // Grok emits StopFailure when a turn ends on an API error. It is
            // still terminal for island state, so both spellings fold onto Stop.
            "stop_failure" | "StopFailure" => "Stop",
            "subagent_start" => "SubagentStart",
            "subagent_stop" => "SubagentStop",
            "pre_compact" => "PreCompact",
            "post_compact" => "PostCompact",
            "notification" => "Notification",
            // Hermes (Nous Research) -- snake_case but diverged from Claude/Gemini.
            "pre_tool_call" => "PreToolUse",
            "post_tool_call" => "PostToolUse",
            "pre_llm_call" => "UserPromptSubmit",
            // Both fire once per turn from Hermes's turn finalizer. Despite its
            // name, `on_session_end` is the end of a turn, not of the session --
            // mapping it to SessionEnd deleted the card after every reply.
            // `post_llm_call` keeps its own name so a reply alone never pops a
            // completion; the Stop right behind it does that.
            "post_llm_call" => "AgentTurnSettled",
            "on_session_start" => "SessionStart",
            "on_session_end" => "Stop",
            // The real end of a session: `/new`, quitting the CLI, closing a
            // desktop or TUI session, the gateway's `/new` or shutdown.
            "on_session_finalize" => "SessionEnd",
            "on_session_reset" => "SessionEnd",
            // Cline (VS Code extension)
            "TaskStart" => "SessionStart",
            "TaskResume" => "UserPromptSubmit",
            "TaskComplete" => "TaskRoundComplete",
            "TaskCancel" => "TaskRoundComplete",
            other => other,
        }
    }

    /// Decide how the system should treat an event.
    ///
    /// `has_question` is true when the payload carries a `question` field, which
    /// is how a Notification that is really a question is told apart from an
    /// ordinary notification.
    ///
    /// `asks_user` marks a `PermissionRequest` that is really the agent asking a
    /// question (`AskUserQuestion`, measured 2026-10-14). It arrives on the
    /// permission channel but must not be answered with approve/deny: approving it
    /// without `updatedInput` would run the tool with nobody having answered.
    pub fn route_kind(event_name: &str, has_question: bool, asks_user: bool) -> RouteKind {
        let normalized = Self::normalize(event_name);
        if normalized == "PermissionRequest" {
            return if asks_user { RouteKind::Question } else { RouteKind::Permission };
        }
        if normalized == "Notification" && has_question {
            return RouteKind::Question;
        }
        RouteKind::Event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_event_names_pass_through() {
        assert_eq!(EventNormalizer::normalize("PreToolUse"), "PreToolUse");
        assert_eq!(EventNormalizer::normalize("SessionStart"), "SessionStart");
    }

    #[test]
    fn cursor_and_gemini_events_fold_onto_the_same_vocabulary() {
        assert_eq!(
            EventNormalizer::normalize("beforeShellExecution"),
            "PreToolUse"
        );
        assert_eq!(
            EventNormalizer::normalize("BeforeTool"),
            "PermissionRequest"
        );
        assert_eq!(EventNormalizer::normalize("pre_tool_call"), "PreToolUse");
    }

    #[test]
    fn unknown_names_are_preserved() {
        assert_eq!(EventNormalizer::normalize("SomethingNew"), "SomethingNew");
    }

    #[test]
    fn permission_request_routes_to_permission() {
        assert_eq!(
            EventNormalizer::route_kind("PreToolUse", false, false),
            RouteKind::Event,
            "PreToolUse is a status event on Claude Code, not an approval gate"
        );
        assert_eq!(
            EventNormalizer::route_kind("PermissionRequest", false, false),
            RouteKind::Permission
        );
        assert_eq!(
            EventNormalizer::route_kind("permission_request", false, false),
            RouteKind::Permission,
            "Trae CLI spelling must route identically"
        );
        assert_eq!(
            EventNormalizer::route_kind("BeforeTool", false, false),
            RouteKind::Permission,
            "Gemini spelling must route identically"
        );
    }

    #[test]
    fn notification_is_only_a_question_when_a_question_is_present() {
        assert_eq!(
            EventNormalizer::route_kind("Notification", false, false),
            RouteKind::Event
        );
        assert_eq!(
            EventNormalizer::route_kind("Notification", true, false),
            RouteKind::Question
        );
    }
}

    #[test]
    fn an_ask_user_question_routes_to_the_question_channel() {
        // Measured: AskUserQuestion arrives as PermissionRequest. Approving it like a
        // tool call would run the tool with no answer in it, so it must leave the
        // permission route.
        assert_eq!(
            EventNormalizer::route_kind("PermissionRequest", false, true),
            RouteKind::Question
        );
    }
