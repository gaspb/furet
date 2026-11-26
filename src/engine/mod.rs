//! `furet serve`: a local engine holding database sessions for the herdr panel, over HTTP on 127.0.0.1.

pub mod ch;
pub mod drift;
pub mod graph;
pub mod history;
pub mod pg;
pub mod server;
pub mod sql;

use crate::config::{Entry, Kind, Scope};
use crate::conn;
use crate::store;
use crate::tunnel::Tunnel;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

pub const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);

/// An HTTP-level failure: status and message.
#[derive(Debug)]
pub struct Fail(pub u16, pub String);

impl<E: std::fmt::Display> From<E> for Fail {
    fn from(e: E) -> Fail {
        Fail(500, format!("{e:#}"))
    }
}

pub fn fail<T>(status: u16, msg: impl Into<String>) -> Result<T, Fail> {
    Err(Fail(status, msg.into()))
}

pub struct Session {
    pub id: String,
    pub name: String,
    /// The repo it was opened for: a name means different things in different repos.
    pub root: PathBuf,
    pub prod: bool,
    pub info: Value,
    backend: Backend,
    readonly: AtomicBool,
    last_used: Mutex<Instant>,
    _tunnel: Option<Arc<Tunnel>>,
}

enum Backend {
    Pg {
        pg: Box<Mutex<pg::Pg>>,
        cancel: (
            postgres::CancelToken,
            tokio_postgres_rustls::MakeRustlsConnect,
        ),
    },
    Ch(ch::Ch),
}

impl Backend {
    fn connect(r: &conn::Resolved) -> anyhow::Result<Backend> {
        Ok(match r.kind {
            Kind::Postgres => {
                let pg = pg::Pg::connect(r)?;
                Backend::Pg {
                    cancel: pg.cancel_token(),
                    pg: Box::new(Mutex::new(pg)),
                }
            }
            Kind::Clickhouse => Backend::Ch(ch::Ch::connect(r)?),
        })
    }

    fn server_version(&self) -> String {
        match self {
            Backend::Pg { pg, .. } => pg.lock().unwrap().server_version.clone(),
            Backend::Ch(ch) => ch.server_version.clone(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Backend::Pg { .. } => format!("Postgres {}", self.server_version()),
            Backend::Ch(_) => format!("ClickHouse {}", self.server_version()),
        }
    }
}

impl Session {
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    fn dialect(&self) -> sql::Dialect {
        match self.backend {
            Backend::Pg { .. } => sql::Dialect::Postgres,
            Backend::Ch(_) => sql::Dialect::Clickhouse,
        }
    }

