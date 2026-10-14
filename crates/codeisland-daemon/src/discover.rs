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
    /// The checked-out git branch in `cwd`, when it could be read from disk. Kept on the row
    /// because a session's branch is the second-most useful thing about it after the project,
    /// and because a card that shows it costs one small file read per directory.
    pub branch: Option<String>,
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
///   codex   `rollout-<timestamp>-<thread-uuid>[_<second-uuid>].jsonl`
///
/// Codex needs care. Its name has two shapes and, in the second, two uuids separated by `_`,
/// so splitting on `-` and taking the last five groups produced garbage -- it merged the tail
/// of one uuid into another (`...a86d0b4150b9_01a13a62-...`). The timestamp prefix is fixed
/// width (`YYYY-MM-DDThh-mm-ss`), and the transcript's own `session_meta` record names the
/// first uuid as `session_id`, so: cut the prefix, then take everything before the first `_`.
pub fn session_id_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
    const PREFIX: &str = "rollout-";
    let Some(rest) = stem.strip_prefix(PREFIX) else {
        return Some(stem.to_string());
    };
    // Two shapes: `rollout-<stamp>-<uuid>` and `rollout-<stamp>-<uuid>_<other-uuid>`. Drop the
    // `_` suffix first -- otherwise its dashed tail merges into the uuid -- and then the uuid
    // is simply the last five dash groups, whatever the stamp looked like.
    let head = rest.split('_').next().unwrap_or(rest);
    let groups: Vec<&str> = head.split('-').collect();
    if groups.len() < 5 {
        return Some(head.to_string());
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

/// Match live agent processes to their transcripts, and pick up sessions whose process is not
/// recognisable at all.
///
/// The old version required `process.cwd == transcript_cwd` and nothing else. That key is
/// simply wrong for real harnesses, and this machine proved it: pi\'s process sits in `$HOME`
/// while its transcript records the project directory, because pi chdirs to the home directory
/// at start-up. Matching on cwd equal to the transcript left pi sessions permanently
/// undiscovered -- including the one being typed into at the time.
///
/// Two facts replace it:
///
/// 1. **A harness owns its newest transcripts.** A harness's transcript directory *is* that
///    harness, so the newest few files in it are that harness's live sessions. How many
///    "few" is the number of live processes of that harness plus one, so several sessions
///    running in parallel are all covered without walking a whole archive of years.
/// 2. **A live process confirms a session, and a fresh writer needs no confirmation.** When a
///    live process of the harness can be tied to a transcript (the same directory, or a shell
///    the agent is running a command in) we carry its pid, which is what terminal jump needs.
///    When none can, a transcript written seconds ago is still unmistakably live -- that is
///    the pi case above, and dropping it is exactly the complaint this code exists to fix.
///
/// `max_age_ms` is therefore the window that decides "still there": it bounds both which
/// transcripts are considered and how recently a process-less one must have been written.
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
    let roots = archive_roots(home);
    let mut out: Vec<Discovered> = Vec::new();
    let mut claimed: Vec<PathBuf> = Vec::new();

    for (harness, root) in &roots {
        let live = processes
            .iter()
            .filter(|process| process.harness == *harness)
            .count();
        // No process of this harness is running, so nothing here is live. Discovery is for
        // sessions a hook cannot see; a session with no process and no hook is simply over.
        if live == 0 {
            continue;
        }
        // The newest `probe_limit` transcripts. `probe_limit` bounds a directory walk to a few
        // stats; how many rows come out is bounded separately, by evidence (an open descriptor
        // or a seated process) and by the per-harness live-process count. It is deliberately
        // not `live`, because one daemon can hold several transcripts -- the codex app-server
        // holds every open thread -- and capping the walk at the process count would hide all
        // but one of them.
        let take = probe_limit;

        // Parse once: the cwd and id are needed by both passes, and the file walk is the
        // expensive part.
        struct Entry {
            path: PathBuf,
            cwd: String,
            session_id: String,
            last_write_ms: u64,
        }
        let mut entries: Vec<Entry> = Vec::new();
        for path in candidates(root, take) {
            if claimed.contains(&path) {
                continue;
            }
            let Ok(meta) = path.metadata() else {
                continue;
            };
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
            let (Some(cwd), Some(session_id)) = (cwd, session_id_of(&path)) else {
                continue;
            };
            entries.push(Entry {
                path,
                cwd,
                session_id,
                last_write_ms: millis_of(&meta),
            });
        }

        // Pass one: bind transcripts to live processes by evidence, strongest first.
        //
        //  1. A file the process holds open that names the session: the transcript itself, or
        //     codex's `<uuid>.lock`. This is the harness saying "this conversation is mine,
        //     right now", and it is the only signal that survives a shared daemon -- the codex
        //     app-server holds the rollouts and locks of *every* thread it serves, so one pid
        //     can legitimately own several sessions. Measured here: pid 337767 held two
        //     rollouts and two writer locks at once.
        //  2. A process sitting in the transcript's directory (or a shell spawned there). This
        //     one is one-to-one: a process is in one directory.
        //
        // Taken before the freshness pass, so a newer unclaimed transcript cannot push a
        // proven one out and steal its pid.
        let mut rows: Vec<(usize, u32)> = Vec::new();
        let mut held: Vec<(String, PathBuf, u32)> = Vec::new();
        for process in &processes {
            if process.harness == *harness {
                for (id, path) in held_session_files(process.pid) {
                    held.push((id, realpath(&path), process.pid));
                }
            }
        }
        let mut cwd_used: Vec<u32> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            // Identity by session id, not by path: the same session may be held as a transcript
            // by one process and as a `.lock` by another, and both mean the same session.
            let wanted = realpath(&entry.path);
            let mut owners: Vec<u32> = held
                .iter()
                .filter(|(id, path, _)| {
                    *id == entry.session_id || *path == wanted
                })
                .map(|(_, _, pid)| *pid)
                .collect();
            owners.sort_unstable();
            owners.dedup();
            if let Some(pid) = owners.first() {
                rows.push((index, *pid));
                continue;
            }
            // The cwd rule is the weakest of the three, because a process sitting in a directory
            // proves far less than an open file does: every codex process on this machine sits in
            // $HOME, and a long-finished thread whose transcript recorded $HOME matches on
            // directory alone -- measured here, a 4-hour-old thread was bound this way and shown
            // as running. So a cwd match is trusted without regard to age only for the harness's
            // newest transcript (a parked agent whose transcript nothing has superseded -- the
            // realistic long-quiet case); an older one must be recent to count.
            let is_newest = index == 0;
            if !is_newest && entry.last_write_ms.saturating_add(max_age_ms) < now_ms {
                continue;
            }
            if let Some(pid) = processes
                .iter()
                .find(|process| {
                    process.harness == *harness
                        && !cwd_used.contains(&process.pid)
                        && process_owns_cwd(process.pid, &process.cwd, &entry.cwd)
                })
                .map(|process| process.pid)
            {
                cwd_used.push(pid);
                rows.push((index, pid));
            }
        }

        // Pass two: a running agent owns the freshest archives of its own harness even when no
        // process here admits to their directories. Pi does exactly this on this machine -- it
        // chdirs to $HOME, so its process cwd never equals its transcript cwd, and before this
        // pass a pi session being typed into was invisible. Fill up to one row per live process,
        // and only with a transcript recent enough that no process is needed to believe it: an
        // agent that has just started has not written yet, and without the age bound this would
        // resurrect that harness's previous session instead.
        for (index, entry) in entries.iter().enumerate() {
            if rows.len() >= live {
                break;
            }
            if rows.iter().any(|(taken, _)| *taken == index) {
                continue;
            }
            if entry.last_write_ms.saturating_add(max_age_ms) < now_ms {
                continue;
            }
            rows.push((index, 0));
        }

        for (index, pid) in rows {
            let entry = &entries[index];
            claimed.push(entry.path.clone());
            out.push(Discovered {
                harness: *harness,
                session_id: entry.session_id.clone(),
                cwd: entry.cwd.clone(),
                transcript: entry.path.clone(),
                branch: codeisland_core::git::head_branch(std::path::Path::new(&entry.cwd)),
                last_write_ms: entry.last_write_ms,
                pid: (pid != 0).then_some(pid),
            });
        }
    }

    // Fold subagents into the session they belong to.
    //
    // A subagent's transcript is a step inside a session, not a second session the human
    // opened. Measured on this machine: codex's "Guardian review" thread carries
    // `parent_thread_id` pointing at the conversation it reviews, and pi nests its subagent
    // runs under the parent's own session directory. Counting them as peers overstated how
    // many things were running -- the owner of this project saw "3 sessions" for one codex
    // conversation plus this pi one, and asked why.
    //
    // Only folded when the parent is *also* being shown. A child whose parent has gone stays
    // visible: an orphan subagent still doing work is real activity, and silently dropping it
    // would make the island quieter than the machine. The one exception is a transcript that
    // announces itself as a subagent without naming a parent -- there the safe reading is that
    // it is a step, so it is folded rather than shown as a peer.
    //
    // (Upstream defaults to a per-subagent setting, `pluginSessionMode = "separate"`, and
    // folds only completed children. This island folds unconditionally-with-a-present-parent
    // instead: a compact pill that lists a reviewer as a peer of the chat it reviews reads as
    // "two agents are running" when one is. Recorded as a deliberate divergence.)
    let shown: Vec<String> = out.iter().map(|d| d.session_id.clone()).collect();
    out.retain(|discovered| {
        match child_parent(&discovered.transcript) {
            Some(parent) if !parent.is_empty() => !shown.contains(&parent),
            Some(_) => false,
            None => true,
        }
    });

    // Newest first, so the island lists the sessions actually doing something on top.
    out.sort_by(|a, b| b.last_write_ms.cmp(&a.last_write_ms));

    // One session id, one row.
    //
    // A resumed codex thread has two rollout files carrying the *same* thread uuid (measured:
    // `rollout-…-<uuid>.jsonl` and `rollout-…-<uuid>_<second-uuid>.jsonl`). Both pair with the
    // thread's lock, so both reach this list, and the daemon keys sessions by id -- so the last
    // one registered wins. Registering in newest-first order then meant the *older* file won,
    // and the row showed that stale transcript (an `event_msg` line) instead of the live one.
    // Deduping here, after the sort, keeps the newest by construction.
    let mut seen: Vec<String> = Vec::new();
    out.retain(|discovered| {
        if seen.contains(&discovered.session_id) {
            false
        } else {
            seen.push(discovered.session_id.clone());
            true
        }
    });

    out
}

