//! Installing and re-checking the Claude Code hooks this project needs.
//!
//! The interesting half here is not writing the file, it is *noticing that the
//! file stopped saying what we think it says*. Claude Code regenerates
//! `settings.json` when its configuration changes and does not preserve keys an
//! external installer put there: measured on 2026-09-30, a settings file lost all
//! nine lifecycle hooks and an explicit `autoMemoryEnabled` in one such rewrite,
//! while the handover documentation still described them as installed. The agent
//! kept running; the collection silently stopped.
//!
//! So everything here is written against two rules:
//!
//!   1. **Never remove a key we did not put there.** Unrelated settings, and the
//!      user's own hooks for the same event, survive untouched.
//!   2. **A missing hook is a reportable condition, not a mystery.** `verify`
//!      answers "is the island still wired up?" without needing a live session,
//!      which is what makes the failure above detectable at all.

use std::path::Path;

use serde_json::{Map, Value};

/// The events we install a hook for. Order matters only for reporting.
pub const HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

/// Events codex dispatches, measured on codex-cli 0.162.1: a project-scoped
/// `.codex/hooks.json` holding these six produced six daemon-visible events, and
/// `SessionEnd` was never among them.
///
/// This is deliberately *not* claude's list. Installing a hook for an event the CLI does
/// not dispatch looks harmless -- `verify` would report it healthy -- but it is a lie that
/// silently ages into "the island stopped working" the first time someone reads the config.
pub const CODEX_EVENTS: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
];

/// The event vocabulary belongs to the CLI, and `--source` already names the CLI, so a
/// caller never chooses twice.
pub fn events_for(source: &str) -> &'static [&'static str] {
    match source {
        "codex" => &CODEX_EVENTS,
        _ => &HOOK_EVENTS,
    }
}

/// Per-event hook timeout, in seconds.
///
/// `PermissionRequest` is the outlier on purpose: the bridge blocks on the socket
/// until a human answers, and the daemon holds a request for up to an hour. A
/// short timeout here would make the CLI kill our own bridge mid-wait, so the
/// island would look stuck while the agent had already moved on.
pub fn timeout_for(event: &str) -> u64 {
    match event {
        "PermissionRequest" => 3600,
        "PreToolUse" => 30,
        _ => 10,
    }
}

/// The command line we install. `--source` is not optional: the bridge picks its
/// event vocabulary from it.
pub fn command_for(bridge: &Path, source: &str) -> String {
    format!("{} --source {}", bridge.display(), source)
}

/// Marker used to recognise *our* hook entry among entries we must leave alone.
///
/// Matching on the binary name rather than the whole command means a moved build
/// directory is detected as stale instead of being treated as somebody else's
/// hook -- which matters, because a hook pointing at a deleted binary fails
/// silently in exactly the way this module exists to catch.
fn is_ours(command: &str) -> bool {
    command.contains("codeisland-bridge")
}

/// What an install did, so callers can report it and tests can assert on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallReport {
    /// Events that had no hook of ours and got one appended.
    pub added: Vec<String>,
    /// Events whose hook of ours existed but pointed somewhere else, and was repointed.
    pub refreshed: Vec<String>,
    /// Events already correct, left byte-identical.
    pub unchanged: usize,
    /// Events we refused to touch because the user's file is not shaped as expected.
    pub skipped: Vec<String>,
}

impl InstallReport {
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.refreshed.is_empty()
    }
}

/// Parse settings text, refusing to guess at anything that is not an object.
///
/// A settings file that is an array or a bare string is not something to merge
/// into -- writing there would destroy the user's configuration.
pub fn parse(raw: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(raw).map_err(|err| format!("不是合法 JSON：{err}"))?;
    if !value.is_object() {
        return Err("顶层不是 JSON 对象，拒绝写入".to_string());
    }
    Ok(value)
}