    fn describe(&self) -> Value {
        let mut v = self.info.clone();
        v["id"] = json!(self.id);
        v["readonly"] = json!(self.readonly.load(Ordering::SeqCst));
        v["idle_s"] = json!(self.last_used.lock().unwrap().elapsed().as_secs());
        v
    }
}

#[derive(Default)]
pub struct Engine {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// One slot per connection definition, locked while its tunnel starts.
    tunnels: Mutex<HashMap<String, Arc<Mutex<Weak<Tunnel>>>>>,
    roots: Mutex<HashMap<String, PathBuf>>,
    pub last_request: Mutex<Option<Instant>>,
}

#[derive(Deserialize)]
pub struct DraftReq {
    pub cwd: PathBuf,
    pub draft: store::Draft,
    /// The connection's name before this edit; absent for a new one.
    pub previous: Option<String>,
}

#[derive(Deserialize)]
pub struct OpenReq {
    pub cwd: PathBuf,
    pub name: String,
    #[serde(default)]
    pub write: bool,
    /// The connection's name, typed by the person, to allow writes on prod.
    pub confirm: Option<String>,
}

#[derive(Deserialize)]
pub struct QueryReq {
    pub sql: String,
    /// UTF-16 cursor: run the statement under it. Ignored with `all`.
    pub cursor: Option<usize>,
    /// UTF-16 selection: run exactly that.
    pub selection: Option<(usize, usize)>,
    #[serde(default)]
    pub all: bool,
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    /// Who ran it, for the history: `panel` unless `furet query` says `cli` or `agent (…)`.
    #[serde(default)]
    pub by: Option<String>,
}

#[derive(Deserialize)]
pub struct DriftReq {
    pub cwd: PathBuf,
    pub left: String,
    pub right: String,
    #[serde(default)]
    pub schema: Option<String>,
}

#[derive(Deserialize)]
pub struct CopyReq {
    pub cwd: PathBuf,
    pub from: String,
    pub to: String,
    pub schema: String,
    pub table: String,
    pub column: String,
    pub value: String,
    /// Also the rows pointing at it (an invoice's lines, a customer's orders).
    #[serde(default)]
    pub children: bool,
    #[serde(default = "yes")]
    pub mask: bool,
    /// Only say what would be copied.
    #[serde(default)]
    pub dry: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct SaveQueryReq {
    pub cwd: PathBuf,
    pub name: String,
    pub sql: String,
    #[serde(default)]
    pub connection: Option<String>,
}

#[derive(Deserialize)]
pub struct NameReq {
    pub cwd: PathBuf,
    pub name: String,
}

#[derive(Deserialize)]
pub struct ApplyReq {
    pub statements: Vec<String>,
}

#[derive(Deserialize)]
pub struct LookupReq {
    pub schema: String,
    pub table: String,
    pub column: String,
    #[serde(default)]
    pub value: Option<String>,
    /// Several values at once: the rows they point at, in one query.
    #[serde(default)]
    pub values: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct ReferrersReq {
    pub schema: String,
    pub table: String,
    /// The row: column name → value (as text).
    pub row: HashMap<String, Option<String>>,
}

#[derive(Deserialize)]
pub struct ModeReq {
    pub write: bool,
    pub confirm: Option<String>,
}

fn random_id() -> String {
    server::random_hex(8)
}

/// The port the engine connects to, which a tunnel must make reachable: ClickHouse's HTTP one.
fn engine_port(r: &conn::Resolved) -> u16 {
    match r.kind {
        Kind::Postgres => r.port,
        Kind::Clickhouse => r.http_port,
    }
}

fn check_write(name: &str, prod: bool, write: bool, confirm: Option<&str>) -> Result<(), Fail> {
    if write && prod && confirm != Some(name) {
        return fail(
            403,
            format!("{name} is prod: type its name to allow writes"),
        );
    }
    Ok(())
}

impl Engine {
    pub fn connections(&self, cwd: &str) -> Result<Value, Fail> {
        let scope = Scope::load(std::path::Path::new(cwd))?.with_detected();
        let sessions = self.sessions.lock().unwrap();
        let mut rows = serde_json::to_value(crate::rows(&scope))?;
        for row in rows.as_array_mut().into_iter().flatten() {
            let name = row["name"].as_str().unwrap_or_default();
            let open = sessions
                .values()
                .filter(|s| s.root == scope.root && s.name == name)
                .count();
            row["sessions"] = json!(open);
        }
        Ok(json!({
            "root": scope.root,
            "file": crate::config::project_file(&scope.root),
            "connections": rows,
            "docker": scope.docker,
            "update": crate::update::available(),
        }))
    }

    /// Closes every session on a connection of this repo; its tunnel stops with the last one.
    pub fn disconnect(&self, req: NameReq) -> Result<Value, Fail> {
        let root = Scope::load(&req.cwd)?.root;
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|_, s| !(s.root == root && s.name == req.name));
        Ok(json!({ "closed": before - sessions.len() }))
    }

    pub fn draft(&self, cwd: &str, name: &str) -> Result<Value, Fail> {
        let scope = Scope::load_for(std::path::Path::new(cwd), name)?;
        let d = store::draft_of(&scope, name).map_err(|e| Fail(400, format!("{e:#}")))?;
        Ok(serde_json::to_value(d)?)
    }

    /// Connects with a draft (tunnel included) without saving it.
    pub fn test(&self, mut req: DraftReq) -> Result<Value, Fail> {
        store::from_docker(&mut req.draft).map_err(|e| Fail(400, format!("{e:#}")))?;
        let scope = Scope::load(&req.cwd)?;
        let target = if req.draft.global {
            crate::config::Origin::Global
        } else {
            crate::config::Origin::Project
        };
        let existing = req
            .previous
            .as_deref()
            .and_then(|p| scope.get(p).ok())
            .filter(|e| scope.layer(e).origin == target)
            .map(|e| e.conn.clone());
        let conn = store::conn_of(&req.draft, existing.as_ref())
            .map_err(|e| Fail(400, format!("{e:#}")))?;
        let entry = Entry {
            name: req.draft.name.clone(),
            layer: 0,
            conn,
        };
        let layer = scope
            .layers
            .iter()
            .find(|l| l.origin == target)
            .unwrap_or(&scope.layers[0]);
        let r = conn::resolve(&entry, layer).map_err(|e| Fail(400, format!("{e:#}")))?;
        let file = layer.file.clone();
        let owner = format!(
            "{}#{}",
            crate::config::tilde(&file),
            req.previous.as_deref().unwrap_or(&req.draft.name)
        );
        let _tunnel = match &entry.conn.tunnel {
            Some(cmd) => self.tunnel(cmd, &r.host, engine_port(&r), &layer.base, &owner)?,
            None => None,
        };
        match Backend::connect(&r) {
            Ok(b) => Ok(json!({ "ok": true, "message": format!("Connected · {}", b.describe()) })),
            Err(e) => Ok(json!({ "ok": false, "message": format!("{e:#}") })),
        }
    }

