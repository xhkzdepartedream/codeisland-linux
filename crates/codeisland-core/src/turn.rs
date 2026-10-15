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

/// First window to read from the end of a transcript. The newest lifecycle record usually sits
/// at the very end, so this finds it after one read and one line parsed.
const TAIL_START: u64 = 64 * 1024;
/// Largest window to expand to before giving up.
///
/// The window has to grow because a record can sit far behind EOF: a turn writes its
/// `task_started` and then streams -- measured on this machine, one live codex turn had
/// `task_started` **456 KB** from the end. A fixed 64 KB tail missed it and fell back to a
/// freshness guess (which said idle while the turn was running). 8 MB covers transcripts here
/// several times over while keeping the worst case bounded.
const TAIL_MAX: u64 = 8 * 1024 * 1024;
/// How much the window grows on each retry.
const TAIL_GROWTH: u64 = 8;

/// The turn state a transcript states about itself, or `None` when the tail holds no
/// lifecycle record at all (an unknown harness, a brand-new empty file, or a shape from a
/// newer version).
///
/// `None` is deliberately distinct from `Idle`: the caller must be able to tell "the file
/// says it is idle" from "the file says nothing", because only the first is a fact.
pub fn turn_state(path: &Path) -> Option<SessionStatus> {
    let len = fs::metadata(path).ok()?.len();
    if len == 0 {
        return None;
    }
    let mut window = TAIL_START.min(len);
    loop {
        if let Some(text) = read_tail(path, window, len) {
            // Newest first: the most recent lifecycle record describes the session *now*, so an
            // earlier `task_complete` must not keep a card green once the next turn has begun.
            // Anything newer than the record we return is closer to EOF and therefore inside the
            // same window, so the first hit scanning backwards is the newest overall.
            for line in text.lines().rev() {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    // A row cut in half by the window, or a malformed row. Skipping is correct:
                    // the next line back is still a complete record.
                    continue;
                };
                if let Some(state) = codex_turn_state(&value)
                    .or_else(|| pi_turn_state(&value))
                    .or_else(|| claude_turn_state(&value))
                {
                    return Some(state);
                }
            }
        }
        // Nothing in this window. Widen and try again, or give up at the cap / file start.
        if window >= len || window >= TAIL_MAX {
            return None;
        }
        window = window.saturating_mul(TAIL_GROWTH).min(TAIL_MAX).min(len);
    }
}

/// codex: `{"type":"event_msg","payload":{"type":"task_started"|"task_complete"|…}}`.
///
/// A terminal marker means the turn is over; `task_started` means it is not. Both are
/// statements by codex, not guesses -- which is why a `task_started` with nothing after it
/// reports Working even if the file has not been touched for minutes.
///
/// Returns `None` for every other row, **including a codex `event_msg` that is not one of the
/// four markers** (`item_completed`, `token_count`, `thread_settings_applied`, ...). Those rows
/// stream continuously *after* the marker that matters, so treating one as an answer stopped
/// the scan and made codex fall back to the freshness guess almost every time -- the bug that
/// hid the whole feature for codex while pi and claude looked fine.
fn codex_turn_state(value: &serde_json::Value) -> Option<SessionStatus> {
    if value.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
        return None;
    }
    match value
        .get("payload")
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())?
    {
        "task_started" => Some(SessionStatus::Working),
        // The turn ended. Upstream folds every terminal marker (complete / aborted / failed)
        // into "not working", and the same shape has to hold here: a turn that is over must stop
        // looking busy.
        "task_complete" => Some(SessionStatus::Done),
        // A user pressed Esc. That is an ordinary end of turn waiting for the next prompt -- it
        // is not an error, and painting it red would scold the person who did it. Upstream
        // treats abort exactly like completion; Done is our nearer equivalent.
        "turn_aborted" => Some(SessionStatus::Done),
        // A genuine failure is worth its own word, and it is a fact the transcript states
        // (`turn_failed`), not a guess.
        "turn_failed" => Some(SessionStatus::Error),
        _ => None,
    }
}

/// pi: `{"type":"message","message":{"role":…,"content":[…]}}`.
fn pi_turn_state(value: &serde_json::Value) -> Option<SessionStatus> {
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
                Some(SessionStatus::Working)
            } else {
                // Only text: it finished speaking, so the turn is the human's now.
                Some(SessionStatus::Done)
            }
        }
        // The tool answered; the model continues from here.
        "toolResult" => Some(SessionStatus::Working),
        // A real prompt from the human started a turn.
        "user" => Some(SessionStatus::Working),
        _ => None,
    }
}

