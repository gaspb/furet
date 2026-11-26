//! furet was called dbx: its connections, history and panel move over on first run.

use std::fs;
use std::path::Path;

pub const OLD: &str = "dbx";

pub fn from_dbx() {
    if std::env::var_os("FURET_HOME").is_none() {
        config(
            &common::scope::config_home().join(OLD),
            &crate::config::home(),
        );
    }
    if std::env::var_os("FURET_STATE").is_none() {
        state(
            &common::scope::state_home().join(OLD),
            &crate::engine::server::dir(),
        );
    }
}

fn config(old: &Path, new: &Path) {
    if new.exists() || !old.is_dir() || fs::rename(old, new).is_err() {
        return;
    }
    let mut files = vec![new.to_path_buf()];
    if let Ok(projects) = fs::read_dir(new.join("projects")) {
        files.extend(projects.flatten().map(|e| e.path()));
    }
    for dir in files {
        let _ = fs::rename(dir.join("dbx.toml"), dir.join(crate::config::FILE));
    }
}

/// The old engine is stopped first, so its tunnels free their ports and the lock is let go.
fn state(old: &Path, new: &Path) {
    if new.exists() || !old.is_dir() {
        return;
    }
    crate::engine::server::stop_in(old);
    let _ = fs::rename(old, new);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_the_config_and_renames_connection_files() {
        let base = std::env::temp_dir().join(format!("furet-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (old, new) = (base.join("dbx"), base.join("furet"));
        fs::create_dir_all(old.join("projects/app-1234")).unwrap();
        fs::write(old.join("dbx.toml"), "global").unwrap();
        fs::write(old.join("projects/app-1234/dbx.toml"), "app").unwrap();
        fs::write(old.join("projects/app-1234/.root"), "/src/app").unwrap();
        config(&old, &new);
        assert!(!old.exists());
        assert_eq!(
            fs::read_to_string(new.join("furet.toml")).unwrap(),
            "global"
        );
        assert_eq!(
            fs::read_to_string(new.join("projects/app-1234/furet.toml")).unwrap(),
            "app"
        );
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("dbx.toml"), "newer").unwrap();
        config(&old, &new);
        assert_eq!(
            fs::read_to_string(new.join("furet.toml")).unwrap(),
            "global"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
