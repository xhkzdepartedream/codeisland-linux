//! `codeisland-daemon` — the long-lived process that owns the hook socket.
//!
//! Mirrors `Sources/CodeIsland/HookServer.swift` in the upstream macOS project,
//! plus a second socket that streams live session state to UI clients.
//!
//! # The safety contract
//!
//! A blocking hook sits between an agent and its own permission prompt. If this
//! daemon cannot deliver a decision, the answer is *silence*: the bridge then
//! exits with no output and the CLI falls back to its normal prompt. Replying
//! `deny` would change the user's behaviour; replying `allow` would be a forged
//! approval. Silence is the only honest answer, and it is what happens whenever
//! no UI is listening.

mod journal;
mod jump;
mod notify;
mod pending;
mod view;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use codeisland_core::event::RouteKind;
use codeisland_core::protocol::{permission_answer, Behavior, HookEvent};
use codeisland_core::question::{self, Question};
use codeisland_core::session::{AnswerChoice, PermissionRequestView, SessionRegistry, ViewUpdate};
use codeisland_core::socket_path;

use journal::{default_journal_path, Journal};
use jump::JUMP_COMMAND_ENV;
use notify::Notifier;
use pending::{await_decision, Decision, PendingRequests};
use view::{view_socket_path, ViewClients};

/// Largest payload accepted, matching the upstream limit that exists to
/// accommodate large file diffs from tools like Codex.
const MAX_PAYLOAD: u64 = 10 * 1024 * 1024;

/// How long to wait for a connection to deliver its payload. Finite, so a stuck
/// peer cannot pin a thread forever, and generous, because the bridge may be
/// collecting process ancestry on a loaded machine.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the hook threads and the view threads share.
struct Shared {
    registry: Mutex<SessionRegistry>,
    clients: Mutex<ViewClients>,
    journal: Mutex<Journal>,
    /// Hooks parked waiting for a human, keyed by request id.
    pending: Mutex<PendingRequests>,
    /// Desktop notifications, on a worker thread so a hook never waits for one.
    notify: Arc<Notifier>,
    /// Where each session's terminal lives, as reported by the bridge. Kept here
    /// rather than in the session model because jumping is a desktop concern: the
    /// island shows a session, the daemon is the one that has to find its window.
    terminals: Mutex<HashMap<String, codeisland_core::terminal::TerminalContext>>,
    /// Whether a `Stop` also sends a notification. Resolved once at startup from
    /// `CODEISLAND_NOTIFY_ON_STOP`; an atomic so no event path re-reads the
    /// environment, and so the value in the log matches the value in use.
    notify_on_stop: Arc<AtomicBool>,
    next_request_id: AtomicU64,
}

impl Shared {
    fn new(journal: Journal) -> Self {
        Self {
            registry: Mutex::new(SessionRegistry::new()),
            clients: Mutex::new(ViewClients::default()),
            journal: Mutex::new(journal),
            pending: Mutex::new(PendingRequests::default()),
            terminals: Mutex::new(HashMap::new()),
            notify: Notifier::spawn(),
            notify_on_stop: Arc::new(AtomicBool::new(notify::parse_flag(
                std::env::var_os(notify::ON_STOP_ENV).as_deref().and_then(|v| v.to_str()),
            ))),
            next_request_id: AtomicU64::new(1),
        }
    }
}

