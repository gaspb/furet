//! Which project a directory belongs to, and where a tool keeps its per-project files.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `git -C cwd …` ignoring any GIT_DIR, GIT_WORK_TREE… inherited from the caller, which would
/// otherwise make a folder pass for another repo.
fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd.arg("-C").arg(cwd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
}

fn git(cwd: &Path, args: &[&str]) -> Option<PathBuf> {
    let s = git_output(cwd, args)?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// The main checkout of `dir`'s git repo, so its linked worktrees share it; outside git, `dir`.
/// A folder only counts as part of a repo if it's inside the main checkout or one of the
/// worktrees that repo lists: a `.git` file pointing somewhere doesn't make it so.
pub fn project_root(dir: &Path) -> PathBuf {
    let tree = worktree_root(dir);
    if let Some(common) = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) && common.file_name().is_some_and(|n| n == ".git")
        && let Some(main) = common.parent()
    {
        let main = main.canonicalize().unwrap_or_else(|_| main.to_path_buf());
        let listed = || {
            git_output(&main, &["worktree", "list", "--porcelain"]).is_some_and(|out| {
                out.lines()
                    .filter_map(|l| l.strip_prefix("worktree "))
                    .any(|w| Path::new(w).canonicalize().is_ok_and(|w| w == tree))
            })
        };
        if tree.starts_with(&main) || listed() {
            return main;
        }
    }
    tree
}

/// The top of the working tree `dir` is in (a linked worktree stays itself); outside git, `dir`.
pub fn worktree_root(dir: &Path) -> PathBuf {
    let top = git(dir, &["rev-parse", "--show-toplevel"]).unwrap_or_else(|| dir.to_path_buf());
    top.canonicalize().unwrap_or(top)
}

/// A path as a single directory name, distinct for every path: a readable tail and a hash of the
/// whole path (`/home/me/src/api` → `src-api-1f3b9c2d4e5a6b7c`).
pub fn encode(root: &Path) -> String {
    let full = root.to_string_lossy();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in full.as_bytes() {
        hash = (hash ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    let parts: Vec<&str> = full.split('/').filter(|p| !p.is_empty()).collect();
    let tail = parts[parts.len().saturating_sub(2)..].join("-");
    let tail: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    format!("{tail}-{hash:016x}")
}

/// How `encode` named folders before it was made unambiguous (`/home/me/api` → `-home-me-api`).
pub fn legacy_encode(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

pub fn config_home() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

pub fn data_home() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

pub fn state_home() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state")
}

/// `~/…` and relative paths, resolved against `base`.
pub fn expand(path: &str, base: &Path) -> PathBuf {
    if path == "~" {
        return home();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home().join(rest);
    }
    base.join(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths_as_one_dir_name_without_collisions() {
        assert_eq!(
            legacy_encode(Path::new("/home/me/my api")),
            "-home-me-my-api"
        );
        let names: Vec<String> = ["/x/a-b", "/x/a/b", "/x/a b", "/x/a_b"]
            .iter()
            .map(|p| encode(Path::new(p)))
            .collect();
        assert!(names[0].starts_with("x-a-b-") && names[3].starts_with("x-a_b-"));
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(encode(Path::new("/x/a/b")), encode(Path::new("/x/a/b")));
    }

    #[test]
    fn expands_relative_paths_against_base() {
        assert_eq!(
            expand("a/.env", Path::new("/r")),
            PathBuf::from("/r/a/.env")
        );
        assert_eq!(expand("/abs", Path::new("/r")), PathBuf::from("/abs"));
    }
}