    pub fn save(&self, mut req: DraftReq) -> Result<Value, Fail> {
        store::from_docker(&mut req.draft).map_err(|e| Fail(400, format!("{e:#}")))?;
        let saved = store::save(&req.cwd, &req.draft, req.previous.as_deref())
            .map_err(|e| Fail(400, format!("{e:#}")))?;
        Ok(serde_json::to_value(saved)?)
    }

    pub fn remove(&self, cwd: &str, name: &str) -> Result<Value, Fail> {
        let file = store::remove(std::path::Path::new(cwd), name)
            .map_err(|e| Fail(400, format!("{e:#}")))?;
        Ok(json!({ "removed": name, "file": file }))
    }

    pub fn open(&self, req: OpenReq) -> Result<Value, Fail> {
        let scope = Scope::load_for(&req.cwd, &req.name)?;
        let entry = scope
            .get(&req.name)
            .map_err(|e| Fail(404, format!("{e:#}")))?;
        let layer = scope.layer(entry);
        let prod = entry.conn.is_prod();
        let readonly_default = entry.conn.readonly();
        check_write(
            &req.name,
            prod && readonly_default,
            req.write,
            req.confirm.as_deref(),
        )?;
        let mut r = conn::resolve(entry, layer)?;
        if req.write {
            r.readonly = false;
        }
        let tunnel = match &entry.conn.tunnel {
            Some(cmd) => self.tunnel(
                cmd,
                &r.host,
                engine_port(&r),
                &layer.base,
                &scope.owner(entry),
            )?,
            None => None,
        };
        let backend = Backend::connect(&r).map_err(|e| Fail(502, format!("{e:#}")))?;
        if let Backend::Ch(ch) = &backend
            && ch.server_readonly
        {
            r.readonly = true;
        }
        let info = json!({
            "name": req.name,
            "kind": r.kind.name(),
            "stage": entry.conn.stage(),
            "prod": prod,
            "database": r.database,
            "user": r.user,
            "host": r.host,
            "port": r.port,
            "server_version": backend.server_version(),
            "cwd": req.cwd,
        });
        let s = Arc::new(Session {
            id: random_id(),
            name: req.name,
            root: scope.root.clone(),
            prod: prod && readonly_default,
            info,
            backend,
            readonly: AtomicBool::new(r.readonly),
            last_used: Mutex::new(Instant::now()),
            _tunnel: tunnel,
        });
        let v = s.describe();
        self.sessions.lock().unwrap().insert(s.id.clone(), s);
        Ok(v)
    }

    /// Sessions on the same connection definition share its tunnel; it stops with the last of them.
    /// Starting one only blocks other opens of that same definition.
    fn tunnel(
        &self,
        cmd: &str,
        host: &str,
        port: u16,
        base: &std::path::Path,
        owner: &str,
    ) -> Result<Option<Arc<Tunnel>>, Fail> {
        let slot = self
            .tunnels
            .lock()
            .unwrap()
            .entry(owner.to_string())
            .or_default()
            .clone();
        let mut held = slot.lock().unwrap();
        if let Some(t) = held.upgrade() {
            return Ok(Some(t));
        }
        match Tunnel::ensure(cmd, host, port, base, owner)
            .map_err(|e| Fail(502, format!("{e:#}")))?
        {
            Some(t) => {
                let t = Arc::new(t);
                *held = Arc::downgrade(&t);
                Ok(Some(t))
            }
            None => Ok(None),
        }
    }

