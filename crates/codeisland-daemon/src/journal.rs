//! Append-only event journal.
//!
//! Phase 0 has no user interface, so this file is the only way to observe that
//! the pipeline works. It stays useful afterwards: it is the record that lets a
//! session be reconstructed after a daemon restart, and the thing to read when a
//! card shows something surprising.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use codeisland_core::protocol::HookEvent;

/// Where the journal lives unless overridden.
pub fn default_journal_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("CODEISLAND_JOURNAL") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    base.join("codeisland/events.jsonl")
}

/// One-line-per-event writer.
pub struct Journal {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl Journal {
    /// Open the journal, creating parent directories as needed.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: BufWriter::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one event.
    ///
    /// Flushed immediately: a daemon killed without warning must not lose the
    /// last events, and the volume here is a handful of lines per minute.
    pub fn record(&mut self, event: &HookEvent) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.writer, event)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeisland_core::protocol::HookEvent;

    #[test]
    fn records_one_json_object_per_line() {
        let dir =
            std::env::temp_dir().join(format!("codeisland-journal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let _ = std::fs::remove_file(&path);

        {
            let mut journal = Journal::open(&path).unwrap();
            for id in ["s1", "s2"] {
                let event = HookEvent {
                    session_id: Some(id.to_string()),
                    event: Some("PreToolUse".into()),
                    ..Default::default()
                };
                journal.record(&event).unwrap();
            }
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected two records, got {contents:?}");
        for line in lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("session_id").is_some());
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
