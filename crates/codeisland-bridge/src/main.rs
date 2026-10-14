//! `codeisland-bridge` -- the hook client every agent CLI runs.
//!
//! Ported from `Sources/CodeIslandBridge/main.swift` in the upstream macOS
//! project. Spawned once per hook event, so it must start fast and never be the
//! reason an agent stalls.
//!
//! # The safety contract
//!
//! This process sits between an agent and its own permission prompt. On *any*
//! problem -- no daemon, refused connection, timeout, malformed input, an
//! unexpected panic -- it must exit 0 having written nothing to stdout. The CLI
//! then shows its normal prompt.
//!
//! It must never auto-approve, and never hang. Those two failure modes are a
//! security hole and an availability hole respectively, which is why the
//! timeouts here are short and bounded and why the panic hook below re-exits
//! cleanly instead of unwinding with a non-zero status.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use codeisland_core::event::{EventNormalizer, RouteKind};
use codeisland_core::protocol::{summarize_tool_call, HookEvent};
use codeisland_core::socket_path;

/// Exit code that means "no decision, use your own prompt".
const EXIT_OK: u8 = 0;

/// How long to wait for the connection itself. Kept short so that a status
/// event never makes the agent's tool call feel slow.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a fire-and-forget event waits for the daemon's `{}` acknowledgement
/// before giving up. The event has already been delivered by then.
const EVENT_REPLY_TIMEOUT: Duration = Duration::from_millis(1000);

/// Ceiling on how long a permission request waits for a human.
///
/// One hour: long enough that stepping away from the desk does not silently
/// drop the decision, short enough that a wedged daemon eventually releases the
/// agent instead of pinning it forever.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(3600);

/// Install a panic hook that exits cleanly.
///
/// A panic while holding the agent's tool call hostage is worse than any error
/// the panic is reporting. Turning it into exit 0 + no output hands control back
/// to the CLI.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_info| {
        // Intentionally silent: anything on stdout is a protocol message.
        std::process::exit(EXIT_OK as i32);
    }));
}

fn main() -> ExitCode {
    install_panic_hook();
    let code = run();
    ExitCode::from(code)
}

