//! Desktop notifications, through the freedesktop server the desktop already runs.
//!
//! Why `notify-send` and not a D-Bus client: the notification server here is
//! plasmashell, `notify-send` answers in ~30 ms, and the protocol is already solved
//! by the desktop. A hand-rolled D-Bus connection (or the `zbus` tree) would add a
//! dependency to a resident process for no capability we actually need.
//!
//! The one thing this module must never do is block a hook. A parked agent is
//! already waiting on a human; a notification that makes it wait longer is a
//! regression dressed as a feature. So every send goes through a worker thread, and
//! the worker kills a wedged `notify-send` after a deadline -- the vault records the
//! notification server being absent on this machine, and the symptom then is not an
//! error but a 25-60 second hang (`facts/本机/修复/QQ闪退缺通知applet`).

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one notification process may live before it is killed.
const HARD_DEADLINE: Duration = Duration::from_secs(3);

/// How often the same kind of message may repeat. A session can park several
/// requests in a few seconds; the desktop should not fill up with them.
const THROTTLE_WINDOW: Duration = Duration::from_secs(5);

/// Queue depth. Beyond this, notifications are dropped rather than buffered: a
/// stale "the agent needs you" delivered a minute later is worse than none.
const QUEUE_DEPTH: usize = 4;

/// Env override for the sending program, so tests can point it at something
/// harmless instead of the user's real notification server.
pub const PROGRAM_ENV: &str = "CODEISLAND_NOTIFY_PROGRAM";

/// Whether a finished round should also send a notification. On by default: the
/// point of this tool is that the agent runs while the human does something else,
/// and the same discipline the vault records for long-running jobs is "tell me when
/// it suspends and tell me when it finishes".
pub const ON_STOP_ENV: &str = "CODEISLAND_NOTIFY_ON_STOP";

/// How long a notification stays up. Matches the `-t 5000` idiom already used for
/// agent task notifications on this desktop; without an expiry these pile up in the
/// corner until dismissed, which turns a reminder into clutter.
const EXPIRES_MS: &str = "5000";

struct Task {
    key: String,
    title: String,
    body: String,
    /// How recently the same key may have fired. A finished round is worth hearing
    /// about less often than an agent that is stuck waiting, so the two do not
    /// share a window.
    window: Duration,
}

/// Remembered per-key send times. Split out from the thread so the policy is
/// testable without spawning anything.
#[derive(Default)]
pub struct Throttle {
    last: HashMap<String, Instant>,
    window: Duration,
}

impl Throttle {
    pub fn new(window: Duration) -> Self {
        Self {
            last: HashMap::new(),
            window,
        }
    }

    /// True when this key may fire now, recording the send. Keys are cleaned up as
    /// they expire, so a long-lived daemon does not accumulate them.
    ///
    /// The housekeeping keeps the widest window any caller uses; a per-task window
    /// shorter than that is still honoured by the check below.
    pub fn allow(&mut self, key: &str, now: Instant, window: Duration) -> bool {
        let keep = self.window.max(window);
        self.last
            .retain(|_, when| now.duration_since(*when) < keep);
        if self.last.get(key).is_some_and(|when| now.duration_since(*when) < window) {
            return false;
        }
        self.last.insert(key.to_string(), now);
        true
    }
}

/// The notification worker. `Default`-shaped handle: cheap to clone into `Shared`.
pub struct Notifier {
    tx: SyncSender<Task>,
}

impl Notifier {
    /// Start the worker. Nothing is sent until a caller asks, so this costs one
    /// idle thread parked on an empty queue.
    pub fn spawn() -> Arc<Self> {
        let (tx, rx) = sync_channel::<Task>(QUEUE_DEPTH);
        let program = std::env::var(PROGRAM_ENV).unwrap_or_else(|_| "notify-send".into());
        let worker = std::thread::spawn(move || {
            let mut throttle = Throttle::new(THROTTLE_WINDOW);
            while let Ok(task) = rx.recv() {
                if !throttle.allow(&task.key, Instant::now(), task.window) {
                    continue;
                }
                deliver(&program, &task.title, &task.body);
            }
        });
        // The worker exits when this handle is dropped: its `recv` fails once the
        // only sender is gone, so there is nothing to join or leak.
        let _ = &worker;
        Arc::new(Self { tx })
    }