/// Files a process currently holds open that name a session, as `(session_id, path)`.
///
/// Two shapes, both measured on this machine:
///
/// - A transcript held open directly. The codex app-server keeps the rollouts of the threads
///   it serves open, and an agent mid-append may hold its own.
/// - A lock file whose *name* is the session id. Codex writes
///   `~/.codex/thread-writer-locks/<uuid>.lock` while a thread is live and removes it after,
///   which is a far better answer than "its mtime is recent" -- it is the harness saying which
///   threads exist right now. Matching on the file name keeps this independent of codex's
///   private directory layout: any file named `<id>.lock` is a claim about `<id>`.
///
/// A read error is treated as "holds nothing": the process may have exited between listing
/// and reading, and a failed scan must never manufacture a live session.
fn held_session_files(pid: u32) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return out;
    };
    for fd in fds.flatten() {
        let Ok(target) = fs::read_link(fd.path()) else {
            continue;
        };
        let Some(name) = target.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if target.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(id) = session_id_of(&target) {
                out.push((id, target));
            }
        } else if let Some(id) = name.strip_suffix(".lock") {
            // Only uuid-shaped locks: codex keeps unrelated locks (`state`, `.lock`) in the
            // same tree, and matching those would invent sessions that do not exist.
            if id.len() == 36 && id.split('-').count() == 5 {
                out.push((id.to_string(), target));
            }
        }
    }
    out
}