fn main() {
    let hook_path = socket_path();
    let view_path = view_socket_path();
    let journal_path = default_journal_path();

    let journal = match Journal::open(&journal_path) {
        Ok(journal) => journal,
        Err(err) => {
            eprintln!("codeisland-daemon: cannot open journal {journal_path:?}: {err}");
            std::process::exit(1);
        }
    };

    let shared = Arc::new(Shared::new(journal));

    if let Err(err) = std::fs::create_dir_all(view_path.parent().unwrap_or(Path::new("/tmp"))) {
        eprintln!("codeisland-daemon: cannot create view socket directory: {err}");
    }

    let hook_listener = match bind(&hook_path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("codeisland-daemon: cannot bind hook socket {hook_path:?}: {err}");
            std::process::exit(1);
        }
    };
    let view_listener = match bind(&view_path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("codeisland-daemon: cannot bind view socket {view_path:?}: {err}");
            // Losing the UI socket is not fatal: hooks still work and still get
            // the correct silence fallback.
            eprintln!("codeisland-daemon: continuing without a UI socket");
            std::process::exit(1);
        }
    };

    println!(
        "codeisland-daemon: hooks  {}",
        hook_path.display()
    );
    println!(
        "codeisland-daemon: view   {}",
        view_path.display()
    );
    println!(
        "codeisland-daemon: journal {}",
        shared.journal.lock().expect("journal lock").path().display()
    );

    let view_shared = Arc::clone(&shared);
    std::thread::spawn(move || {
        for connection in view_listener.incoming() {
            match connection {
                Ok(stream) => handle_new_viewer(stream, &view_shared),
                Err(err) => eprintln!("codeisland-daemon: view accept failed: {err}"),
            }
        }
    });

    for connection in hook_listener.incoming() {
        match connection {
            Ok(stream) => {
                let thread_shared = Arc::clone(&shared);
                // One thread per connection. A permission request blocks its
                // thread while a human decides, and concurrency here is one tool
                // call at a time across a handful of sessions, so a pool would
                // add machinery without buying anything.
                std::thread::spawn(move || {
                    if let Err(err) = handle_hook(stream, &thread_shared) {
                        eprintln!("codeisland-daemon: connection error: {err}");
                    }
                });
            }
            // A failed accept must not kill the daemon: the listener is usually
            // still healthy and the next connection will work.
            Err(err) => eprintln!("codeisland-daemon: accept failed: {err}"),
        }
    }
}

/// Bind a unix socket, clearing any stale file first.
fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A socket file left by a crashed daemon makes bind() fail. Removing it is
    // safe: this daemon is the single instance by construction (the systemd user
    // unit and the absence of an flock make that a soft guarantee at best, so the
    // removal is deliberate rather than assumed-race).
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Both sockets carry tool-call payloads that can name files and commands.
    // Keep them owner-only rather than relying on umask.
    set_owner_only(path)?;
    Ok(listener)
}

fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// A UI client appeared: register it, send full state, and start reading it.
///
/// The read half is what makes approval possible at all: the island sends a
/// decision back over the same connection, and without a reader that decision
/// would sit unread in the socket buffer while the agent stayed parked.
fn handle_new_viewer(stream: UnixStream, shared: &Arc<Shared>) {
    let viewer_id = {
        let Ok(mut clients) = shared.clients.lock() else {
            return;
        };
        clients.add(stream.try_clone().ok())
    };
    let Some(viewer_id) = (viewer_id != u64::MAX).then_some(viewer_id) else {
        eprintln!("codeisland-daemon: could not register the new UI client");
        return;
    };
    let snapshot = match shared.registry.lock() {
        Ok(registry) => registry.snapshot(),
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };
    if let Ok(mut clients) = shared.clients.lock() {
        clients.broadcast(&snapshot);
    }
    println!(
        "codeisland-daemon: viewer connected ({} total, id {})",
        clients_count(shared),
        viewer_id
    );

    let read_shared = Arc::clone(shared);
    std::thread::spawn(move || {
        read_viewer(viewer_id, &read_shared);
        // Reaching here means the island went away. Any card it was holding has
        // to be withdrawn as *silence*, never as a denial.
        let withdrawn = match read_shared.pending.lock() {
            Ok(mut pending) => pending.withdraw_viewer(viewer_id),
            Err(poisoned) => poisoned.into_inner().withdraw_viewer(viewer_id),
        };
        if !withdrawn.is_empty() {
            println!(
                "codeisland-daemon: viewer {viewer_id} gone, withdrew {} parked request(s)",
                withdrawn.len()
            );
            // Clear the cards *and* the flag they were holding up. Broadcasting
            // PermissionResolved alone is what left a restarted island expanded on
            // "N sessions need you" with no card in it.
            for (request_id, session_id) in &withdrawn {
                release_gate(&read_shared, request_id, session_id);
            }
        }
        if let Ok(mut clients) = read_shared.clients.lock() {
            clients.remove(viewer_id);
        }
    });
}

