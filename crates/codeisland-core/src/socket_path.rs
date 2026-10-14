//! Where the daemon listens for hook events.
//!
//! The path scheme matches the upstream macOS project so the two can coexist
//! on a shared home directory without colliding.

use std::path::PathBuf;

/// Environment override, honoured by both the bridge and the daemon.
pub const SOCKET_PATH_ENV: &str = "CODEISLAND_SOCKET_PATH";

/// Resolve the socket path.
///
/// Order of precedence:
/// 1. `CODEISLAND_SOCKET_PATH` -- used by tests and by SSH remote setups where
///    the socket lives outside the default location.
/// 2. `/tmp/codeisland-<uid>.sock` -- uid-scoped so two users on one machine
///    never fight over a single path.
pub fn socket_path() -> PathBuf {
    if let Ok(explicit) = std::env::var(SOCKET_PATH_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    // SAFETY: getuid() is always safe to call and cannot fail.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/codeisland-{uid}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// The process environment is shared, and cargo runs tests on parallel threads.
    /// Without a lock, one test's `remove_var` lands between another test's
    /// `set_var` and its assertion -- a real flake this suite produced on 2026-10-13.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set `SOCKET_PATH_ENV` and hand back whatever was there, so the caller can
    /// put it back. Callers must already hold `env_lock()`.
    fn set_env(value: Option<&str>) -> Option<std::ffi::OsString> {
        let previous = std::env::var_os(SOCKET_PATH_ENV);
        match value {
            Some(set) => std::env::set_var(SOCKET_PATH_ENV, set),
            None => std::env::remove_var(SOCKET_PATH_ENV),
        }
        previous
    }

    fn restore_env(previous: Option<std::ffi::OsString>) {
        match previous {
            Some(old) => std::env::set_var(SOCKET_PATH_ENV, old),
            None => std::env::remove_var(SOCKET_PATH_ENV),
        }
    }

    /// Run `body` with `SOCKET_PATH_ENV` forced to `value`, then restore the
    /// environment exactly as it was -- including when the variable came from the
    /// caller's shell, which a blanket `remove_var` would silently destroy.
    ///
    /// Not reentrant: the lock is held for the duration, so a nested call would
    /// deadlock. Compose `set_env`/`restore_env` under one `env_lock()` instead.
    fn with_socket_env<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let guard = env_lock();
        let previous = set_env(value);
        // Restore on the way out even if the assertions panic.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        restore_env(previous);
        drop(guard);
        outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    #[test]
    fn default_path_is_uid_scoped() {
        let text = with_socket_env(None, || socket_path().to_string_lossy().into_owned());
        assert!(text.starts_with("/tmp/codeisland-"), "got {text}");
        assert!(text.ends_with(".sock"), "got {text}");
    }

    #[test]
    fn env_override_wins() {
        let found = with_socket_env(
            Some("/tmp/codeisland-test-override.sock"),
            || socket_path(),
        );
        assert_eq!(found, PathBuf::from("/tmp/codeisland-test-override.sock"));
    }

    /// The point of restoring rather than clearing: a suite run from a shell that
    /// already exported an override must not end up testing something else.
    #[test]
    fn an_inherited_override_is_put_back_afterwards() {
        let guard = env_lock();
        // What the caller's shell had before this test touched anything. That is the
        // state the suite owes them at the end -- not "whatever we set last".
        let started_with = std::env::var_os(SOCKET_PATH_ENV);
        let outer = set_env(Some("/tmp/codeisland-outer.sock"));

        let inner = set_env(Some("/tmp/codeisland-inner.sock"));
        let seen = socket_path();
        restore_env(inner);
        let after_inner = std::env::var_os(SOCKET_PATH_ENV);

        restore_env(outer);
        let after_outer = std::env::var_os(SOCKET_PATH_ENV);
        drop(guard);

        assert_eq!(seen, PathBuf::from("/tmp/codeisland-inner.sock"));
        assert_eq!(
            after_inner.as_deref(),
            Some(std::ffi::OsStr::new("/tmp/codeisland-outer.sock")),
            "the inner scope must uncover the outer override, not erase it"
        );
        assert_eq!(
            after_outer, started_with,
            "the caller's own environment must survive the suite"
        );
    }
}
