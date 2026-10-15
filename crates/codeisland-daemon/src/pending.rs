//! Requests parked waiting for a human.
//!
//! A permission hook blocks its own process until answered, so the daemon holds
//! that connection's response channel here while the island shows a card.
//!
//! Every path out of this table must be able to say "nobody answered", and that
//! path has to mean *silence to the CLI*, not `deny`. Denying on the daemon's
//! behalf would make the island responsible for behaviour the human never chose.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use codeisland_core::protocol::Behavior;
use codeisland_core::question::Question;
use serde_json::Value;

/// A human's answer, as it travels back to the parked hook.
///
/// `updated_input` carries a rewritten tool input, which is the only way to answer
/// `AskUserQuestion` (measured 2026-10-14). It is meaningless for a bare approve or
/// deny, and a question card must never be resolved without it.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub behavior: Behavior,
    pub updated_input: Option<Value>,
}

impl Decision {
    /// A plain approve/deny, no rewritten input.
    pub fn plain(behavior: Behavior) -> Self {
        Self {
            behavior,
            updated_input: None,
        }
    }

    pub fn answered(updated_input: Value) -> Self {
        Self {
            behavior: Behavior::Allow,
            updated_input: Some(updated_input),
        }
    }
}

/// One parked request.
struct Entry {
    /// Who to notify with the answer. `None` means nobody answered.
    sender: Sender<Option<Decision>>,
    /// Which UI client was shown this card, so its disconnect can withdraw the
    /// request instead of leaving the agent parked on a window that is gone.
    viewer_id: u64,
    /// Which session this card belongs to. Releasing a card has to say whether
    /// that session is still waiting on a human, and a session can have more than
    /// one card up at a time.
    session_id: String,
    /// Non-empty when the card asks a question. The answer then has to be built
    /// from these questions, so a bare allow cannot resolve it -- see `answer`.
    questions: Vec<Question>,
    /// The original `tool_input`, needed to rebuild `updatedInput` verbatim.
    tool_input: Option<Value>,
}

#[derive(Default)]
pub struct PendingRequests {
    entries: HashMap<String, Entry>,
}

impl PendingRequests {
    /// Register a parked hook and return what to wait on.
    pub fn park(
        &mut self,
        request_id: &str,
        viewer_id: u64,
        session_id: &str,
        questions: Vec<Question>,
        tool_input: Option<Value>,
    ) -> Receiver<Option<Decision>> {
        let (sender, receiver) = mpsc::channel();
        self.entries.insert(
            request_id.to_string(),
            Entry {
                sender,
                viewer_id,
                session_id: session_id.to_string(),
                questions,
                tool_input,
            },
        );
        receiver
    }

    /// What the card is asking, when it is a question card. The daemon needs this
    /// to turn ticked options into an `answers` map at reply time.
    pub fn question_of(&self, request_id: &str) -> Option<(Vec<Question>, Option<Value>)> {
        self.entries
            .get(request_id)
            .filter(|entry| !entry.questions.is_empty())
            .map(|entry| (entry.questions.clone(), entry.tool_input.clone()))
    }

    /// Is any request from this session still parked? Answering one of two cards
    /// must not clear the session's claim on the human -- the other is still up.
    pub fn has_parked_for(&self, session_id: &str) -> bool {
        self.entries
            .values()
            .any(|entry| entry.session_id == session_id)
    }