/// Read decisions from one UI client until it disconnects.
///
/// The stream is cloned so this reader does not contend with the writer that
/// broadcasts updates; a unix socket is full-duplex, but `ViewClients` owns the
/// single handle it was handed.
fn read_viewer(viewer_id: u64, shared: &Arc<Shared>) {
    use codeisland_core::session::ViewCommand;

    // Reaching the client needs the stream, which lives in ViewClients. Take a
    // dedicated read handle by asking the client set for a clone.
    let Some(mut stream) = shared.clients.lock().ok().and_then(|mut c| c.reader(viewer_id))
    else {
        return;
    };

    stream.set_read_timeout(Some(Duration::from_secs(600))).ok();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return, // peer closed
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                // Newline-delimited, same framing as every other socket here.
                while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=position).collect();
                    // Strip the line ending by hand: `[T]::trim_end_matches` is
                    // unstable, and a plain loop over two trailing bytes is
                    // clearer than a feature gate.
                    let mut trimmed = line.as_slice();
                    while trimmed.last().is_some_and(|byte| *byte == b'\n' || *byte == b'\r') {
                        trimmed = &trimmed[..trimmed.len() - 1];
                    }
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_slice::<ViewCommand>(trimmed) {
                        Ok(ViewCommand::PermissionDecision {
                            request_id,
                            behavior,
                            answers,
                        }) => {
                            let decision = decision_for(&shared, &request_id, behavior, answers);
                            let answered = match (&decision, shared.pending.lock()) {
                                (Some(decision), Ok(mut pending)) => {
                                    pending.answer(&request_id, decision.clone()).is_some()
                                }
                                (Some(decision), Err(poisoned)) => poisoned
                                    .into_inner()
                                    .answer(&request_id, decision.clone())
                                    .is_some(),
                                (None, _) => false,
                            };
                            if decision.is_none() {
                                // An answer that does not fit the card that is up:
                                // refused on purpose, so the tool never runs with a
                                // half-filled answer set.
                                println!(
                                    "codeisland-daemon: refusing incomplete answer for request {request_id}"
                                );
                            }
                            if answered {
                                println!(
                                    "codeisland-daemon: request {request_id} answered {:?} by viewer {viewer_id}",
                                    match decision.as_ref().map(|d| d.behavior) {
                                        Some(Behavior::Allow) => "allow",
                                        Some(Behavior::Deny) => "deny",
                                        None => "none",
                                    }
                                );
                                if let Ok(mut clients) = shared.clients.lock() {
                                    clients.broadcast(&ViewUpdate::PermissionResolved {
                                        request_id,
                                    });
                                }
                            } else {
                                // A click that lands after withdrawal or after another
                                // client answered. Ignoring it is the only safe reading.
                                println!(
                                    "codeisland-daemon: ignoring decision for unknown request {request_id}"
                                );
                            }
                        }
                        Ok(ViewCommand::Jump { session_id }) => {
                            let target = shared
                                .terminals
                                .lock()
                                .ok()
                                .and_then(|terminals| terminals.get(&session_id).cloned());
                            let plan = jump::plan(
                                target.as_ref().unwrap_or(&Default::default()),
                                std::env::var(JUMP_COMMAND_ENV).ok().as_deref(),
                            );
                            println!(
                                "codeisland-daemon: jump requested for session {session_id}: {:?}",
                                plan
                            );
                            // Its own thread: `kdotool` talks to KWin through a
                            // script and can take seconds, and a slow jump must not
                            // stall the reader that also carries the human's answers.
                            std::thread::spawn(move || {
                                let outcome = jump::execute(&plan);
                                println!("codeisland-daemon:   -> jump {outcome:?}");
                            });
                        }
                        Err(err) => {
                            eprintln!("codeisland-daemon: malformed command from viewer: {err}")
                        }
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            // Timeout here means the client is idle, not gone: loop again.
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return,
        }
    }
}

