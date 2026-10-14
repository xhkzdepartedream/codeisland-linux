/// Jump back to the terminal an agent is sitting in.
///
/// What is verified on this machine (2026-10-14, KDE Plasma 6.7.5 Wayland, Konsole
/// 26.08.2): activating a window by emulator pid works, end to end --
/// `kdotool search --pid <pid> --limit 1` then `kdotool windowactivate <id>` leaves
/// `getactivewindow` pointing at that pid's window.
///
/// What is NOT, and is therefore deliberately absent: tab-precise focus. Three
/// Konsole D-Bus methods were tried against a live session and none produced an
/// observable tab change (the window title, which follows the current tab, never
/// moved): `Window.setCurrentSession(i)` did nothing for any argument, and
/// `Window.setCurrentView(i)` returned `true` while doing nothing. An API that
/// reports success without acting is worse than no API -- it would have shipped a
/// jump that looks like it worked. So the tab handle is only *shown* to the human,
/// never claimed to be focused.
///
/// The command itself is run by the desktop's existing tooling rather than a
/// hand-rolled D-Bus client: `kdotool` is already the KWin scripting bridge on this
/// desktop, and `CODEISLAND_JUMP_COMMAND` covers any machine without it.

use std::process::Command;

use codeisland_core::terminal::TerminalContext;

/// Tool that can resolve a pid to a window and activate it.
const KDTOOL: &str = "kdotool";

/// Optional user override, run through the shell with the target exported.
pub const JUMP_COMMAND_ENV: &str = "CODEISLAND_JUMP_COMMAND";

/// What the jump would do, decided without touching the system so it can be tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Two calls to kdotool: resolve the window for this pid, then activate it.
    Kdotool { pid: u32 },
    /// A user-supplied command; `$CODEISLAND_TARGET_PID` and
    /// `$CODEISLAND_TARGET_SESSION` are exported into it.
    External { command: String, pid: Option<u32>, session: Option<String> },
    /// Nothing to act on: no emulator pid, no override.
    Nowhere,
}

/// Decide how to jump. The override wins: on a desktop where the default tooling is
/// wrong, the user's own command is the answer, and it must not be shadowed.
pub fn plan(target: &TerminalContext, override_command: Option<&str>) -> Plan {
    if let Some(command) = override_command.map(str::trim).filter(|c| !c.is_empty()) {
        return Plan::External {
            command: command.to_string(),
            pid: target.emulator_pid,
            session: target.hints.get("KONSOLE_DBUS_SESSION").cloned(),
        };
    }
    match target.emulator_pid {
        Some(pid) => Plan::Kdotool { pid },
        None => Plan::Nowhere,
    }
}

/// Outcome of a jump attempt, for the daemon log and for tests.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The window was activated.
    Activated { pid: u32, window: String },
    /// Nothing was tried, with the reason.
    Skipped(String),
    /// Something was tried and failed, with whatever the tool said.
    Failed(String),
}

