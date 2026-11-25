//! Whether a newer dbx is out (asked of GitHub at most twice a day, remembered in the state
//! dir), and `dbx update`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RELEASES: &str = "https://api.github.com/repos/gaspb/tui-tools/releases?per_page=30";
const TAG_PREFIX: &str = "dbx-v";
const EVERY: Duration = Duration::from_secs(12 * 3600);
pub const INSTALL: &str =
    "curl -LsSf https://github.com/gaspb/tui-tools/releases/latest/download/dbx-installer.sh | sh";

static LATEST: Mutex<Option<String>> = Mutex::new(None);

#[derive(Serialize, Deserialize, Default)]
struct Seen {
    checked: u64,
    latest: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn version(v: &str) -> Vec<u64> {
    v.split(['.', '-']).map_while(|p| p.parse().ok()).collect()
}

/// A newer release than this binary, if the last check found one.
pub fn available() -> Option<String> {
    let latest = LATEST.lock().unwrap().clone()?;
    (version(&latest) > version(env!("CARGO_PKG_VERSION"))).then_some(latest)
}

fn fetch() -> Result<Option<String>> {
    let body = ureq::get(RELEASES)
        .header("User-Agent", concat!("dbx/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .call()?
        .body_mut()
        .read_to_string()?;
    newest(&body)
}

/// The newest published dbx in a GitHub releases listing (the repo has other tools' tags too).
fn newest(body: &str) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
        draft: bool,
        prerelease: bool,
    }
    let releases: Vec<Release> = serde_json::from_str(body)?;
    Ok(releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| r.tag_name.strip_prefix(TAG_PREFIX).map(String::from))
        .max_by_key(|v| version(v)))
}

/// Keeps LATEST current from a background thread; `DBX_NO_UPDATE_CHECK=1` turns it off.
pub fn watch() {
    if std::env::var_os("DBX_NO_UPDATE_CHECK").is_some() {
        return;
    }
    std::thread::spawn(|| {
        let file = crate::engine::server::dir().join("update.json");
        loop {
            let mut seen: Seen = std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            if now().saturating_sub(seen.checked) >= EVERY.as_secs()
                && let Ok(latest) = fetch()
            {
                seen = Seen {
                    checked: now(),
                    latest,
                };
                if let Ok(text) = serde_json::to_string(&seen) {
                    let _ = crate::store::write_private(&file, &text);
                }
            }
            *LATEST.lock().unwrap() = seen.latest;
            std::thread::sleep(Duration::from_secs(3600));
        }
    });
}

/// `dbx update`: the installer's updater, then the panel and the engine follow.
pub fn run() -> Result<i32> {
    let exe = std::env::current_exe().context("cannot tell where dbx is")?;
    let updater = exe.with_file_name("dbx-update");
    let status = if updater.is_file() {
        Command::new(&updater).status()?
    } else if exe.components().any(|c| c.as_os_str() == ".cargo") {
        let args = [
            "install",
            "--locked",
            "--force",
            "--git",
            "https://github.com/gaspb/tui-tools",
            "dbx",
        ];
        println!("cargo {}", args.join(" "));
        Command::new("cargo").args(args).status()?
    } else {
        bail!("dbx wasn't installed by its installer or cargo; reinstall with:\n  {INSTALL}");
    };
    if !status.success() {
        return Ok(status.code().unwrap_or(1));
    }
    // The new binary refreshes the panel and replaces the engine; open panels reconnect.
    let _ = Command::new(&exe).args(["serve", "--stop"]).status();
    Command::new(&exe).args(["setup", "herdr"]).status()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{newest, version};

    #[test]
    fn picks_the_newest_published_dbx() {
        let body = r#"[
            {"tag_name": "dbx-v0.3.0-rc.1", "draft": false, "prerelease": true},
            {"tag_name": "other-v9.0.0", "draft": false, "prerelease": false},
            {"tag_name": "dbx-v0.10.0", "draft": false, "prerelease": false},
            {"tag_name": "dbx-v0.9.0", "draft": false, "prerelease": false},
            {"tag_name": "dbx-v1.0.0", "draft": true, "prerelease": false}
        ]"#;
        assert_eq!(newest(body).unwrap().as_deref(), Some("0.10.0"));
        assert_eq!(newest("[]").unwrap(), None);
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(version("0.10.0") > version("0.9.3"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert_eq!(version("0.2.0-rc.1"), vec![0, 2, 0]);
    }
}
