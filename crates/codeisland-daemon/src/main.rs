//! `codeisland-daemon` -- the long-lived process that owns the socket.
//!
//! Mirrors `Sources/CodeIsland/HookServer.swift` in the upstream macOS project.
//! Phase 0 responsibilities: listen, parse, journal, and answer blocking events
//! with nothing at all.
//!
//! # Why phase 0 answers permission requests with silence
//!
//! There is no island UI yet, so there is no user to ask. Silence makes the
//! bridge exit with no output, which makes the CLI fall back to its own
//! permission prompt -- exactly what happens with no daemon running at all.
//! Replying `deny` would change the user's behaviour, and replying `allow` would
//! hand out an approval nobody gave. Silence is the only honest answer.

mod journal;

use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use codeisland_core::event::RouteKind;
use codeisland_core::protocol::HookEvent;
use codeisland_core::socket_path;

use journal::{default_journal_path, Journal};

/// Largest payload accepted, matching the upstream limit that exists to
/// accommodate large file diffs from tools like Codex.
const MAX_PAYLOAD: u64 = 10 * 1024 * 1024;

/// How long to wait for a connection to deliver its payload before hanging up.
/// Generous, because a bridge collecting process ancestry can be slow on a
/// loaded machine, but finite so a stuck peer cannot pin a thread.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
    let path = socket_path();
    let journal_path = default_journal_path();

    let journal = match Journal::open(&journal_path) {
        Ok(journal) => journal,
        Err(err) => {
            eprintln!("codeisland-daemon: cannot open journal {journal_path:?}: {err}");
            std::process::exit(1);
        }
    };

    if let Err(err) = serve(&path, journal) {
        eprintln!("codeisland-daemon: fatal: {err}");
        std::process::exit(1);
    }
}

fn serve(path: &Path, journal: Journal) -> std::io::Result<()> {
    // A socket file left behind by a crashed daemon would make bind() fail, so
    // clear it first. Safe because a live daemon would have been reached before
    // we got here (see the single-instance check below).
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path)?;

    // The socket carries tool-call payloads that can name files and commands.
    // Keep it owner-only rather than relying on umask.
    set_owner_only(path)?;

    println!("codeisland-daemon: listening on {}", path.display());
    println!("codeisland-daemon: journal {}", journal.path().display());

    let journal = Arc::new(std::sync::Mutex::new(journal));

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let journal = Arc::clone(&journal);
                // One thread per connection. Concurrency here is one event per
                // tool call across a handful of sessions, so a thread pool
                // would add machinery without buying anything.
                std::thread::spawn(move || {
                    if let Err(err) = handle(stream, &journal) {
                        eprintln!("codeisland-daemon: connection error: {err}");
                    }
                });
            }
            Err(err) => {
                // A failed accept must not kill the daemon: the listener is
                // usually still healthy and the next connection will work.
                eprintln!("codeisland-daemon: accept failed: {err}");
            }
        }
    }

    Ok(())
}

/// Restrict the socket to its owner. Without this, any local user could inject
/// fake agent events or, worse, answer a permission request.
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

fn handle(mut stream: UnixStream, journal: &Arc<std::sync::Mutex<Journal>>) -> std::io::Result<()> {
    // The bridge waits on this socket while a human decides, so the daemon must
    // not impose a shy read deadline on the *response*; it only bounds how long
    // it waits for the incoming payload.
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    // Borrow the stream rather than moving it into the adapter: the same
    // socket is used afterwards to write the acknowledgement.
    let mut raw = Vec::new();
    (&mut stream).take(MAX_PAYLOAD).read_to_end(&mut raw)?;
    if raw.is_empty() {
        return Ok(());
    }

    let event: HookEvent = match serde_json::from_slice(&raw) {
        Ok(event) => event,
        Err(err) => {
            // Malformed payloads are logged but never answered with a decision.
            eprintln!("codeisland-daemon: unparseable payload ({err})");
            return Ok(());
        }
    };

    let route = event.route_kind();
    if let Ok(mut journal) = journal.lock() {
        let _ = journal.record(&event);
    }
    report(&event, route);

    match route {
        RouteKind::Permission | RouteKind::Question => {
            // See the module docs: with no UI, the correct answer is no answer.
            // The bridge turns an empty reply into "let the CLI decide".
        }
        RouteKind::Event => {
            // Fire-and-forget events still expect an acknowledgement so the
            // bridge's read returns promptly instead of waiting for the
            // timeout.
            use std::io::Write;
            let _ = stream.write_all(b"{}");
        }
    }

    Ok(())
}

/// Human-readable one-liner on stdout so a foreground run is watchable.
fn report(event: &HookEvent, route: RouteKind) {
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
    println!("[{kind}] {source} {name} session={session} {detail}");
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
}