/// Run a plan. Never blocks the caller's thread for longer than the tools do, and
/// never panics on a missing binary -- a desktop without kdotool gets a clear
/// message, not a broken daemon.
pub fn execute(plan: &Plan) -> Outcome {
    match plan {
        Plan::Nowhere => {
            return Outcome::Skipped("这个会话没有可跳转的终端窗口".to_string())
        }
        Plan::External {
            command,
            pid,
            session,
        } => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            if let Some(pid) = pid {
                cmd.env("CODEISLAND_TARGET_PID", pid.to_string());
            }
            if let Some(session) = session {
                cmd.env("CODEISLAND_TARGET_SESSION", session);
            }
            return match cmd.output() {
                Ok(output) if output.status.success() => Outcome::Activated {
                    pid: pid.unwrap_or(0),
                    window: "<外部命令>".to_string(),
                },
                Ok(output) => Outcome::Failed(format!(
                    "跳转命令失败: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(err) => Outcome::Failed(format!("无法运行跳转命令: {err}")),
            };
        }
        Plan::Kdotool { pid } => {
            let window = match run(&[KDTOOL, "search", "--pid", &pid.to_string(), "--limit", "1"]) {
                Ok(text) => {
                    let first = text.lines().map(str::trim).find(|l| !l.is_empty());
                    match first {
                        Some(window) => window.to_string(),
                        None => {
                            return Outcome::Skipped(format!("找不到 pid {pid} 的窗口"))
                        }
                    }
                }
                Err(err) => return Outcome::Failed(err),
            };
            match run(&[KDTOOL, "windowactivate", &window]) {
                Ok(_) => Outcome::Activated { pid: *pid, window },
                Err(err) => Outcome::Failed(err),
            }
        }
    }
}

/// Run a tool and capture stdout, turning "not installed" into a message rather
/// than a panic.
fn run(args: &[&str]) -> Result<String, String> {
    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .map_err(|err| format!("无法运行 {}: {err}", args[0]))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{} 退出 {}: {}",
            args[0],
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn target(pid: Option<u32>, session: Option<&str>) -> TerminalContext {
        let mut hints = BTreeMap::new();
        if let Some(session) = session {
            hints.insert("KONSOLE_DBUS_SESSION".to_string(), session.to_string());
        }
        TerminalContext {
            emulator_pid: pid,
            emulator: pid.map(|_| "konsole".to_string()),
            multiplexer: None,
            hints,
        }
    }

    #[test]
    fn a_known_emulator_pid_becomes_a_kdotool_pair() {
        let plan = plan(&target(Some(4568), Some("/Sessions/2")), None);
        assert_eq!(plan, Plan::Kdotool { pid: 4568 });
    }

    #[test]
    fn the_user_override_wins_over_the_default_tooling() {
        let plan = plan(
            &target(Some(4568), Some("/Sessions/2")),
            Some("mywm-client focus 4568"),
        );
        match plan {
            Plan::External {
                command,
                pid,
                session,
            } => {
                assert_eq!(command, "mywm-client focus 4568");
                assert_eq!(pid, Some(4568));
                assert_eq!(session.as_deref(), Some("/Sessions/2"));
            }
            other => panic!("expected the override to be used, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_override_is_not_a_command() {
        // `Environment=CODEISLAND_JUMP_COMMAND=` must fall back to the default,
        // not try to run nothing.
        let plan = plan(&target(Some(4568), None), Some("   "));
        assert_eq!(plan, Plan::Kdotool { pid: 4568 });
    }

    #[test]
    fn no_emulator_and_no_override_means_nothing_to_do() {
        assert_eq!(plan(&target(None, None), None), Plan::Nowhere);
        // ...but an override still runs: it may know something we could not detect.
        assert!(matches!(
            plan(&target(None, None), Some("true")),
            Plan::External { pid: None, .. }
        ));
    }

    #[test]
    fn executing_a_plan_with_no_target_is_skipped_not_failed() {
        let outcome = execute(&Plan::Nowhere);
        assert!(matches!(outcome, Outcome::Skipped(_)), "{outcome:?}");
    }

    #[test]
    fn a_missing_tool_reports_itself_instead_of_vanishing() {
        // Stands in for "kdotool is not installed here": the error must name the
        // program, so the log says why nothing happened.
        let outcome = execute(&Plan::External {
            command: "definitely-not-a-real-program-42".to_string(),
            pid: None,
            session: None,
        });
        match outcome {
            Outcome::Failed(text) => assert!(
                text.contains("definitely-not-a-real-program-42") || text.contains("sh"),
                "unhelpful message: {text}"
            ),
            other => panic!("expected a failure naming the command, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_override_is_reported_as_activated() {
        let outcome = execute(&Plan::External {
            command: "true".to_string(),
            pid: Some(1),
            session: None,
        });
        assert!(matches!(outcome, Outcome::Activated { pid: 1, .. }), "{outcome:?}");
    }

    #[test]
    fn a_failing_override_carries_its_stderr() {
        let outcome = execute(&Plan::External {
            command: "echo nope >&2; exit 3".to_string(),
            pid: None,
            session: None,
        });
        match outcome {
            Outcome::Failed(text) => assert!(text.contains("nope"), "{text}"),
            other => panic!("expected the stderr to be surfaced, got {other:?}"),
        }
    }
}