    /// A session of the repo at `root`: another repo's session doesn't exist from here.
    pub fn session(&self, root: &std::path::Path, id: &str) -> Result<Arc<Session>, Fail> {
        let s = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .filter(|s| s.root == root);
        let Some(s) = s else {
            return fail(
                404,
                "no such session (disconnected, or idle for 30 minutes)",
            );
        };
        s.touch();
        Ok(s)
    }

    /// The repo a client folder belongs to (cached: resolving runs git).
    pub fn root_of(&self, cwd: &str) -> Result<PathBuf, Fail> {
        if let Some(r) = self.roots.lock().unwrap().get(cwd) {
            return Ok(r.clone());
        }
        let root = Scope::load(std::path::Path::new(cwd))?.root;
        self.roots
            .lock()
            .unwrap()
            .insert(cwd.to_string(), root.clone());
        Ok(root)
    }

    pub fn list(&self, root: &std::path::Path) -> Value {
        Value::Array(
            self.sessions
                .lock()
                .unwrap()
                .values()
                .filter(|s| s.root == root)
                .map(|s| s.describe())
                .collect(),
        )
    }

    pub fn close(&self, root: &std::path::Path, id: &str) -> Result<Value, Fail> {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(s) if s.root == root => {
                sessions.remove(id);
                Ok(json!({ "closed": id }))
            }
            _ => fail(404, "no such session"),
        }
    }

    pub fn query(&self, root: &std::path::Path, id: &str, req: QueryReq) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        let sql = &req.sql;
        let clamp = |o: usize| sql::utf16_to_byte(sql, o);
        let range = if req.all {
            (0, sql.len())
        } else if let Some((a, b)) = req.selection.filter(|(a, b)| a != b) {
            let (a, b) = (clamp(a.min(b)), clamp(a.max(b)));
            let picked = &sql[a..b];
            let start = a + (picked.len() - picked.trim_start().len());
            let end = a + picked.trim_end().trim_end_matches(';').trim_end().len();
            (start, end.max(start))
        } else {
            match sql::at(sql, clamp(req.cursor.unwrap_or(0)), s.dialect()) {
                Some(r) => r,
                None => return fail(400, "nothing to run"),
            }
        };
        let readonly = s.readonly.load(Ordering::SeqCst);
        if readonly
            && matches!(s.backend, Backend::Pg { .. })
            && sql::lifts_readonly(&sql[range.0..range.1])
        {
            return fail(
                409,
                "that would turn read-only off: switch the session to read-write instead (prod asks to confirm)",
            );
        }
        let limit = req.limit.unwrap_or(500).clamp(1, 10_000);
        let out = match &s.backend {
            Backend::Pg { pg, .. } => pg
                .lock()
                .unwrap()
                .run(sql, range, req.all, limit, req.offset),
            Backend::Ch(ch) => ch.run(sql, range, limit, req.offset),
        };
        if out.error.as_ref().is_some_and(|e| e.closed) {
            self.sessions.lock().unwrap().remove(id);
        }
        let ran = sql[range.0..range.1].trim();
        if !ran.is_empty() {
            let last = out.results.last();
            history::record(
                &s.root,
                &history::Entry {
                    at: history::now(),
                    sql: ran.to_string(),
                    ms: out.elapsed_ms,
                    rows: last.map(|r| r.affected.unwrap_or(r.rows.len() as u64)),
                    error: out.error.as_ref().map(|e| e.message.clone()),
                    by: req.by.clone().unwrap_or_else(|| "panel".into()),
                    conn: s.name.clone(),
                },
            );
        }
        let mut v = serde_json::to_value(out)?;
        if let Backend::Pg { pg, .. } = &s.backend {
            let mut pg = pg.lock().unwrap();
            match pg.is_readonly() {
                Some(false) if readonly => {
                    let _ = pg.set_readonly(true);
                    if v["error"].is_null() {
                        v["error"] = json!({ "message": "that turned read-only off; furet turned it back on" });
                    }
                }
                Some(now) => s.readonly.store(now, Ordering::SeqCst),
                None => {}
            }
        }
        v["readonly"] = json!(s.readonly.load(Ordering::SeqCst));
        Ok(v)
    }

