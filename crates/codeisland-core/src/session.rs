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

/// How the island learned about a session, and the difference is a claim about truth.
///
/// `Hook` means the agent told us, live: its status can say "waiting for you" because a
/// request is genuinely parked on a socket. `Discovered` means we inferred the session from a
/// transcript file that a live process owns: we may say "writing" or "quiet", and we may not
/// say anything about what it is waiting for, because no socket proves that. The UI reads this
/// to keep the two apart instead of letting an inferred row look like a real one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Established by a hook event.
    #[default]
    Hook,
    /// Inferred from a live process and its transcript.
    Discovered,
}

/// How long a discovered row survives after discovery stops seeing it.
/// Grace because one skipped scan (a slow directory walk) must not make a live session
/// blink out of the island.
pub const DISCOVERED_GRACE_MS: u64 = 30_000;

/// How long a hook row survives after the agent's own event stream goes silent,
/// when nothing else speaks for it either: no parked request, and no live process
/// discovery can still pair it to.
///
/// A hook row is the agent's statement, so it is never dropped on a whim -- only
/// when every witness has gone quiet. Upstream removes the same sessions on the
/// same clock: its maintenance timer drops stale sessions without a process
/// monitor after `defaultStaleIdleMinutes = 10`. The agent thinking for minutes
/// is covered on this side by discovery: while its process lives, the id stays in
/// `alive` and the row stays up no matter how long the last tool call takes.
pub const HOOK_STALE_MS: u64 = 10 * 60 * 1000;

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

/// Token usage a harness stated about one session.
///
/// codex writes a running cumulative total (`event_msg`/`token_count` →
/// `payload.info.total_token_usage`), so the newest row IS the session's
/// usage. Measured on this machine: 941 such rows across two days of rollouts.
/// claude's `message.usage` is per-response and would need a whole-file sum
/// (unbounded on 12 MB transcripts), so it stays out until a bounded source
/// is measured; pi has no structured usage records at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One agent session as rendered on the island.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub source: String,
    /// The conversation's own title, when the harness stated one. The row leads
    /// with this and falls back to the project label when absent -- see
    /// `SessionTitleStore.swift` upstream, which reads the same sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Token usage the harness stated about this session, when it does. Codex's
    /// cumulative counter; see `Usage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
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
    /// How this session became known. Defaults to Hook so frames written before discovery
    /// existed, and any CLI that omits it, keep behaving as the authoritative case.
    #[serde(default)]
    pub origin: Origin,
    /// True when `status` is something the harness *stated* rather than something inferred.
    ///
    /// A discovered row's status comes from one of two places: the transcript's own turn
    /// lifecycle record (codex `task_started`/`task_complete`, pi's toolCall-vs-text, claude's
    /// tool_use-vs-text) -- a statement of fact -- or, when the file states nothing, a fallback
    /// guess from how recently it was written. Only the second deserves the word "inferred" on
    /// the card, and this flag is what lets the UI make that distinction instead of labelling
    /// both alike.
    #[serde(default)]
    pub status_stated: bool,
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
    /// A tool call is waiting for this client's yes/no. The hook process is
    /// parked on its socket until a `ViewCommand::PermissionDecision` arrives or
    /// the request is withdrawn.
    PermissionRequested { request: PermissionRequestView },
    /// The request no longer needs an answer -- answered elsewhere, timed out, or
    /// the agent gave up waiting. The UI must drop its card.
    PermissionResolved { request_id: String },
}

/// A pending permission request as the island needs to see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequestView {
    /// Correlates the decision back to the parked hook connection.
    pub request_id: String,
    pub session_id: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// What the tool is about to do, already reduced to one display line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Set for a question card (`AskUserQuestion`): what to offer. Empty for a
    /// plain approve/deny gate, and skipped on the wire so an older client sees
    /// exactly the shape it already knows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<crate::question::Question>,
}

/// One picked option, as the UI reports it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerChoice {
    /// The question text, which is the key `answers` is stored under.
    pub question: String,
    /// The option label. Must be one this question offered -- the daemon checks,
    /// because inventing a label would put words in the human's mouth.
    pub label: String,
}

