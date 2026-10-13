//! Session state: what the island actually displays.
//!
//! The hook protocol is a firehose of per-tool events; the island needs one row
//! per agent session with a single status. This module is that reduction, and it
//! lives in `codeisland-core` so the daemon owns it once.
//!
//! Deliberate design choice: the state machine is pure -- it takes an event and
//! returns a change, with no I/O, no clock reads and no threading. That is what
//! makes the transitions testable without a socket, and it keeps the daemon and
//! any future UI sharing one definition instead of each deriving their own. The
//! upstream project's own rule file about permission routing drifting between the
//! bridge and the server is the cautionary tale here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::RouteKind;
use crate::protocol::HookEvent;

/// What an agent session is doing, as the island must present it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Busy: running a tool or generating.
    Working,
    /// Blocked on a permission decision. Highest urgency.
    WaitingApproval,
    /// Blocked on a free-form question from the agent.
    WaitingInput,
    /// Turn finished, waiting for the user's next prompt.
    Done,
    /// Session exists but nothing is happening.
    Idle,
    /// Turn ended on an error.
    Error,
}

impl SessionStatus {
    /// Urgency order. The island sorts blocked sessions to the top, which is the
    /// entire point of the tool: telling you who needs you right now.
    pub fn urgency(&self) -> u8 {
        match self {
            SessionStatus::WaitingApproval => 0,
            SessionStatus::WaitingInput => 1,
            SessionStatus::Error => 2,
            SessionStatus::Working => 3,
            SessionStatus::Done => 4,
            SessionStatus::Idle => 5,
        }
    }
}

/// One agent session as rendered on the island.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub source: String,
    pub status: SessionStatus,
    /// Directory the agent is working in; the island shows its last component as
    /// the session's human label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Git branch, when it could be determined. Later phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Latest human-readable activity line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Unix epoch milliseconds of the last event that moved this session.
    pub last_event_ms: u64,
    /// Unix epoch milliseconds when this session appeared.
    pub started_ms: u64,
    /// Count of tool calls observed, for the card's activity figure.
    pub tool_calls: u32,
}

impl Session {
    /// Last path component of the working directory, which is what fits on a card.
    pub fn project_label(&self) -> String {
        self.cwd
            .as_deref()
            .and_then(|cwd| cwd.trim_end_matches('/').rsplit('/').next())
            .filter(|s| !s.is_empty())
            .unwrap_or(self.source.as_str())
            .to_string()
    }
}

/// A change the daemon should push to connected UI clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewUpdate {
    /// Full state, sent when a client first connects so it never has to replay
    /// history.
    Snapshot { sessions: Vec<Session> },
    /// One session changed. Carrying the whole session rather than a diff keeps
    /// the client trivial: last write wins.
    SessionUpsert { session: Session },
    /// One session is gone.
    SessionRemove { id: String },
}