fn clients_count(shared: &Arc<Shared>) -> usize {
    shared
        .clients
        .lock()
        .map(|clients| clients.count())
        .unwrap_or(0)
}

fn handle_hook(mut stream: UnixStream, shared: &Arc<Shared>) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    // Borrow rather than move the stream into the adapter: the same socket writes
    // the acknowledgement afterwards.
    //
    // One newline-terminated frame, like every other socket here. Reading to EOF
    // instead would force the hook to half-close, and then an EOF could no longer
    // mean "the agent exited" -- which is the only way to notice a card whose
    // owner is already gone.
    let mut raw = Vec::new();
    {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new((&mut stream).take(MAX_PAYLOAD));
        if reader.read_until(b'\n', &mut raw).unwrap_or(0) == 0 {
            // Connected and left without sending anything.
            return Ok(());
        }
    }
    while raw.last().is_some_and(|byte| *byte == b'\n' || *byte == b'\r') {
        raw.pop();
    }
    if raw.is_empty() {
        return Ok(());
    }

    let event: HookEvent = match serde_json::from_slice(&raw) {
        Ok(event) => event,
        Err(err) => {
            // Malformed payloads are never answered with a decision.
            eprintln!("codeisland-daemon: unparseable payload ({err})");
            return Ok(());
        }
    };

    let route = event.route_kind();
    let now = now_ms();

    if let Ok(mut journal) = shared.journal.lock() {
        let _ = journal.record(&event);
    }

    let update = {
        let mut registry = match shared.registry.lock() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        registry.apply(&event, now)
    };
    let label = match &update {
        Some(ViewUpdate::SessionUpsert { session }) => Some(session.project_label()),
        _ => None,
    };
    if let Some(update) = update {
        if let Ok(mut clients) = shared.clients.lock() {
            clients.broadcast(&update);
        }
    }

    report(&event, route, label.as_deref());

    if let (Some(session_id), Some(context)) = (
        event.session_id.as_deref(),
        event.terminal.as_ref(),
    ) {
        if let Ok(mut terminals) = shared.terminals.lock() {
            terminals.insert(session_id.to_string(), context.clone());
        }
    }

    // A finished round is announced too, and muted by configuration rather than by
    // removing the code: this is the message that makes it safe to walk away and let
    // the agent run. Checked here, on the way past `report`, because the session's
    // own status is already updated by now and the wording can use it.
    if event.normalized_event.as_deref() == Some("Stop")
        && shared.notify_on_stop.load(Ordering::Relaxed)
    {
        let project = event
            .cwd
            .as_deref()
            .map(|cwd| cwd.rsplit('/').next().unwrap_or(cwd))
            .filter(|label| !label.is_empty())
            .unwrap_or("会话");
        shared.notify.round_finished(
            &event.session_id.clone().unwrap_or_default(),
            &format!("{project} 完成了一轮"),
            event.detail_line().as_deref().unwrap_or("等待下一步指令"),
        );
    }

    // A permission event is answered by handle_permission, which needs the event
    // itself; keep the ordinary paths untouched.

    match route {
        RouteKind::Question => {
            // Two very different things land here. `AskUserQuestion` (measured
            // 2026-10-14) arrives on the permission channel and can be answered by
            // rewriting its input -- that path parks and waits. A free-form question
            // from a Notification has no answer channel at all, so it must be
            // acknowledged and left silent; the bridge waits `PERMISSION_TIMEOUT` on
            // this route, and acking with `{}` is what keeps it from stalling an hour
            // on a message nobody can answer here.
            match question::parse(event.tool_name.as_deref(), event.tool_input.as_ref()) {
                Some(questions) => handle_permission(&event, questions, &shared, &mut stream)?,
                None => {
                    use std::io::Write;
                    let _ = stream.write_all(b"{}");
                    let _ = stream.flush();
                }
            }
        }
        RouteKind::Permission => {
            handle_permission(&event, Vec::new(), &shared, &mut stream)?;
        }
        RouteKind::Event => {
            use std::io::Write;
            // Fire-and-forget events still get an acknowledgement so the bridge's
            // read returns promptly instead of waiting out its timeout.
            let _ = stream.write_all(b"{}");
            let _ = stream.flush();
        }
    }

    Ok(())
}