/// A command the UI sends back to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewCommand {
    /// The human answered. `behavior` is the decision to hand back to the agent,
    /// and `answers` carries the picked options when the card asked a question.
    ///
    /// An empty `answers` on a question card is not an answer: the daemon refuses
    /// it rather than letting the tool run with questions still open.
    ///
    /// `session_rule` turns an `allow` into an "always allow this tool for the
    /// rest of this session" -- the CLI's native session-scoped rule, not a
    /// preference this island keeps. The daemon only honours it for Claude Code,
    /// the one source whose reply envelope it has verified can carry the rule;
    /// any other source degrades to a one-time allow.
    PermissionDecision {
        request_id: String,
        behavior: crate::protocol::Behavior,
        #[serde(default)]
        answers: Vec<AnswerChoice>,
        #[serde(default)]
        session_rule: bool,
    },
    /// "Bring me the terminal this session is running in."
    ///
    /// A view action, not a decision: it must never unblock a parked hook, because
    /// looking at the agent and answering it are different choices. A client that
    /// cannot jump (an older shell, or a remote viewer) simply does not send this.
    Jump { session_id: String },
    /// "I will answer this one in the terminal instead."
    ///
    /// The card comes down and its hook is released to *silence* -- never allow,
    /// never deny -- so the CLI falls back to its own prompt. This is the escape
    /// hatch that keeps a question card from being a dead end when none of the
    /// offered options fit, and it is the one command a human can always send:
    /// it asks nothing of the agent and risks nothing on its behalf.
    Withdraw { request_id: String },
    /// "Stop / resume telling me when a turn finishes."
    ///
    /// The desktop-notification bell on the island's toolbar. Flipping it changes
    /// only what the daemon tells the *human*; it never changes what any agent is
    /// told, so it is safe to accept at any time.
    SetNotify { on: bool },
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
            let mut prior = self.sessions.get(&id).cloned().expect("just checked");
            // A hook event speaks for this session now. An inferred row becomes an
            // authoritative one, which is what lets it start saying "waiting for you" --
            // something discovery may never claim.
            prior.origin = Origin::Hook;
            prior
        } else {
            Session {
                id: id.clone(),
                source: source.clone(),
                title: None,
                usage: None,
                status: SessionStatus::Idle,
                cwd: None,
                branch: None,
                detail: None,
                origin: Origin::Hook,
                // A hook is the agent stating its own state, so this is never a guess.
                status_stated: true,
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

    /// The human gate on this session is over: its card was answered, withdrawn,
    /// or nobody was watching to be asked in the first place.
    ///
    /// Only a session actually parked on approval moves, and it moves to
    /// `Working` -- either the tool is now running or the agent has fallen back to
    /// prompting itself, and both mean it stopped waiting on this island.
    ///
    /// Without this the flags set by `RouteKind::Permission` and `RouteKind::Question`
    /// outlive every card:
    /// the header keeps reading "1 session needs you" with nothing in it to
    /// answer, which is indistinguishable from a card that failed to draw.
    ///
    /// Returns the update to broadcast, or `None` when there was nothing to
    /// release. Callers must check that no *other* request from the same session
    /// is still parked before calling -- two cards can be up at once.
    /// Take note of a session that a live process owns but that never fired a hook.
    ///
    /// Returns an update only when something the UI should see actually changed, and never
    /// touches a session a hook already claimed: a discovered row is an inference, so the
    /// moment the real event arrives the real event wins.
    pub fn register_discovered(
        &mut self,
        id: &str,
        source: &str,
        cwd: &str,
        detail: Option<String>,
        status: SessionStatus,
        branch: Option<String>,
        status_stated: bool,
        now_ms: u64,
    ) -> Option<ViewUpdate> {
        if let Some(existing) = self.sessions.get_mut(id) {
            if existing.origin != Origin::Discovered {
                return None; // hooks know better than we do
            }
            let changed = existing.status != status
                || existing.detail != detail
                || existing.branch != branch
                || existing.status_stated != status_stated;
            existing.status = status;
            existing.detail = detail.or_else(|| existing.detail.clone());
            existing.branch = branch;
            existing.status_stated = status_stated;
            existing.last_event_ms = now_ms;
            return changed.then(|| ViewUpdate::SessionUpsert {
                session: existing.clone(),
            });
        }
        let session = Session {
            id: id.to_string(),
            source: source.to_string(),
            title: None,
            usage: None,
            status,
            cwd: Some(cwd.to_string()),
            branch,
            detail,
            origin: Origin::Discovered,
            status_stated,
            last_event_ms: now_ms,
            started_ms: now_ms,
            tool_calls: 0,
        };
        self.sessions.insert(id.to_string(), session.clone());
        // `ordered()` walks `order`, not the map -- inserting only the map leaves a session
        // that exists, is updated, and is never displayed. That is exactly the bug this line
        // replaced: discovery paired a session on every scan and the island stayed empty.
        if !self.order.iter().any(|existing| existing == id) {
            self.order.push(id.to_string());
        }
        Some(ViewUpdate::SessionUpsert { session })
    }

    /// Drop discovered rows whose process is gone. Called with the set of ids discovery still
    /// sees; anything discovered earlier and missing now is no longer running.
    pub fn forget_discovered_not_in(&mut self, alive: &[String], now_ms: u64) -> Vec<ViewUpdate> {
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.origin == Origin::Discovered)
            .filter(|(id, session)| !alive.contains(id) && session.last_event_ms + DISCOVERED_GRACE_MS < now_ms)
            .map(|(id, _)| id.clone())
            .collect();
        stale
            .into_iter()
            .map(|id| {
                self.sessions.remove(&id);
                self.order.retain(|existing| existing != &id);
                ViewUpdate::SessionRemove { id }
            })
            .collect()
    }

    /// Drop hook rows whose event stream has gone silent, when no other witness
    /// speaks for them.
    ///
    /// A hook row is the agent's own statement, so three things must all hold
    /// before it becomes history:
    ///
    /// 1. **Not in `alive`** -- discovery still pairs the session to a live
    ///    process. This is the Linux stand-in for upstream's process monitor:
    ///    while the agent's process lives, an hour-long tool call is a session,
    ///    not a stale row.
    /// 2. **Not in `parked`** -- a card from this session is still up, so a human
    ///    is being asked something right now. Dropping the row would orphan the
    ///    card's "needs you" claim on the header.
    /// 3. **Older than the caller's stale window** -- the last event is far
    ///    enough back that "it will speak again" has stopped being the likely
    ///    reading. Passed in rather than fixed so the daemon honour its
    ///    `CODEISLAND_HOOK_STALE_MS` override; the default is [`HOOK_STALE_MS`].
    ///
    /// What remains is exactly the row nobody can vouch for: a killed agent, a
    /// CLI that never sends `SessionEnd`, or a leftover demo card. Measured on
    /// this machine: a card-holder's session kept showing `working` for over an
    /// hour after its last event, and nothing ever removed it.
    pub fn forget_hook_stale(
        &mut self,
        alive: &[String],
        parked: &[String],
        stale_ms: u64,
        now_ms: u64,
    ) -> Vec<ViewUpdate> {
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.origin == Origin::Hook)
            .filter(|(id, session)| {
                !alive.contains(id)
                    && !parked.contains(id)
                    && session.last_event_ms + stale_ms < now_ms
            })
            .map(|(id, _)| id.clone())
            .collect();
        stale
            .into_iter()
            .map(|id| {
                self.sessions.remove(&id);
                self.order.retain(|existing| existing != &id);
                ViewUpdate::SessionRemove { id }
            })
            .collect()
    }

    /// Give a session its conversation title, from whatever its transcript states.
    ///
    /// Applies to rows of both origins: hook events carry a cwd but never a title,
    /// so the transcript is the only source, and the discovery scan is what reads
    /// it. `None` never clears -- a transient read failure must not blank a row
    /// that had a title a second ago. `last_event_ms` is deliberately untouched:
    /// a title arriving late is not activity, and moving it would reorder rows
    /// and reset the staleness clock.
    pub fn set_title(&mut self, id: &str, title: Option<String>) -> Option<ViewUpdate> {
        let Some(title) = title else { return None };
        let session = self.sessions.get_mut(id)?;
        if session.title.as_deref() == Some(title.as_str()) {
            return None;
        }
        session.title = Some(title);
        Some(ViewUpdate::SessionUpsert {
            session: session.clone(),
        })
    }

    /// State the token usage a harness reported about a session. Same rules as
    /// `set_title`: applies to rows of both origins, `None` never clears (a
    /// transient read failure must not blank a number that was true), and
    /// `last_event_ms` is untouched -- a counter is not activity.
    pub fn set_usage(&mut self, id: &str, usage: Option<Usage>) -> Option<ViewUpdate> {
        let Some(usage) = usage else { return None };
        let session = self.sessions.get_mut(id)?;
        if session.usage == Some(usage) {
            return None;
        }
        session.usage = Some(usage);
        Some(ViewUpdate::SessionUpsert {
            session: session.clone(),
        })
    }

    pub fn release_request(&mut self, session_id: &str) -> Option<ViewUpdate> {
        let session = self.sessions.get_mut(session_id)?;
        // Both "waiting" states mean the island was asked to act: a permission gate
        // and, since the question card, an `AskUserQuestion`. Either must be cleared
        // when nobody is left to act, or the header will read "needs you" forever
        // with nothing to answer.
        if !matches!(
            session.status,
            SessionStatus::WaitingApproval | SessionStatus::WaitingInput
        ) {
            return None;
        }
        session.status = SessionStatus::Working;
        Some(ViewUpdate::SessionUpsert {
            session: session.clone(),
        })
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

    /// The shape measured on 2026-10-14: a question arrives on the *permission*
    /// channel with the AskUserQuestion tool name, and must route to Question.
    fn question_event(session: &str) -> HookEvent {
        HookEvent {
            tool_name: Some(crate::question::ASK_TOOL.into()),
            tool_input: Some(serde_json::json!({"questions": [
                {"question": "Red or blue?", "options": [{"label": "Red"}, {"label": "Blue"}]}]})),
            ..event("PermissionRequest", session)
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
    fn releasing_a_permission_parks_nothing_afterwards() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "s1"), 1000);
        let mut e = event("PermissionRequest", "s1");
        e.tool_name = Some("Bash".into());
        reg.apply(&e, 2000);
        assert_eq!(reg.get("s1").unwrap().status, SessionStatus::WaitingApproval);

        let update = reg.release_request("s1").expect("card gone, flag gone");
        match update {
            ViewUpdate::SessionUpsert { session } => {
                assert_eq!(session.status, SessionStatus::Working)
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(reg.get("s1").unwrap().status, SessionStatus::Working);
    }

    #[test]
    fn a_waiting_input_session_is_released_too() {
        // The question card parks on RouteKind::Question, which sets WaitingInput.
        // If only WaitingApproval were cleared, an unanswerable question would leave
        // the phantom "needs you" header we removed on 2026-10-13.
        let mut reg = SessionRegistry::default();
        reg.apply(&event("SessionStart", "s9"), 1_000);
        reg.apply(&question_event("s9"), 1_100);
        assert_eq!(reg.get("s9").unwrap().status, SessionStatus::WaitingInput);
        assert!(reg.release_request("s9").is_some());
        assert_eq!(reg.get("s9").unwrap().status, SessionStatus::Working);
    }

    #[test]
    fn releasing_leaves_sessions_that_werent_waiting_alone() {
        let mut reg = SessionRegistry::new();
        reg.apply(&event("SessionStart", "s1"), 1000);
        reg.apply(&event("Stop", "s1"), 2000);
        assert!(
            reg.release_request("s1").is_none(),
            "a finished turn must not be dragged back to working"
        );
        assert_eq!(reg.get("s1").unwrap().status, SessionStatus::Done);

        // An unknown session is not an error, there is simply nothing to release.
        assert!(reg.release_request("nope").is_none());
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

#[cfg(test)]
mod permission_wire_tests {
    use super::*;
    use crate::protocol::Behavior;

    /// Same shape as the first test module's `event()`, built here because this
    /// module cannot reach that module's private helper.
    fn hook_event(name: &str, session: &str) -> HookEvent {
        HookEvent {
            source: Some("claude".into()),
            normalized_event: Some(name.into()),
            session_id: Some(session.into()),
            ..Default::default()
        }
    }

    #[test]
    fn permission_request_round_trips_through_json() {
        let update = ViewUpdate::PermissionRequested {
            request: PermissionRequestView {
                request_id: "r1".into(),
                session_id: "s1".into(),
                source: "claude".into(),
                tool_name: Some("Bash".into()),
                summary: Some("Bash: touch /tmp/x".into()),
                cwd: Some("/home/dev".into()),
                questions: Vec::new(),
            },
        };
        let text = serde_json::to_string(&update).unwrap();
        assert!(text.contains("\"type\":\"permission_requested\""), "{text}");
        assert!(
            !text.contains("questions"),
            "a plain gate must not grow a questions field: {text}"
        );
        let back: ViewUpdate = serde_json::from_str(&text).unwrap();
        assert_eq!(back, update);
    }

    #[test]
    fn a_question_card_travels_with_its_options() {
        let update = ViewUpdate::PermissionRequested {
            request: PermissionRequestView {
                request_id: "q1".into(),
                session_id: "s1".into(),
                source: "claude".into(),
                tool_name: Some(crate::question::ASK_TOOL.into()),
                summary: Some("AskUserQuestion".into()),
                cwd: Some("/home/dev".into()),
                questions: vec![crate::question::Question {
                    header: "Color".into(),
                    question: "Red or blue?".into(),
                    options: vec![
                        crate::question::QuestionOption {
                            label: "Red".into(),
                            description: "The red option.".into(),
                        },
                        crate::question::QuestionOption {
                            label: "Blue".into(),
                            description: String::new(),
                        },
                    ],
                }],
            },
        };
        let text = serde_json::to_string(&update).unwrap();
        let back: ViewUpdate = serde_json::from_str(&text).unwrap();
        assert_eq!(back, update, "the shell has to receive every option verbatim");
        assert!(text.contains("Red or blue?"), "{text}");
    }

    /// The shell that predates question cards sends no `answers` at all. Defaulting
    /// it keeps that message valid instead of failing the whole command.
    #[test]
    fn a_jump_command_names_only_the_session() {
        // Deliberately carries no request id: jumping must not be mistaken for an
        // answer, and the session is what owns a terminal.
        let command: ViewCommand =
            serde_json::from_str(r#"{"type":"jump","session_id":"s7"}"#).unwrap();
        match command {
            ViewCommand::Jump { session_id } => assert_eq!(session_id, "s7"),
            other => panic!("unexpected {other:?}"),
        }
        let text = serde_json::to_string(&ViewCommand::Jump {
            session_id: "s7".into(),
        })
        .unwrap();
        assert_eq!(text, r#"{"type":"jump","session_id":"s7"}"#, "{text}");
    }

    #[test]
    fn a_withdraw_command_names_only_the_request() {
        // "Skip this question, I will answer in the terminal." It carries no
        // behavior and no answers, because it is neither: the daemon must not be
        // able to read an allow or a deny out of it, even by accident.
        let command: ViewCommand =
            serde_json::from_str(r#"{"type":"withdraw","request_id":"q7"}"#).unwrap();
        match command {
            ViewCommand::Withdraw { request_id } => assert_eq!(request_id, "q7"),
            other => panic!("unexpected {other:?}"),
        }
        let text = serde_json::to_string(&ViewCommand::Withdraw {
            request_id: "q7".into(),
        })
        .unwrap();
        assert_eq!(text, r#"{"type":"withdraw","request_id":"q7"}"#, "{text}");
        assert!(
            !text.contains("behavior") && !text.contains("answers"),
            "a withdraw must not smell like a decision: {text}"
        );
    }

    #[test]
    fn a_set_notify_command_is_just_the_switch() {
        // The toolbar bell. It only ever changes what the daemon tells the human,
        // so both states must survive the wire exactly.
        let on: ViewCommand = serde_json::from_str(r#"{"type":"set_notify","on":true}"#).unwrap();
        assert_eq!(on, ViewCommand::SetNotify { on: true });
        let off = serde_json::from_str::<ViewCommand>(r#"{"type":"set_notify","on":false}"#).unwrap();
        assert_eq!(off, ViewCommand::SetNotify { on: false });
        assert_eq!(
            serde_json::to_string(&ViewCommand::SetNotify { on: true }).unwrap(),
            r#"{"type":"set_notify","on":true}"#
        );
    }

    #[test]
    fn a_decision_without_answers_still_parses() {
        let command: ViewCommand = serde_json::from_str(
            r#"{"type":"permission_decision","request_id":"r3","behavior":"allow"}"#,
        )
        .unwrap();
        match command {
            ViewCommand::PermissionDecision { request_id, answers, .. } => {
                assert_eq!(request_id, "r3");
                assert!(answers.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn answers_arrive_as_question_label_pairs() {
        let command: ViewCommand = serde_json::from_str(
            r#"{"type":"permission_decision","request_id":"q1","behavior":"allow",
                "answers":[{"question":"Red or blue?","label":"Blue"}]}"#,
        )
        .unwrap();
        match command {
            ViewCommand::PermissionDecision { answers, .. } => {
                assert_eq!(
                    answers,
                    vec![AnswerChoice {
                        question: "Red or blue?".into(),
                        label: "Blue".into()
                    }]
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decision_carries_the_allow_deny_word_the_shell_will_send() {
        let command = ViewCommand::PermissionDecision {
            request_id: "r1".into(),
            behavior: Behavior::Allow,
            answers: Vec::new(),
            session_rule: false,
        };
        let value: serde_json::Value = serde_json::to_value(&command).unwrap();
        assert_eq!(value["type"], "permission_decision");
        assert_eq!(value["behavior"], "allow");

        let deny: ViewCommand =
            serde_json::from_str(r#"{"type":"permission_decision","request_id":"r2","behavior":"deny"}"#)
                .unwrap();
        assert_eq!(
            deny,
            ViewCommand::PermissionDecision {
                request_id: "r2".into(),
                behavior: Behavior::Deny,
                answers: Vec::new(),
                session_rule: false,
            }
        );

        // An old client that never heard of session rules still parses: absent
        // means false, so a stale shell cannot accidentally request one.
        let old_client: ViewCommand =
            serde_json::from_str(r#"{"type":"permission_decision","request_id":"r3","behavior":"allow"}"#)
                .unwrap();
        assert!(!matches!(
            old_client,
            ViewCommand::PermissionDecision { session_rule: true, .. }
        ));
        let with_rule: ViewCommand = serde_json::from_str(
            r#"{"type":"permission_decision","request_id":"r4","behavior":"allow","session_rule":true}"#,
        )
        .unwrap();
        assert!(matches!(
            with_rule,
            ViewCommand::PermissionDecision { session_rule: true, .. }
        ));
    }

    #[test]
    fn resolved_is_withdrawal_not_an_answer() {
        // Distinct from a decision: the UI must remove the card without the agent
        // being told allow or deny by anyone.
        let update = ViewUpdate::PermissionResolved {
            request_id: "r9".into(),
        };
        let text = serde_json::to_string(&update).unwrap();
        assert!(text.contains("permission_resolved"), "{text}");
        assert!(!text.contains("behavior"), "{text}");
    }

    #[test]
    fn a_discovered_session_is_visible_in_the_snapshot() {
        // The bug this guards: register_discovered inserted into the map but not into the
        // ordering vector, and ordered() walks the ordering vector -- so discovery paired a
        // session on every scan, broadcast it, and the island still showed nothing. An update
        // nobody can see is not an update.
        let mut reg = SessionRegistry::new();
        let update = reg
            .register_discovered(
                "d1",
                "pi",
                "/tmp/proj",
                Some("存档 · message".into()),
                SessionStatus::Working,
                Some("feat/x".into()),
                true,
                1_000,
            )
            .expect("a new discovered session must produce an update");
        assert!(matches!(update, ViewUpdate::SessionUpsert { .. }));
        let snapshot = reg.snapshot();
        let ViewUpdate::Snapshot { sessions } = snapshot else {
            panic!("snapshot shape changed");
        };
        assert_eq!(sessions.len(), 1, "the discovered session must be in the snapshot");
        assert_eq!(sessions[0].id, "d1");
        assert_eq!(sessions[0].origin, Origin::Discovered);

        // And a hook event for the same id upgrades the row rather than duplicating it.
        let mut event = HookEvent::default();
        event.session_id = Some("d1".into());
        event.source = Some("pi".into());
        event.normalized_event = Some("SessionStart".into());
        reg.apply(&event, 2_000);
        let ViewUpdate::Snapshot { sessions } = reg.snapshot() else {
            panic!("snapshot shape changed");
        };
        assert_eq!(sessions.len(), 1, "one session, not two rows for one id");
        assert_eq!(sessions[0].origin, Origin::Hook, "a hook speaks for the session now");
    }

    #[test]
    fn a_forgotten_discovered_session_leaves_the_ordering_too() {
        let mut reg = SessionRegistry::new();
        reg.register_discovered("d2", "pi", "/tmp/p", None, SessionStatus::Idle, None, true, 1_000);
        let gone = reg.forget_discovered_not_in(&[], 10_000_000);
        assert_eq!(gone.len(), 1);
        let ViewUpdate::Snapshot { sessions } = reg.snapshot() else {
            panic!("snapshot shape changed");
        };
        assert!(sessions.is_empty(), "removed rows must not linger in the order");
    }

    /// The measured bug this file's hook sweep exists for: a card-holder's session
    /// whose last event was over an hour ago kept showing `working` on the island
    /// forever, because only discovered rows were ever forgotten.
    #[test]
    fn a_stale_hook_session_is_forgotten() {
        let mut reg = SessionRegistry::new();
        reg.apply(&hook_event("SessionStart", "s1"), 1_000);
        reg.apply(&hook_event("Stop", "s1"), 2_000);
        let now = 2_000 + HOOK_STALE_MS + 1;
        let gone = reg.forget_hook_stale(&[], &[], HOOK_STALE_MS, now);
        assert_eq!(
            gone,
            vec![ViewUpdate::SessionRemove { id: "s1".into() }],
            "a silent hook row with no witness is history: {gone:?}"
        );
        assert_eq!(reg.len(), 0, "removed rows must not linger in the order");
    }

    #[test]
    fn a_hook_session_discovery_still_sees_is_kept() {
        // The Linux stand-in for upstream's process monitor: while discovery pairs
        // the session to a live process, an hour-long tool call is a session, not
        // a stale row. No hook event is needed to keep it.
        let mut reg = SessionRegistry::new();
        reg.apply(&hook_event("SessionStart", "s1"), 1_000);
        let now = 1_000 + 10 * HOOK_STALE_MS;
        let gone = reg.forget_hook_stale(&["s1".into()], &[], HOOK_STALE_MS, now);
        assert!(gone.is_empty(), "a live process vouches for the row: {gone:?}");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn a_hook_session_with_a_parked_request_is_kept() {
        // A card from this session is still up: a human is being asked something
        // right now, and dropping the row would orphan the card's claim.
        let mut reg = SessionRegistry::new();
        reg.apply(&hook_event("SessionStart", "s1"), 1_000);
        let now = 1_000 + 10 * HOOK_STALE_MS;
        let gone = reg.forget_hook_stale(&[], &["s1".into()], HOOK_STALE_MS, now);
        assert!(gone.is_empty(), "a parked card vouches for the row: {gone:?}");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn a_fresh_hook_session_is_kept() {
        // Ten minutes is upstream's clock for monitor-less sessions; inside it,
        // "it will speak again" is still the likely reading and the row stays.
        let mut reg = SessionRegistry::new();
        reg.apply(&hook_event("SessionStart", "s1"), 1_000);
        let now = 1_000 + HOOK_STALE_MS - 1;
        let gone = reg.forget_hook_stale(&[], &[], HOOK_STALE_MS, now);
        assert!(gone.is_empty(), "inside the grace window nothing is dropped: {gone:?}");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn the_hook_sweep_never_touches_discovered_rows() {
        // Discovered rows have their own sweep (`forget_discovered_not_in`, 30 s
        // grace). A hook sweep that also ate them would apply a 10-minute clock
        // to rows whose own rule is 30 seconds, and vice versa -- the two clocks
        // must not cross.
        let mut reg = SessionRegistry::new();
        reg.register_discovered("d1", "pi", "/tmp/p", None, SessionStatus::Idle, None, true, 1_000);
        let now = 1_000 + 10 * HOOK_STALE_MS;
        let gone = reg.forget_hook_stale(&[], &[], HOOK_STALE_MS, now);
        assert!(gone.is_empty(), "discovered rows are another sweep's business: {gone:?}");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn the_stale_window_is_the_caller_s_choice() {
        // The daemon honours CODEISLAND_HOOK_STALE_MS, so the window is a
        // parameter, not a constant -- a one-minute sweep must work exactly like
        // the ten-minute one, just faster.
        let mut reg = SessionRegistry::new();
        reg.apply(&hook_event("SessionStart", "s1"), 1_000);
        let one_minute = 60_000;
        assert!(reg.forget_hook_stale(&[], &[], one_minute, 1_000 + one_minute - 1).is_empty());
        assert_eq!(
            reg.forget_hook_stale(&[], &[], one_minute, 1_000 + one_minute + 1).len(),
            1,
            "the sweep fires just past the caller's window"
        );
    }

    #[test]
    fn a_title_arrives_late_without_pretending_to_be_activity() {
        // The discovery scan reads the transcript on its own clock, so a title
        // lands seconds after the row exists. It must reach both origins, must
        // not bump last_event_ms (that would reorder rows and reset the staleness
        // clock), and must not re-broadcast once set.
        let mut reg = SessionRegistry::new();
        reg.apply(&hook_event("SessionStart", "hooked"), 1_000);
        reg.register_discovered("disc", "pi", "/tmp/p", None, SessionStatus::Working, None, true, 2_000);

        let update = reg.set_title("hooked", Some("修复登录页".into())).expect("a title is an update");
        match update {
            ViewUpdate::SessionUpsert { session } => {
                assert_eq!(session.title.as_deref(), Some("修复登录页"));
                assert_eq!(session.last_event_ms, 1_000, "a title is not activity");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(reg.set_title("hooked", Some("修复登录页".into())).is_none(), "unchanged title, no update");
        reg.set_title("disc", Some("看kcu接入".into()));
        assert_eq!(reg.get("disc").unwrap().title.as_deref(), Some("看kcu接入"));

        // None never clears: a transient read failure must not blank a titled row.
        reg.set_title("hooked", None);
        assert_eq!(reg.get("hooked").unwrap().title.as_deref(), Some("修复登录页"));
        // A missing session is simply nothing to do.
        assert!(reg.set_title("nope", Some("x".into())).is_none());
    }

    #[test]
    fn usage_flows_like_a_title_not_like_activity() {
        // Same three rules as titles: both origins get it, `None` never clears,
        // and `last_event_ms` is untouched -- a counter is not activity.
        let mut reg = SessionRegistry::new();
        reg.apply(&hook_event("SessionStart", "s1"), 1_000);
        reg.set_usage("s1", Some(Usage { input_tokens: 28_766, output_tokens: 361 }));
        let row = reg.get("s1").unwrap();
        assert_eq!(row.usage, Some(Usage { input_tokens: 28_766, output_tokens: 361 }));
        assert_eq!(row.last_event_ms, 1_000, "a counter is not activity");
        // Unchanged usage, no update.
        assert!(reg
            .set_usage("s1", Some(Usage { input_tokens: 28_766, output_tokens: 361 }))
            .is_none());
        // A growing counter is an update.
        assert!(reg
            .set_usage("s1", Some(Usage { input_tokens: 60_762, output_tokens: 664 }))
            .is_some());
        // None never clears what was true.
        reg.set_usage("s1", None);
        assert!(reg.get("s1").unwrap().usage.is_some());
    }

}