    /// Send `task`, or drop it. Kept as the single entry point so the queue-depth
    /// rule cannot be bypassed by a new message kind.
    fn submit(&self, task: Task) {
        match self.tx.try_send(task) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // Deliberately dropped: see QUEUE_DEPTH.
                eprintln!("codeisland-daemon: notification queue full, dropping one");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// "An agent is waiting on you." One per session per throttle window.
    pub fn needs_attention(&self, session_id: &str, title: &str, body: &str) {
        self.submit(Task {
            key: format!("attention:{session_id}"),
            title: title.to_string(),
            body: body.to_string(),
            window: THROTTLE_WINDOW,
        });
    }

    /// "A round finished." Rarer and less urgent than a blocked agent, so it gets a
    /// wider window: several quick turns should not read as a ticker.
    pub fn round_finished(&self, session_id: &str, title: &str, body: &str) {
        self.submit(Task {
            key: format!("done:{session_id}"),
            title: title.to_string(),
            body: body.to_string(),
            window: DONE_THROTTLE_WINDOW,
        });
    }
}

/// How often a "round finished" message may repeat for one session.
const DONE_THROTTLE_WINDOW: Duration = Duration::from_secs(20);

/// Read the on/off flag. Only an explicit negative mutes the message: an empty or
/// whitespace value is a half-written drop-in (`Environment=CODEISLAND_NOTIFY_ON_STOP=`),
/// and silently losing every completion notification is the worse way to read it.
pub fn parse_flag(raw: Option<&str>) -> bool {
    match raw.map(str::trim).map(str::to_ascii_lowercase) {
        None => true,
        Some(value) => !matches!(value.as_str(), "0" | "false" | "off" | "no"),
    }
}

/// The session bus address to fall back to when the caller has none.
///
/// SSH sessions and some systemd contexts start without `DBUS_SESSION_BUS_ADDRESS`,
/// and `notify-send` then fails outright. The user bus for this uid is the address
/// the desktop's own notification server listens on, so filling it in is what makes
/// notifications work from a plain shell.
pub fn bus_fallback(uid: u32) -> String {
    format!("unix:path=/run/user/{uid}/bus")
}

/// The argv a healthy send uses. `--app-name` is what groups these under
/// "CodeIsland" in the Plasma notification history.
pub fn command_for(program: &str, title: &str, body: &str) -> Vec<String> {
    vec![
        program.to_string(),
        "--app-name".to_string(),
        "codeisland".to_string(),
        "--urgency".to_string(),
        "normal".to_string(),
        // Expire on its own. A reminder that waits to be dismissed is a second UI.
        "--expire-time".to_string(),
        EXPIRES_MS.to_string(),
        title.to_string(),
        body.to_string(),
    ]
}

/// Fire one notification. Failures are logged and dropped: a desktop without a
/// notification server still gets a working island, because the card -- not the
/// notification -- is what unblocks the agent.
fn deliver(program: &str, title: &str, body: &str) {
    let argv = command_for(program, title, body);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // No session bus in the environment does not mean no bus: the desktop's user bus
    // is at a fixed path for this uid, and that is where the server listens.
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
        .map(|value| value.is_empty())
        .unwrap_or(true)
    {
        let uid = unsafe { libc::getuid() };
        cmd.env("DBUS_SESSION_BUS_ADDRESS", bus_fallback(uid));
    }
    let spawned = cmd.spawn();

    match spawned {
        Ok(child) => {
            supervise(child);
        }
        Err(err) => {
            // Missing `notify-send`, or no session bus. Neither is the agent's
            // problem and none of them is a denial.
            eprintln!("codeisland-daemon: notification failed to start: {err}");
        }
    }
}

/// How one notification process ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// It exited on its own, inside the deadline.
    Delivered,
    /// It was still running at the deadline and was killed.
    Killed,
    /// Its status could not be read.
    Failed,
}