/// Insert or repair our hooks in `settings`, in place.
pub fn install(settings: &mut Value, bridge: &Path, source: &str) -> InstallReport {
    let mut report = InstallReport::default();
    let command = command_for(bridge, source);
    let events = events_for(source);

    let root = match settings.as_object_mut() {
        Some(root) => root,
        None => {
            report.skipped = events.iter().map(|e| (*e).to_string()).collect();
            return report;
        }
    };

    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = match hooks.as_object_mut() {
        Some(hooks) => hooks,
        // `hooks` exists but is not an object (a string, an array, ...). That is
        // the user's data, not ours to reshape.
        None => {
            report.skipped = events.iter().map(|e| (*e).to_string()).collect();
            return report;
        }
    };

    for event in events.iter().copied() {
        let entry = hooks.entry(event.to_string()).or_insert_with(|| Value::Array(Vec::new()));
        let list = match entry.as_array_mut() {
            Some(list) => list,
            None => {
                report.skipped.push(event.to_string());
                continue;
            }
        };

        let ours = list.iter_mut().find_map(|matcher| {
            let hooks = matcher.get_mut("hooks")?.as_array_mut()?;
            // Keep this matcher only if one of its commands is ours. The search is
            // the decision, so it is named as one rather than as an unused binding.
            let contains_ours = hooks.iter().any(|inner| {
                inner
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_ours)
            });
            contains_ours.then_some(hooks)
        });

        match ours {
            Some(hooks) => {
                let stale = {
                    let mut stale = false;
                    for inner in hooks.iter_mut() {
                        let current = inner.get("command").and_then(Value::as_str).unwrap_or("");
                        if !is_ours(current) {
                            continue;
                        }
                        if current != command {
                            inner["command"] = Value::String(command.clone());
                            stale = true;
                        }
                    }
                    stale
                };
                if stale {
                    report.refreshed.push(event.to_string());
                } else {
                    report.unchanged += 1;
                }
            }
            None => {
                list.push(serde_json::json!({
                    "matcher": "",
                    "hooks": [{
                        "type": "command",
                        "command": command,
                        "timeout": timeout_for(event),
                    }]
                }));
                report.added.push(event.to_string());
            }
        }
    }

    report
}

