//! Which project a directory belongs to, and where a tool keeps its per-project files.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(cwd: &Path, args: &[&str]) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// The main checkout of `dir`'s git repo, so clones' linked worktrees share it; outside git, `dir`.
pub fn project_root(dir: &Path) -> PathBuf {
    if let Some(common) = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) && common.file_name().is_some_and(|n| n == ".git")
        && let Some(parent) = common.parent()
    {
        return parent.to_path_buf();
    }
    worktree_root(dir)
}

/// The top of the working tree `dir` is in (a linked worktree stays itself); outside git, `dir`.
pub fn worktree_root(dir: &Path) -> PathBuf {
    git(dir, &["rev-parse", "--show-toplevel"])
        .unwrap_or_else(|| dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()))
}

/// A path as a single directory name: `/home/me/api` → `-home-me-api`.
pub fn encode(root: &Path) -> String {
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
    fn encodes_paths_as_one_dir_name() {
        assert_eq!(encode(Path::new("/home/me/my api")), "-home-me-my-api");
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
