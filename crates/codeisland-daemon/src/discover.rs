//! Finding sessions that were already running when the island started.
//!
//! Hooks are the authority on a session, but they only ever tell us about a session that
//! fires one: anything started before the daemon, or in a project whose hooks were never
//! installed, is invisible. Upstream solves this the same way (its `ConfigDirs.swift` says
//! it plainly -- "Discovery maps a *running process* to its transcript"), so this module
//! does the same thing on Linux terms:
//!
//!   1. enumerate live agent processes through /proc, identifying them by their command
//!      line rather than by `comm`
//!   2. read each process's own cwd, and find the transcript under that harness's archive
//!      root whose recorded `cwd` matches
//!   3. report it as a *discovered* session, whose status can only ever be active or quiet
//!
//! Two rules this module exists to enforce, both learned the hard way today:
//!
//! - **`comm` is not an identity.** Matching on `comm.starts_with("pi")` reports pipewire as
//!   a Pi session, and this very session runs inside `pi-web` whose `comm` is
//!   `node-MainThread`. Identification looks at argv paths instead.
//! - **Transcripts are not small.** The live one right now is 11.9 MB. Nothing here reads a
//!   whole file: the head is read for `cwd` (bounded bytes) and the tail only for
//!   "is it still moving", and file mtimes do the filtering before any read happens.
//!
//! And the rule that shapes what the UI may claim: a discovered session is a *statement
//! about a file's mtime*, not a statement about what the agent is waiting for. Discovery can
//! never produce `WaitingApproval` or `WaitingInput` -- only a hook can park a request -- so
//! those states are unreachable from here by construction, not by convention.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Which harness a discovered session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Harness {
    Claude,
    Codex,
    Pi,
}

impl Harness {
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
        }
    }

    /// Identify a process from its argv. `comm` is deliberately not consulted -- see the
    /// module note about pipewire matching "pi".
    fn from_argv(argv: &[String]) -> Option<Harness> {
        let first = argv.first()?;
        let launcher = Path::new(first)
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        // An interpreter means the *next* argument is the script it was told to run; anything
        // else means argv[0] is the program itself. Nothing after that is ever consulted, and
        // nothing is matched with a substring search over the command line: a shell running
        // `-c "...pi-coding-agent..."` is a shell, not a Pi session. Both of those rules were
        // learned by watching this code classify its own build command as an agent.
        match launcher.as_str() {
            "claude" => return Some(Harness::Claude),
            "codex" => return Some(Harness::Codex),
            "pi" => return Some(Harness::Pi),
            "node" | "bun" | "deno" => {}
            _ => return None,
        }
        let script = argv.get(1).map(|word| word.to_lowercase()).unwrap_or_default();
        if script.contains("pi-coding-agent") || script.ends_with("pi/dist/bundle/cli.js") {
            return Some(Harness::Pi);
        }
        if script.contains("@anthropic-ai/claude-code") || script.ends_with("claude-code/cli.js") {
            return Some(Harness::Claude);
        }
        if script.contains("@openai/codex") || script.contains("codex-linux-") {
            return Some(Harness::Codex);
        }
        None
    }
}

/// One session found on disk, believed to belong to a live process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub harness: Harness,
    /// Derived from the file name, which is the harness's own session id for all three
    /// (claude: a uuid; pi: `<timestamp>_<uuid>` -- the same string the pi extension reports;
    /// codex: the trailing thread uuid). That is what lets a later hook event merge into the
    /// same row instead of opening a second one.
    pub session_id: String,
    pub cwd: String,
    pub transcript: PathBuf,
    /// Milliseconds since the epoch of the last write to the transcript.
    pub last_write_ms: u64,
    /// The process we matched it to, when one is running.
    pub pid: Option<u32>,
}

/// A process that looks like an agent, with the directory it is sitting in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcess {
    pub harness: Harness,
    pub pid: u32,
    pub cwd: PathBuf,
}