/// All live sessions.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<String, Session>,
    /// Insertion order, because HashMap iteration would make the island's card
    /// order jitter between updates.
    order: Vec<String>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Sessions in display order: most urgent first, then most recently active.
    pub fn ordered(&self) -> Vec<&Session> {
        let mut list: Vec<&Session> = self.order.iter().filter_map(|id| self.sessions.get(id)).collect();
        list.sort_by(|a, b| {
            a.status
                .urgency()
                .cmp(&b.status.urgency())
                .then_with(|| b.last_event_ms.cmp(&a.last_event_ms))
        });
        list
    }

    pub fn snapshot(&self) -> ViewUpdate {
        ViewUpdate::Snapshot {
            sessions: self.ordered().into_iter().cloned().collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Apply one hook event and return the update the UI needs, if any.
    ///
    /// `now_ms` is passed in rather than read from the clock so the transitions
    /// below are deterministic under test.
    pub fn apply(&mut self, event: &HookEvent, now_ms: u64) -> Option<ViewUpdate> {
        let id = event.session_id.clone()?;
        let name = event
            .normalized_event
            .as_deref()
            .or(event.event.as_deref())
            .unwrap_or("")
            .to_string();
        let source = event.source.clone().unwrap_or_else(|| "unknown".into());

        let existed = self.sessions.contains_key(&id);
        let mut session = if existed {
            self.sessions.get(&id).cloned().expect("just checked")
        } else {
            Session {
                id: id.clone(),
                source: source.clone(),
                status: SessionStatus::Idle,
                cwd: None,
                branch: None,
                detail: None,
                last_event_ms: now_ms,
                started_ms: now_ms,
                tool_calls: 0,
            }
        };

        // cwd and source arrive on many events, not just the first, so refresh
        // whenever present.
        if let Some(cwd) = &event.cwd {
            session.cwd = Some(cwd.clone());
        }
        if source != "unknown" {
            session.source = source;
        }

        // SessionEnd removes the card entirely; everything else updates it.
        if name == "SessionEnd" {
            self.sessions.remove(&id);
            self.order.retain(|x| x != &id);
            return Some(ViewUpdate::SessionRemove { id });
        }

        session.last_event_ms = now_ms;
        match event.route_kind() {
            RouteKind::Permission => session.status = SessionStatus::WaitingApproval,
            RouteKind::Question => session.status = SessionStatus::WaitingInput,
            RouteKind::Event => {
                session.status = match name.as_str() {
                    "SessionStart" => SessionStatus::Working,
                    "UserPromptSubmit" => SessionStatus::Working,
                    "PreToolUse" => SessionStatus::Working,
                    "PostToolUse" => SessionStatus::Working,
                    "PostToolUseFailure" | "PermissionDenied" => SessionStatus::Error,
                    // A turn settling without a Stop is still the agent working;
                    // Hermes sends `post_llm_call` immediately before Stop.
                    "AgentTurnSettled" | "SubagentStart" | "SubagentStop" | "AfterAgentResponse" => {
                        SessionStatus::Working
                    }
                    "Notification" => SessionStatus::Idle,
                    "Stop" => SessionStatus::Done,
                    _ if existed => session.status,
                    _ => SessionStatus::Working,
                }
            }
        }

        if name == "PreToolUse" {
            session.tool_calls += 1;
        }
        if let Some(detail) = event.detail_line() {
            session.detail = Some(detail);
        }

        let update = ViewUpdate::SessionUpsert {
            session: session.clone(),
        };
        if !existed {
            self.order.push(id.clone());
        }
        self.sessions.insert(id, session);
        Some(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, session: &str) -> HookEvent {
        HookEvent {
            source: Some("claude".into()),
            event: Some(name.into()),
            normalized_event: Some(name.into()),
            session_id: Some(session.into()),
            cwd: Some("/home/dev/projects/my-app".into()),
            ..Default::default()
        }
    }

    #[test]
    fn session_start_creates_a_working_session() {
        let mut reg = SessionRegistry::new();
        let update = reg.apply(&event("SessionStart", "s1"), 1000).unwrap();
        match update {
            ViewUpdate::SessionUpsert { session } => {
                assert_eq!(session.status, SessionStatus::Working);
                assert_eq!(session.id, "s1");
                assert_eq!(session.project_label(), "my-app");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn permission_request_marks_waiting_approval() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "s1"), 1000);
        let mut e = event("PermissionRequest", "s1");
        e.tool_name = Some("Bash".into());
        e.summary = Some("Bash: rm -rf /tmp/x".into());
        reg.apply(&e, 2000);
        assert_eq!(reg.get("s1").unwrap().status, SessionStatus::WaitingApproval);
        assert_eq!(reg.get("s1").unwrap().detail.as_deref(), Some("Bash: rm -rf /tmp/x"));
    }

    #[test]
    fn stop_marks_done_and_does_not_delete() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "s1"), 1000);
        reg.apply(&event("Stop", "s1"), 2000);
        assert_eq!(reg.get("s1").unwrap().status, SessionStatus::Done);
        assert_eq!(reg.len(), 1, "a finished turn keeps its card");
    }

    #[test]
    fn session_end_deletes() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "s1"), 1000);
        let update = reg.apply(&event("SessionEnd", "s1"), 2000).unwrap();
        assert_eq!(
            update,
            ViewUpdate::SessionRemove {
                id: "s1".into()
            }
        );
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn ordering_puts_blocked_sessions_first() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "working"), 1000);
        reg.apply(&event("Stop", "finished"), 1000);
        reg.apply(&event("SessionStart", "blocked"), 1000);
        let mut perm = event("PermissionRequest", "blocked");
        perm.tool_name = Some("Edit".into());
        reg.apply(&perm, 1500);

        let ids: Vec<&str> = reg.ordered().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids[0], "blocked", "the session needing you must be first");
        assert_eq!(*ids.last().unwrap(), "finished");
    }

    #[test]
    fn tool_calls_are_counted() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "s1"), 1000);
        for _ in 0..3 {
            reg.apply(&event("PreToolUse", "s1"), 2000);
        }
        assert_eq!(reg.get("s1").unwrap().tool_calls, 3);
    }

    #[test]
    fn events_for_an_unknown_session_do_not_get_dropped() {
        // Some CLIs skip SessionStart. A card that appears late beats no card.
        let mut reg = SessionRegistry::new();
        reg.apply(&event("PostToolUse", "orphan"), 1000);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("orphan").unwrap().status, SessionStatus::Working);
    }

    #[test]
    fn snapshot_is_a_full_serializable_state() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "s1"), 1000);
        let snap = reg.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"type\":\"snapshot\""), "{json}");
        assert!(json.contains("\"s1\""), "{json}");
    }

    #[test]
    fn project_label_falls_back_to_source_when_no_cwd() {
        let mut reg = SessionRegistry::new();
        let mut e = event("SessionStart", "s1");
        e.cwd = None;
        reg.apply(&e, 1000);
        assert_eq!(reg.get("s1").unwrap().project_label(), "claude");
    }

    #[test]
    fn urgency_is_a_total_order_matching_the_display() {
        assert!(SessionStatus::WaitingApproval.urgency() < SessionStatus::WaitingInput.urgency());
        assert!(SessionStatus::WaitingInput.urgency() < SessionStatus::Working.urgency());
        assert!(SessionStatus::Working.urgency() < SessionStatus::Done.urgency());
    }
}