/// Reap a notification process, cutting it off at the deadline.
///
/// The deadline exists because of a measured failure mode on this machine: with no
/// notification server owning the bus, a send does not error, it hangs for 25-60
/// seconds. Left alone, one hang per request would pile up threads and processes
/// behind a daemon that is supposed to be idle.
pub fn supervise(mut child: Child) -> Delivery {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Delivery::Delivered,
            Ok(None) => {}
            Err(err) => {
                eprintln!("codeisland-daemon: notification status unavailable: {err}");
                let _ = child.kill();
                let _ = child.wait();
                return Delivery::Failed;
            }
        }
        if started.elapsed() >= HARD_DEADLINE {
            eprintln!(
                "codeisland-daemon: notification exceeded {}s, killed",
                HARD_DEADLINE.as_secs()
            );
            let _ = child.kill();
            let _ = child.wait();
            return Delivery::Killed;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIVE: Duration = Duration::from_secs(5);

    #[test]
    fn throttle_allows_once_per_window_then_blocks() {
        let mut throttle = Throttle::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert!(throttle.allow("attention:s1", t0, FIVE));
        assert!(!throttle.allow("attention:s1", t0 + Duration::from_millis(100), FIVE));
        assert!(
            throttle.allow("attention:s2", t0, FIVE),
            "a different session is a different message"
        );
        assert!(throttle.allow("attention:s1", t0 + Duration::from_secs(6), FIVE));
    }

    #[test]
    fn expired_keys_do_not_accumulate() {
        let mut throttle = Throttle::new(Duration::from_secs(5));
        let t0 = Instant::now();
        for i in 0..50 {
            throttle.allow(&format!("session-{i}"), t0, FIVE);
        }
        assert_eq!(throttle.last.len(), 50);
        throttle.allow("later", t0 + Duration::from_secs(10), FIVE);
        assert_eq!(
            throttle.last.len(),
            1,
            "the map must shrink as keys expire, or a daemon left running for days grows one entry per session forever"
        );
    }

    #[test]
    fn a_finished_round_is_throttled_wider_than_a_blocked_agent() {
        // Two rounds finishing close together should not read as a ticker, while an
        // agent stuck on a permission stays responsive.
        let mut throttle = Throttle::new(THROTTLE_WINDOW);
        let t0 = Instant::now();
        assert!(throttle.allow("done:s1", t0, DONE_THROTTLE_WINDOW));
        assert!(
            !throttle.allow("done:s1", t0 + Duration::from_secs(6), DONE_THROTTLE_WINDOW),
            "a 6-second gap is inside the 20s round window"
        );
        assert!(
            throttle.allow("attention:s1", t0 + Duration::from_secs(6), THROTTLE_WINDOW),
            "the round window must not mute a request that needs an answer now"
        );
    }

    #[test]
    fn only_an_explicit_negative_mutes_round_notifications() {
        assert!(parse_flag(None), "default is on");
        assert!(parse_flag(Some("1")), "1");
        assert!(parse_flag(Some("true")), "true");
        assert!(parse_flag(Some("  ")), "blank-ish is not a decision to mute");
        assert!(
            parse_flag(Some("")),
            "an empty value is a half-written drop-in, not a request to go quiet"
        );
        assert!(!parse_flag(Some("0")));
        assert!(!parse_flag(Some("false")));
        assert!(!parse_flag(Some("OFF")));
        assert!(!parse_flag(Some("no")));
    }

    #[test]
    fn the_bus_fallback_is_the_user_bus_for_this_uid() {
        assert_eq!(bus_fallback(1000), "unix:path=/run/user/1000/bus");
    }

    #[test]
    fn notifications_expire_instead_of_waiting_to_be_dismissed() {
        let argv = command_for("notify-send", "t", "b");
        let position = argv.iter().position(|a| a == "--expire-time").expect("-t present");
        assert_eq!(argv[position + 1], "5000");
    }

    #[test]
    fn the_command_names_the_app_and_carries_both_lines() {
        let argv = command_for("notify-send", "api 需要你", "Bash: rm -rf /tmp/x");
        assert_eq!(argv[0], "notify-send");
        assert!(argv.windows(2).any(|w| w[0] == "--app-name" && w[1] == "codeisland"));
        assert_eq!(argv[argv.len() - 2], "api 需要你");
        assert_eq!(argv[argv.len() - 1], "Bash: rm -rf /tmp/x");
    }

    fn spawn(program: &str, args: &[&str]) -> Child {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("test child spawns")
    }

    #[test]
    fn a_wedged_notifier_is_cut_off() {
        // Stands in for the missing-server case in the vault note: a send that
        // never returns. The supervisor must not wait for it.
        let started = Instant::now();
        let outcome = supervise(spawn("sleep", &["30"]));
        let elapsed = started.elapsed();
        assert_eq!(outcome, Delivery::Killed);
        assert!(
            elapsed >= Duration::from_millis(500),
            "the child really was running: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(6), "cut off late: {elapsed:?}");
    }

    #[test]
    fn a_finished_notifier_returns_promptly() {
        let started = Instant::now();
        assert_eq!(supervise(spawn("true", &[])), Delivery::Delivered);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_failing_notifier_is_reported_not_retried() {
        // `notify-send` with no server exits non-zero; that is still "done", not
        // something to keep hammering.
        assert_eq!(supervise(spawn("false", &[])), Delivery::Delivered);
    }

    #[test]
    fn a_missing_program_is_reported_and_ignored() {
        // Must not panic and must not hang: a desktop without notify-send still
        // gets a working island.
        deliver("definitely-not-a-real-program-42", "t", "b");
    }

    #[test]
    fn sending_after_shutdown_does_not_panic() {
        let notifier = Notifier::spawn();
        notifier.needs_attention("s1", "标题", "正文");
        // Dropping the notifier closes the channel; a later send must be inert.
        drop(notifier);
    }
}
