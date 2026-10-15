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
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// The conversation's own title, when the harness stated one: codex's thread name
    /// from `session_index.jsonl`, claude's `custom-title`/`ai-title` rows, or the first
    /// user prompt as the universal fallback. The row leads with this instead of the
    /// project label when present -- see `transcript_title`.
    pub title: Option<String>,
    /// Token usage the harness stated about the session, when it does (codex's
    /// cumulative counter; see `transcript_usage`).
    pub usage: Option<codeisland_core::session::Usage>,
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
    /// Epoch milliseconds of the process's start, or 0 when it could not be read.
    ///
    /// A transcript last written *before* this moment cannot belong to the process:
    /// the harness's previous session ended with its own process, and a freshly started
    /// agent that has not written anything yet must not inherit that session's row.
    /// Upstream carries the same field for the same reason -- its comment reads
    /// "Process start time filters stale transcript files".
    pub started_ms: u64,
}

/// How much slack a transcript's last write gets against the process start, in
/// milliseconds. Upstream allows 10 seconds (`modified < start.addingTimeInterval(-10)`);
/// the same tolerance absorbs coarse filesystem timestamps and the one-second
/// rounding between `/proc` tick arithmetic and wall time.
const START_GRACE_MS: u64 = 10_000;


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

/// Everything a scan remembers between runs, so a file nobody is rewriting
/// is parsed once ever and a growing file is re-read only when it grew.
///
/// The cache keys each file by its size, which is cheap and monotonic: any
/// append changes the size, and nothing else about a transcript changes.
#[derive(Default)]
pub struct ParseCache {
    pub cwd: std::collections::HashMap<PathBuf, (u64, Option<String>)>,
    pub title: std::collections::HashMap<PathBuf, (u64, Option<String>)>,
    pub usage: std::collections::HashMap<PathBuf, (u64, Option<codeisland_core::session::Usage>)>,
}

/// Token usage one transcript states about its session.
///
/// codex writes a running cumulative counter -- `event_msg` / `token_count` →
/// `payload.info.total_token_usage` -- so the newest such row IS the session's
/// total and a tail read suffices. Measured on this machine: two days of
/// rollouts carry 941 of these rows, so the bottom bar's numbers are real.
/// claude's `message.usage` is per-response (a session total needs a whole-file
/// sum, unbounded on big transcripts) and pi writes nothing structured, so both
/// stay out until a bounded source is measured -- the bar shows what is true.
pub fn transcript_usage(path: &Path) -> Option<codeisland_core::session::Usage> {
    let tail = read_tail(path, 64 * 1024)?;
    for line in tail.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let Some(payload) = value.get("payload") else { continue };
        if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
            continue;
        }
        // `total_token_usage` is the harness's own running sum for the session:
        // input_tokens counts every token read, output_tokens every one written.
        let Some(total) = payload.get("info").and_then(|info| info.get("total_token_usage")) else {
            continue;
        };
        let usage = codeisland_core::session::Usage {
            input_tokens: total.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            output_tokens: total.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        };
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            return Some(usage);
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

/// Thread names codex itself wrote, from `session_index.jsonl` under its home.
///
/// One line per thread update: `{"id":…,"thread_name":…,"updated_at":…}`. The same
/// thread can appear several times; the file is append-ordered, so the last line
/// for an id wins -- upstream compares `updated_at` timestamps to the same effect.
/// This is the one title source that needs no transcript read at all.
pub fn codex_thread_names(codex_home: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(index) = read_head(&codex_home.join("session_index.jsonl"), 4 * 1024 * 1024) else {
        return out;
    };
    for line in index.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let (Some(id), Some(name)) = (
            value.get("id").and_then(|v| v.as_str()),
            value.get("thread_name").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let name = name.trim();
        if id.is_empty() || name.is_empty() {
            continue;
        }
        out.insert(id.to_string(), single_line(name));
    }
    out
}

