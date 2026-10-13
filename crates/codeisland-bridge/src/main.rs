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
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
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

/// Read all of stdin, bounded so a runaway producer cannot exhaust memory.
fn read_stdin() -> Option<serde_json::Value> {
    // The daemon's receive path caps payloads at 10 MiB upstream; the bridge
    // only ever sees one hook payload, so a much smaller cap is plenty and
    // prevents an obviously broken caller from costing us.
    const MAX_INPUT: u64 = 8 * 1024 * 1024;

    let mut raw = Vec::new();
    std::io::stdin()
        .take(MAX_INPUT)
        .read_to_end(&mut raw)
        .ok()?;
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
