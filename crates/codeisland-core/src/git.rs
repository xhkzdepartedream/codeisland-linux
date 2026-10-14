//! Which branch is a working directory on, read from disk.
//!
//! A faithful port of upstream's `GitBranchReader.swift`, including its central decision:
//! **never spawn `git`**. Sessions fire hooks many times a minute, and a subprocess per event
//! would cost far more than reading two small files; the cwd may also be under a sandbox where
//! a subprocess is not permitted at all. `HEAD` and the `.git` pointer are stable, documented
//! formats, so reading them is the cheaper *and* more reliable path.
//!
//! Handles the three shapes that actually occur:
//!
//! - a normal checkout: `<repo>/.git/HEAD`, opened by walking up from the cwd;
//! - a linked worktree or submodule: `.git` is a *file* containing `gitdir: <path>`;
//! - a detached HEAD: `HEAD` holds a 40-hex sha instead of a ref, and the short sha is the
//!   most useful label available.
//!
//! The upward walk is depth-capped. A bogus cwd (a deleted directory, or `/`) must not send
//! this scanning toward the filesystem root on every scan.

use std::fs;
use std::path::{Path, PathBuf};

/// How far up from a cwd to look for `.git`. Deep enough for real projects, shallow enough
/// that a stray cwd cannot turn every scan into a filesystem walk.
const MAX_DEPTH: usize = 12;

/// The checked-out branch (or short sha) for `cwd`, or `None` when there is no repository.
pub fn head_branch(cwd: &Path) -> Option<String> {
    let git = find_git_dir(cwd)?;
    let head = fs::read_to_string(git.join("HEAD")).ok()?;
    parse_head(&head)
}

/// Walk up from `cwd` to the nearest `.git`, resolving the `gitdir:` pointer file that linked
/// worktrees and submodules use in place of a directory.
fn find_git_dir(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd;
    for _ in 0..MAX_DEPTH {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let body = fs::read_to_string(&candidate).ok()?;
            let pointer = parse_gitdir_pointer(&body)?;
            // A relative pointer is resolved against the directory holding `.git`, which is
            // how submodules record their path.
            let resolved = if pointer.is_absolute() {
                pointer
            } else {
                dir.join(pointer)
            };
            return Some(normalize(&resolved));
        }
        dir = dir.parent()?;
    }
    None
}

/// `.git` pointer file body: `gitdir: <absolute-or-relative path>`.
fn parse_gitdir_pointer(body: &str) -> Option<PathBuf> {
    let line = body.lines().find(|l| !l.trim().is_empty())?;
    let rest = line.trim().strip_prefix("gitdir:")?.trim();
    if rest.is_empty() {
        return None;
    }
    Some(PathBuf::from(rest))
}

/// `ref: refs/heads/<branch>` becomes `<branch>`; a detached 40-hex HEAD becomes its first
/// seven characters, which is what `git` itself would print and all that fits on a card.
fn parse_head(body: &str) -> Option<String> {
    let line = body.lines().find(|l| !l.trim().is_empty())?;
    let line = line.trim();
    if let Some(reference) = line.strip_prefix("ref:") {
        let reference = reference.trim();
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        if name.is_empty() {
            return None;
        }
        return Some(name.to_string());
    }
    // A detached HEAD is a bare sha. Anything else (an empty or half-written file) is not a
    // branch and must not be presented as one.
    if line.len() >= 7 && line.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(line[..7].to_string());
    }
    None
}

/// Remove `.` and `..` components without touching the filesystem, so a `gitdir:` pointer into
/// a directory that no longer exists still yields the path the file meant.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ci-branch-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_normal_checkout_reports_its_branch() {
        let dir = scratch("normal");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feat/dashboard\n").unwrap();
        let nested = dir.join("src/components");
        fs::create_dir_all(&nested).unwrap();
        // The walk must find the repository from a subdirectory, which is the common case:
        // an agent's cwd is wherever the work is, not the repository root.
        assert_eq!(head_branch(&nested).as_deref(), Some("feat/dashboard"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_detached_head_reports_a_short_sha() {
        let dir = scratch("detached");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "4b825dc642cb6eb9a060e54bf8d69288fbee4904\n").unwrap();
        assert_eq!(head_branch(&dir).as_deref(), Some("4b825dc"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_worktree_pointer_file_is_followed() {
        // `git worktree add` makes `.git` a *file* pointing at the real gitdir. Reading it as
        // a directory would silently report no branch on every worktree.
        let root = scratch("worktree");
        let real = root.join("main/.git/worktrees/feature");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();
        let wt = root.join("feature");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/feature\n").unwrap();
        assert_eq!(head_branch(&wt).as_deref(), Some("feature-x"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_repository_is_not_an_error_and_not_a_branch() {
        let dir = scratch("bare");
        assert_eq!(head_branch(&dir), None);
        // A `.git` directory with a malformed HEAD must also yield nothing rather than a
        // half-written string presented as a branch name.
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "").unwrap();
        assert_eq!(head_branch(&dir), None);
        fs::write(dir.join(".git/HEAD"), "not a ref at all\n").unwrap();
        assert_eq!(head_branch(&dir), None);
        fs::remove_dir_all(&dir).ok();
    }
}