/// The title of the conversation a transcript holds, when the harness stated one.
///
/// Sources in the order they are trusted (upstream's `SessionTitleStore` reads the
/// same two, custom first):
///
/// 1. **A title the harness wrote down.** Claude Code appends
///    `{"type":"custom-title","customTitle":…}` / `ai-title` rows (measured on
///    this machine); codex keeps thread names in `session_index.jsonl` instead.
/// 2. **The first user prompt.** The user's own words are the most literal
///    "what is this conversation about" that every harness records. Measured
///    shapes: claude `type:"user"` with string content, codex `response_item`
///    message role user `input_text`, pi `message` role user text blocks.
/// 3. Nothing -- a transcript with neither (claude writes the first prompt
///    lazily; a brand-new agent has written nothing) has no title, and the row
///    keeps its project label.
///
/// Claude's synthetic rows (`<local-command-caveat>`, `<command-name>`, …) are
/// skipped: they are terminal plumbing, not conversation.
pub fn transcript_title(path: &Path) -> Option<String> {
    let head = read_head(path, 128 * 1024)?;
    let tail = read_tail(path, 64 * 1024).unwrap_or_default();

    let mut custom: Option<String> = None;
    let mut ai: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    for line in head.lines().chain(tail.lines()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("custom-title") => {
                if let Some(text) = value.get("customTitle").and_then(|v| v.as_str()) {
                    custom = Some(single_line(text));
                }
                continue;
            }
            Some("ai-title") => {
                if let Some(text) = value.get("aiTitle").and_then(|v| v.as_str()) {
                    ai = Some(single_line(text));
                }
                continue;
            }
            _ => {}
        }
        if first_prompt.is_none() {
            first_prompt = first_user_text(&value);
        }
    }
    custom.or(ai).or(first_prompt).map(|text| truncate(&text, 80))
}

/// The first user prompt in one transcript row, per harness, or `None`.
fn first_user_text(value: &serde_json::Value) -> Option<String> {
    match value.get("type").and_then(|t| t.as_str()) {
        // claude: {"type":"user","message":{"role":"user","content":"…"}}
        Some("user") => {
            let text = message_text(value.get("message"))?;
            // Claude Code writes terminal plumbing into user rows before the first real
            // prompt; those are not conversation and must not become the title.
            const CAVEATS: [&str; 4] = [
                "<local-command-caveat>",
                "<command-name>",
                "<bash-input>",
                "<system-reminder>",
            ];
            if CAVEATS.iter().any(|caveat| text.starts_with(caveat)) {
                return None;
            }
            Some(text)
        }
        // codex: {"type":"response_item","payload":{"type":"message","role":"user",
        //         "content":[{"type":"input_text","text":"…"}]}}
        Some("response_item") => {
            let payload = value.get("payload")?;
            if payload.get("type").and_then(|t| t.as_str()) != Some("message")
                || payload.get("role").and_then(|r| r.as_str()) != Some("user")
            {
                return None;
            }
            let blocks = payload.get("content")?.as_array()?;
            let text = blocks
                .iter()
                .find_map(|block| block.get("text").and_then(|t| t.as_str()))?;
            Some(single_line(text))
        }
        // pi: {"type":"message","message":{"role":"user","content":[{"type":"text","text":"…"}]}}
        Some("message") => {
            let message = value.get("message")?;
            if message.get("role").and_then(|r| r.as_str()) != Some("user") {
                return None;
            }
            message_text(Some(message))
        }
        _ => None,
    }
}

/// The text of one message row: string content as-is, or the first text block
/// of array content. Shared by the title and detail readers.
fn message_text(message: Option<&serde_json::Value>) -> Option<String> {
    let content = message?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(single_line(text));
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
        .find_map(|block| block.get("text").and_then(|t| t.as_str()))?;
    Some(single_line(text))
}

/// Collapse to one line: a card title or detail line never wraps.
fn single_line(text: &str) -> String {
    text.trim().replace(['\n', '\r'], " ")
}

/// Live agent processes, identified by argv and located by their cwd.
pub fn agent_processes() -> Vec<AgentProcess> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    let boot_epoch_ms = boot_epoch_ms();
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
        let started_ms = boot_epoch_ms
            .and_then(|boot| process_start_ms(pid).map(|start| boot.saturating_add(start)));
        out.push(AgentProcess {
            harness,
            pid,
            cwd,
            started_ms: started_ms.unwrap_or(0),
        });
    }
    out
}

/// Epoch milliseconds of the last boot, from `/proc/uptime`'s first field.
fn boot_epoch_ms() -> Option<u64> {
    let uptime = fs::read_to_string("/proc/uptime").ok()?;
    let seconds = uptime.split_whitespace().next()?.parse::<f64>().ok()?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(now_ms.saturating_sub((seconds * 1000.0) as u64))
}