/// Read at most `limit` bytes from the start, and separately report the file size.
fn read_head(path: &Path, limit: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let take = len.min(limit);
    let mut buffer = vec![0u8; take as usize];
    file.read_exact(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

fn read_tail(path: &Path, limit: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let take = len.min(limit);
    file.seek(SeekFrom::End(-(take as i64))).ok()?;
    let mut buffer = vec![0u8; take as usize];
    file.read_exact(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// The `cwd` a transcript was written for, from its first lines.
///
/// Only the head is parsed, and only string fields named `cwd` -- transcripts are append-only
/// and their first records are the session metadata, which is where all three harnesses put
/// the working directory.
pub fn transcript_cwd(path: &Path) -> Option<String> {
    let head = read_head(path, 64 * 1024)?;
    for line in head.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        for candidate in [
            value.get("cwd").and_then(|v| v.as_str()),
            value
                .get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|v| v.as_str()),
            value
                .get("session_meta")
                .and_then(|m| m.get("cwd"))
                .and_then(|v| v.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Session id from a transcript file name.
///
/// All three harnesses put their own id in the name, which is what lets a discovered row and
/// a later hook event converge on one session instead of opening two:
///   claude  `<uuid>.jsonl`
///   pi      `<timestamp>_<uuid>.jsonl` -- the exact string the pi extension reports
///   codex   `rollout-<date>T<hh-mm-ss>-<uuid>.jsonl` -- the uuid is what hooks report
pub fn session_id_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
    let Some(rest) = stem.strip_prefix("rollout-") else {
        return Some(stem.to_string());
    };
    // A uuid is the last five dash-separated groups; everything before it is the timestamp.
    let groups: Vec<&str> = rest.split('-').collect();
    if groups.len() < 5 {
        return Some(rest.to_string());
    }
    Some(groups[groups.len() - 5..].join("-"))
}

/// Live agent processes, identified by argv and located by their cwd.
pub fn agent_processes() -> Vec<AgentProcess> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let argv: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        if argv.is_empty() {
            continue;
        }
        let Some(harness) = Harness::from_argv(&argv) else {
            continue;
        };
        let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        out.push(AgentProcess {
            harness,
            pid,
            cwd,
        });
    }
    out
}

/// Where each harness keeps its transcripts, honouring the env var the *daemon* was given.
///
/// Upstream reads the target process's own `CLAUDE_CONFIG_DIR` / `CODEX_HOME` instead of
/// this, which is more correct when several roots are in play. That needs `ptrace_read`
/// semantics on `/proc/<pid>/environ` and is a deliberate simplification here, recorded in
/// the docs rather than hidden: with one root, a session in a second root is simply not
/// discovered -- it is never misattributed.
fn archive_roots(home: &Path) -> BTreeMap<Harness, PathBuf> {
    let mut roots = BTreeMap::new();
    let claude = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".claude").join("projects"));
    let claude = if claude.is_dir() {
        claude
    } else {
        home.join(".claude").join("projects")
    };
    roots.insert(Harness::Claude, claude);
    roots.insert(
        Harness::Codex,
        std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".codex"))
            .join("sessions"),
    );
    roots.insert(Harness::Pi, home.join(".pi").join("agent").join("sessions"));
    roots
}

/// Newest-first transcript files under a root, bounded so a big archive costs a directory
/// walk and a handful of stats, never a read.
fn candidates(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            if !name.ends_with(".jsonl") {
                continue;
            };
            let Ok(meta) = path.metadata() else { continue };
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            files.push((modified, path));
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(limit);
    files.into_iter().map(|(_modified, path)| path).collect()
}

/// Match live agent processes to their transcripts.
///
/// `max_age_ms` bounds how old a transcript may be and still count as "this process's
/// session": a process whose newest archive is three weeks old is a different thing from a
/// session that is running now.
pub fn discover(
    now_ms: u64,
    max_age_ms: u64,
    probe_limit: usize,
    cwd_cache: &mut std::collections::HashMap<PathBuf, (u64, Option<String>)>,
) -> Vec<Discovered> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    discover_in(&home, agent_processes(), now_ms, max_age_ms, probe_limit, cwd_cache)
}

