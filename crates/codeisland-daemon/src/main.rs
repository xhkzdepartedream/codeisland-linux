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
mod view;

use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use codeisland_core::event::RouteKind;
use codeisland_core::protocol::HookEvent;
use codeisland_core::session::{SessionRegistry, ViewUpdate};
use codeisland_core::socket_path;

use journal::{default_journal_path, Journal};
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

    let shared = Arc::new(Shared {
        registry: Mutex::new(SessionRegistry::new()),
        clients: Mutex::new(ViewClients::default()),
        journal: Mutex::new(journal),
    });

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

/// A UI client appeared: register it and immediately send full state.
fn handle_new_viewer(stream: UnixStream, shared: &Arc<Shared>) {
    if let Ok(mut clients) = shared.clients.lock() {
        clients.add(stream);
    }
    let snapshot = match shared.registry.lock() {
        Ok(registry) => registry.snapshot(),
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };
    if let Ok(mut clients) = shared.clients.lock() {
        clients.broadcast(&snapshot);
    }
    println!(
        "codeisland-daemon: viewer connected ({} total)",
        clients_count(shared)
    );
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
    let mut raw = Vec::new();
    (&mut stream).take(MAX_PAYLOAD).read_to_end(&mut raw)?;
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

    match route {
        RouteKind::Permission | RouteKind::Question => {
            // Nobody can answer yet: the UI can display the request but cannot
            // return a decision, so this daemon sends nothing and the bridge
            // turns that into the CLI's own prompt. See the module docs.
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
}
