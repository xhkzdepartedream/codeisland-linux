//! What a session's transcript says its turn state *is*, read from the file.
//!
//! This exists because "is it working?" was being guessed from the transcript's mtime -- a
//! file touched 20 seconds ago was reported as working, one touched a minute ago as idle.
//! That is an inference, and the UI had to admit it ("推断"). Upstream does not guess: every
//! one of these harnesses *records* the turn lifecycle in its own transcript, and the newest
//! lifecycle record is a statement of fact about where the turn stands.
//!
//! The measured shapes (2026-10-14, real sessions on this machine):
//!
//! - **codex** writes `event_msg` rows whose payload carries the turn markers:
//!   `task_started`, then one of `task_complete` / `turn_aborted` / `turn_failed`. Upstream
//!   reads exactly these (`codexLatestTerminalTurnTimestamp`). A `task_started` with no later
//!   terminal marker is a turn in flight.
//! - **pi** writes `message` rows: an assistant row that carries a `toolCall` is waiting for
//!   the tool result, so the turn is in flight; an assistant row with only text has finished
//!   speaking; a `toolResult` row means the model is about to continue.
//! - **claude** writes `type: assistant` / `type: user` rows. A `user` row whose content is a
//!   `tool_result` is the tool answering, so the turn continues; a `user` row that is a real
//!   prompt starts a turn; an `assistant` row with only text ends it.
//!
//! Only the tail is read: these records are the newest lines by construction, and the live
//! transcripts here reach 12-19 MB.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::session::SessionStatus;

/// How much of the end of a transcript to read. The newest lifecycle record sits within the
/// last few rows, and a row can be large (a tool result carrying a file), so this is generous
/// while still bounded -- the alternative is reading 19 MB on every scan.
const TAIL_BYTES: u64 = 64 * 1024;

/// The turn state a transcript states about itself, or `None` when the tail holds no
/// lifecycle record at all (an unknown harness, a brand-new empty file, or a shape from a
/// newer version).
///
/// `None` is deliberately distinct from `Idle`: the caller must be able to tell "the file
/// says it is idle" from "the file says nothing", because only the first is a fact.
pub fn turn_state(path: &Path) -> Option<SessionStatus> {
    let tail = read_tail(path, TAIL_BYTES)?;
    let lines: Vec<&str> = tail.lines().collect();

    // Newest first: the most recent lifecycle record is what describes the session *now*.
    for line in lines.iter().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // A row cut in half by the tail window, or a malformed row. Skipping is correct:
            // the next line back is still a complete record.
            continue;
        };
        if let Some(state) = codex_turn_state(&value)
            .or_else(|| pi_turn_state(&value))
            .or_else(|| claude_turn_state(&value))
        {
            return state;
        }
    }
    None
}

/// codex: `{"type":"event_msg","payload":{"type":"task_started"|"task_complete"|…}}`.
///
/// A terminal marker means the turn is over; `task_started` means it is not. Both are
/// statements by codex, not guesses -- which is why a `task_started` with nothing after it
/// reports Working even if the file has not been touched for minutes.
fn codex_turn_state(value: &serde_json::Value) -> Option<Option<SessionStatus>> {
    if value.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
        return None;
    }
    let kind = value
        .get("payload")
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())?;
    match kind {
        "task_started" => Some(Some(SessionStatus::Working)),
        // Upstream treats all three as terminal turns, including the failures -- a turn that
        // died is over, and the card must stop looking busy.
        "task_complete" => Some(Some(SessionStatus::Done)),
        "turn_aborted" | "turn_failed" => Some(Some(SessionStatus::Error)),
        _ => Some(None),
    }
}

/// pi: `{"type":"message","message":{"role":…,"content":[…]}}`.
fn pi_turn_state(value: &serde_json::Value) -> Option<Option<SessionStatus>> {
    if value.get("type").and_then(|t| t.as_str()) != Some("message") {
        return None;
    }
    let message = value.get("message")?;
    let role = message.get("role").and_then(|r| r.as_str())?;
    match role {
        // The model emitted a tool call and is waiting for its result.
        "assistant" => {
            let content = message.get("content");
            if content_has_block(content, "toolCall") {
                Some(Some(SessionStatus::Working))
            } else {
                // Only text: it finished speaking, so the turn is the human's now.
                Some(Some(SessionStatus::Done))
            }
        }
        // The tool answered; the model continues from here.
        "toolResult" => Some(Some(SessionStatus::Working)),
        // A real prompt from the human started a turn.
        "user" => Some(Some(SessionStatus::Working)),
        _ => Some(None),
    }
}

/// claude: `{"type":"assistant"|"user","message":{"role":…,"content":[…]}}`.
fn claude_turn_state(value: &serde_json::Value) -> Option<Option<SessionStatus>> {
    let kind = value.get("type").and_then(|t| t.as_str())?;
    if kind != "assistant" && kind != "user" {
        return None;
    }
    let message = value.get("message")?;
    let content = message.get("content");
    if kind == "assistant" {
        // A tool call means it is waiting on the tool; plain text means it stopped talking.
        if content_has_block(content, "tool_use") {
            return Some(Some(SessionStatus::Working));
        }
        return Some(Some(SessionStatus::Done));
    }
    // A `user` row is either a real prompt (a new turn) or the tool's own result arriving --
    // claude records both under the same type, and only the content tells them apart.
    if content_has_block(content, "tool_result") {
        return Some(Some(SessionStatus::Working));
    }
    Some(Some(SessionStatus::Working))
}