/// Everything currently wrong with the hooks in `settings`.
///
/// An empty vector means the island is still wired up. This is the check that
/// turns "the agent stopped reporting" into a one-line answer.
pub fn verify(settings: &Value, bridge: &Path, source: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let command = command_for(bridge, source);
    let events = events_for(source);

    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        problems.push("配置里没有 hooks 块（被 CLI 重生成掉了？claude 是 settings.json，codex 是 hooks.json）".to_string());
        return problems;
    };

    for event in events.iter().copied() {
        let Some(list) = hooks.get(event) else {
            problems.push(format!("{event}: 缺少本项目的 hook"));
            continue;
        };
        let Some(list) = list.as_array() else {
            problems.push(format!("{event}: hooks 条目不是数组，安装器会跳过它"));
            continue;
        };

        let ours: Vec<&str> = list
            .iter()
            .flat_map(|matcher| matcher.get("hooks").and_then(Value::as_array))
            .flatten()
            .filter_map(|inner| inner.get("command").and_then(Value::as_str))
            .filter(|command| is_ours(command))
            .collect();

        match ours.as_slice() {
            [] => problems.push(format!("{event}: 缺少本项目的 hook")),
            [only] if *only == command => {}
            [only] => problems.push(format!("{event}: hook 指向了别处的桥接程序（当前 {only}）")),
            many => problems.push(format!("{event}: 有 {} 个本项目的 hook，重复安装", many.len())),
        }
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bridge() -> PathBuf {
        PathBuf::from("/opt/codeisland/target/release/codeisland-bridge")
    }

    fn settings_with(raw: &str) -> Value {
        parse(raw).expect("test fixture must be a JSON object")
    }

    #[test]
    fn an_empty_file_gets_every_event_installed() {
        let mut settings = settings_with("{}");
        let report = install(&mut settings, &bridge(), "claude");
        assert_eq!(report.added, HOOK_EVENTS.to_vec());
        assert!(report.changed());
        assert!(verify(&settings, &bridge(), "claude").is_empty(), "{:?}", verify(&settings, &bridge(), "claude"));
    }

    /// The regression this whole module exists for: a settings rewrite that took
    /// our hooks with it must be *reported*, not quietly accepted.
    #[test]
    fn verify_notices_hooks_that_vanished_in_a_rewrite() {
        let mut settings = settings_with("{}");
        install(&mut settings, &bridge(), "claude");
        assert!(verify(&settings, &bridge(), "claude").is_empty());

        // Claude Code regenerates the file: our block is gone, other keys moved on.
        let rewritten = settings_with(r#"{"model":"sonnet","permissions":{"defaultMode":"bypassPermissions"}}"#);
        let problems = verify(&rewritten, &bridge(), "claude");
        assert_eq!(problems.len(), 1, "a missing hooks block is one clear problem: {problems:?}");
        assert!(problems[0].contains("hooks"));
    }

    #[test]
    fn installing_twice_changes_nothing() {
        let mut settings = settings_with("{}");
        install(&mut settings, &bridge(), "claude");
        let before = settings.clone();
        let second = install(&mut settings, &bridge(), "claude");
        assert!(!second.changed(), "a repeat install must be a no-op: {second:?}");
        assert_eq!(second.unchanged, HOOK_EVENTS.len());
        assert_eq!(settings, before, "idempotence must reach the bytes, not just the verdict");
    }

    /// Unrelated keys are the user's. `autoMemoryEnabled` is in here because that
    /// is exactly the key the 2026-09-30 rewrite lost alongside the hooks.
    #[test]
    fn unrelated_settings_survive_an_install() {
        let mut settings = settings_with(
            r#"{"model":"haiku","autoMemoryEnabled":false,"env":{"FOO":"bar"}}"#,
        );
        install(&mut settings, &bridge(), "claude");
        assert_eq!(settings["model"], "haiku");
        assert_eq!(settings["autoMemoryEnabled"], Value::Bool(false));
        assert_eq!(settings["env"]["FOO"], "bar");
    }

    #[test]
    fn the_users_own_hooks_for_an_event_are_kept() {
        let mut settings = settings_with(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"/usr/bin/my-audit.sh","timeout":5}]}]}}"#,
        );
        let report = install(&mut settings, &bridge(), "claude");
        assert!(report.added.contains(&"PreToolUse".to_string()));

        let list = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(list.len(), 2, "the user's entry and ours, side by side");
        assert_eq!(list[0]["hooks"][0]["command"], "/usr/bin/my-audit.sh");
        assert_eq!(list[0]["matcher"], "Bash", "their matcher must not be flattened");
        assert!(verify(&settings, &bridge(), "claude").is_empty());
    }

    #[test]
    fn a_moved_build_directory_is_repaired_not_duplicated() {
        let mut settings = settings_with("{}");
        install(&mut settings, &PathBuf::from("/old/place/codeisland-bridge"), "claude");

        let report = install(&mut settings, &bridge(), "claude");
        assert!(report.added.is_empty(), "a stale entry is refreshed, not appended: {report:?}");
        assert_eq!(report.refreshed.len(), HOOK_EVENTS.len());

        let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(command, command_for(&bridge(), "claude"));
        assert!(verify(&settings, &bridge(), "claude").is_empty());
    }

    #[test]
    fn verify_reports_a_stale_path_and_a_double_install() {
        let mut settings = settings_with("{}");
        install(&mut settings, &bridge(), "claude");

        let problems = verify(&settings, &PathBuf::from("/somewhere/else/codeisland-bridge"), "claude");
        assert_eq!(problems.len(), HOOK_EVENTS.len(), "every event points at the wrong binary");
        assert!(problems[0].contains("别处"));

        // Same command twice: a duplicate is worth complaining about, because it
        // means two bridges park on one event.
        let duplicated = settings.clone();
        let mut hooks = duplicated["hooks"]["Stop"].as_array().unwrap().clone();
        hooks.push(hooks[0].clone());
        let mut settings = duplicated;
        settings["hooks"]["Stop"] = Value::Array(hooks);
        let problems = verify(&settings, &bridge(), "claude");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("重复安装"), "{problems:?}");
    }

    #[test]
    fn a_hooks_value_we_cannot_merge_with_is_skipped_not_clobbered() {
        let mut settings = settings_with(r#"{"hooks":"not-an-object"}"#);
        let before = settings.clone();
        let report = install(&mut settings, &bridge(), "claude");
        assert_eq!(report.skipped.len(), HOOK_EVENTS.len());
        assert_eq!(settings, before, "refusing to write must be literal, not rhetorical");

        let problems = verify(&settings, &bridge(), "claude");
        assert!(!problems.is_empty());
    }

    #[test]
    fn a_non_object_file_is_refused() {
        assert!(parse(r#"["not","an","object"]"#).is_err());
        assert!(parse("nonsense").is_err());
        assert!(parse("{}").is_ok());
    }

    #[test]
    fn timeouts_match_the_shipped_template() {
        // agents/claude-project-hooks.json is the shape this module writes. If one
        // drifts, the docs and the installer disagree -- which is the same class of
        // bug as a settings file that no longer matches the handover doc.
        let raw = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../agents/claude-project-hooks.json"),
        )
        .expect("template must be readable from the crate source");
        let template: Value = serde_json::from_str(&raw).expect("template must be valid JSON");
        let hooks = template["hooks"].as_object().expect("template hooks");

        assert_eq!(hooks.len(), HOOK_EVENTS.len(), "template and installer disagree on the event list");
        for event in HOOK_EVENTS {
            let entry = &hooks[event];
            assert_eq!(
                entry[0]["hooks"][0]["timeout"].as_u64(),
                Some(timeout_for(event)),
                "{event}: template timeout disagrees with timeout_for()"
            );
            assert_eq!(entry[0]["matcher"], "", "{event}: template matcher drifted");
        }
    }

    #[test]
    fn codex_installs_only_the_events_it_measured() {
        // Six, in codex's own order, and never SessionEnd: an event the CLI does not
        // dispatch would sit in the config looking healthy forever.
        assert_eq!(events_for("codex"), &CODEX_EVENTS);
        assert_eq!(events_for("codex").len(), 6);
        assert!(!events_for("codex").contains(&"SessionEnd"));
        assert_eq!(events_for("claude").len(), 7);
        assert_eq!(events_for("trae"), events_for("claude"), "unknown sources keep claude's vocabulary");
    }

    #[test]
    fn codex_install_skips_the_event_claude_has_and_codex_does_not() {
        let mut settings = Value::Object(Map::new());
        let report = install(&mut settings, Path::new("/opt/ci/codeisland-bridge"), "codex");
        assert_eq!(report.added.len(), 6, "codex must get exactly its six events");
        let hooks = settings["hooks"].as_object().unwrap();
        assert!(!hooks.contains_key("SessionEnd"), "codex config gained an event it never fires");
        assert!(hooks.contains_key("PermissionRequest"));
        // The command still names the source, or the bridge picks the wrong vocabulary.
        assert_eq!(hooks["Stop"][0]["hooks"][0]["command"].as_str().unwrap(),
                   "/opt/ci/codeisland-bridge --source codex");
        assert_eq!(verify(&settings, Path::new("/opt/ci/codeisland-bridge"), "codex"), Vec::<String>::new());
    }

    #[test]
    fn verify_for_codex_does_not_complain_about_the_missing_session_end() {
        // A claude-shaped verify() run over a codex file would report one phantom problem
        // per config; the per-source event list is what prevents that.
        let mut settings = Value::Object(Map::new());
        install(&mut settings, Path::new("/opt/ci/codeisland-bridge"), "codex");
        let problems = verify(&settings, Path::new("/opt/ci/codeisland-bridge"), "codex");
        assert!(problems.is_empty(), "{problems:?}");
        let claude_problems = verify(&settings, Path::new("/opt/ci/codeisland-bridge"), "claude");
        assert_eq!(claude_problems.iter().filter(|p| p.starts_with("SessionEnd")).count(), 1,
                   "claude's verifier must still notice a missing SessionEnd in a codex-only file");
    }

}
