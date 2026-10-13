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
    writers: Vec<(u64, Arc<Mutex<UnixStream>>)>,
    next_id: u64,
}

impl ViewClients {
    /// Register a client and return its id.
    pub fn add(&mut self, stream: UnixStream) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.writers.push((id, Arc::new(Mutex::new(stream))));
        id
    }

    pub fn remove(&mut self, id: u64) {
        self.writers.retain(|(wid, _)| *wid != id);
    }

    pub fn count(&self) -> usize {
        self.writers.len()
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
        clients.add(a);

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
        clients.add(a);
        assert_eq!(clients.count(), 1);

        // Reader gone: the first broadcast should fail and evict the entry rather
        // than wedge the event thread.
        drop(b);
        clients.broadcast(&ViewUpdate::Snapshot { sessions: vec![] });
        clients.broadcast(&ViewUpdate::Snapshot { sessions: vec![] });
        assert_eq!(clients.count(), 0, "dead client was not evicted");
    }
}