/// Is this process sitting in `cwd`, or is the agent running a command there?
///
/// The second half matters for harnesses that run Bash tool calls: the shell they spawn
/// inherits the session directory, while the agent's own cwd may be anywhere. Walking the
/// parent chain a short way finds that shell. Bounded, and only ever used to attach a pid --
/// never to decide whether a session exists.
fn process_owns_cwd(pid: u32, own_cwd: &Path, cwd: &str) -> bool {
    let target = realpath(Path::new(cwd));
    if realpath(own_cwd) == target {
        return true;
    }
    let mut current = pid;
    for _ in 0..6 {
        let Some(parent) = parent_of(current) else {
            return false;
        };
        let Ok(parent_cwd) = fs::read_link(format!("/proc/{parent}/cwd")) else {
            return false;
        };
        if realpath(&parent_cwd) == target {
            return true;
        }
        current = parent;
    }
    false
}

/// Parent pid from `/proc/<pid>/stat`, whose fields are shifted by the comm column.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // After the closing paren the next field is the state, so the parent is the second one.
    fields.get(1)?.parse::<u32>().ok()
}

fn realpath(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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

/// The session a transcript declares itself a *child* of, if any.
///
/// A subagent's transcript is not a session the human opened -- it is a step inside one. The
/// measured shapes:
///
/// - **codex**: a top-level `payload.parent_thread_id` on the `session_meta` row. Guardian
///   (auto-review) threads carry exactly this, and its own `source.subagent` describes the
///   role; upstream parses the same field (`codexSubagentMetadata`).
/// - **pi**: a nested run directory, `<parent-session>/<child-uuid>/run-<n>/session.jsonl`,
///   with `session_info.name` like `subagent-reviewer-…`. The parent id is the enclosing
///   session directory's name.
///
/// Deliberately returns the parent id rather than just a bool: the caller drops a child only
/// when that parent is itself being shown, so a subagent whose parent has gone is still
/// visible instead of silently vanishing.
pub fn child_parent(path: &Path) -> Option<String> {
    // pi: the nesting is in the path, so this needs no file read.
    let parts: Vec<&str> = path.iter().filter_map(|p| p.to_str()).collect();
    if let Some(run_at) = parts.iter().position(|p| p.starts_with("run-")) {
        if run_at >= 1 {
            // `<…>/<parent-session-id>/<child-uuid>/run-0/session.jsonl`
            if let Some(parent) = parts.get(run_at.saturating_sub(2)) {
                if parent.contains('_') {
                    return Some(parent.to_string());
                }
            }
        }
    }

    let head = read_head(path, 64 * 1024)?;
    for line in head.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // codex: `{"type":"session_meta","payload":{"id":…,"parent_thread_id":…}}`
        if let Some(parent) = value
            .get("payload")
            .and_then(|p| p.get("parent_thread_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            return Some(parent.to_string());
        }
        // pi fallback: the name marks a subagent even when the path shape is unfamiliar.
        if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
            if name.starts_with("subagent-") {
                return Some(String::new());
            }
        }
    }
    None
}