    /// Session ids that still have a parked request, deduplicated.
    ///
    /// Lets the lifecycle sweep keep those rows up no matter how old their last
    /// event is: a session whose card is on screen is waiting on a human, and
    /// removing it would orphan the card.
    pub fn parked_sessions(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .entries
            .values()
            .map(|entry| entry.session_id.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// A human answered. Returns the session it belonged to when the request was
    /// actually still parked, which lets the caller ignore a decision that arrived
    /// after a withdrawal.
    ///
    /// A question card refuses a decision with no rewritten input and stays parked:
    /// `allow` without `updatedInput` would run `AskUserQuestion` with nobody having
    /// answered it, which is worse than not answering at all.
    pub fn answer(&mut self, request_id: &str, decision: Decision) -> Option<String> {
        if let Some(entry) = self.entries.get(request_id) {
            if !entry.questions.is_empty() && decision.updated_input.is_none() {
                return None;
            }
        }
        match self.entries.remove(request_id) {
            Some(entry) => {
                if entry.sender.send(Some(decision)).is_ok() {
                    Some(entry.session_id)
                } else {
                    None
                }
            }
            None => None,
        }
    }

    pub fn withdraw(&mut self, request_id: &str) -> Option<String> {
        match self.entries.remove(request_id) {
            Some(entry) => {
                if entry.sender.send(None).is_ok() {
                    Some(entry.session_id)
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// The UI client that was shown these cards is gone.
    ///
    /// Returns `(request_id, session_id)` for each withdrawn request so the caller
    /// can clear their cards from any remaining clients *and* release the sessions
    /// they were holding on a human.
    pub fn withdraw_viewer(&mut self, viewer_id: u64) -> Vec<(String, String)> {
        let doomed: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.viewer_id == viewer_id)
            .map(|(id, entry)| (id.clone(), entry.session_id.clone()))
            .collect();
        for (id, _) in &doomed {
            self.withdraw(id);
        }
        doomed
    }

    /// How many hooks are parked right now. Logged when a request is held, so a
    /// stuck island is visible in the daemon's output instead of silent.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Longest a hook will be held for one decision.
///
/// Matches the bridge's own ceiling so the two never disagree about who gives up
/// first. If the daemon's timer wins it sends nothing, which is the same outcome
/// as the bridge timing out -- the agent falls back to its own prompt.
pub const PERMISSION_HOLD: Duration = Duration::from_secs(3600);

/// Wait for a decision on this receiver, giving up after `PERMISSION_HOLD`.
///
/// `None` means nobody answered, which the caller must translate into silence.
pub fn await_decision(receiver: &Receiver<Option<Decision>>) -> Option<Decision> {
    // Timeout, or the sender dropped because the daemon is shutting down: both
    // mean no answer, and both must fall back to silence. `unwrap_or_default`
    // reads as exactly that -- an absent answer is None.
    receiver.recv_timeout(PERMISSION_HOLD).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeisland_core::question::QuestionOption;
    use serde_json::json;

    /// A plain approve/deny card: no questions attached.
    fn park_plain(pending: &mut PendingRequests, id: &str, viewer: u64, session: &str) -> Receiver<Option<Decision>> {
        pending.park(id, viewer, session, vec![], None)
    }

    fn question_card(pending: &mut PendingRequests, id: &str, viewer: u64, session: &str) -> Receiver<Option<Decision>> {
        pending.park(
            id,
            viewer,
            session,
            vec![Question {
                header: "Color".into(),
                question: "Red or blue?".into(),
                options: vec![
                    QuestionOption { label: "Red".into(), description: String::new() },
                    QuestionOption { label: "Blue".into(), description: String::new() },
                ],
            }],
            Some(json!({"questions": [{"question": "Red or blue?", "options": [{"label": "Red"}, {"label": "Blue"}]}]})),
        )
    }

    #[test]
    fn an_answer_reaches_the_parked_hook() {
        let mut pending = PendingRequests::default();
        let receiver = park_plain(&mut pending, "r1", 7, "s1");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending.answer("r1", Decision::plain(Behavior::Allow)).as_deref(),
            Some("s1"),
            "the caller needs the session back to release its flag"
        );
        assert_eq!(await_decision(&receiver), Some(Decision::plain(Behavior::Allow)));
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn a_withdrawal_is_silence_not_a_denial() {
        let mut pending = PendingRequests::default();
        let receiver = park_plain(&mut pending, "r1", 7, "s1");
        pending.withdraw("r1");
        assert_eq!(
            await_decision(&receiver),
            None,
            "withdrawing must not be reported to the agent as deny"
        );
    }

    #[test]
    fn a_question_card_refuses_a_bare_allow() {
        // The invariant that keeps the question card from running AskUserQuestion
        // with an empty answer set. It stays parked so the human can still answer,
        // and the hook keeps holding -- which the watchdog resolves to silence if
        // the agent dies.
        let mut pending = PendingRequests::default();
        let receiver = question_card(&mut pending, "q1", 7, "s1");
        assert_eq!(pending.answer("q1", Decision::plain(Behavior::Allow)), None);
        assert_eq!(pending.len(), 1, "the card must still be up");

        let answered = json!({"answers": {"Red or blue?": "Blue"}});
        assert_eq!(
            pending.answer("q1", Decision::answered(answered.clone())).as_deref(),
            Some("s1")
        );
        let got = await_decision(&receiver).unwrap();
        assert_eq!(got.updated_input.as_ref(), Some(&answered));
    }

    #[test]
    fn question_details_are_readable_before_answering() {
        let mut pending = PendingRequests::default();
        question_card(&mut pending, "q1", 7, "s1");
        let (questions, input) = pending.question_of("q1").expect("a question card reports its questions");
        assert_eq!(questions[0].question, "Red or blue?");
        assert!(input.is_some(), "the original tool_input is needed to rebuild the answer");
        assert!(pending.question_of("r9").is_none(), "unknown request has no questions");

        park_plain(&mut pending, "r1", 7, "s1");
        assert!(
            pending.question_of("r1").is_none(),
            "a permission card is not a question card"
        );
    }

    #[test]
    fn one_session_can_hold_two_cards() {
        // Releasing one of two cards must leave the session parked: the human
        // still owes the other one an answer.
        let mut pending = PendingRequests::default();
        let first = park_plain(&mut pending, "r1", 7, "shared");
        let second = park_plain(&mut pending, "r2", 7, "shared");

        pending.withdraw("r1");
        assert!(pending.has_parked_for("shared"), "r2 is still up");
        assert_eq!(await_decision(&first), None);

        pending.answer("r2", Decision::plain(Behavior::Deny));
        assert!(
            !pending.has_parked_for("shared"),
            "both cards are gone, so the session no longer waits on a human"
        );
        assert_eq!(await_decision(&second), Some(Decision::plain(Behavior::Deny)));
    }

    #[test]
    fn parked_sessions_are_deduplicated() {
        // The sweep passes this list to the registry, so one session with two
        // cards up must not appear twice -- and a session whose cards are all
        // gone must not appear at all.
        let mut pending = PendingRequests::default();
        park_plain(&mut pending, "r1", 7, "shared");
        park_plain(&mut pending, "r2", 7, "shared");
        park_plain(&mut pending, "r3", 8, "other");

        assert_eq!(pending.parked_sessions(), vec!["other".to_string(), "shared".to_string()]);

        pending.withdraw("r1");
        pending.withdraw("r2");
        assert_eq!(
            pending.parked_sessions(),
            vec!["other".to_string()],
            "a session with no cards up no longer waits on a human"
        );
    }

    #[test]
    fn viewer_disconnect_withdraws_only_that_viewers_requests() {
        let mut pending = PendingRequests::default();
        let mine = park_plain(&mut pending, "mine", 1, "s1");
        let theirs = park_plain(&mut pending, "theirs", 2, "s2");

        let withdrawn = pending.withdraw_viewer(1);
        assert_eq!(withdrawn, vec![("mine".to_string(), "s1".to_string())]);
        assert_eq!(await_decision(&mine), None);

        pending.answer("theirs", Decision::plain(Behavior::Deny));
        assert_eq!(await_decision(&theirs), Some(Decision::plain(Behavior::Deny)));
    }

    #[test]
    fn answering_an_unknown_request_is_ignored() {
        let mut pending = PendingRequests::default();
        assert!(
            pending.answer("r1", Decision::plain(Behavior::Allow)).is_none(),
            "a decision for a request that already went away must not be sent anywhere"
        );
    }

    #[test]
    fn a_closed_receiver_reports_no_answer() {
        let (_sender, receiver) = mpsc::channel::<Option<Decision>>();
        drop(_sender);
        assert_eq!(await_decision(&receiver), None);
    }
}