/// Hold a permission request until a human answers, then tell the agent.
///
/// Three outcomes, and the distinction between the last two is the whole point:
///
///   1. answered           -> write `allow`/`deny` back to the hook
///   2. nobody watching    -> write nothing, immediately
///   3. withdrawn or timed -> write nothing
///
/// Outcomes 2 and 3 are identical to the bridge's own "daemon unreachable" path,
/// which is what keeps the safety contract intact: the agent falls back to its own
/// prompt. A withdrawn request must never be reported as a denial, because nobody
/// denied it.
fn handle_permission(
    event: &HookEvent,
    questions: Vec<Question>,
    shared: &Arc<Shared>,
    stream: &mut UnixStream,
) -> std::io::Result<()> {
    // Is there anyone to ask? Without a UI client this must not park the agent.
    let Some(viewer_id) = shared
        .clients
        .lock()
        .map(|clients| clients.any_viewer())
        .unwrap_or(None)
    else {
        // `handle_hook` already reported this event with its project label; a second
        // line here only produced a `project=-` twin of it (and that duplicate noise
        // cost a debugging round on 2026-10-14, when it read like a double card).
        println!("codeisland-daemon:   -> no UI connected, leaving the decision to the CLI");
        // Nothing is parked, so nothing is waiting on *this* island -- but
        // `registry.apply` already flagged the session as needing approval. If we
        // leave it set, the next shell to connect opens expanded with a "needs you"
        // header and no card to answer, forever.
        demote_session(shared, &event.session_id.clone().unwrap_or_default());
        notify_attention(shared, event, false);
        return Ok(());
    };

    let request = PermissionRequestView {
        request_id: {
            // `q` for a question card, `p` for a permission gate: ids show up in the
            // daemon log and in stale-decision messages, so the prefix carries which
            // kind of card a human was looking at.
            let n = shared.next_request_id.fetch_add(1, Ordering::Relaxed);
            if questions.is_empty() { format!("p{n}") } else { format!("q{n}") }
        },
        session_id: event.session_id.clone().unwrap_or_default(),
        source: event.source.clone().unwrap_or_else(|| "unknown".into()),
        tool_name: event.tool_name.clone(),
        summary: event.summary.clone(),
        cwd: event.cwd.clone(),
        questions,
    };

    // Watchdog handles: one clone for the watcher thread, one whose drop stops it.
    // Both must exist before parking, so a failure here cannot leak a parked hook.
    let watched = stream.try_clone()?;
    let stopper = stream.try_clone()?;

    let receiver = {
        let mut pending = shared
            .pending
            .lock()
            .map_err(|_| std::io::Error::other("pending lock poisoned"))?;
        let receiver = pending.park(
            &request.request_id,
            viewer_id,
            &request.session_id,
            request.questions.clone(),
            event.tool_input.clone(),
        );
        println!(
            "codeisland-daemon:   parked {} ({} request(s) now held)",
            request.request_id,
            pending.len()
        );
        receiver
    };

    // A parked hook sends nothing else, so if its socket becomes readable the
    // agent is gone. Without this the daemon -- blocked in `await_decision` --
    // would keep the card on screen for up to PERMISSION_HOLD, offering buttons
    // that answer nobody.
    {
        let watch_shared = Arc::clone(shared);
        let watch_id = request.request_id.clone();
        std::thread::spawn(move || watch_hook_exit(watched, watch_shared, watch_id));
    }
    // Stops the watcher on every exit path, including a failed write below.
    let _watchdog_stop = WatchdogStop(stopper);

    if let Ok(mut clients) = shared.clients.lock() {
        clients.broadcast(&ViewUpdate::PermissionRequested {
            request: request.clone(),
        });
    }

    // The card is on screen now; the notification is the reminder for whoever is
    // looking at the other desktop.
    notify_attention(shared, event, true);

    let decision = await_decision(&receiver);

    // Clear the card on every client, whether or not this one answered, and take
    // the session's "needs you" flag down with it.
    release_gate(shared, &request.request_id, &request.session_id);

    match decision {
        Some(decision) => {
            let body = permission_answer(
                "PermissionRequest",
                decision.behavior,
                decision.updated_input.as_ref(),
            );
            stream.write_all(body.as_bytes())?;
            stream.flush()?;
            println!(
                "codeisland-daemon:   -> answered {:?}{} for request {}",
                decision.behavior,
                if decision.updated_input.is_some() { " with rewritten input" } else { "" },
                request.request_id
            );
        }
        None => {
            // Silence: the bridge writes nothing and the CLI prompts itself.
            println!(
                "codeisland-daemon:   -> nobody answered request {}, leaving the decision to the CLI",
                request.request_id
            );
        }
    }
    Ok(())
}