/// claude: `{"type":"assistant"|"user","message":{"role":…,"content":[…]}}`.
fn claude_turn_state(value: &serde_json::Value) -> Option<SessionStatus> {
    let kind = value.get("type").and_then(|t| t.as_str())?;
    if kind != "assistant" && kind != "user" {
        return None;
    }
    let message = value.get("message")?;
    let content = message.get("content");
    if kind == "assistant" {
        // A tool call means it is waiting on the tool; plain text means it stopped talking.
        if content_has_block(content, "tool_use") {
            return Some(SessionStatus::Working);
        }
        return Some(SessionStatus::Done);
    }
    // A `user` row is either a real prompt (a new turn) or the tool's own result arriving --
    // claude records both under the same type, and only the content tells them apart.
    if content_has_block(content, "tool_result") {
        return Some(SessionStatus::Working);
    }
    Some(SessionStatus::Working)
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

/// Read at most `window` bytes from the end of a file. When the window does not reach the start
/// of the file the buffer begins mid-line, so the leading partial line is dropped -- a record cut
/// there is simply not seen yet, and the next expansion reads it whole.
fn read_tail(path: &Path, window: u64, len: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let take = window.min(len);
    if take == 0 {
        return None;
    }
    file.seek(SeekFrom::End(-(take as i64))).ok()?;
    let mut buffer = vec![0u8; take as usize];
    file.read_exact(&mut buffer).ok()?;
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if take < len {
        if let Some(newline) = text.find('\n') {
            text.drain(..=newline);
        }
    }
    Some(text)
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
    fn codex_terminal_turns_stop_looking_busy() {
        // Upstream folds complete / aborted / failed into "not working"; all three must keep the
        // card from looking busy. An abort is the user's own Esc, not an error, so it reads as a
        // finished turn; only a real failure gets the Error word.
        for (marker, expected) in [
            ("task_complete", SessionStatus::Done),
            ("turn_aborted", SessionStatus::Done),
            ("turn_failed", SessionStatus::Error),
        ] {
            let path = write(&format!("codex-{marker}"), &format!(
                "{}\n{}\n",
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                format!(r#"{{"type":"event_msg","payload":{{"type":"{marker}"}}}}"#)
            ));
            assert_eq!(turn_state(&path), Some(expected), "{marker}");
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
    fn codex_a_non_marker_event_msg_does_not_end_the_search() {
        // The bug that hid the feature for codex: the rows after a turn marker are ordinary
        // event_msgs (`item_completed` / `token_count`), and treating one as an answer stopped
        // the scan on the newest rows -- so codex fell back to the freshness guess almost every
        // time while pi and claude (whose rows all carry meaning) looked fine.
        let path = write("codex-nonmarker", concat!(
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"item_completed"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning"}}"#, "\n",
        ));
        assert_eq!(turn_state(&path), Some(SessionStatus::Done));
    }

    #[test]
    fn a_codex_turn_started_far_from_the_end_is_still_found() {
        // The bug this guards, found in live data after the small synthetic tests passed: a turn
        // writes `task_started` and then streams hundreds of KB before it ends, so a fixed 64 KB
        // tail missed the marker and fell back to a freshness guess -- reporting idle while codex
        // was working. Measured: 456 KB from the end on one real turn.
        let mut body = String::new();
        body.push_str(r#"{"type":"event_msg","payload":{"type":"task_started"}}"#);
        body.push('\n');
        let filler = format!(
            r#"{{"type":"response_item","payload":{{"type":"reasoning","text":"{}"}}}}"#,
            "x".repeat(900)
        );
        for _ in 0..600 {
            body.push_str(&filler);
            body.push('\n');
        }
        let path = write("codex-far", &body);
        assert!(
            fs::metadata(&path).unwrap().len() > 500 * 1024,
            "filler must push the marker well beyond the first window"
        );
        assert_eq!(turn_state(&path), Some(SessionStatus::Working));
    }

    #[test]
    fn a_codex_terminal_marker_far_from_the_end_still_wins() {
        // The mirror case: the turn finished and then a large result was written, so the terminal
        // marker is far back. It must still be read as Done rather than missed.
        let filler = format!(
            r#"{{"type":"response_item","payload":{{"type":"reasoning","text":"{}"}}}}"#,
            "y".repeat(900)
        );
        let mut body = String::new();
        for _ in 0..600 {
            body.push_str(&filler);
            body.push('\n');
        }
        body.push_str(r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#);
        body.push('\n');
        for _ in 0..600 {
            body.push_str(&filler);
            body.push('\n');
        }
        let path = write("codex-far-done", &body);
        assert_eq!(turn_state(&path), Some(SessionStatus::Done));
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
