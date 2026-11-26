//! Connection files: one per project (outside the repo), the projects it includes, and a global one.

use anyhow::{Context, Result};
use common::scope;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE: &str = "furet.toml";
const ROOT_FILE: &str = ".root";

/// A string that may come from somewhere else than the file, so secrets stay out of it.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Value {
    Plain(String),
    Env { env: String },
    Dotenv { dotenv: String, key: String },
    Cmd { cmd: String },
    Keychain { keychain: String },
}

impl Value {
    pub fn plain(&self) -> Option<&str> {
        match self {
            Value::Plain(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Postgres,
    Clickhouse,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::Clickhouse => "clickhouse",
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Conn {
    pub kind: Option<Kind>,
    pub url: Option<Value>,
    pub host: Option<Value>,
    pub port: Option<u16>,
    /// ClickHouse's HTTP port, used by `furet serve` (`port` is the native one).
    pub http_port: Option<u16>,
    pub user: Option<Value>,
    pub password: Option<Value>,
    pub database: Option<Value>,
    /// Shell command that makes `host:port` reachable, e.g. a `kubectl port-forward`.
    pub tunnel: Option<String>,
    /// local, dev, stg, prod… Anything but local/dev/test is read-only unless `readonly = false`.
    pub stage: Option<String>,
    pub readonly: Option<bool>,
    /// Program to open instead of the default client; `{url}`, `{host}`… are filled in.
    pub client: Option<Vec<String>>,
    pub note: Option<String>,
    /// Why it can't be reached (a container whose port isn't published); never in a file.
    #[serde(skip)]
    pub unreachable: Option<String>,
    /// The Docker container it was found in; never in a file.
    #[serde(skip)]
    pub container: Option<String>,
}

impl Conn {
    pub fn kind(&self) -> Option<Kind> {
        if self.kind.is_some() {
            return self.kind;
        }
        let url = self.url.as_ref()?.plain()?;
        let scheme = url.split("://").next()?;
        match scheme {
            "postgres" | "postgresql" => Some(Kind::Postgres),
            "clickhouse" => Some(Kind::Clickhouse),
            _ => None,
        }
    }

    pub fn stage(&self) -> &str {
        self.stage.as_deref().unwrap_or(if self.tunnel.is_some() {
            "remote"
        } else {
            "local"
        })
    }

    pub fn is_prod(&self) -> bool {
        matches!(self.stage(), "prod" | "production")
    }

    pub fn readonly(&self) -> bool {
        self.readonly
            .unwrap_or(!matches!(self.stage(), "local" | "dev" | "test"))
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub struct File {
    #[serde(default)]
    pub include: Vec<String>,
    /// Default client per kind (global file only), e.g. `postgres = ["pgcli"]`.
    #[serde(default)]
    pub clients: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub conn: BTreeMap<String, Conn>,
}

impl File {
    pub fn read(path: &Path) -> Result<File> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("FURET_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| scope::config_home().join("furet"))
}

pub fn global_file() -> PathBuf {
    home().join(FILE)
}

pub fn project_file(root: &Path) -> PathBuf {
    home().join("projects").join(scope::encode(root)).join(FILE)
}

/// Moves a project's folder from its old, ambiguous name, if it's really this project's.
fn migrate(root: &Path) {
    let new = home().join("projects").join(scope::encode(root));
    let old = home().join("projects").join(scope::legacy_encode(root));
    let ours = fs::read_to_string(old.join(ROOT_FILE)).is_ok_and(|r| Path::new(r.trim()) == root);
    if !new.exists() && ours {
        let _ = fs::rename(&old, &new);
    }
}

/// The project's connections file, after checking its folder is really this project's.
fn checked_project_file(root: &Path) -> Result<PathBuf> {
    migrate(root);
    let file = project_file(root);
    if let Ok(recorded) = fs::read_to_string(file.with_file_name(ROOT_FILE))
        && Path::new(recorded.trim()) != root
    {
        anyhow::bail!(
            "{} belongs to {}, not {}: not using it",
            file.parent().unwrap().display(),
            recorded.trim(),
            root.display()
        );
    }
    Ok(file)
}

/// Creates a project's directory, recording which repo it is for.
pub fn ensure_project_dir(root: &Path) -> Result<PathBuf> {
    let file = checked_project_file(root)?;
    let dir = file.parent().unwrap();
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for d in [home(), home().join("projects")] {
        let _ = fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    fs::write(dir.join(ROOT_FILE), root.to_string_lossy().as_bytes())?;
    Ok(file)
}

/// Every project that has a connections file.
pub fn all_projects() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(home().join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join(ROOT_FILE)).ok())
        .map(|r| PathBuf::from(r.trim()))
        .collect();
    out.sort();
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Project,
    Include,
    Global,
    /// Running in Docker right now, not configured anywhere.
    Detected,
}

#[derive(Debug)]
pub struct Layer {
    pub origin: Origin,
    /// The project root, for project and included layers.
    pub root: Option<PathBuf>,
    pub file: PathBuf,
    /// Where relative paths (`dotenv = ".env"`) and secret commands start from.
    pub base: PathBuf,
}

impl Layer {
    /// The keychain accounts this file's connections may use: `global#…` or `<repo>#…`. A
    /// reference to another repo's account (copied by hand, say) is refused rather than read.
    pub fn keychain_prefix(&self) -> Option<String> {
        match (self.origin, &self.root) {
            (Origin::Global, _) => Some("global#".into()),
            (Origin::Project | Origin::Include, Some(root)) => Some(format!("{}#", tilde(root))),
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match (self.origin, &self.root) {
            (Origin::Global, _) => "global".into(),
            (Origin::Detected, _) => "docker".into(),
            (Origin::Project, _) => "this repo".into(),
            (Origin::Include, Some(root)) => tilde(root),
            (Origin::Include, None) => "?".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub layer: usize,
    pub conn: Conn,
}

/// The connections visible from a directory: its project's, then included projects', then global ones.
/// A name defined twice keeps the first.
#[derive(Debug)]
pub struct Scope {
    pub root: PathBuf,
    pub layers: Vec<Layer>,
    pub entries: Vec<Entry>,
    pub clients: BTreeMap<String, Vec<String>>,
    /// Why Docker couldn't be asked for running databases, when it couldn't.
    pub docker: Option<String>,
}

impl Scope {
    pub fn load(cwd: &Path) -> Result<Scope> {
        let root = scope::project_root(cwd);
        let worktree = scope::worktree_root(cwd);
        let mut s = Scope {
            root: root.clone(),
            layers: Vec::new(),
            entries: Vec::new(),
            clients: BTreeMap::new(),
            docker: None,
        };
        let mut seen = HashSet::new();
        s.add_project(&root, worktree.clone(), Origin::Project, &mut seen)?;
        crate::store::tighten(&global_file());
        let global = File::read(&global_file())?;
        s.clients = global.clients.clone();
        s.add_layer(
            Layer {
                origin: Origin::Global,
                root: None,
                file: global_file(),
                base: worktree,
            },
            global,
        );
        Ok(s)
    }

    fn add_project(
        &mut self,
        root: &Path,
        base: PathBuf,
        origin: Origin,
        seen: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        if !seen.insert(root.to_path_buf()) {
            return Ok(());
        }
        let file = checked_project_file(root)?;
        crate::store::tighten(&file);
        let content = File::read(&file)?;
        let includes: Vec<PathBuf> = content
            .include
            .iter()
            .map(|p| scope::project_root(&scope::expand(p, root)))
            .collect();
        self.add_layer(
            Layer {
                origin,
                root: Some(root.to_path_buf()),
                file,
                base,
            },
            content,
        );
        for inc in includes {
            self.add_project(&inc, inc.clone(), Origin::Include, seen)?;
        }
        Ok(())
    }

    fn add_layer(&mut self, layer: Layer, file: File) {
        let idx = self.layers.len();
        self.layers.push(layer);
        for (name, conn) in file.conn {
            if !self.entries.iter().any(|e| e.name == name) {
                self.entries.push(Entry {
                    name,
                    layer: idx,
                    conn,
                });
            }
        }
    }

    /// Adds the databases running in Docker, under names nothing configured uses.
    pub fn with_detected(mut self) -> Scope {
        let worktree = self
            .layers
            .last()
            .map(|l| l.base.clone())
            .unwrap_or_else(|| self.root.clone());
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let roots: Vec<&Path> = [self.root.as_path(), worktree.as_path()]
            .into_iter()
            .filter(|r| r.parent().is_some() && home.as_deref().is_none_or(|h| !h.starts_with(r)))
            .collect();
        let detection = crate::detect::running(&roots);
        self.docker = detection.problem;
        if detection.found.is_empty() {
            return self;
        }
        let idx = self.layers.len();
        self.layers.push(Layer {
            origin: Origin::Detected,
            root: None,
            file: PathBuf::new(),
            base: worktree,
        });
        let shared = |n: &str| detection.found.iter().filter(|f| f.name == n).count() > 1;
        for f in &detection.found {
            let name = if shared(&f.name) || self.entries.iter().any(|e| e.name == f.name) {
                f.container.clone()
            } else {
                f.name.clone()
            };
            if self.entries.iter().any(|e| e.name == name) {
                continue;
            }
            self.entries.push(Entry {
                name,
                layer: idx,
                conn: f.conn.clone(),
            });
        }
        self
    }

    /// The scope to find `name` in: Docker is only asked when nothing configured has that name.
    pub fn load_for(cwd: &Path, name: &str) -> Result<Scope> {
        let s = Scope::load(cwd)?;
        Ok(if s.entries.iter().any(|e| e.name == name) {
            s
        } else {
            s.with_detected()
        })
    }

    pub fn get(&self, name: &str) -> Result<&Entry> {
        if let Some(e) = self.entries.iter().find(|e| e.name == name) {
            return Ok(e);
        }
        let names: Vec<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        if names.is_empty() {
            anyhow::bail!(
                "no connections for {} yet; add one with `furet add` or `furet edit`",
                tilde(&self.root)
            );
        }
        anyhow::bail!("no connection named {name:?} (have: {})", names.join(", "))
    }

    /// The definition a connection comes from, `<file>#<name>`: what a tunnel belongs to, so the
    /// same global or included connection shares one, and different ones never do.
    pub fn owner(&self, e: &Entry) -> String {
        let l = self.layer(e);
        let file = if l.origin == Origin::Detected {
            "docker".to_string()
        } else {
            tilde(&l.file)
        };
        format!("{file}#{}", e.name)
    }

    pub fn layer(&self, e: &Entry) -> &Layer {
        &self.layers[e.layer]
    }
}

pub fn tilde(p: &Path) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => match p.strip_prefix(&h) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        },
        _ => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values_from_every_source() {
        let f: File = toml::from_str(
            r#"
            include = ["~/src/api"]
            [conn.stg]
            url = "postgres://app@localhost:15432/app"
            password = { cmd = "echo s3cret" }
            user = { env = "PGUSER" }
            database = { dotenv = ".env", key = "DB" }
            tunnel = "kubectl port-forward svc/pg 15432:5432"
            stage = "stg"
            "#,
        )
        .unwrap();
        let c = &f.conn["stg"];
        assert_eq!(c.kind(), Some(Kind::Postgres));
        assert_eq!(
            c.password,
            Some(Value::Cmd {
                cmd: "echo s3cret".into()
            })
        );
        assert_eq!(
            c.user,
            Some(Value::Env {
                env: "PGUSER".into()
            })
        );
        assert!(c.readonly());
    }

    #[test]
    fn local_is_writable_and_tunnelled_defaults_to_readonly() {
        let local = Conn::default();
        assert!(!local.readonly());
        let tunnelled = Conn {
            tunnel: Some("x".into()),
            ..Conn::default()
        };
        assert_eq!(tunnelled.stage(), "remote");
        assert!(tunnelled.readonly());
        let forced = Conn {
            stage: Some("prod".into()),
            readonly: Some(false),
            ..Conn::default()
        };
        assert!(!forced.readonly() && forced.is_prod());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<File>("[conn.a]\nhots = \"x\"").is_err());
    }
}