fn millis_of(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The testable half, and the only implementation: processes and home are passed in, so a
/// fixture directory drives exactly the code production runs.
pub fn discover_in(
    home: &Path,
    processes: Vec<AgentProcess>,
    now_ms: u64,
    max_age_ms: u64,
    probe_limit: usize,
    cwd_cache: &mut std::collections::HashMap<PathBuf, (u64, Option<String>)>,
) -> Vec<Discovered> {
    if processes.is_empty() {
        return Vec::new();
    }
    let roots = archive_roots(home);
    let mut out: Vec<Discovered> = Vec::new();
    let mut claimed: Vec<PathBuf> = Vec::new();

    for process in processes {
        let Some(root) = roots.get(&process.harness) else {
            continue;
        };
        for path in candidates(root, probe_limit) {
            if claimed.contains(&path) {
                continue;
            }
            let Ok(meta) = path.metadata() else {
                continue;
            };
            let last_write_ms = millis_of(&meta);
            if last_write_ms.saturating_add(max_age_ms) < now_ms {
                // Sorted newest first, so nothing further back is live either.
                break;
            }
            // The cwd of a transcript never changes once written, so cache on size: a file
            // nobody is rewriting is parsed once ever, not once per scan.
            let size = meta.len();
            let cwd = match cwd_cache.get(&path) {
                Some((cached_size, cached_cwd)) if *cached_size == size => cached_cwd.clone(),
                _ => {
                    let found = transcript_cwd(&path);
                    cwd_cache.insert(path.clone(), (size, found.clone()));
                    found
                }
            };
            let Some(cwd) = cwd else {
                continue;
            };
            if Path::new(&cwd) != process.cwd.as_path() {
                continue;
            }
            let Some(session_id) = session_id_of(&path) else {
                continue;
            };
            claimed.push(path.clone());
            out.push(Discovered {
                harness: process.harness,
                session_id,
                cwd,
                transcript: path,
                last_write_ms,
                pid: Some(process.pid),
            });
            break;
        }
    }
    out
}

/// Has this transcript moved recently? That is the whole of what discovery may claim about
/// "status": activity, not intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Written to inside the window.
    Active,
    /// Quiet for longer than the window.
    Quiet,
}

pub fn liveness(last_write_ms: u64, now_ms: u64, window_ms: u64) -> Liveness {
    if last_write_ms.saturating_add(window_ms) >= now_ms {
        Liveness::Active
    } else {
        Liveness::Quiet
    }
}

/// Read the tail looking for the last thing the harness recorded, used only for a one-line
/// detail on a discovered row. Bounded, and never trusted to say anything about waiting.
pub fn tail_hint(path: &Path) -> Option<String> {
    let tail = read_tail(path, 8 * 1024)?;
    for line in tail.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(text) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(str::to_string)
        {
            return Some(truncate(&text, 120));
        }
        if let Some(text) = value.get("text").and_then(|t| t.as_str()).map(str::to_string) {
            return Some(truncate(&text, 120));
        }
        if let Some(kind) = value.get("type").and_then(|t| t.as_str()) {
            return Some(kind.to_string());
        }
    }
    None
}