/// Milliseconds since boot at which the process started, from `/proc/<pid>/stat`'s
/// `starttime` field. Verified against `ps -o lstart` on this machine: agrees to
/// within one second.
fn process_start_ms(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // After the closing paren: fields[0] is the state, fields[19] is `starttime`
    // (field 22 of the whole line), in clock ticks since boot.
    let ticks = fields.get(19)?.parse::<u64>().ok()?;
    let ticks_per_sec = ticks_per_sec();
    Some(ticks.saturating_mul(1000) / ticks_per_sec.max(1))
}

fn ticks_per_sec() -> u64 {
    // `USER_HZ` is 100 on effectively every Linux; sysconf confirms it when it can.
    let value = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if value > 0 {
        value as u64
    } else {
        100
    }
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
    roots.insert(Harness::Codex, codex_home(home).join("sessions"));
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
    cache: &mut ParseCache,
) -> Vec<Discovered> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    discover_in(&home, agent_processes(), now_ms, max_age_ms, probe_limit, cache)
}

/// Where codex's home is, honouring `CODEX_HOME` exactly like `archive_roots` does.
fn codex_home(home: &Path) -> PathBuf {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".codex"))
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
    cache: &mut ParseCache,
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
        // The earliest start of this harness's live processes, when every one of them is
        // known. A transcript last written before that moment cannot have been written by
        // any process still running: it belongs to a session that already ended. The
        // resurrection this prevents is measured, not hypothetical -- opening a fresh,
        // empty pi made the island show the session that had ended twelve minutes earlier,
        // because the freshness pass needed *something* recent to pair the new process with.
        // One unknown start makes the bound unknowable, so it comes off entirely rather than
        // risking a wrong exclusion.
        let starts: Vec<u64> = processes
            .iter()
            .filter(|process| process.harness == *harness)
            .map(|process| process.started_ms)
            .collect();
        let earliest_start: Option<u64> = if !starts.is_empty() && starts.iter().all(|&s| s > 0) {
            starts.iter().min().copied()
        } else {
            None
        };
        // May weak evidence (a cwd match, or freshness) claim this transcript at all?
        // Strong evidence -- a held rollout or a writer lock -- is exempt: a lock file is
        // the harness's own "this thread is live" claim, written and removed by the
        // harness itself, and it outranks anything we can compute from clocks.
        let predates_live_processes = |entry: &Entry| {
            earliest_start
                .map_or(false, |start| entry.last_write_ms.saturating_add(START_GRACE_MS) < start)
        };
        // The newest `probe_limit` transcripts. `probe_limit` bounds a directory walk to a few
        // stats; how many rows come out is bounded separately, by evidence (an open descriptor
        // or a seated process) and by the per-harness live-process count. It is deliberately
        // not `live`, because one daemon can hold several transcripts -- the codex app-server
        // holds every open thread -- and capping the walk at the process count would hide all
        // but one of them.
        let take = probe_limit;

        // Parse once: the cwd, id and title are needed by both passes, and the file walk
        // is the expensive part.
        struct Entry {
            path: PathBuf,
            cwd: String,
            session_id: String,
            title: Option<String>,
            usage: Option<codeisland_core::session::Usage>,
            last_write_ms: u64,
        }
        // Codex names its threads in a small index beside the archives; read it once per
        // scan (it is a few KB) rather than per transcript.
        let codex_names = if *harness == Harness::Codex {
            codex_thread_names(&codex_home(home))
        } else {
            BTreeMap::new()
        };
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
            let cwd = match cache.cwd.get(&path) {
                Some((cached_size, cached_cwd)) if *cached_size == size => cached_cwd.clone(),
                _ => {
                    let found = transcript_cwd(&path);
                    cache.cwd.insert(path.clone(), (size, found.clone()));
                    found
                }
            };
            let session_id = session_id_of(&path);
            // The title *can* appear later -- a first prompt lands with the first turn, a
            // claude custom-title mid-session -- so the cache re-extracts whenever the file
            // has grown, and only holds a settled value for a file that stopped moving.
            let title = match cache.title.get(&path) {
                Some((cached_size, cached_title)) if *cached_size == size => cached_title.clone(),
                _ => {
                    let found = codex_names
                        .get(session_id.as_deref().unwrap_or(""))
                        .cloned()
                        .or_else(|| transcript_title(&path));
                    cache.title.insert(path.clone(), (size, found.clone()));
                    found
                }
            };
            // Usage only grows with a live session, so the same size key re-reads a
            // moving file and settles a quiet one.
            let usage = match cache.usage.get(&path) {
                Some((cached_size, cached_usage)) if *cached_size == size => *cached_usage,
                _ => {
                    let found = transcript_usage(&path);
                    cache.usage.insert(path.clone(), (size, found));
                    found
                }
            };
            let (Some(cwd), Some(session_id)) = (cwd, session_id) else {
                continue;
            };
            entries.push(Entry {
                path,
                cwd,
                session_id,
                title,
                usage,
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
            // realistic long-quiet case), and only when some live process predates that transcript;
            // an older one must be recent to count, and none of them may predate every live
            // process -- that is the previous session, not this one.
            let is_newest = index == 0;
            if predates_live_processes(entry) {
                continue;
            }
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
        //
        // The start bound is what makes that comment true in practice, not just in intent: the
        // age window is hours wide, and a session that ended minutes ago is inside it. A
        // transcript written before every live process started is the previous session's
        // remains -- the measured case was a fresh empty pi claiming a session that ended twelve
        // minutes before the process began. Upstream filters identically, at file selection:
        // "Skip files from old sessions: must be modified after process started".
        for (index, entry) in entries.iter().enumerate() {
            if rows.len() >= live {
                break;
            }
            if rows.iter().any(|(taken, _)| *taken == index) {
                continue;
            }
            if predates_live_processes(entry) {
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
                title: entry.title.clone(),
                usage: entry.usage,
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
///
/// Two passes, newest first, both skipping the harness's bookkeeping rows (codex's
/// `event_msg` token counters and `token_usage_record`s -- the turn lifecycle they
/// carry is the status capsule's business, and their type name was once the whole
/// fallback: the row read "存档 · event_msg", which says nothing at all):
///
/// 1. The last tool call -- "what is it doing" is the question the row is answering,
///    and a tool call is the most literal answer the file holds.
/// 2. The last thing anybody said -- the newest message text, which for a finished
///    turn is the reply itself. Better than silence, better than a type name.
///
/// The window is 64 KB because the measured file had its last tool call 9 rows and
/// 16+ KB from the end: big reasoning and reply rows pushed it out of a 16 KB tail.
pub fn tail_hint(path: &Path) -> Option<String> {
    let tail = read_tail(path, 64 * 1024)?;
    // Newest first, and a tool call beats prose.
    for line in tail.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if is_noise_row(&value) {
            continue;
        }
        if let Some(hint) = tool_call_hint(&value) {
            return Some(hint);
        }
    }
    for line in tail.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if is_noise_row(&value) {
            continue;
        }
        // codex: response_item message text (assistant reply or user prompt).
        if value.get("type").and_then(|t| t.as_str()) == Some("response_item") {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(|t| t.as_str()) == Some("message") {
                    if let Some(blocks) = payload.get("content").and_then(|c| c.as_array()) {
                        if let Some(text) = blocks
                            .iter()
                            .find_map(|block| block.get("text").and_then(|t| t.as_str()))
                        {
                            return Some(truncate(&single_line(text), 120));
                        }
                    }
                }
            }
        }
        // pi / claude: message rows, string or text-block content.
        if let Some(text) = message_text(value.get("message")) {
            return Some(truncate(&text, 120));
        }
        if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
            return Some(truncate(&single_line(text), 120));
        }
    }
    None
}