/// Tell the desktop that an agent is waiting on a human.
///
/// Both outcomes of a permission request deserve this, and they deserve different
/// words. With a card on screen the notification is a reminder; with no UI connected
/// there is no card anywhere, and the terminal is the only place left to answer --
/// which is exactly the case a notification exists for. Neither path changes what
/// the agent is told, which stays silence until a human decides.
fn notify_attention(shared: &Arc<Shared>, event: &HookEvent, shown: bool) {
    let project = event
        .cwd
        .as_deref()
        .map(|cwd| cwd.rsplit('/').next().unwrap_or(cwd))
        .filter(|label| !label.is_empty())
        .unwrap_or("会话");
    let summary = event
        .summary
        .as_deref()
        .unwrap_or("有一个请求在等回答");
    let body = if shown {
        summary.to_string()
    } else {
        format!("{summary}（界面未连接，请在终端里回答）")
    };
    shared.notify.needs_attention(
        &event.session_id.clone().unwrap_or_default(),
        &format!("{project} 需要你"),
        &body,
    );
}

/// Turn a viewer's click into something the parked hook can be answered with.
///
/// `None` means "this click does not answer that card", which is the case for a
/// question card whose selections do not cover every question with a label it
/// actually offered. Refusing is safer than half-answering: `allow` without a
/// complete `answers` map would run `AskUserQuestion` with questions still open.
fn decision_for(
    shared: &Arc<Shared>,
    request_id: &str,
    behavior: Behavior,
    answers: Vec<AnswerChoice>,
) -> Option<Decision> {
    let card = match shared.pending.lock() {
        Ok(pending) => pending.question_of(request_id),
        Err(poisoned) => poisoned.into_inner().question_of(request_id),
    };

    let Some((questions, input)) = card else {
        // Not a question card, so the click is an ordinary approve/deny.
        return Some(Decision::plain(behavior));
    };

    // A question card ignores the `behavior` field: answering it *is* allowing the
    // tool, and the CLI only wants the rewritten input back.
    let pairs: Vec<(String, String)> = answers
        .into_iter()
        .map(|answer| (answer.question, answer.label))
        .collect();
    let input = input?;
    question::answered_input(&input, &questions, &pairs).map(Decision::answered)
}

/// Take a card down, and take the session's "a human is needed" flag down with it.
///
/// The two halves fail independently and both were seen in practice: clearing only
/// the card left the header reading "1 session needs you" over an empty island, and
/// clearing nothing at all left a dead agent's card up with live buttons.
fn release_gate(shared: &Arc<Shared>, request_id: &str, session_id: &str) {
    if let Ok(mut clients) = shared.clients.lock() {
        clients.broadcast(&ViewUpdate::PermissionResolved {
            request_id: request_id.to_string(),
        });
    }
    demote_session(shared, session_id);
}