/// Read the tail looking for the last thing the harness recorded, used only for a one-line
/// detail on a discovered row. Bounded, and never trusted to say anything about waiting.
pub fn tail_hint(path: &Path) -> Option<String> {
    let tail = read_tail(path, 16 * 1024)?;
    // Newest first, and a tool call beats prose: "what is it doing" is the question the row is
    // answering, and a tool call is the most literal answer the file holds. The three shapes
    // below are the measured ones (see the module docs).
    for line in tail.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(hint) = tool_call_hint(&value) {
            return Some(hint);
        }
    }
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

/// The last tool call in one transcript row, as a one-line "what is it doing".
fn tool_call_hint(value: &serde_json::Value) -> Option<String> {
    // codex: `{"type":"response_item","payload":{"type":"function_call","name","arguments"}}`
    if let Some(payload) = value.get("payload") {
        if payload.get("type").and_then(|t| t.as_str()) == Some("function_call") {
            let name = payload.get("name").and_then(|n| n.as_str())?;
            return Some(summarize_tool(name, parse_arguments(payload.get("arguments"))));
        }
    }
    // pi / claude: `{"message":{"content":[{"type":"toolCall"|"tool_use","name",…}]}}`
    if let Some(blocks) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    {
        for block in blocks.iter().rev() {
            let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if kind == "toolCall" || kind == "tool_use" {
                let name = block.get("name").and_then(|n| n.as_str())?;
                let args = block.get("input").or_else(|| block.get("arguments"));
                return Some(summarize_tool(name, parse_arguments(args)));
            }
        }
    }
    None
}

/// Tool arguments arrive as an object from pi/claude and as a JSON *string* from codex
/// rollouts. Both are accepted; anything else becomes `None`.
fn parse_arguments(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let value = value?;
    if value.is_object() {
        return Some(value.clone());
    }
    let text = value.as_str()?;
    serde_json::from_str::<serde_json::Value>(text).ok()
}

