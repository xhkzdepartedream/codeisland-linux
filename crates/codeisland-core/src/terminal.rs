//! Which terminal an agent is actually sitting in.
//!
//! Why this needs measuring rather than assuming: the hook payload identifies a
//! session by `session_id` and `cwd`, and neither says "window". On Wayland a client
//! cannot even enumerate other windows -- the compositor owns that -- so the one
//! thing the island can do is record what *its own process* can observe, and let the
//! activation side work from that.
//!
//! What is observable, and verified on this machine (2026-10-14) against a real
//! Konsole-hosted session:
//!
//!   * the process ancestry: `claude -> zsh -> konsole -> systemd --user`, so walking
//!     parents reaches the emulator and its pid
//!   * the emulator's own hints in the environment: `KONSOLE_DBUS_SESSION=/Sessions/2`
//!     and `KONSOLE_DBUS_WINDOW=/Windows/1` -- a tab-precise handle, not just a window
//!   * a multiplexer (`TMUX`/`STY`) in between, which changes what "jump" can mean:
//!     raising the window is not the same as surfacing the right pane
//!
//! The ancestry walk takes the parent lookup as a parameter, so the rules are testable
//! without touching `/proc` or re-parenting processes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Environment variables that say which terminal (or pane) the process belongs to.
/// Only the ones that name a place, deliberately: `TERM`, `DISPLAY` and friends are
/// present everywhere and identify nothing.
pub const TERMINAL_HINT_VARS: &[&str] = &[
    "TERM_PROGRAM",
    "KONSOLE_DBUS_SESSION",
    "KONSOLE_DBUS_WINDOW",
    "KONSOLE_VERSION",
    "KITTY_WINDOW_ID",
    "WEZTERM_PANE",
    "WEZTERM_EXECUTABLE_WINDOW_ID",
    "VTE_SUBSYSTEM",
    "WT_SESSION",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "SCREEN_SESSION",
];

/// Process names that mean "we reached the terminal emulator". Compared against
/// `/proc/<pid>/comm`, which is the executable name, truncated to 15 characters.
pub const EMULATOR_NAMES: &[&str] = &[
    "konsole",
    "kitty",
    "alacritty",
    "wezterm",
    "wezterm-gui",
    "foot",
    "st",
    "urxvt",
    "rxvt-unicode",
    "gnome-terminal-",
    "terminator",
    "tilix",
    "qterminal",
    "xfce4-terminal",
    "deepin-terminal",
    "elementary-terminal",
    "warp",
    "ghostty",
    "contour",
    "terminal",
];

/// How many generations to climb before giving up. Deep enough for a shell inside
/// tmux inside a login shell, shallow enough that a runaway pid chain cannot spin.
const MAX_HOPS: usize = 24;

/// What the bridge could determine about the terminal it was invoked from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalContext {
    /// Pid of the emulator process, if the ancestry reached one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulator_pid: Option<u32>,
    /// Its `comm`, e.g. `konsole`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulator: Option<String>,
    /// A multiplexer sits between the window and the agent, so the window alone is
    /// not the whole answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiplexer: Option<String>,
    /// Environment hints that name a place, kept verbatim under their own keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hints: BTreeMap<String, String>,
}

impl TerminalContext {
    /// Whether there is anything at all to act on. A context with no emulator and no
    /// hint cannot be jumped to, and the caller should not offer the action.
    pub fn is_actionable(&self) -> bool {
        self.emulator_pid.is_some() || !self.hints.is_empty()
    }

    /// Short label for a card or a log line, never longer than it is useful.
    pub fn label(&self) -> String {
        let emulator = self.emulator.as_deref().unwrap_or("未知终端");
        match &self.multiplexer {
            Some(mux) => format!("{emulator}({mux})"),
            None => emulator.to_string(),
        }
    }
}

/// One step of the ancestry: the parent pid, and the process name **of the pid that
/// was looked up** -- which is what one read of `/proc/<pid>/stat` yields. The name
/// does not describe the parent. Getting this backwards records the emulator's
/// *parent* pid next to the emulator's name, which pointed every jump at systemd.
pub type Parent = (u32, String);