fn truncate(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if out.chars().count() >= limit {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    /// The production clock is injected by the caller so scans are testable; this is
    /// only the test fixture's own helper.
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn fixture(root: &Path, harness: Harness, cwd: &str, id: &str, body: &str) -> PathBuf {
        let dir = match harness {
            Harness::Claude => root.join(".claude/projects/-tmp-proj"),
            Harness::Codex => root.join(".codex/sessions"),
            Harness::Pi => root.join(".pi/agent/sessions/--tmp-proj--"),
        };
        fs::create_dir_all(&dir).unwrap();
        let name = match harness {
            Harness::Codex => format!("rollout-2026-10-14T17-19-44-{id}.jsonl"),
            _ => format!("{id}.jsonl"),
        };
        let path = dir.join(name);
        let first = serde_json::json!({ "cwd": cwd, "type": "user" }).to_string();
        fs::write(&path, format!("{first}\n{body}\n")).unwrap();
        path
    }

    #[test]
    fn argv_identity_ignores_comm_because_comm_lies() {
        // The bug this guards: `pipewire` starts with "pi", and a Pi session runs inside a
        // process whose comm is node-MainThread.
        assert_eq!(Harness::from_argv(&["/usr/bin/pipewire".into()]), None);
        assert_eq!(Harness::from_argv(&["pi-web".into(), "-p".into()]), None);
        assert_eq!(
            Harness::from_argv(&["/home/u/.local/share/bin/pi".into(), "-p".into()]),
            Some(Harness::Pi)
        );
        assert_eq!(
            Harness::from_argv(&["node".into(), "/x/pi-coding-agent/dist/bundle/cli.js".into()]),
            Some(Harness::Pi)
        );
        assert_eq!(
            Harness::from_argv(&["/usr/local/bin/claude".into()]),
            Some(Harness::Claude)
        );
        assert_eq!(Harness::from_argv(&["codex".into(), "app-server".into()]), Some(Harness::Codex));
        // The false positive this guards, measured today: a shell command that merely mentions
        // a claude path is not a running Claude session.
        assert_eq!(
            Harness::from_argv(&[
                "/bin/sh".into(),
                "-c".into(),
                "exec -a /home/u/.local/share/bin/claude sleep 25".into()
            ]),
            None
        );
        assert_eq!(Harness::from_argv(&["grep".into(), "-r".into(), "/claude".into()]), None);
    }

    #[test]
    fn a_process_is_matched_only_to_a_transcript_with_its_own_cwd() {
        let root = std::env::temp_dir().join(format!("ci-discover-{}", now_ms()));
        let here = fixture(&root, Harness::Pi, "/tmp/proj", "2026-10-14T11-18-41-330Z_abc", r#"{"type":"x"}"#);
        let other = fixture(&root, Harness::Pi, "/tmp/somewhere-else", "other-id", r#"{"type":"y"}"#);

        let processes = vec![AgentProcess {
            harness: Harness::Pi,
            pid: 4242,
            cwd: PathBuf::from("/tmp/proj"),
        }];
        let home = root.clone();
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&home, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 1, "exactly one session should match the cwd");
        assert_eq!(found[0].transcript, here);
        assert_ne!(found[0].transcript, other);
        assert_eq!(found[0].pid, Some(4242));
        assert_eq!(found[0].session_id, "2026-10-14T11-18-41-330Z_abc");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stale_transcripts_are_not_reported_as_live_sessions() {
        let root = std::env::temp_dir().join(format!("ci-discover-old-{}", now_ms()));
        fixture(&root, Harness::Claude, "/tmp/proj", "uuid-1", "{}");
        // No mtime surgery needed: asking "what is live as of an hour from now" makes an
        // hour-old file stale by definition, and keeps the test free of libc FFI.
        let an_hour_later = now_ms() + 3_600_000;

        let processes = vec![AgentProcess {
            harness: Harness::Claude,
            pid: 1,
            cwd: PathBuf::from("/tmp/proj"),
        }];
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, processes, an_hour_later, 60_000, 50, &mut cache);
        assert!(found.is_empty(), "an hour-old transcript is not a live session: {found:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_rollout_names_yield_the_thread_uuid_that_hooks_report() {
        let path = Path::new("rollout-2026-10-14T17-19-44-01a139b5-e5a5-77b0-a481-a86d0b4150b9.jsonl");
        assert_eq!(
            session_id_of(path).as_deref(),
            Some("01a139b5-e5a5-77b0-a481-a86d0b4150b9")
        );
    }

    #[test]
    fn liveness_only_ever_says_active_or_quiet() {
        assert_eq!(liveness(1_000, 2_000, 2_000), Liveness::Active);
        assert_eq!(liveness(1_000, 9_000, 2_000), Liveness::Quiet);
    }

    #[test]
    fn two_processes_sharing_a_directory_yield_two_rows_and_never_the_same_transcript_twice() {
        // What is actually guaranteed, and what is not. Two live processes in one directory
        // really do have two sessions, so two rows is correct -- the guarantee is that one
        // transcript is claimed by at most one process. What is NOT determined is *which*
        // process owns which transcript: newest-first is a convention, so on a shared cwd the
        // pid pairing is arbitrary. Recorded here because it is a real limit of discovery, and
        // it is harmless only because discovered rows carry no jump and no buttons.
        let root = std::env::temp_dir().join(format!("ci-discover-two-{}", now_ms()));
        fixture(&root, Harness::Pi, "/tmp/proj", "one", "{}");
        fixture(&root, Harness::Pi, "/tmp/proj", "two", "{}");
        let processes = vec![
            AgentProcess {
                harness: Harness::Pi,
                pid: 1,
                cwd: PathBuf::from("/tmp/proj"),
            },
            AgentProcess {
                harness: Harness::Pi,
                pid: 2,
                cwd: PathBuf::from("/tmp/proj"),
            },
        ];
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 2, "two sessions in one directory are two rows: {found:?}");
        let transcripts: Vec<PathBuf> = found.iter().map(|d| d.transcript.clone()).collect();
        assert_ne!(transcripts[0], transcripts[1], "one transcript claimed twice");
        let ids: Vec<&str> = found.iter().map(|d| d.session_id.as_str()).collect();
        assert!(ids.contains(&"one") && ids.contains(&"two"), "both ids present: {ids:?}");

        // A single process in that same directory takes exactly one row -- the newest.
        let mut cache2 = std::collections::HashMap::new();
        let alone = discover_in(
            &root,
            vec![AgentProcess {
                harness: Harness::Pi,
                pid: 7,
                cwd: PathBuf::from("/tmp/proj"),
            }],
            now_ms(),
            600_000,
            50,
            &mut cache2,
        );
        assert_eq!(alone.len(), 1, "one process must not invent two sessions: {alone:?}");
        fs::remove_dir_all(&root).ok();
    }
}
