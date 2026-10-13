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

    #[test]
    fn default_path_is_uid_scoped() {
        std::env::remove_var(SOCKET_PATH_ENV);
        let path = socket_path();
        let text = path.to_string_lossy().into_owned();
        assert!(text.starts_with("/tmp/codeisland-"), "got {text}");
        assert!(text.ends_with(".sock"), "got {text}");
    }

    #[test]
    fn env_override_wins() {
        std::env::set_var(SOCKET_PATH_ENV, "/tmp/codeisland-test-override.sock");
        assert_eq!(
            socket_path(),
            PathBuf::from("/tmp/codeisland-test-override.sock")
        );
        std::env::remove_var(SOCKET_PATH_ENV);
    }
}