    pub fn cancel(&self, root: &std::path::Path, id: &str) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        match &s.backend {
            Backend::Pg { cancel, .. } => {
                let (token, tls) = cancel.clone();
                token.cancel_query(tls)?;
            }
            Backend::Ch(ch) => ch.cancel()?,
        }
        Ok(json!({ "cancelled": id }))
    }

    pub fn mode(&self, root: &std::path::Path, id: &str, req: ModeReq) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        check_write(&s.name, s.prod, req.write, req.confirm.as_deref())?;
        match &s.backend {
            Backend::Pg { pg, .. } => pg.lock().unwrap().set_readonly(!req.write)?,
            Backend::Ch(ch) => ch
                .set_readonly(!req.write)
                .map_err(|e| Fail(409, format!("{e:#}")))?,
        }
        s.readonly.store(!req.write, Ordering::SeqCst);
        Ok(s.describe())
    }

    pub fn catalog(&self, root: &std::path::Path, id: &str) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        Ok(match &s.backend {
            Backend::Pg { pg, .. } => pg.lock().unwrap().catalog()?,
            Backend::Ch(ch) => ch.catalog()?,
        })
    }

    pub fn apply(&self, root: &std::path::Path, id: &str, req: ApplyReq) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        if req.statements.is_empty() {
            return fail(400, "nothing to apply");
        }
        if s.readonly.load(Ordering::SeqCst) {
            return fail(
                409,
                "the session is read-only: switch it to read-write to apply changes",
            );
        }
        let Backend::Pg { pg, .. } = &s.backend else {
            return fail(
                400,
                "ClickHouse changes rows with asynchronous mutations (ALTER TABLE … UPDATE/DELETE): run them from a console",
            );
        };
        Ok(match pg.lock().unwrap().apply(&req.statements) {
            Ok(n) => json!({ "applied": n }),
            Err((index, message)) => {
                json!({ "applied": 0, "error": { "index": index, "message": message } })
            }
        })
    }

    pub fn table(
        &self,
        root: &std::path::Path,
        id: &str,
        schema: &str,
        name: &str,
    ) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        Ok(match &s.backend {
            Backend::Pg { pg, .. } => pg.lock().unwrap().table(schema, name)?,
            Backend::Ch(ch) => ch.table(schema, name)?,
        })
    }

    /// The row a foreign key value points at (Postgres): its columns carry their own references.
    pub fn lookup(&self, root: &std::path::Path, id: &str, req: LookupReq) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        let Backend::Pg { pg, .. } = &s.backend else {
            return fail(400, "ClickHouse has no foreign keys");
        };
        let target = pg::Ref {
            schema: req.schema,
            table: req.table,
            column: req.column,
        };
        let mut pg = pg.lock().unwrap();
        let limit = req.limit.unwrap_or(500).clamp(1, 10_000);
        let set = match (&req.value, &req.values) {
            (_, Some(values)) => pg.lookup_many(&target, values),
            (Some(value), None) => pg.lookup(&target, value, limit),
            (None, None) => return fail(400, "give value or values"),
        }
        .map_err(|e| Fail(400, format!("{e:#}")))?;
        Ok(serde_json::to_value(set)?)
    }

    /// The foreign keys pointing at a row, with how many rows each has there (Postgres).
    pub fn referrers(
        &self,
        root: &std::path::Path,
        id: &str,
        req: ReferrersReq,
    ) -> Result<Value, Fail> {
        let s = self.session(root, id)?;
        let Backend::Pg { pg, .. } = &s.backend else {
            return fail(400, "ClickHouse has no foreign keys");
        };
        let out = pg
            .lock()
            .unwrap()
            .referrers(&req.schema, &req.table, &req.row)
            .map_err(|e| Fail(400, format!("{e:#}")))?;
        Ok(json!({ "referrers": out }))
    }

    /// How two Postgres connections' schemas differ, and the SQL that would align the right one.
    pub fn drift(&self, req: DriftReq) -> Result<Value, Fail> {
        let open = |name: &str| {
            self.open(OpenReq {
                cwd: req.cwd.clone(),
                name: name.into(),
                write: false,
                confirm: None,
            })
            .map(|v| v["id"].as_str().unwrap_or_default().to_string())
        };
        let left = open(&req.left)?;
        let right = match open(&req.right) {
            Ok(id) => id,
            Err(e) => {
                self.sessions.lock().unwrap().remove(&left);
                return Err(e);
            }
        };
        let get = |id: &str| self.sessions.lock().unwrap().get(id).cloned();
        let result = (|| {
            let (Some(a), Some(b)) = (get(&left), get(&right)) else {
                return fail(500, "sessions went away");
            };
            let (Backend::Pg { pg: l, .. }, Backend::Pg { pg: r, .. }) = (&a.backend, &b.backend)
            else {
                return fail(400, "comparing schemas works between Postgres connections");
            };
            let out = drift::compare(
                &mut l.lock().unwrap(),
                &mut r.lock().unwrap(),
                req.schema.as_deref(),
            )
            .map_err(|e| Fail(400, format!("{e:#}")))?;
            Ok(serde_json::to_value(out)?)
        })();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(&left);
        sessions.remove(&right);
        result
    }

    /// Copies a row and what it needs to a local/dev/test connection (Postgres to Postgres).
    pub fn copy(&self, req: CopyReq) -> Result<Value, Fail> {
        let scope = Scope::load_for(&req.cwd, &req.to)?;
        let target = scope
            .get(&req.to)
            .map_err(|e| Fail(404, format!("{e:#}")))?;
        if target.conn.is_prod() || target.conn.readonly() {
            return fail(
                403,
                format!(
                    "{} isn't a local, dev or test connection: copies only go there",
                    req.to
                ),
            );
        }
        if req.from == req.to {
            return fail(400, "pick another connection to copy to");
        }
        let open = |name: &str, write: bool| {
            self.open(OpenReq {
                cwd: req.cwd.clone(),
                name: name.into(),
                write,
                confirm: None,
            })
            .map(|v| v["id"].as_str().unwrap_or_default().to_string())
        };
        let from = open(&req.from, false)?;
        let to = match open(&req.to, true) {
            Ok(id) => id,
            Err(e) => {
                self.sessions.lock().unwrap().remove(&from);
                return Err(e);
            }
        };
        let get = |id: &str| self.sessions.lock().unwrap().get(id).cloned();
        let result = (|| {
            let (Some(a), Some(b)) = (get(&from), get(&to)) else {
                return fail(500, "sessions went away");
            };
            let (Backend::Pg { pg: src, .. }, Backend::Pg { pg: dst, .. }) =
                (&a.backend, &b.backend)
            else {
                return fail(400, "copying rows works between Postgres connections");
            };
            let rows = graph::collect(
                &mut src.lock().unwrap(),
                &req.schema,
                &req.table,
                &req.column,
                &req.value,
                req.children,
            )
            .map_err(|e| Fail(400, format!("{e:#}")))?;
            let out = if req.dry {
                graph::Outcome {
                    plan: graph::plan(&rows),
                    ..Default::default()
                }
            } else {
                graph::insert(&mut dst.lock().unwrap(), &rows, req.mask)
                    .map_err(|e| Fail(400, format!("{e:#}")))?
            };
            Ok(serde_json::to_value(out)?)
        })();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(&from);
        sessions.remove(&to);
        result
    }

    pub fn history(&self, cwd: &str, name: Option<&str>, limit: usize) -> Result<Value, Fail> {
        let root = self.root_of(cwd)?;
        Ok(json!({ "history": history::recent(&root, name, limit.clamp(1, 2000)) }))
    }

    pub fn queries(&self, cwd: &str) -> Result<Value, Fail> {
        Ok(json!({ "queries": history::saved(std::path::Path::new(cwd)) }))
    }

    pub fn save_query(&self, req: SaveQueryReq) -> Result<Value, Fail> {
        let file = history::save(&req.cwd, &req.name, &req.sql, req.connection.as_deref())
            .map_err(|e| Fail(400, format!("{e:#}")))?;
        Ok(json!({ "saved": req.name, "file": file }))
    }

    pub fn remove_query(&self, cwd: &str, name: &str) -> Result<Value, Fail> {
        history::remove(std::path::Path::new(cwd), name)
            .map_err(|e| Fail(400, format!("{e:#}")))?;
        Ok(json!({ "removed": name }))
    }

    /// Closes sessions idle for longer than `SESSION_IDLE` or whose connection dropped.
    pub fn sweep(&self) {
        self.sessions.lock().unwrap().retain(|_, s| {
            let idle = s.last_used.lock().unwrap().elapsed() < SESSION_IDLE;
            let alive = match &s.backend {
                Backend::Pg { pg, .. } => pg.try_lock().map(|pg| !pg.is_closed()).unwrap_or(true),
                Backend::Ch(_) => true,
            };
            idle && alive
        });
    }

    /// Closes every session; their tunnels stop with them.
    pub fn close_all(&self) {
        self.sessions.lock().unwrap().clear();
        self.tunnels.lock().unwrap().clear();
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}