/// One display line for a tool call: the tool name, plus what it is acting on when the
/// harness recorded something concrete. Never invents a label for a tool it does not know.
fn summarize_tool(name: &str, args: Option<serde_json::Value>) -> String {
    let Some(args) = args else {
        return name.to_string();
    };
    // codex's plan tool: the row is "which step is it on", so count the steps.
    if let Some(plan) = args.get("plan").and_then(|p| p.as_array()) {
        let current = plan.iter().find(|step| {
            step.get("status").and_then(|s| s.as_str()) == Some("in_progress")
        });
        let total = plan.len();
        if let Some(step) = current {
            let text = step.get("step").and_then(|s| s.as_str()).unwrap_or("");
            return format!("{} · {}/{} {}", name, index_of(plan, step) + 1, total, truncate(text, 60));
        }
        return format!("{name} · {total} 步");
    }
    for key in ["command", "cmd", "file_path", "path", "pattern", "query", "url"] {
        if let Some(text) = args.get(key).and_then(|v| v.as_str()) {
            let first = text.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                return format!("{name} · {}", truncate(first, 90));
            }
        }
    }
    name.to_string()
}

fn index_of(list: &[serde_json::Value], needle: &serde_json::Value) -> usize {
    list.iter().position(|item| item == needle).unwrap_or(0)
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
    fn a_session_whose_process_sits_elsewhere_is_still_found() {
        // The bug this guards, measured on this machine: pi chdirs to $HOME, so its process cwd
        // is never the project directory its transcript records. Requiring cwd equality meant
        // the session being typed into at that moment was invisible -- the whole complaint.
        // One live pi process, one transcript written seconds ago in an unrelated directory,
        // and the session must still appear (with no pid, since none can be proven).
        let root = std::env::temp_dir().join(format!("ci-discover-pi-{}", now_ms()));
        let transcript = fixture(
            &root,
            Harness::Pi,
            "/home/u/pi-sessions/agent universe",
            "2026-10-13T06-24-42-279Z_01a133ef",
            r#"{"type":"message"}"#,
        );
        let processes = vec![AgentProcess {
            harness: Harness::Pi,
            pid: 369446,
            cwd: PathBuf::from("/home/u"),
        }];
        let home = root.clone();
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&home, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 1, "the running session must be visible: {found:?}");
        assert_eq!(found[0].transcript, transcript);
        assert_eq!(found[0].cwd, "/home/u/pi-sessions/agent universe");
        assert_eq!(found[0].pid, None, "no process is in that directory, so no pid is claimed");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_harness_with_no_running_process_reports_nothing() {
        // A transcript with no hook and no process is an archive, not a session. Without this
        // bound the island would list every recent conversation as if it were running.
        let root = std::env::temp_dir().join(format!("ci-discover-idle-{}", now_ms()));
        fixture(&root, Harness::Pi, "/tmp/proj", "fresh", "{}");
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, Vec::new(), now_ms(), 600_000, 50, &mut cache);
        assert!(found.is_empty(), "no process means no session: {found:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stale_transcripts_are_not_reported_as_live_sessions() {
        let root = std::env::temp_dir().join(format!("ci-discover-old-{}", now_ms()));
        fixture(&root, Harness::Claude, "/tmp/proj", "uuid-1", "{}");
        // No mtime surgery needed: asking "what is live as of an hour from now" makes an
        // hour-old file stale by definition, and keeps the test free of libc FFI.
        let an_hour_later = now_ms() + 3_600_000;

        // A running process in some *other* directory: it cannot claim this transcript by
        // cwd, so only the freshness pass could pick it up, and the age bound must refuse.
        let processes = vec![AgentProcess {
            harness: Harness::Claude,
            pid: 1,
            cwd: PathBuf::from("/tmp/elsewhere"),
        }];
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, processes, an_hour_later, 60_000, 50, &mut cache);
        assert!(found.is_empty(), "an hour-old transcript is not a live session: {found:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_quiet_session_with_a_live_process_is_a_session_not_an_archive() {
        // The other side of the age bound: an agent parked at a prompt for an hour has not
        // written anything, but its process is still sitting in the session directory. That is
        // a live idle session, and discovery must say so instead of letting it vanish.
        let root = std::env::temp_dir().join(format!("ci-discover-quiet-{}", now_ms()));
        fixture(&root, Harness::Claude, "/tmp/proj", "uuid-quiet", "{}");
        let processes = vec![AgentProcess {
            harness: Harness::Claude,
            pid: 1,
            cwd: PathBuf::from("/tmp/proj"),
        }];
        let an_hour_later = now_ms() + 3_600_000;
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, processes, an_hour_later, 60_000, 50, &mut cache);
        assert_eq!(found.len(), 1, "a quiet session with a live process is still a session");
        assert_eq!(found[0].pid, Some(1));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_held_transcript_binds_a_process_that_shares_no_directory() {
        // The codex shape: an app-server daemon holds the rollouts of every thread it serves,
        // and its own cwd is $HOME. Directory matching cannot see that, but the open descriptor
        // says it outright -- so it is checked first. This test holds the file open itself,
        // which is the only way to assert on real /proc state without a live codex.
        let dir = std::env::temp_dir().join(format!("ci-discover-fd-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-10-14T20-28-07-01a139b5-e5a5-77b0-a481-a86d0b4150b9.jsonl");
        fs::write(&path, "{\"cwd\":\"/tmp/elsewhere\"}\n").unwrap();
        let held = fs::File::open(&path).unwrap();
        let me = std::process::id();

        let names: Vec<String> = held_session_files(me).into_iter().map(|(id, _)| id).collect();
        assert!(
            names.iter().any(|id| id == "01a139b5-e5a5-77b0-a481-a86d0b4150b9"),
            "a held transcript must name its session: {names:?}"
        );
        drop(held);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_writer_lock_names_a_live_session_and_a_random_lock_does_not() {
        // Codex declares a live thread with `thread-writer-locks/<uuid>.lock`. The id is in the
        // *file name*, so this works without knowing codex's directory layout -- which is the
        // point, since that layout is private and can change. Unrelated locks in the same tree
        // (`state`, `.lock`) must not be read as sessions, or discovery invents threads.
        let dir = std::env::temp_dir().join(format!("ci-discover-lock-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let uuid = "01a139b5-e5a5-77b0-a481-a86d0b4150b9";
        let lock = dir.join(format!("{uuid}.lock"));
        fs::write(&lock, "").unwrap();
        let decoy = dir.join("state.lock");
        fs::write(&decoy, "").unwrap();
        let held_lock = fs::File::open(&lock).unwrap();
        let held_decoy = fs::File::open(&decoy).unwrap();
        let me = std::process::id();

        let found = held_session_files(me);
        assert!(
            found.iter().any(|(id, _)| id == uuid),
            "a uuid-named writer lock must be reported: {found:?}"
        );
        assert!(
            !found.iter().any(|(id, _)| id == "state"),
            "an unrelated lock must never become a session: {found:?}"
        );
        drop(held_lock);
        drop(held_decoy);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_subagent_is_folded_into_the_session_it_belongs_to() {
        // Measured complaint: the island showed three sessions for a codex conversation plus one
        // pi session, because a codex "Guardian review" subagent was counted as a peer of the
        // chat it reviews. The parent is named in the child's own session_meta, which is the
        // signal used here. Two live processes -> the parent is discovered too, so the child
        // folds and exactly one row remains.
        let root = std::env::temp_dir().join(format!("ci-discover-fold-{}", now_ms()));
        let parent_id = "01a139b5-e5a5-77b0-a481-a86d0b4150b9";
        let child_id = "01a13a9a-a58d-75c1-a244-440e53b540a5";
        // The parent transcript, with fake process cwds that both point at it.
        let dir = root.join(".codex/sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("rollout-2026-10-14T20-28-07-{parent_id}.jsonl")),
            format!("{}\n", serde_json::json!({"type":"session_meta","payload":{"cwd":"/tmp/proj","id":parent_id}})),
        ).unwrap();
        // The child, whose session_meta names the parent.
        fs::write(
            dir.join(format!("rollout-2026-10-14T21-29-35-{child_id}.jsonl")),
            format!("{}\n", serde_json::json!({
                "type":"session_meta",
                "payload":{"cwd":"/tmp/proj","id":child_id,"parent_thread_id":parent_id,
                           "source":{"subagent":{"other":"guardian"}}}
            })),
        ).unwrap();

        let processes = vec![
            AgentProcess { harness: Harness::Codex, pid: 10, cwd: PathBuf::from("/tmp/proj") },
            AgentProcess { harness: Harness::Codex, pid: 11, cwd: PathBuf::from("/tmp/proj") },
        ];
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        let ids: Vec<&str> = found.iter().map(|d| d.session_id.as_str()).collect();
        assert!(ids.contains(&parent_id), "the parent must be shown: {ids:?}");
        assert!(!ids.contains(&child_id), "the subagent must fold into it: {ids:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_orphan_subagent_stays_visible() {
        // The other side of the fold: a child whose parent is not being shown is still real
        // activity. Dropping it would make the island quieter than the machine, which is the
        // failure mode this whole feature exists to avoid.
        let root = std::env::temp_dir().join(format!("ci-discover-orphan-{}", now_ms()));
        let parent_id = "00000000-0000-0000-0000-000000000000";
        let child_id = "01a13a9a-a58d-75c1-a244-440e53b540a5";
        let dir = root.join(".codex/sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("rollout-2026-10-14T21-29-35-{child_id}.jsonl")),
            format!("{}\n", serde_json::json!({
                "type":"session_meta",
                "payload":{"cwd":"/tmp/proj","id":child_id,"parent_thread_id":parent_id}
            })),
        ).unwrap();
        let processes = vec![AgentProcess { harness: Harness::Codex, pid: 10, cwd: PathBuf::from("/tmp/proj") }];
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 1, "an orphan subagent is still a live session: {found:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_tool_call_becomes_the_current_step_line() {
        // "What is it doing" is the question the row answers, and a tool call is the most
        // literal answer the transcript holds. The three shapes below are the measured ones.
        let dir = std::env::temp_dir().join(format!("ci-discover-step-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();

        // codex: function_call with arguments as a JSON *string*.
        let codex = dir.join("codex.jsonl");
        fs::write(&codex, format!("{}\n", serde_json::json!({
            "type":"response_item",
            "payload":{"type":"function_call","name":"exec_command",
                       "arguments":"{\"cmd\":\"python train.py --steps 100\"}"}
        }))).unwrap();
        let hint = tail_hint(&codex).unwrap();
        assert!(hint.contains("exec_command"), "names the tool: {hint}");
        assert!(hint.contains("python train.py"), "and what it is running: {hint}");

        // pi: message.content[].type == "toolCall".
        let pi = dir.join("pi.jsonl");
        fs::write(&pi, format!("{}\n", serde_json::json!({
            "type":"message",
            "message":{"content":[{"type":"toolCall","name":"bash",
                                     "arguments":{"command":"cargo test --release"}}]}
        }))).unwrap();
        let hint = tail_hint(&pi).unwrap();
        assert!(hint.contains("bash") && hint.contains("cargo test"), "pi shape: {hint}");

        // claude: message.content[].type == "tool_use" with `input`.
        let claude = dir.join("claude.jsonl");
        fs::write(&claude, format!("{}\n", serde_json::json!({
            "type":"assistant",
            "message":{"content":[{"type":"tool_use","name":"Edit",
                                     "input":{"file_path":"src/main.rs"}}]}
        }))).unwrap();
        let hint = tail_hint(&claude).unwrap();
        assert!(hint.contains("Edit") && hint.contains("src/main.rs"), "claude shape: {hint}");

        // codex's plan tool reports a position in the checklist instead of a command.
        let plan = dir.join("plan.jsonl");
        fs::write(&plan, format!("{}\n", serde_json::json!({
            "type":"response_item",
            "payload":{"type":"function_call","name":"update_plan",
                       "arguments":{"plan":[{"step":"读代码","status":"completed"},
                                              {"step":"改判定","status":"in_progress"},
                                              {"step":"跑测试","status":"pending"}]}}
        }))).unwrap();
        let hint = tail_hint(&plan).unwrap();
        assert!(hint.contains("2/3"), "a plan reports its position: {hint}");
        assert!(hint.contains("改判定"), "and the current step: {hint}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_subagent_is_named_by_its_own_session_meta() {
        let dir = std::env::temp_dir().join(format!("ci-discover-parent-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let child = dir.join("child.jsonl");
        fs::write(&child, format!("{}\n", serde_json::json!({
            "type":"session_meta",
            "payload":{"id":"child","parent_thread_id":"parent-thread","cwd":"/repo"}
        }))).unwrap();
        assert_eq!(child_parent(&child).as_deref(), Some("parent-thread"));

        // A top-level session names no parent.
        let top = dir.join("top.jsonl");
        fs::write(&top, format!("{}\n", serde_json::json!({
            "type":"session_meta","payload":{"id":"top","cwd":"/repo"}
        }))).unwrap();
        assert_eq!(child_parent(&top), None);

        // pi nests the child under the parent's session directory.
        let pi_child = dir.join("2026-10-13T06-24-42-279Z_01a133ef-4927-754d-ad79-ec98df083751")
            .join("341766e4-860a-43c4-b278-d63c1d35f57e").join("run-0").join("session.jsonl");
        fs::create_dir_all(pi_child.parent().unwrap()).unwrap();
        fs::write(&pi_child, "{\"type\":\"session\"}\n").unwrap();
        assert_eq!(
            child_parent(&pi_child).as_deref(),
            Some("2026-10-13T06-24-42-279Z_01a133ef-4927-754d-ad79-ec98df083751")
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_thread_with_two_rollouts_yields_one_row_and_the_newest_wins() {
        // Measured: a resumed codex thread leaves two rollout files carrying the same thread
        // uuid. Both pair with the thread's live lock, so both reach discovery, and because the
        // daemon keys sessions by id, whichever registers last wins. Registering in newest-first
        // order without deduping meant the *older* file won, and the row showed that stale
        // transcript instead of the live one.
        let root = std::env::temp_dir().join(format!("ci-discover-dup-{}", now_ms()));
        let thread = "01a139b5-e5a5-77b0-a481-a86d0b4150b9";
        let dir = root.join(".codex/sessions");
        fs::create_dir_all(&dir).unwrap();
        // Older file: quiet, its last record is an event message.
        let older = dir.join(format!("rollout-2026-10-14T17-19-44-{thread}.jsonl"));
        fs::write(&older, format!("{}\n{}", serde_json::json!({
            "type":"session_meta","payload":{"cwd":"/tmp/proj","id":thread}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}}))).unwrap();
        // Newer file: same thread, still being written.
        let newer = dir.join(format!("rollout-2026-10-14T20-28-07-{thread}_01a13a62-5eaf-7160-93f6-471ee359a365.jsonl"));
        fs::write(&newer, format!("{}\n{}", serde_json::json!({
            "type":"session_meta","payload":{"cwd":"/tmp/proj","id":thread}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call",
                "name":"exec_command","arguments":"{\"cmd\":\"python train.py\"}"}}))).unwrap();
        // Make the order unambiguous regardless of filesystem timestamp resolution.
        filetime(&newer, now_ms());
        filetime(&older, now_ms() - 3_600_000);

        let processes = vec![AgentProcess { harness: Harness::Codex, pid: 10, cwd: PathBuf::from("/tmp/proj") }];
        let mut cache = std::collections::HashMap::new();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 1, "one thread is one row: {found:?}");
        assert_eq!(found[0].transcript, newer, "the live rollout must win, not the stale one");
        let hint = tail_hint(&found[0].transcript).unwrap();
        assert!(hint.contains("python train.py"), "the row describes the live work: {hint}");
        fs::remove_dir_all(&root).ok();
    }

    /// Set a file's mtime, so ordering tests do not depend on how fast the test writes.
    fn filetime(path: &Path, ms: u64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        let when = UNIX_EPOCH + std::time::Duration::from_millis(ms);
        file.set_modified(when).unwrap();
    }

    #[test]
    fn codex_rollout_names_yield_the_thread_uuid_that_hooks_report() {
        let path = Path::new("rollout-2026-10-14T17-19-44-01a139b5-e5a5-77b0-a481-a86d0b4150b9.jsonl");
        assert_eq!(
            session_id_of(path).as_deref(),
            Some("01a139b5-e5a5-77b0-a481-a86d0b4150b9")
        );
        // The second shape, measured on this machine: two uuids joined by `_`. The old
        // dash-splitting rule returned `a86d0b4150b9_01a13a62-5eaf-7160-93f6-471ee359a365`,
        // which matches nothing a hook ever reports.
        let resumed = Path::new(
            "rollout-2026-10-14T20-28-07-01a139b5-e5a5-77b0-a481-a86d0b4150b9_01a13a62-5eaf-7160-93f6-471ee359a365.jsonl",
        );
        assert_eq!(
            session_id_of(resumed).as_deref(),
            Some("01a139b5-e5a5-77b0-a481-a86d0b4150b9"),
            "the first uuid is the one the transcript's own session_meta names as session_id"
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
