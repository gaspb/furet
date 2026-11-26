//! Writing connection files (for `furet add` and the panel's form), and passwords in the OS keychain.

use crate::config::{self, Conn, File, Scope, Value, tilde};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, InlineTable, Item, Table, value};

const KEYCHAIN_SERVICE: &str = "furet";

pub const PROJECT_TEMPLATE: &str = r#"# furet connections for {root}
# Kept outside the repo; its worktrees share them. Values can be read when needed:
#   { keychain = "…" } (the OS keychain), { env = "VAR" }, { dotenv = ".env", key = "VAR" }
#   (from the current worktree), { cmd = "…" }
#
# include = ["~/src/other-clone"]     # also use another clone's connections
#
# [conn.local]
# url = { dotenv = ".env", key = "DATABASE_URL" }
#
# [conn.stg]
# url = "postgres://app@localhost:15432/app"
# password = { cmd = "kubectl -n stg get secret pg -o jsonpath='{.data.password}' | base64 -d" }
# tunnel = "kubectl -n stg port-forward svc/postgres 15432:5432"
# stage = "stg"                       # anything but local/dev/test opens read-only (furet -w to write)
#
# [conn.events]
# url = "clickhouse://default@localhost:9000/events"
# client = ["clickhouse-client", "--host", "{host}", "--port", "{port}"]   # optional
"#;

pub const GLOBAL_TEMPLATE: &str = r#"# furet connections available from every repo (each repo's own win on a name clash).
# Same format as a repo's file; `dotenv` paths start from the current worktree.
#
# [clients]
# postgres = ["pgcli"]
# clickhouse = ["clickhouse-client", "--host", "{host}", "--port", "{port}", "--user", "{user}"]
#
# [conn.scratch]
# url = "postgres://postgres:postgres@localhost:5432/postgres"
"#;

fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// This repo's connections file, or the global one; their folders are private to you.
pub fn target_file(cwd: &Path, global: bool) -> Result<PathBuf> {
    if global {
        let f = config::global_file();
        private_dir(f.parent().unwrap())?;
        return Ok(f);
    }
    let f = config::ensure_project_dir(&common::scope::project_root(cwd))?;
    private_dir(&config::home())?;
    private_dir(f.parent().unwrap())?;
    Ok(f)
}

pub fn load_doc(file: &Path) -> Result<DocumentMut> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
    };
    text.parse()
        .with_context(|| format!("in {}", file.display()))
}

pub fn template(file: &Path, global: bool) -> String {
    if global {
        return GLOBAL_TEMPLATE.into();
    }
    let root = fs::read_to_string(file.with_file_name(".root")).unwrap_or_default();
    PROJECT_TEMPLATE.replace("{root}", &tilde(Path::new(root.trim())))
}

/// Writes `text` readable only by you, atomically.
pub fn write_private(file: &Path, text: &str) -> Result<()> {
    let tmp = file.with_extension("tmp~");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, file)?;
    Ok(())
}

pub fn save_doc(file: &Path, doc: &DocumentMut, global: bool) -> Result<()> {
    let mut text = doc.to_string();
    if !file.exists() {
        text = format!("{}\n{text}", template(file, global));
    }
    toml::from_str::<File>(&text)
        .with_context(|| format!("would make {} invalid", file.display()))?;
    write_private(file, &text)
}

fn conns(doc: &mut DocumentMut) -> Result<&mut Table> {
    let t = doc
        .entry("conn")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .context("`conn` is not a table")?;
    t.set_implicit(true);
    Ok(t)
}

// ── keychain ────────────────────────────────────────────────────────────

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).context("cannot reach the keychain")
}

/// A password saved while furet was dbx moves to furet's name the first time it's read.
pub fn keychain_get(account: &str) -> Result<String> {
    let e = entry(account)?;
    if let Ok(p) = e.get_password() {
        return Ok(p);
    }
    let old = keyring::Entry::new(crate::migrate::OLD, account)
        .and_then(|o| o.get_password().map(|p| (o, p)));
    if let Ok((o, p)) = old
        && e.set_password(&p).is_ok()
    {
        let _ = o.delete_credential();
        return Ok(p);
    }
    bail!("no password for {account:?} in the keychain")
}