fn run() -> u8 {
    let args: Vec<String> = std::env::args().collect();
    let source = flag_value(&args, "--source");

    // Opt-out honoured by upstream so tests and scripts can silence the bridge.
    if std::env::var_os("CODEISLAND_SKIP").is_some() {
        return EXIT_OK;
    }

    let stdin_payload = match read_stdin() {
        Some(payload) => payload,
        None => return EXIT_OK,
    };

    let event_name = stdin_payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| flag_value(&args, "--event"));

    let session_id = stdin_payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    // Without a session id there is nothing to attach the event to. Upstream
    // synthesises `<source>-ppid-<pid>` rather than dropping the event; that
    // fallback needs process-ancestry walking, so it lands in a later phase.
    // Dropping is the conservative choice: a missing card beats a wrong one.
    if session_id.is_empty() {
        debug_log("no session_id, dropping");
        return EXIT_OK;
    }

    let normalized = event_name
        .as_deref()
        .map(EventNormalizer::normalize)
        .map(str::to_owned);

    let question = stdin_payload
        .get("question")
        .and_then(|v| v.as_str())
        .filter(|q| !q.is_empty())
        .map(str::to_owned);

    // AskUserQuestion rides the permission channel but must not be answered with a
    // bare approve/deny, so it takes the question route on its way to the daemon.
    let asks_user = stdin_payload.get("tool_name").and_then(|v| v.as_str())
        == Some(codeisland_core::question::ASK_TOOL);

    let route = match normalized.as_deref() {
        Some(name) => EventNormalizer::route_kind(name, question.is_some(), asks_user),
        None => RouteKind::Event,
    };

    // The daemon not being reachable is the common case (nobody is running the
    // island), so check the socket exists before paying for a connect attempt.
    let path = socket_path();
    if !is_socket(&path) {
        debug_log("socket missing, exiting silently");
        return EXIT_OK;
    }

    let tool_name = stdin_payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    let event = HookEvent {
        source: source.clone(),
        event: event_name,
        normalized_event: normalized,
        session_id: Some(session_id),
        // The payload's cwd is the agent's project directory and is what the
        // island labels the card with. This process's own working directory is
        // only a fallback: a hook can be spawned from anywhere, and using it
        // preferentially mislabels every session.
        cwd: stdin_payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            }),
        tool_name: Some(tool_name.clone()).filter(|s| !s.is_empty()),
        tool_input: stdin_payload.get("tool_input").cloned(),
        question,
        summary: Some(summarize_tool_call(
            &tool_name,
            stdin_payload.get("tool_input"),
        )),
        ts_ms: now_ms(),
        // Taken from *this* process, not from the payload: the hook runs as a child
        // of the agent, so its ancestry is the agent's terminal.
        terminal: Some(codeisland_core::terminal::capture()).filter(|t| t.is_actionable()),
        // Whatever the CLI sent that we do not model is carried through, so the
        // journal shows the real payload instead of a projection of it. Until now
        // this was `Default::default()`, which silently dropped fields like
        // `message` -- and a dropped `message` is why a "round finished"
        // notification had nothing to say.
        extra: collect_extra(&stdin_payload),
    };

    let mut body = match serde_json::to_vec(&event) {
        Ok(body) => body,
        Err(_) => return EXIT_OK,
    };
    // One frame per connection, terminated by a newline. Compact JSON never
    // contains a raw newline, so the delimiter cannot appear inside a payload.
    body.push(b'\n');

    let mut stream = match connect(&path) {
        Some(stream) => stream,
        None => {
            debug_log("connect failed, exiting silently");
            return EXIT_OK;
        }
    };

    if stream.write_all(&body).is_err() || stream.flush().is_err() {
        debug_log("write failed, exiting silently");
        return EXIT_OK;
    }
    // The write half deliberately stays open. The daemon reads one newline-
    // terminated frame, so it no longer needs an EOF to know the payload ended --
    // and keeping the connection half-open is the only way it can tell a hook that
    // is still waiting from one whose agent has already exited. Half-closing here
    // would make every parked card look like a vanished agent.

    let wait = match route {
        RouteKind::Permission => PERMISSION_TIMEOUT,
        RouteKind::Question => PERMISSION_TIMEOUT,
        RouteKind::Event => EVENT_REPLY_TIMEOUT,
    };
    let _ = stream.set_read_timeout(Some(wait));

    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);

    // Only a blocking event writes to stdout, and only when the daemon actually
    // produced an answer. Anything else is silence, which is what makes the CLI
    // fall back to its own prompt.
    if matches!(route, RouteKind::Permission | RouteKind::Question) && !response.is_empty() {
        let _ = std::io::stdout().write_all(&response);
        let _ = std::io::stdout().flush();
    }

    EXIT_OK
}