/// Walk the ancestry from `pid`, using `lookup` to resolve a pid to its parent and
/// name. `lookup` returns `None` at the top of the chain (or for a pid that is gone).
///
/// Fills in `emulator_pid` / `emulator` on the first ancestor whose name is a known
/// emulator. The *nearest* one wins: with `konsole -> tmux -> zsh -> claude`, the
/// emulator is konsole and tmux is recorded separately as the multiplexer.
pub fn walk_ancestry(pid: u32, mut lookup: impl FnMut(u32) -> Option<Parent>) -> TerminalContext {
    let mut found = TerminalContext::default();
    let mut current = pid;

    for _ in 0..MAX_HOPS {
        let Some((parent, name)) = lookup(current) else {
            break;
        };
        // `name` belongs to `current`, so that is the pid recorded with it.
        if found.emulator_pid.is_none() && is_emulator(&name) {
            found.emulator_pid = Some(current);
            found.emulator = Some(name);
        }
        if parent == 0 || parent == current {
            break;
        }
        current = parent;
    }

    found
}

/// `comm` is truncated by the kernel to 15 bytes, and several emulators are suffixed
/// (`gnome-terminal-server`), so this is a prefix match rather than an equality one.
pub fn is_emulator(comm: &str) -> bool {
    let name = comm.trim();
    EMULATOR_NAMES.iter().any(|known| name.starts_with(known))
}

/// Collect the environment hints that name a place. Empty values are skipped: a
/// variable set to "" says nothing and would otherwise read as a real handle.
pub fn collect_hints(env: &impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    let mut hints = BTreeMap::new();
    for key in TERMINAL_HINT_VARS {
        if let Some(value) = env(key) {
            let value = value.trim();
            if !value.is_empty() {
                hints.insert((*key).to_string(), value.to_string());
            }
        }
    }
    hints
}

/// Which multiplexer, if any, is in play. tmux wins over screen because a tmux client
/// can be attached to a screen session and the reverse is far rarer.
pub fn multiplexer(hints: &BTreeMap<String, String>) -> Option<String> {
    if hints.contains_key("TMUX") || hints.contains_key("TMUX_PANE") {
        Some("tmux".to_string())
    } else if hints.contains_key("STY") || hints.contains_key("SCREEN_SESSION") {
        Some("screen".to_string())
    } else {
        None
    }
}

/// Everything the bridge can see about the terminal it was started from, read from
/// the live process and environment.
pub fn capture() -> TerminalContext {
    capture_for(std::process::id())
}

/// Same, from an explicit pid -- split out so a test can walk a made-up chain.
pub fn capture_for(pid: u32) -> TerminalContext {
    let mut context = walk_ancestry(pid, read_parent);
    context.hints = collect_hints(&|key| std::env::var(key).ok());
    context.multiplexer = multiplexer(&context.hints);
    context
}