pub fn keychain_set(account: &str, password: &str) -> Result<()> {
    entry(account)?.set_password(password).context(
        "cannot save to the keychain (on Linux it needs a Secret Service, e.g. gnome-keyring or KeePassXC)",
    )
}

pub fn keychain_delete(account: &str) {
    for service in [KEYCHAIN_SERVICE, crate::migrate::OLD] {
        if let Ok(e) = keyring::Entry::new(service, account) {
            let _ = e.delete_credential();
        }
    }
}

/// The keychain account for a connection: which file it lives in, and its name.
pub fn account(file_label: &str, name: &str) -> String {
    format!("{file_label}#{name}")
}

// ── drafts: what the panel's form edits ─────────────────────────────────

/// Where the password comes from. `secret` is only ever sent in, never read back.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum Password {
    /// None, or whatever the URL says.
    #[default]
    None,
    /// Leave the saved one as it is (editing).
    Keep,
    Keychain {
        #[serde(default, skip_serializing)]
        secret: Option<String>,
    },
    Env {
        name: String,
    },
    Dotenv {
        file: String,
        key: String,
    },
    Cmd {
        cmd: String,
    },
    /// Saved in the file as is (older entries, or no keychain).
    File,
    /// The container's own, read when saving (into the keychain).
    Docker {
        container: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Draft {
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub tunnel: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub password: Password,
    /// Saved in the global file rather than this repo's.
    #[serde(default)]
    pub global: bool,
}

fn blank(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// A kept password only goes where it went before: same host, port and user.
fn same_target(old: Option<&str>, new: &str) -> Result<()> {
    let parts = |u: &str| {
        url::Url::parse(u.trim()).ok().map(|u| {
            (
                u.host_str().map(String::from),
                u.port(),
                u.username().to_string(),
            )
        })
    };
    if old.and_then(parts).is_none_or(|o| Some(o) != parts(new)) {
        bail!("the host, port or user changed: type the password again");
    }
    Ok(())
}

/// The URL without its password, and the password it had.
fn split_password(raw: &str) -> Result<(String, Option<String>)> {
    let mut u = url::Url::parse(raw.trim()).context("invalid url")?;
    if !matches!(u.scheme(), "postgres" | "postgresql" | "clickhouse") {
        bail!("the url should start with postgres:// or clickhouse://");
    }
    let pw = u.password().map(|p| {
        url::form_urlencoded::parse(format!("x={p}").as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    });
    if pw.is_some() {
        let _ = u.set_password(None);
    }
    Ok((u.to_string(), pw))
}

/// The form's fields for an existing connection; secrets are never included.
pub fn draft_of(scope: &Scope, name: &str) -> Result<Draft> {
    let e = scope.get(name)?;
    let c = &e.conn;
    if scope.layer(e).origin == config::Origin::Detected {
        let mut e = e.clone();
        e.conn.unreachable = None;
        let mut r = crate::conn::resolve(&e, scope.layer(&e))?;
        if r.kind == config::Kind::Clickhouse && r.http_port != 8123 {
            r.params.push(("http_port".into(), r.http_port.to_string()));
        }
        r.password = None;
        return Ok(Draft {
            name: name.into(),
            url: r.url(),
            stage: Some("local".into()),
            note: c.note.clone(),
            password: match &c.container {
                Some(container) => Password::Docker {
                    container: container.clone(),
                },
                None => Password::None,
            },
            ..Draft::default()
        });
    }
    let url = match &c.url {
        Some(Value::Plain(u)) => split_password(u)
            .map(|(u, _)| u)
            .unwrap_or_else(|_| u.clone()),
        Some(_) => bail!("{name}'s url is read from elsewhere: edit it in the file"),
        None => bail!("{name} is written as host/port fields: edit it in the file"),
    };
    let has_url_password = matches!(&c.url, Some(Value::Plain(u)) if split_password(u).is_ok_and(|(_, p)| p.is_some()));
    let password = match &c.password {
        None if has_url_password => Password::File,
        None => Password::None,
        Some(Value::Keychain { .. }) => Password::Keychain { secret: None },
        Some(Value::Env { env }) => Password::Env { name: env.clone() },
        Some(Value::Dotenv { dotenv, key }) => Password::Dotenv {
            file: dotenv.clone(),
            key: key.clone(),
        },
        Some(Value::Cmd { cmd }) => Password::Cmd { cmd: cmd.clone() },
        Some(Value::Plain(_)) => Password::File,
    };
    Ok(Draft {
        name: name.into(),
        url,
        stage: c.stage.clone(),
        tunnel: c.tunnel.clone(),
        note: c.note.clone(),
        password,
        global: scope.layer(e).origin == config::Origin::Global,
    })
}

/// Swaps a container's password in for `Password::Docker`, checking the draft still points at it.
pub fn from_docker(draft: &mut Draft) -> Result<()> {
    let Password::Docker { container } = &draft.password else {
        return Ok(());
    };
    let u = url::Url::parse(draft.url.trim()).context("invalid url")?;
    let host = u
        .host_str()
        .unwrap_or("")
        .trim_start_matches('[')
        .trim_end_matches(']');
    let user = url::form_urlencoded::parse(format!("x={}", u.username()).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    let port = u.port().context("the url needs the container's port")?;
    let secret =
        crate::detect::password(container, host, port, &user).map_err(anyhow::Error::msg)?;
    draft.password = match secret {
        Some(s) => Password::Keychain { secret: Some(s) },
        None => Password::None,
    };
    Ok(())
}

/// A connection as it would be saved, with a keychain secret still in hand: for testing a draft.
pub fn conn_of(draft: &Draft, existing: Option<&Conn>) -> Result<Conn> {
    let (url, url_pw) = split_password(&draft.url)?;
    let kept = || -> Result<Option<Value>> {
        let Some(pw) = existing.and_then(|c| c.password.clone()) else {
            return Ok(url_pw.clone().map(Value::Plain));
        };
        let old_url = match existing.and_then(|c| c.url.as_ref()) {
            Some(Value::Plain(u)) => Some(u.as_str()),
            _ => None,
        };
        same_target(old_url, &url)?;
        Ok(Some(pw))
    };
    let password = match &draft.password {
        Password::None => url_pw.map(Value::Plain),
        Password::Keep | Password::File => kept()?,
        Password::Keychain { secret: Some(s) } if !s.is_empty() => Some(Value::Plain(s.clone())),
        Password::Keychain { .. } => kept()?,
        Password::Env { name } => Some(Value::Env { env: name.clone() }),
        Password::Dotenv { file, key } => Some(Value::Dotenv {
            dotenv: file.clone(),
            key: key.clone(),
        }),
        Password::Cmd { cmd } => Some(Value::Cmd { cmd: cmd.clone() }),
        Password::Docker { .. } => bail!("the container's password wasn't read yet"),
    };
    let url = if matches!(draft.password, Password::File) && password.is_none() {
        draft.url.clone()
    } else {
        url
    };
    Ok(Conn {
        url: Some(Value::Plain(url)),
        password,
        stage: blank(&draft.stage).map(String::from),
        tunnel: blank(&draft.tunnel).map(String::from),
        note: blank(&draft.note).map(String::from),
        ..Conn::default()
    })
}

/// What saving did, for the person.
#[derive(Serialize, Debug)]
pub struct Saved {
    pub file: PathBuf,
    pub notes: Vec<String>,
}

fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!("a name is letters, digits, - _ or . (no spaces)");
    }
    Ok(())
}

/// Adds or replaces a connection. `previous` is its name before an edit (it may be renamed).
/// A password typed in the URL or the form goes to the keychain; the file only names it.
pub fn save(cwd: &Path, draft: &Draft, previous: Option<&str>) -> Result<Saved> {
    valid_name(&draft.name)?;
    let file = target_file(cwd, draft.global)?;
    let label = if draft.global {
        "global".to_string()
    } else {
        tilde(&common::scope::project_root(cwd))
    };
    let mut doc = load_doc(&file)?;
    let mut notes = Vec::new();
    let old = previous.and_then(|p| doc.get("conn").and_then(|c| c.get(p)).cloned());
    if previous != Some(draft.name.as_str())
        && doc.get("conn").and_then(|c| c.get(&draft.name)).is_some()
    {
        bail!("{} already exists in {}", draft.name, tilde(&file));
    }
    let (url, url_pw) = split_password(&draft.url)?;
    let mut t = Table::new();
    t["url"] = value(&url);
    let account = account(&label, &draft.name);
    let keychain = |secret: &str, notes: &mut Vec<String>| -> Result<Item> {
        keychain_set(&account, secret)?;
        notes.push("password saved in the keychain".into());
        let mut it = InlineTable::new();
        it.insert("keychain", account.as_str().into());
        Ok(value(it))
    };
    let reference = |k: &str, v: &str| {
        let mut it = InlineTable::new();
        it.insert(k, v.into());
        value(it)
    };
    let password: Option<Item> = match &draft.password {
        Password::Keychain { secret: Some(s) } if !s.is_empty() => Some(keychain(s, &mut notes)?),
        Password::None | Password::Keychain { .. } if url_pw.is_some() => {
            Some(keychain(url_pw.as_deref().unwrap(), &mut notes)?)
        }
        Password::Keep | Password::Keychain { .. } | Password::File => {
            let kept = old.as_ref().and_then(|o| o.get("password")).cloned();
            if kept.is_some() {
                same_target(
                    old.as_ref()
                        .and_then(|o| o.get("url"))
                        .and_then(|u| u.as_str()),
                    &url,
                )?;
            }
            let old_account = kept
                .as_ref()
                .and_then(|p| p.get("keychain"))
                .and_then(|a| a.as_str())
                .map(String::from);
            match old_account {
                // Renamed: the secret follows the name, so a new connection can't take it over.
                Some(a) if a != account => {
                    let secret = keychain_get(&a)?;
                    let item = keychain(&secret, &mut Vec::new())?;
                    keychain_delete(&a);
                    Some(item)
                }
                _ => kept,
            }
        }
        Password::None => None,
        Password::Env { name } => Some(reference("env", name)),
        Password::Dotenv { file, key } => {
            let mut it = InlineTable::new();
            it.insert("dotenv", file.as_str().into());
            it.insert("key", key.as_str().into());
            Some(value(it))
        }
        Password::Cmd { cmd } => Some(reference("cmd", cmd)),
        Password::Docker { .. } => bail!("the container's password wasn't read yet"),
    };
    if let Some(p) = password {
        t["password"] = p;
    }
    for (k, v) in [
        ("stage", &draft.stage),
        ("tunnel", &draft.tunnel),
        ("note", &draft.note),
    ] {
        if let Some(v) = blank(v) {
            t[k] = value(v);
        }
    }
    // Keep what the form doesn't show (kind, client, readonly…).
    if let Some(Item::Table(o)) = &old {
        for (k, v) in o.iter() {
            if ![
                "url", "password", "stage", "tunnel", "note", "host", "port", "user", "database",
            ]
            .contains(&k)
            {
                t[k] = v.clone();
            }
        }
    }
    let c = conns(&mut doc)?;
    if let Some(p) = previous.filter(|p| *p != draft.name) {
        c.remove(p);
    }
    c.insert(&draft.name, Item::Table(t));
    save_doc(&file, &doc, draft.global)?;
    Ok(Saved { file, notes })
}

/// Removes a connection from this repo's file or the global one, and its keychain password.
pub fn remove(cwd: &Path, name: &str) -> Result<PathBuf> {
    let scope = Scope::load(cwd)?;
    let e = scope.get(name)?;
    let layer = scope.layer(e);
    if layer.origin == config::Origin::Detected {
        bail!("{name} is running in Docker, not saved anywhere: nothing to remove");
    }
    if layer.origin == config::Origin::Include {
        bail!("{name} comes from {}: remove it there", layer.label());
    }
    let global = layer.origin == config::Origin::Global;
    let mut doc = load_doc(&layer.file)?;
    let removed = conns(&mut doc)?.remove(name);
    if removed.is_none() {
        bail!("{name} is not in {}", tilde(&layer.file));
    }
    if let Some(Value::Keychain { keychain }) = &e.conn.password
        && layer
            .keychain_prefix()
            .is_some_and(|p| keychain.starts_with(&p))
    {
        keychain_delete(keychain);
    }
    save_doc(&layer.file, &doc, global)?;
    Ok(layer.file.clone())
}

/// Makes existing connection files and their folders private (older versions created them with
/// default permissions).
pub fn tighten(file: &Path) {
    let dirs = [
        Some(config::home()),
        Some(config::home().join("projects")),
        file.parent().map(Path::to_path_buf),
    ];
    for dir in dirs.iter().flatten() {
        if let Ok(m) = fs::metadata(dir)
            && m.is_dir()
            && m.permissions().mode() & 0o077 != 0
        {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    if let Ok(m) = fs::metadata(file)
        && m.permissions().mode() & 0o077 != 0
    {
        let _ = fs::set_permissions(file, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_password_out_of_a_url() {
        let (u, p) =
            split_password("postgres://app:p%40ss@localhost:5432/app?sslmode=require").unwrap();
        assert_eq!(u, "postgres://app@localhost:5432/app?sslmode=require");
        assert_eq!(p.as_deref(), Some("p@ss"));
        assert!(split_password("mysql://x").is_err());
    }

    #[test]
    fn a_kept_password_never_follows_a_new_host() {
        let url = "postgres://app@db.stg:5432/app";
        let old = Some(url);
        assert!(same_target(old, "postgres://app@db.stg:5432/other?sslmode=require").is_ok());
        for new in [
            "postgres://app@evil.example:5432/app",
            "postgres://app@db.stg:6543/app",
            "postgres://root@db.stg:5432/app",
        ] {
            assert!(same_target(old, new).is_err(), "{new}");
        }
        assert!(same_target(None, "postgres://app@db.stg:5432/app").is_err());
        let existing = Conn {
            url: Some(Value::Plain(url.into())),
            password: Some(Value::Keychain {
                keychain: "x#stg".into(),
            }),
            ..Conn::default()
        };
        let moved = Draft {
            url: "postgres://app@evil.example:5432/app".into(),
            password: Password::Keep,
            ..Draft::default()
        };
        assert!(conn_of(&moved, Some(&existing)).is_err());
    }

    #[test]
    fn drafts_read_passwords_from_the_chosen_source() {
        let d = Draft {
            name: "stg".into(),
            url: "postgres://app:inline@h/db".into(),
            password: Password::Env {
                name: "PGPW".into(),
            },
            ..Draft::default()
        };
        let c = conn_of(&d, None).unwrap();
        assert_eq!(c.password, Some(Value::Env { env: "PGPW".into() }));
        assert_eq!(c.url, Some(Value::Plain("postgres://app@h/db".into())));
        let d = Draft {
            password: Password::Keychain {
                secret: Some("s".into()),
            },
            ..d
        };
        assert_eq!(
            conn_of(&d, None).unwrap().password,
            Some(Value::Plain("s".into()))
        );
    }

    #[test]
    fn names_are_simple() {
        assert!(valid_name("stg-eu_1.ro").is_ok());
        assert!(valid_name("my db").is_err());
        assert!(valid_name("").is_err());
    }
}