/// Put a session back to working, once no card of its own is still parked.
fn demote_session(shared: &Arc<Shared>, session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let still_parked = match shared.pending.lock() {
        Ok(pending) => pending.has_parked_for(session_id),
        Err(poisoned) => poisoned.into_inner().has_parked_for(session_id),
    };
    if still_parked {
        return;
    }
    let update = match shared.registry.lock() {
        Ok(mut registry) => registry.release_request(session_id),
        Err(poisoned) => poisoned.into_inner().release_request(session_id),
    };
    if let Some(update) = update {
        if let Ok(mut clients) = shared.clients.lock() {
            clients.broadcast(&update);
        }
    }
}

/// Wakes `watch_hook_exit` when the parked phase ends, however it ends.
///
/// `Shutdown::Read` lands on the socket itself rather than one file descriptor, so
/// it reaches the watcher's clone without disturbing the write half the answer uses.
struct WatchdogStop(UnixStream);

impl Drop for WatchdogStop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Read);
    }
}

/// Release a parked request the moment its hook disappears.
///
/// Readability on a parked hook means EOF: the agent exited, was cancelled, or its
/// own timeout fired. The request is withdrawn -- silence to the CLI, never a
/// denial -- and the card goes with it.
fn watch_hook_exit(mut hook: UnixStream, shared: Arc<Shared>, request_id: String) {
    let mut byte = [0u8; 1];
    loop {
        match hook.read(&mut byte) {
            Ok(0) => break,
            // Nothing is expected from a parked hook; ignore it and keep watching.
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let released = match shared.pending.lock() {
        Ok(mut pending) => pending.withdraw(&request_id),
        Err(poisoned) => poisoned.into_inner().withdraw(&request_id),
    };
    if let Some(session_id) = released {
        println!(
            "codeisland-daemon:   hook for {request_id} vanished, releasing the card"
        );
        release_gate(&shared, &request_id, &session_id);
    }
}

/// Human-readable one-liner on stdout so a foreground run is watchable.
fn report(event: &HookEvent, route: RouteKind, label: Option<&str>) {
    let source = event.source.as_deref().unwrap_or("unknown");
    let name = event
        .normalized_event
        .as_deref()
        .or(event.event.as_deref())
        .unwrap_or("unknown");
    let session = event.session_id.as_deref().unwrap_or("-");
    let kind = match route {
        RouteKind::Permission => "permission",
        RouteKind::Question => "question",
        RouteKind::Event => "event",
    };
    let detail = event.summary.as_deref().unwrap_or("");
    let label = label.unwrap_or("-");
    println!("[{kind}] {source} {name} session={session} project={label} {detail}");
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A journal in a per-test temp file, so nothing lands in the user's real log.
    fn test_journal(tag: &str) -> Journal {
        let dir = std::env::temp_dir().join(format!("codeisland-decision-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Journal::open(&dir.join("events.jsonl")).expect("journal opens")
    }

    #[test]
    fn a_plain_card_answers_with_the_bare_behavior() {
        // The regression the probe caught on 2026-10-14: routing questions through
        // the same handler must not turn an ordinary approve into "not an answer".
        let shared = Arc::new(Shared::new(test_journal("t1")));
        {
            let mut pending = shared.pending.lock().unwrap();
            let _rx = pending.park("p1", 0, "s1", Vec::new(), None);
        }
        let decision = decision_for(&shared, "p1", Behavior::Allow, Vec::new());
        assert_eq!(
            decision,
            Some(Decision::plain(Behavior::Allow)),
            "a permission card is answered by the click alone"
        );
    }

    #[test]
    fn a_question_card_needs_every_answer() {
        use codeisland_core::question::{Question, QuestionOption};
        let shared = Arc::new(Shared::new(test_journal("t1")));
        let questions = vec![Question {
            header: "Color".into(),
            question: "Red or blue?".into(),
            options: vec![
                QuestionOption { label: "Red".into(), description: String::new() },
                QuestionOption { label: "Blue".into(), description: String::new() },
            ],
        }];
        let input = serde_json::json!({"questions": [{"question": "Red or blue?",
            "options": [{"label": "Red"}, {"label": "Blue"}]}]});
        {
            let mut pending = shared.pending.lock().unwrap();
            let _rx = pending.park("q1", 0, "s1", questions, Some(input));
        }

        assert_eq!(
            decision_for(&shared, "q1", Behavior::Allow, Vec::new()),
            None,
            "no selection is not an answer"
        );

        let picked = vec![AnswerChoice { question: "Red or blue?".into(), label: "Blue".into() }];
        let decision = decision_for(&shared, "q1", Behavior::Allow, picked).expect("a full answer");
        assert_eq!(decision.behavior, Behavior::Allow);
        let input = decision.updated_input.expect("a question answer rewrites the input");
        assert_eq!(input["answers"]["Red or blue?"], serde_json::json!("Blue"));
        assert_eq!(
            input["questions"][0]["question"],
            serde_json::json!("Red or blue?"),
            "the original questions must survive untouched"
        );
        assert_eq!(input["questions"][0]["options"].as_array().unwrap().len(), 2);
        assert_eq!(input["annotations"], serde_json::json!({}));
    }

    #[test]
    fn owner_only_permissions_are_applied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("codeisland-perm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        std::fs::write(&path, b"").unwrap();
        set_owner_only(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn binding_clears_a_stale_socket_file() {
        let dir = std::env::temp_dir().join(format!("codeisland-bind-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        std::fs::write(&path, b"stale").unwrap();
        let listener = bind(&path).expect("bind over a stale file must succeed");
        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hook frame must end at its newline, not at a half-close.
    ///
    /// This is the load-bearing detail behind releasing a card when its agent
    /// dies: the daemon reads EOF on a parked hook as "the agent exited", which is
    /// only meaningful while the hook leaves its write half open. Were the daemon
    /// still reading to EOF, the acknowledgement below would never arrive and the
    /// read would hit its deadline instead of returning `{}`.
    #[test]
    fn a_frame_is_completed_by_its_newline_not_by_a_half_close() {
        let dir = std::env::temp_dir().join(format!("codeisland-frame-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sock");
        let listener = bind(&path).unwrap();
        let shared = Arc::new(Shared::new(Journal::open(&dir.join("journal.jsonl")).unwrap()));

        let server_shared = Arc::clone(&shared);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_hook(stream, &server_shared)
        });

        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"hook_event_name\":\"SessionStart\",\"session_id\":\"F1\"}\n")
            .unwrap();
        client.flush().unwrap();
        // No shutdown(Write) here on purpose.
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        let mut ack = String::new();
        let read = client.read_to_string(&mut ack);
        assert!(read.is_ok(), "the daemon never acknowledged a newline-terminated frame");
        assert_eq!(ack, "{}", "a fire-and-forget event must be acked with {{}}");
        assert_eq!(
            shared.registry.lock().unwrap().len(),
            1,
            "the frame must have been parsed, not just buffered"
        );

        server.join().unwrap().unwrap();
        drop(client);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A hook that connects and leaves without sending must not create anything.
    #[test]
    fn a_connection_that_sends_nothing_is_dropped_quietly() {
        let dir = std::env::temp_dir().join(format!("codeisland-empty-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sock");
        let listener = bind(&path).unwrap();
        let shared = Arc::new(Shared::new(Journal::open(&dir.join("journal.jsonl")).unwrap()));

        let server_shared = Arc::clone(&shared);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_hook(stream, &server_shared)
        });

        drop(UnixStream::connect(&path).unwrap());

        let outcome = server.join().unwrap();
        assert!(outcome.is_ok(), "an empty connection must not be an error: {outcome:?}");
        assert_eq!(shared.registry.lock().unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