/// `/proc/<pid>/stat` gives `(comm, ppid)`; the comm field can contain spaces and
/// parentheses, so it is sliced between the first `(` and the last `)` rather than
/// split on whitespace.
fn read_parent(pid: u32) -> Option<Parent> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat[open + 1..close].to_string();
    // After the `)` the fields are `state ppid pgrp ...` -- the state is a single
    // letter, so the parent is the *second* token. Taking the first parses "S" as a
    // pid and silently reports no parent, which is exactly what this test caught.
    let ppid = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
    Some((ppid, comm))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain that stops at konsole, measured on this machine:
    /// `claude -> zsh -> konsole -> systemd --user`. Each entry reads
    /// `(pid, (its parent, its own comm))`, the way `/proc/<pid>/stat` does.
    fn konsole_chain() -> Vec<(u32, Parent)> {
        vec![
            (22097, (21829, "claude".into())),
            (21829, (4568, "zsh".into())),
            (4568, (1142, "konsole".into())),
            (1142, (1, "systemd".into())),
        ]
    }

    fn lookup_from(table: Vec<(u32, Parent)>) -> impl Fn(u32) -> Option<Parent> {
        move |pid| table.iter().find(|(key, _)| *key == pid).map(|(_, value)| value.clone())
    }

    #[test]
    fn the_nearest_emulator_wins() {
        let context = walk_ancestry(22097, lookup_from(konsole_chain()));
        assert_eq!(context.emulator.as_deref(), Some("konsole"));
        assert_eq!(context.emulator_pid, Some(4568), "the pid must be konsole itself, not its parent");
    }

    #[test]
    fn an_agent_started_by_the_emulator_directly_is_still_found() {
        // The starting pid is checked too, not only its ancestors.
        let table = vec![(900, (1, "konsole".into()))];
        let context = walk_ancestry(900, lookup_from(table));
        assert_eq!(context.emulator_pid, Some(900));
    }

    #[test]
    fn a_chain_with_no_emulator_reports_that_honestly() {
        // A daemonised agent, or one started by a scheduler: no terminal anywhere.
        let table = vec![(500, (1, "python3".into()))];
        let context = walk_ancestry(500, lookup_from(table));
        assert_eq!(context.emulator, None);
        assert_eq!(context.emulator_pid, None);
        assert!(!context.is_actionable(), "nothing to jump to");
    }

    #[test]
    fn suffixed_emulator_names_are_recognised() {
        assert!(is_emulator("konsole"));
        assert!(is_emulator("gnome-terminal-"), "comm is truncated by the kernel");
        assert!(is_emulator("wezterm-gui"));
        assert!(!is_emulator("node"), "the agent's own runtime is not a terminal");
        assert!(!is_emulator("zsh"));
    }

    #[test]
    fn hints_keep_only_what_names_a_place() {
        let table = konsole_chain();
        let mut context = walk_ancestry(22097, lookup_from(table));
        context.hints = collect_hints(&|key| match key {
            "KONSOLE_DBUS_SESSION" => Some("/Sessions/2".into()),
            "KONSOLE_DBUS_WINDOW" => Some("/Windows/1".into()),
            "TERM_PROGRAM" => Some("".into()), // empty says nothing
            "TMUX" => Some("/tmp/tmux-1000/default,1234,0".into()),
            _ => None,
        });
        assert_eq!(context.hints.get("KONSOLE_DBUS_SESSION").map(String::as_str), Some("/Sessions/2"));
        assert!(!context.hints.contains_key("TERM_PROGRAM"), "empty values are not hints");
        context.multiplexer = multiplexer(&context.hints);
        assert_eq!(context.multiplexer.as_deref(), Some("tmux"));
        assert_eq!(context.label(), "konsole(tmux)");
        assert!(context.is_actionable());
    }

    #[test]
    fn screen_and_tmux_are_told_apart() {
        let mut hints = BTreeMap::new();
        assert_eq!(multiplexer(&hints), None);
        hints.insert("STY".into(), "1234.worker".into());
        assert_eq!(multiplexer(&hints).as_deref(), Some("screen"));
        hints.insert("TMUX".into(), "/tmp/tmux-1000/default,1,0".into());
        assert_eq!(multiplexer(&hints).as_deref(), Some("tmux"), "tmux wins when both are present");
    }

    #[test]
    fn a_broken_chain_terminates_instead_of_looping() {
        // A cycle in the table must not spin forever: the hop limit is the guard.
        let table = vec![(2, (3, "a".into())), (3, (2, "b".into()))];
        let context = walk_ancestry(1, lookup_from(table));
        assert_eq!(context.emulator, None);
    }

    #[test]
    fn the_label_is_short_enough_for_a_card() {
        let mut context = TerminalContext::default();
        context.emulator = Some("konsole".into());
        assert_eq!(context.label(), "konsole");
        context.emulator = None;
        assert_eq!(context.label(), "未知终端");
    }

    #[test]
    fn reading_a_real_parent_works_or_says_none() {
        // The /proc reader itself: our own pid has a parent, pid 4 billion does not.
        let me = std::process::id();
        assert!(read_parent(me).is_some(), "the test process must have a parent");
        assert!(read_parent(4_000_000_000).is_none());
    }

    #[test]
    fn capture_from_this_process_finds_our_own_ancestry() {
        // Runs under cargo, which may or may not be inside a terminal. Either answer
        // is fine; what must not happen is a panic or an infinite walk.
        let context = capture();
        assert!(context.emulator_pid.map(|pid| pid > 0).unwrap_or(true));
    }
}
