//! Live view fan-out: the socket the island UI connects to.
//!
//! Kept separate from the hook socket on purpose. The hook socket is written by
//! arbitrary agent hook processes and must be reachable from an SSH remote host,
//! so its permissions and lifetime are dictated by the bridge. This one only ever
//! has local UI clients reading from it. Mixing them would mean one listener
//! serving two different contracts.
//!
//! Protocol: newline-delimited JSON `ViewUpdate` values, pushed by the daemon.
//! A client receives a full `snapshot` on connect, then `session_upsert` and
//! `session_remove` deltas. Last write wins, so a client never replays history or
//! reconciles a diff.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use codeisland_core::session::ViewUpdate;

/// Path override for the view socket, mirroring the hook socket's env var.
pub const VIEW_SOCKET_ENV: &str = "CODEISLAND_VIEW_SOCKET";

/// Where the UI connects.
///
/// `XDG_RUNTIME_DIR` is preferred over `/tmp` here: this socket is long-lived and
/// owned by the desktop session, and the runtime directory is cleaned up on
/// logout, so a crashed daemon cannot leave debris behind.
pub fn view_socket_path() -> PathBuf {
    if let Ok(explicit) = std::env::var(VIEW_SOCKET_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("{base}/codeisland-view-{uid}.sock"))
}

/// Connected UI clients.
#[derive(Default)]
pub struct ViewClients {
    /// Write half, shared with the broadcaster.
    writers: Vec<(u64, Arc<Mutex<UnixStream>>)>,
    /// Read half, handed to the per-client reader thread. Kept as a factory
    /// rather than a moved handle so `reader()` can be asked once per client.
    readers: Vec<(u64, UnixStream)>,
    next_id: u64,
}

impl ViewClients {
    /// Register a client. Returns `u64::MAX` when no write handle was supplied.
    pub fn add(&mut self, stream: Option<UnixStream>) -> u64 {
        let Some(stream) = stream else {
            return u64::MAX;
        };
        let id = self.next_id;
        self.next_id += 1;
        let reader = stream.try_clone().ok();
        self.writers.push((id, Arc::new(Mutex::new(stream))));
        if let Some(reader) = reader {
            self.readers.push((id, reader));
        }
        id
    }

    /// Take the read half for a client, once.
    ///
    /// Handed over rather than cloned so exactly one thread can own it.
    pub fn reader(&mut self, id: u64) -> Option<UnixStream> {
        let position = self.readers.iter().position(|(wid, _)| *wid == id)?;
        Some(self.readers.remove(position).1)
    }

    pub fn remove(&mut self, id: u64) {
        self.writers.retain(|(wid, _)| *wid != id);
        self.readers.retain(|(wid, _)| *wid != id);
    }

    pub fn count(&self) -> usize {
        self.writers.len()
    }

    /// Pick a client to show a card to.
    ///
    /// With one island this is the only client. With several, the first is chosen
    /// deliberately: a permission request must be shown to exactly one of them,
    /// because two answers for one parked hook is a decision race.
    pub fn any_viewer(&self) -> Option<u64> {
        self.writers.first().map(|(id, _)| *id)
    }

    /// Send one update to every client, dropping those that have gone away.
    ///
    /// Writes are serialised per client, so two event threads cannot interleave
    /// half a JSON line and corrupt the stream.
    pub fn broadcast(&mut self, update: &ViewUpdate) {
        let Ok(line) = serde_json::to_vec(update) else {
            return;
        };
        let mut dead = Vec::new();
        for (id, writer) in &self.writers {
            let mut guard = match writer.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut slice: &[u8] = &line;
            let mut failed = false;
            while !slice.is_empty() {
                match guard.write(slice) {
                    Ok(0) => {
                        failed = true;
                        break;
                    }
                    Ok(n) => slice = &slice[n..],
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            if failed || guard.write_all(b"\n").is_err() || guard.flush().is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            self.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeisland_core::session::ViewUpdate;

    #[test]
    fn env_override_wins_for_the_view_socket() {
        std::env::set_var(VIEW_SOCKET_ENV, "/tmp/view-override.sock");
        assert_eq!(view_socket_path(), PathBuf::from("/tmp/view-override.sock"));
        std::env::remove_var(VIEW_SOCKET_ENV);
    }

    #[test]
    fn broadcast_reaches_a_connected_client_whole() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut clients = ViewClients::default();
        clients.add(Some(a));

        let update = ViewUpdate::Snapshot { sessions: vec![] };
        clients.broadcast(&update);

        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(&b).read_line(&mut line).unwrap();
        assert!(line.ends_with('\n'), "framing lost: {line:?}");
        assert!(line.contains("\"snapshot\""), "bad payload: {line}");
        // The remainder must parse: a torn write would fail here.
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["type"], "snapshot");
    }

    #[test]
    fn a_dead_client_is_dropped_not_blocked() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut clients = ViewClients::default();
        clients.add(Some(a));
        assert_eq!(clients.count(), 1);

        // Reader gone: the first broadcast should fail and evict the entry rather
        // than wedge the event thread.
        drop(b);
        clients.broadcast(&ViewUpdate::Snapshot { sessions: vec![] });
        clients.broadcast(&ViewUpdate::Snapshot { sessions: vec![] });
        assert_eq!(clients.count(), 0, "dead client was not evicted");
    }
}
