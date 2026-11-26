//! What ran, per repo and connection: the panel's consoles, `furet query`, and agents through it.
//! Kept beside the engine's state (readable only by you), the newest 2000 statements per connection.
//! Also the repo's saved queries: `.furet/queries/<name>.sql` in the worktree, shared through git.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const KEEP: usize = 2000;
const QUERIES: &str = ".furet/queries";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    /// Unix seconds.
    pub at: u64,
    pub sql: String,
    pub ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `panel`, `cli`, or `agent (claude-code)`…
    pub by: String,
    #[serde(default)]
    pub conn: String,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn dir(root: &Path) -> PathBuf {
    super::server::dir()
        .join("history")
        .join(common::scope::encode(root))
}

fn file(root: &Path, conn: &str) -> PathBuf {
    dir(root).join(format!("{}.jsonl", conn.replace(['/', '\\'], "_")))
}

/// Appends a statement; a file past its share is cut back to the newest `KEEP`.
pub fn record(root: &Path, entry: &Entry) {
    let path = file(root, &entry.conn);
    let write = || -> Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        crate::store::tighten(&path);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{}", serde_json::to_string(entry)?)?;
        if f.metadata()?.len() > 2 * 1024 * 1024 {
            let text = fs::read_to_string(&path)?;
            let lines: Vec<&str> = text.lines().collect();
            let keep = lines[lines.len().saturating_sub(KEEP)..].join("\n") + "\n";
            crate::store::write_private(&path, &keep)?;
        }
        Ok(())
    };
    let _ = write();
}

/// The newest statements, for one connection or all of the repo's.
pub fn recent(root: &Path, conn: Option<&str>, limit: usize) -> Vec<Entry> {
    let files: Vec<PathBuf> = match conn {
        Some(c) => vec![file(root, c)],
        None => fs::read_dir(dir(root))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default(),
    };
    let mut out: Vec<Entry> = files
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok())
        .flat_map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.at));
    out.truncate(limit);
    out
}

#[derive(Serialize, Debug)]
pub struct Saved {
    pub name: String,
    pub sql: String,
    /// From a `-- connection: <name>` first line, when there is one.
    pub connection: Option<String>,
    pub file: PathBuf,
}

fn queries_dir(cwd: &Path) -> PathBuf {
    common::scope::worktree_root(cwd).join(QUERIES)
}

fn valid(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 80
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        bail!("a query name is letters, digits, - and _ (got {name:?})");
    }
    Ok(())
}

pub fn saved(cwd: &Path) -> Vec<Saved> {
    let Ok(entries) = fs::read_dir(queries_dir(cwd)) else {
        return Vec::new();
    };
    let mut out: Vec<Saved> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .filter_map(|p| {
            let text = fs::read_to_string(&p).ok()?;
            let name = p.file_stem()?.to_string_lossy().into_owned();
            let connection = text
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("-- connection:"))
                .map(|c| c.trim().to_string());
            let sql = match connection {
                Some(_) => text
                    .split_once('\n')
                    .map_or("", |(_, rest)| rest)
                    .trim()
                    .to_string(),
                None => text.trim().to_string(),
            };
            Some(Saved {
                name,
                sql,
                connection,
                file: p,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn read_saved(cwd: &Path, name: &str) -> Result<Saved> {
    valid(name)?;
    saved(cwd)
        .into_iter()
        .find(|s| s.name == name)
        .with_context(|| format!("no saved query {name:?} in {}", queries_dir(cwd).display()))
}

pub fn save(cwd: &Path, name: &str, sql: &str, connection: Option<&str>) -> Result<PathBuf> {
    valid(name)?;
    let dir = queries_dir(cwd);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.sql"));
    let head = connection
        .map(|c| format!("-- connection: {c}\n"))
        .unwrap_or_default();
    fs::write(&path, format!("{head}{}\n", sql.trim()))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

pub fn remove(cwd: &Path, name: &str) -> Result<()> {
    valid(name)?;
    fs::remove_file(queries_dir(cwd).join(format!("{name}.sql")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_reads_queries_with_their_connection() {
        let dir = std::env::temp_dir().join(format!("furet-queries-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        save(
            &dir,
            "stuck-invoices",
            "select * from invoices where status = 'stuck';",
            Some("stg"),
        )
        .unwrap();
        save(&dir, "plain", "select 1", None).unwrap();
        let all = saved(&dir);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].name, "stuck-invoices");
        assert_eq!(all[1].connection.as_deref(), Some("stg"));
        assert_eq!(all[1].sql, "select * from invoices where status = 'stuck';");
        assert_eq!(all[0].connection, None);
        assert!(save(&dir, "../escape", "x", None).is_err());
        remove(&dir, "plain").unwrap();
        assert_eq!(saved(&dir).len(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
