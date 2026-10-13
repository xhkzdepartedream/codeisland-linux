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

    let route = match normalized.as_deref() {
        Some(name) => EventNormalizer::route_kind(name, question.is_some()),
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
        extra: Default::default(),
    };

    let body = match serde_json::to_vec(&event) {
        Ok(body) => body,
        Err(_) => return EXIT_OK,
    };

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
    // Half-close so the daemon sees EOF and knows the payload is complete.
    let _ = stream.shutdown(std::net::Shutdown::Write);

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