/// How long to wait for the CLI to hand over its payload and close the pipe.
///
/// A hook producer that forgets to close stdin would otherwise block this
/// process forever, and this process sits in the agent's critical path. Upstream
/// solves the same problem with alarm(5); polling with a deadline does it without
/// relying on a process-wide signal.
///
/// Shortened via CODEISLAND_STDIN_TIMEOUT_MS so the deadline itself is testable
/// without a 5 second suite.
fn stdin_deadline() -> Duration {
    std::env::var("CODEISLAND_STDIN_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(5))
}

/// Cap on the payload. The daemon accepts up to 10 MiB (large Codex diffs); the
/// bridge only sees one hook payload, so a smaller ceiling is plenty and stops an
/// obviously broken producer from costing memory.
const MAX_INPUT: usize = 8 * 1024 * 1024;

/// Read all of stdin, bounded by both a deadline and a size ceiling.
///
/// Returns None on timeout, on empty input, and on unparsable JSON -- every one
/// of those takes the same silent-exit path, so there is nothing to distinguish
/// for the caller.
fn read_stdin() -> Option<serde_json::Value> {
    use std::os::fd::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    let deadline = std::time::Instant::now() + stdin_deadline();
    let mut raw: Vec<u8> = Vec::with_capacity(4096);
    let mut buf = [0u8; 8192];

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            debug_log("stdin deadline reached, giving up on this event");
            return None;
        }
        if raw.len() >= MAX_INPUT {
            debug_log("stdin exceeded size ceiling, dropping payload");
            return None;
        }

        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: pfd is a valid local, nfds is 1, and poll() only writes to it.
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if ready == 0 {
            debug_log("stdin deadline reached, giving up on this event");
            return None;
        }

        let mut stdin = std::io::stdin();
        match stdin.read(&mut buf) {
            Ok(0) => break, // EOF: the CLI closed the pipe, payload is complete.
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() >= MAX_INPUT {
                    debug_log("stdin exceeded size ceiling, dropping payload");
                    return None;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }

    if raw.is_empty() {
        return None;
    }
    serde_json::from_slice(&raw).ok()
}


fn connect(path: &std::path::Path) -> Option<UnixStream> {
    // UnixStream::connect has no timeout knob; a non-blocking connect polled
    // with poll(2) is the portable way to bound it. A local socket either
    // accepts immediately or fails, so the deadline is belt-and-braces.
    let stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
    Some(stream)
}

/// Whether the path exists and is a unix socket, without following the
/// convention of treating any file as a candidate listener.
fn is_socket(path: &std::path::Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
}

fn now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Top-level payload fields that already have a typed home in `HookEvent`.
const CONSUMED_FIELDS: &[&str] = &[
    "hook_event_name",
    "session_id",
    "cwd",
    "tool_name",
    "tool_input",
    "question",
    "source",
];

/// Fields never carried, whatever their size. A PostToolUse `tool_response` can be
/// megabytes, and the journal is an append-only file on the user's disk; keeping it
/// would turn a debugging aid into a disk-filling hazard.
const DROPPED_FIELDS: &[&str] = &["tool_response"];

/// Longest unknown value kept intact.
const EXTRA_VALUE_CAP: usize = 512;

/// Collect the payload's unmodelled fields, capped so one oversized value cannot
/// bloat every frame and journal line.
fn collect_extra(payload: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut extra = serde_json::Map::new();
    let Some(map) = payload.as_object() else {
        return extra;
    };
    for (key, value) in map {
        if CONSUMED_FIELDS.contains(&key.as_str()) || DROPPED_FIELDS.contains(&key.as_str()) {
            continue;
        }
        extra.insert(key.clone(), cap_value(value));
    }
    extra
}

fn cap_value(value: &serde_json::Value) -> serde_json::Value {
    let size = value.to_string().len();
    if size <= EXTRA_VALUE_CAP {
        return value.clone();
    }
    match value.as_str() {
        Some(text) => {
            let mut head: String = text.chars().take(EXTRA_VALUE_CAP).collect();
            head.push('…');
            serde_json::Value::String(head)
        }
        // Structure is not guessed at: an over-long object or array is replaced by a
        // marker that says how big it was, which is the honest version of a truncation.
        None => serde_json::json!({ "truncated_bytes": size }),
    }
}

/// Diagnostics go to a file, never stdout -- stdout is a protocol channel.
fn debug_log(message: &str) {
    if std::env::var_os("CODEISLAND_DEBUG").is_none() {
        return;
    }
    let path = std::env::var("CODEISLAND_BRIDGE_LOG")
        .unwrap_or_else(|_| "/tmp/codeisland-bridge.log".to_string());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[{}] {message}", now_ms().unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unmodelled_fields_survive_the_bridge() {
        let payload = json!({
            "hook_event_name": "Stop",
            "session_id": "s1",
            "cwd": "/home/dev/api",
            "message": "重构完成，测试全绿",
            "model": "haiku",
            "stop_hook_active": false
        });
        let extra = collect_extra(&payload);
        assert_eq!(extra["message"], json!("重构完成，测试全绿"));
        assert_eq!(extra["model"], json!("haiku"));
        assert_eq!(extra["stop_hook_active"], json!(false));
        assert!(
            !extra.contains_key("session_id"),
            "typed fields must not be duplicated into extra"
        );
    }

    #[test]
    fn a_tool_response_is_never_carried() {
        let payload = json!({"hook_event_name": "PostToolUse", "tool_response": {"stdout": "x".repeat(5000)}});
        let extra = collect_extra(&payload);
        assert!(!extra.contains_key("tool_response"), "the journal must not swallow tool output");
    }

    #[test]
    fn long_values_are_capped_not_copybombed() {
        let payload = json!({"transcript_path": "/tmp/".to_owned() + &"a".repeat(4000)});
        let extra = collect_extra(&payload);
        let kept = extra["transcript_path"].as_str().unwrap();
        assert!(kept.len() <= EXTRA_VALUE_CAP + 12, "kept {} bytes", kept.len());
        assert!(kept.ends_with('…'), "the truncation is visible: {kept}");

        let big = json!({"meta": {"blob": "y".repeat(4000)}});
        let extra = collect_extra(&big);
        assert!(extra["meta"]["truncated_bytes"].is_number(), "structure is reported, not guessed at");
    }

    #[test]
    fn a_non_object_payload_produces_no_extra() {
        assert!(collect_extra(&json!("just a string")).is_empty());
        assert!(collect_extra(&json!(null)).is_empty());
    }
}