/// Bookkeeping rows that must never become a row's one-line detail: they carry
/// no answer to "what is it doing", and their type name as a fallback produced
/// the measured "存档 · event_msg" -- a detail line that says nothing.
fn is_noise_row(value: &serde_json::Value) -> bool {
    matches!(
        value.get("type").and_then(|t| t.as_str()),
        Some("event_msg") | Some("token_usage_record")
    )
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
            started_ms: now_ms() - 60_000,
        }];
        let home = root.clone();
        let mut cache = ParseCache::default();
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
            started_ms: now_ms() - 60_000,
        }];
        let home = root.clone();
        let mut cache = ParseCache::default();
        let found = discover_in(&home, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 1, "the running session must be visible: {found:?}");
        assert_eq!(found[0].transcript, transcript);
        assert_eq!(found[0].cwd, "/home/u/pi-sessions/agent universe");
        assert_eq!(found[0].pid, None, "no process is in that directory, so no pid is claimed");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_transcript_written_before_every_live_process_started_is_not_resurrected() {
        // The measured bug this bound exists for: opening a fresh, empty pi agent (nothing
        // sent, so no transcript of its own yet) made the island show the session that had
        // ended twelve minutes earlier, because the freshness pass needed *something*
        // recent to pair the new process with. A transcript last written before every live
        // process started is the previous session's remains. Upstream rejects it at file
        // selection ("must be modified after process started"); this is the same rule.
        let root = std::env::temp_dir().join(format!("ci-discover-resurrect-{}", now_ms()));
        let transcript = fixture(&root, Harness::Pi, "/home/xhkz/apps/old-project", "old-session", "{}");
        let at = now_ms();
        filetime(&transcript, at - 12 * 60_000); // the old session ended 12 minutes ago
        let processes = vec![AgentProcess {
            harness: Harness::Pi,
            pid: 1234,
            cwd: PathBuf::from("/home/xhkz"), // pi chdirs to $HOME at start-up
            started_ms: at, // the new agent opened *after* the old session ended
        }];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, at + 60_000, 2 * 60 * 60 * 1000, 50, &mut cache);
        assert!(
            found.is_empty(),
            "an empty new agent must not inherit the previous session: {found:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_quiet_session_whose_process_predates_its_last_write_stays() {
        // The other side of the bound: an agent parked at its prompt wrote its transcript
        // 90 minutes ago and has been quiet since. Its process predates the write, so the
        // transcript *can* be its session, and the freshness pass must keep showing it --
        // tightening past this point would make parked agents vanish.
        let root = std::env::temp_dir().join(format!("ci-discover-parked-{}", now_ms()));
        let transcript = fixture(&root, Harness::Pi, "/home/xhkz/apps/proj", "parked-session", "{}");
        let at = now_ms();
        filetime(&transcript, at);
        let processes = vec![AgentProcess {
            harness: Harness::Pi,
            pid: 1234,
            cwd: PathBuf::from("/home/xhkz"),
            started_ms: at - 60_000, // alive before the last write
        }];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, at + 90 * 60_000, 2 * 60 * 60 * 1000, 50, &mut cache);
        assert_eq!(found.len(), 1, "a parked agent is still a session: {found:?}");
        assert_eq!(found[0].transcript, transcript);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_transcript_written_while_any_live_process_existed_can_pair() {
        // Two processes: one old enough to have written the transcript, one brand new.
        // The bound is the *earliest* start on purpose -- a transcript written while some
        // live process existed has a possible owner, and a newer process must not veto it.
        let root = std::env::temp_dir().join(format!("ci-discover-mixed-{}", now_ms()));
        fixture(&root, Harness::Pi, "/home/xhkz/apps/proj", "shared-harness", "{}");
        let at = now_ms();
        let dir = root.join(".pi/agent/sessions/--tmp-proj--");
        let transcript = dir.join("shared-harness.jsonl");
        filetime(&transcript, at - 30 * 60_000); // written 30 minutes ago
        let processes = vec![
            AgentProcess { harness: Harness::Pi, pid: 1, cwd: PathBuf::from("/home/xhkz"), started_ms: at - 3_600_000 },
            AgentProcess { harness: Harness::Pi, pid: 2, cwd: PathBuf::from("/home/xhkz"), started_ms: at - 60_000 },
        ];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, at, 2 * 60 * 60 * 1000, 50, &mut cache);
        assert_eq!(found.len(), 1, "the older process can still own it: {found:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_new_process_in_the_transcripts_directory_does_not_inherit_the_old_session() {
        // The cwd rule's newest-exemption used to trust a fresh process sitting in the
        // transcript's directory no matter when the transcript was written. Same
        // resurrection through a different door: launching a new agent in a directory
        // whose previous session ended minutes ago must not show that session as running.
        let root = std::env::temp_dir().join(format!("ci-discover-cwdnew-{}", now_ms()));
        let transcript = fixture(&root, Harness::Claude, "/tmp/proj", "ended-earlier", "{}");
        let at = now_ms();
        filetime(&transcript, at - 12 * 60_000); // ended 12 minutes ago
        let processes = vec![AgentProcess {
            harness: Harness::Claude,
            pid: 1,
            cwd: PathBuf::from("/tmp/proj"), // sitting right in it
            started_ms: at, // opened after the session ended
        }];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, at + 60_000, 2 * 60 * 60 * 1000, 50, &mut cache);
        assert!(found.is_empty(), "the directory match must not resurrect: {found:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_process_with_unknown_start_leaves_the_bound_off() {
        // When the start time cannot be read (0), the bound must come off entirely rather
        // than exclude wrongly: an unknown start could be arbitrarily early, so claiming
        // to know the earliest one would be a guess. Pairing then behaves exactly as it
        // did before the bound existed.
        let root = std::env::temp_dir().join(format!("ci-discover-unknown-{}", now_ms()));
        fixture(&root, Harness::Pi, "/home/xhkz/apps/proj", "fresh-anyway", "{}");
        let processes = vec![AgentProcess {
            harness: Harness::Pi,
            pid: 1234,
            cwd: PathBuf::from("/home/xhkz"),
            started_ms: 0, // unknown
        }];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 1, "no bound is applied when the start is unknown: {found:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_harness_with_no_running_process_reports_nothing() {
        // A transcript with no hook and no process is an archive, not a session. Without this
        // bound the island would list every recent conversation as if it were running.
        let root = std::env::temp_dir().join(format!("ci-discover-idle-{}", now_ms()));
        fixture(&root, Harness::Pi, "/tmp/proj", "fresh", "{}");
        let mut cache = ParseCache::default();
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
        // The process predates the transcript's write, so the start bound lets it through
        // and the test stays about the age bound.
        let processes = vec![AgentProcess {
            harness: Harness::Claude,
            pid: 1,
            cwd: PathBuf::from("/tmp/elsewhere"),
            started_ms: now_ms() - 3_600_000,
        }];
        let mut cache = ParseCache::default();
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
            started_ms: now_ms() - 60_000,
        }];
        let an_hour_later = now_ms() + 3_600_000;
        let mut cache = ParseCache::default();
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
            AgentProcess { harness: Harness::Codex, pid: 10, cwd: PathBuf::from("/tmp/proj"), started_ms: now_ms() - 60_000 },
            AgentProcess { harness: Harness::Codex, pid: 11, cwd: PathBuf::from("/tmp/proj"), started_ms: now_ms() - 60_000 },
        ];
        let mut cache = ParseCache::default();
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
        let processes = vec![AgentProcess { harness: Harness::Codex, pid: 10, cwd: PathBuf::from("/tmp/proj"), started_ms: now_ms() - 60_000 }];
        let mut cache = ParseCache::default();
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

        let processes = vec![AgentProcess { harness: Harness::Codex, pid: 10, cwd: PathBuf::from("/tmp/proj"), started_ms: now_ms() - 60_000 }];
        let mut cache = ParseCache::default();
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
                started_ms: now_ms() - 60_000,
            },
            AgentProcess {
                harness: Harness::Pi,
                pid: 2,
                cwd: PathBuf::from("/tmp/proj"),
                started_ms: now_ms() - 60_000,
            },
        ];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 2, "two sessions in one directory are two rows: {found:?}");
        let transcripts: Vec<PathBuf> = found.iter().map(|d| d.transcript.clone()).collect();
        assert_ne!(transcripts[0], transcripts[1], "one transcript claimed twice");
        let ids: Vec<&str> = found.iter().map(|d| d.session_id.as_str()).collect();
        assert!(ids.contains(&"one") && ids.contains(&"two"), "both ids present: {ids:?}");

        // A single process in that same directory takes exactly one row -- the newest.
        let mut cache2 = ParseCache::default();
        let alone = discover_in(
            &root,
            vec![AgentProcess {
                harness: Harness::Pi,
                pid: 7,
                cwd: PathBuf::from("/tmp/proj"),
                started_ms: now_ms() - 60_000,
            }],
            now_ms(),
            600_000,
            50,
            &mut cache2,
        );
        assert_eq!(alone.len(), 1, "one process must not invent two sessions: {alone:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn titles_come_from_each_harnesss_own_record() {
        // Three measured title sources, one test: codex's `session_index.jsonl`
        // thread name, claude's `custom-title` row, and the first user prompt
        // for pi -- which upstream deliberately does not support, and where the
        // user's own words are the only source there is.
        let root = std::env::temp_dir().join(format!("ci-discover-title-{}", now_ms()));
        let thread = "01a139b5-e5a5-77b0-a481-a86d0b4150b9";

        // codex: the index names the thread; the transcript need not be read.
        let codex_dir = root.join(".codex/sessions");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join(format!("rollout-2026-10-15T13-11-30-{thread}.jsonl")),
            format!("{}\n", serde_json::json!({"type":"session_meta","payload":{"cwd":"/tmp/p","id":thread}})),
        ).unwrap();
        fs::write(
            root.join(".codex/session_index.jsonl"),
            format!("{}\n{}\n", serde_json::json!({"id":"unrelated","thread_name":"别的会话"}),
                serde_json::json!({"id":thread,"thread_name":"handoff 看一下这个东西"})),
        ).unwrap();

        // claude: a custom-title row beats the first prompt. Real claude user rows
        // carry the cwd alongside the message, which is where discovery reads it.
        let claude_dir = root.join(".claude/projects/-tmp-p");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("c1.jsonl"),
            format!("{}\n{}\n",
                serde_json::json!({"type":"user","cwd":"/tmp/p","message":{"role":"user","content":"被标题盖住的首条提问"}}),
                serde_json::json!({"type":"custom-title","customTitle":"fix-doc-path-batch-edit"})),
        ).unwrap();

        // pi: no title record exists, so the first user prompt is the title. The
        // session row carries the cwd, as pi writes it.
        let pi_dir = root.join(".pi/agent/sessions/--tmp-p--");
        fs::create_dir_all(&pi_dir).unwrap();
        fs::write(
            pi_dir.join("p1.jsonl"),
            format!("{}\n{}\n", serde_json::json!({"type":"session","version":3,"id":"p1","cwd":"/tmp/p"}),
                serde_json::json!({"type":"message","message":{"role":"user",
                "content":[{"type":"text","text":"你看一下kcu这个mcp如何接入\n换行也应该被压平"}]}})),
        ).unwrap();

        let processes = vec![
            AgentProcess { harness: Harness::Codex, pid: 1, cwd: PathBuf::from("/tmp/p"), started_ms: now_ms() - 60_000 },
            AgentProcess { harness: Harness::Claude, pid: 2, cwd: PathBuf::from("/tmp/p"), started_ms: now_ms() - 60_000 },
            AgentProcess { harness: Harness::Pi, pid: 3, cwd: PathBuf::from("/home/u"), started_ms: now_ms() - 60_000 },
        ];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        let title_of = |harness: Harness| {
            found
                .iter()
                .find(|d| d.harness == harness)
                .and_then(|d| d.title.clone())
                .unwrap_or_default()
        };
        assert_eq!(title_of(Harness::Codex), "handoff 看一下这个东西", "codex reads its own index");
        assert_eq!(title_of(Harness::Claude), "fix-doc-path-batch-edit", "a written title beats the prompt");
        assert_eq!(title_of(Harness::Pi), "你看一下kcu这个mcp如何接入 换行也应该被压平", "pi falls back to the first prompt, one line");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_claude_prompt_that_is_terminal_plumbing_is_not_a_title() {
        // Claude Code writes synthetic user rows (`<local-command-caveat>`, …)
        // before the first real prompt. Measured on this machine: the first
        // user row of a transcript is often exactly that. Those are not
        // conversation, and as titles they would be pure noise.
        let dir = std::env::temp_dir().join(format!("ci-discover-caveat-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        fs::write(
            &path,
            format!("{}\n{}\n",
                serde_json::json!({"type":"user","message":{"role":"user",
                    "content":"<local-command-caveat>The command below was run directly…"}}),
                serde_json::json!({"type":"user","message":{"role":"user","content":"真正的第一个提问"}})),
        ).unwrap();
        assert_eq!(
            transcript_title(&path).as_deref(),
            Some("真正的第一个提问"),
            "caveat rows must be skipped, not titled"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_detail_line_skips_noise_and_reports_the_last_real_activity() {
        // The measured complaint: a row whose detail read "存档 · event_msg" --
        // the *type name* of codex's bookkeeping rows, which says nothing. The
        // last real activity was a tool call that sat more than 16 KB from the
        // end behind big reasoning/reply rows and a wall of counter rows, so
        // neither pass of the old reader reached it.
        let dir = std::env::temp_dir().join(format!("ci-discover-detail-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();

        let big_reply = "x".repeat(20 * 1024); // pushes the tool call past a 16 KB tail
        let noisy = dir.join("noisy.jsonl");
        fs::write(
            &noisy,
            format!("{}\n{}\n{}\n{}\n{}\n",
                serde_json::json!({"type":"response_item","payload":{"type":"function_call",
                    "name":"exec_command","arguments":"{\"cmd\":\"python train.py\"}"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":big_reply}]}}),
                serde_json::json!({"type":"token_usage_record"}),
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count"}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"item_completed"}})),
        ).unwrap();
        let hint = tail_hint(&noisy).expect("the tool call is the answer");
        assert!(hint.contains("exec_command"), "names the tool behind the noise: {hint}");
        assert!(hint.contains("python train.py"), "and what it runs: {hint}");

        // No tool call at all: the last reply text is the detail, never a type name.
        let done = dir.join("done.jsonl");
        fs::write(
            &done,
            format!("{}\n{}\n{}\n",
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":"已按规范完成整理，并提交。"}]}}),
                serde_json::json!({"type":"token_usage_record"}),
                serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}})),
        ).unwrap();
        let hint = tail_hint(&done).expect("the last reply is the answer");
        assert!(hint.contains("已按规范完成整理"), "the reply is the detail: {hint}");

        // Only bookkeeping rows: no detail at all, rather than "event_msg".
        let bare = dir.join("bare.jsonl");
        fs::write(
            &bare,
            format!("{}\n", serde_json::json!({"type":"event_msg","payload":{"type":"token_count"}})),
        ).unwrap();
        assert!(tail_hint(&bare).is_none(), "noise alone must not become a detail line");

        // pi message rows still work: text blocks are readable both ways.
        let pi = dir.join("pi.jsonl");
        fs::write(
            &pi,
            format!("{}\n{}\n",
                serde_json::json!({"type":"message","message":{"role":"assistant",
                    "content":[{"type":"text","text":"先看配置再改代码。"}]}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"item_completed"}})),
        ).unwrap();
        let hint = tail_hint(&pi).expect("pi text is readable");
        assert!(hint.contains("先看配置再改代码"), "pi message text: {hint}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn usage_reads_codexs_own_running_total_from_the_tail() {
        // The measured shape (rollouts on this machine carry 941 of these rows
        // across two days): `event_msg`/`token_count` → `payload.info.total_token_usage`,
        // a cumulative counter the harness itself maintains. The newest row IS
        // the session total, so the tail suffices and no summing is done here.
        let dir = std::env::temp_dir().join(format!("ci-discover-usage-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        fs::write(
            &path,
            format!("{}\n{}\n{}\n",
                // An older, smaller counter -- must lose to the newer one below.
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{
                    "total_token_usage":{"input_tokens":28_766,"output_tokens":361}}}}),
                // Noise rows between the two counters.
                serde_json::json!({"type":"event_msg","payload":{"type":"item_completed"}}),
                // The newest row: the answer.
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{
                    "total_token_usage":{"input_tokens":60_762,"output_tokens":664}}}})),
        ).unwrap();
        let usage = transcript_usage(&path).expect("the counter is stated");
        assert_eq!(usage.input_tokens, 60_762, "newest total wins: {usage:?}");
        assert_eq!(usage.output_tokens, 664);

        // A transcript that states no counter states no usage -- an all-zero
        // counter is the same absence, not a session that used nothing.
        let bare = dir.join("bare.jsonl");
        fs::write(
            &bare,
            format!("{}\n{}\n",
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{
                    "total_token_usage":{"input_tokens":0,"output_tokens":0}}}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"item_completed"}})),
        ).unwrap();
        assert!(transcript_usage(&bare).is_none(), "zeros are not usage");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovered_rows_carry_the_usage_the_harness_stated() {
        // End to end through the same discover_in the production scan runs:
        // the counter must reach the row, where scan_once hands it to the
        // registry for both origins to see.
        let root = std::env::temp_dir().join(format!("ci-discover-usage-e2e-{}", now_ms()));
        let dir = root.join(".codex/sessions");
        fs::create_dir_all(&dir).unwrap();
        let thread = "01a139b5-e5a5-77b0-a481-a86d0b4150b9";
        fs::write(
            dir.join(format!("rollout-2026-10-15T13-11-30-{thread}.jsonl")),
            format!("{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"cwd":"/tmp/proj","id":thread}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{
                    "total_token_usage":{"input_tokens":60_762,"output_tokens":664}}}})),
        ).unwrap();
        let processes = vec![AgentProcess {
            harness: Harness::Codex,
            pid: 10,
            cwd: PathBuf::from("/tmp/proj"),
            started_ms: now_ms() - 60_000,
        }];
        let mut cache = ParseCache::default();
        let found = discover_in(&root, processes, now_ms(), 600_000, 50, &mut cache);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].usage,
            Some(codeisland_core::session::Usage { input_tokens: 60_762, output_tokens: 664 }),
            "the harness's own counter reaches the row: {:?}",
            found[0]
        );
        fs::remove_dir_all(&root).ok();
    }
}