/// Does this content hold a block of the given type? Content is a string for some rows and an
/// array of blocks for others, and a bare string never carries a block.
fn content_has_block(content: Option<&serde_json::Value>, kind: &str) -> bool {
    let Some(blocks) = content.and_then(|c| c.as_array()) else {
        return false;
    };
    blocks.iter().any(|block| {
        block.get("type").and_then(|t| t.as_str()) == Some(kind)
    })
}

/// Read at most `limit` bytes from the end of a file.
fn read_tail(path: &Path, limit: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let take = len.min(limit);
    file.seek(SeekFrom::End(-(take as i64))).ok()?;
    let mut buffer = vec![0u8; take as usize];
    file.read_exact(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ci-turn-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn codex_a_started_turn_with_no_marker_is_working() {
        // The bug this replaces: state came from the file's mtime, so a turn that had been
        // quietly running for two minutes read as idle. codex *says* it started and never
        // said it finished, so the honest answer is Working regardless of mtime.
        let path = write("codex-started", concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/p"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command"}}"#, "\n",
        ));
        assert_eq!(turn_state(&path), Some(SessionStatus::Working));
    }

    #[test]
    fn codex_a_completed_turn_is_done() {
        let path = write("codex-done", concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}"#, "\n",
        ));
        assert_eq!(turn_state(&path), Some(SessionStatus::Done));
    }

    #[test]
    fn codex_a_second_started_turn_after_a_completed_one_is_working_again() {
        // The whole point of "newest record wins": an earlier completion must not keep the card
        // green once the next turn has begun.
        let path = write("codex-two-turns", concat!(
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t2"}}"#, "\n",
        ));
        assert_eq!(turn_state(&path), Some(SessionStatus::Working));
    }

    #[test]
    fn codex_aborted_and_failed_turns_are_terminal_and_not_working() {
        // Upstream: an aborted or failed turn is over. It must not keep looking busy.
        for marker in ["turn_aborted", "turn_failed"] {
            let path = write(&format!("codex-{marker}"), &format!(
                "{}\n{}\n",
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                format!(r#"{{"type":"event_msg","payload":{{"type":"{marker}"}}}}"#)
            ));
            let state = turn_state(&path);
            assert!(
                state == Some(SessionStatus::Error),
                "{marker} must be terminal, got {state:?}"
            );
        }
    }

    #[test]
    fn pi_a_tool_call_waits_and_a_text_only_reply_finishes() {
        let working = write("pi-tool", concat!(
            r#"{"type":"message","message":{"role":"assistant","content":"#,
            r#"[{"type":"thinking"},{"type":"toolCall","name":"bash"}]}}"#, "\n",
        ));
        assert_eq!(turn_state(&working), Some(SessionStatus::Working));

        let done = write("pi-text", concat!(
            r#"{"type":"message","message":{"role":"assistant","content":"#,
            r#"[{"type":"thinking"},{"type":"text","text":"done"}]}}"#, "\n",
        ));
        assert_eq!(turn_state(&done), Some(SessionStatus::Done));

        let continuing = write("pi-result", concat!(
            r#"{"type":"message","message":{"role":"toolResult","content":"#,
            r#"[{"type":"text","text":"output"}]}}"#, "\n",
        ));
        assert_eq!(turn_state(&continuing), Some(SessionStatus::Working));
    }

    #[test]
    fn claude_a_tool_use_waits_and_a_text_reply_finishes() {
        let asking = write("claude-tool", concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":"#,
            r#"[{"type":"tool_use","name":"Bash"}]}}"#, "\n",
        ));
        assert_eq!(turn_state(&asking), Some(SessionStatus::Working));

        // A trailing tool_result (recorded as a `user` row) means the tool answered and the
        // model is about to continue -- not that a human typed something and is being waited on.
        let running = write("claude-result", concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":"#,
            r#"[{"type":"tool_use","name":"Bash"}]}}"#, "\n",
            r#"{"type":"user","message":{"role":"user","content":"#,
            r#"[{"type":"tool_result","content":"ok"}]}}"#, "\n",
        ));
        assert_eq!(turn_state(&running), Some(SessionStatus::Working));

        let finished = write("claude-done", concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":"#,
            r#"[{"type":"text","text":"all done"}]}}"#, "\n",
        ));
        assert_eq!(turn_state(&finished), Some(SessionStatus::Done));
    }

    #[test]
    fn a_transcript_that_says_nothing_is_not_reported_as_idle() {
        // `None` means "the file states nothing" and must stay distinguishable from "the file
        // states idle" -- otherwise silence would be laundered into a fact.
        let path = write("silent", concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/p"}}"#, "\n",
            r#"{"type":"model_change","provider":"x"}}"#, "\n",
        ));
        assert_eq!(turn_state(&path), None);

        // And a row the tail window cut in half is skipped, not misread.
        let cut = write("cut", "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_st");
        assert_eq!(turn_state(&cut), None);
    }
}
