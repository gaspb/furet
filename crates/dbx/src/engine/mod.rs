//! `dbx serve`: a local engine holding database sessions for the herdr panel, over HTTP on 127.0.0.1.

pub mod pg;
pub mod server;
pub mod sql;

use crate::config::{Kind, Scope};
use crate::conn;
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
    pub prod: bool,
    pub info: Value,
    pg: Mutex<pg::Pg>,
    cancel: (
        postgres::CancelToken,
        tokio_postgres_rustls::MakeRustlsConnect,
    ),
    readonly: AtomicBool,
    last_used: Mutex<Instant>,
    _tunnel: Option<Arc<Tunnel>>,
}

impl Session {
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
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
    tunnels: Mutex<HashMap<(String, u16), Weak<Tunnel>>>,
    pub last_request: Mutex<Option<Instant>>,
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
}

#[derive(Deserialize)]
pub struct ModeReq {
    pub write: bool,
    pub confirm: Option<String>,
}

fn random_id() -> String {
    server::random_hex(8)
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
        let scope = Scope::load(std::path::Path::new(cwd))?;
        Ok(json!({
            "root": scope.root,
            "file": crate::config::project_file(&scope.root),
            "connections": crate::rows(&scope),
        }))
    }

    pub fn open(&self, req: OpenReq) -> Result<Value, Fail> {
        let scope = Scope::load(&req.cwd)?;
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
        if entry.conn.kind() == Some(Kind::Clickhouse) {
            return fail(
                501,
                format!(
                    "{} is clickhouse: only Postgres is supported in the panel so far",
                    req.name
                ),
            );
        }
        let mut r = conn::resolve(entry, layer)?;
        if r.kind != Kind::Postgres {
            return fail(
                501,
                format!(
                    "{} is {}: only Postgres is supported in the panel so far",
                    req.name,
                    r.kind.name()
                ),
            );
        }
        if req.write {
            r.readonly = false;
        }
        let tunnel = match &entry.conn.tunnel {
            Some(cmd) => self.tunnel(cmd, &r.host, r.port, &layer.base)?,
            None => None,
        };
        let pg = pg::Pg::connect(&r).map_err(|e| Fail(502, format!("{e:#}")))?;
        let info = json!({
            "name": req.name,
            "kind": r.kind.name(),
            "stage": entry.conn.stage(),
            "prod": prod,
            "database": r.database,
            "user": r.user,
            "host": r.host,
            "port": r.port,
            "server_version": pg.server_version,
            "cwd": req.cwd,
        });
        let s = Arc::new(Session {
            id: random_id(),
            name: req.name,
            prod: prod && readonly_default,
            info,
            cancel: pg.cancel_token(),
            pg: Mutex::new(pg),
            readonly: AtomicBool::new(r.readonly),
            last_used: Mutex::new(Instant::now()),
            _tunnel: tunnel,
        });
        let v = s.describe();
        self.sessions.lock().unwrap().insert(s.id.clone(), s);
        Ok(v)
    }

    /// Sessions on the same tunnelled port share one tunnel; it stops with the last of them.
    fn tunnel(
        &self,
        cmd: &str,
        host: &str,
        port: u16,
        base: &std::path::Path,
    ) -> Result<Option<Arc<Tunnel>>, Fail> {
        let key = (host.to_string(), port);
        let mut tunnels = self.tunnels.lock().unwrap();
        if let Some(t) = tunnels.get(&key).and_then(Weak::upgrade) {
            return Ok(Some(t));
        }
        match Tunnel::ensure(cmd, host, port, base).map_err(|e| Fail(502, format!("{e:#}")))? {
            Some(t) => {
                let t = Arc::new(t);
                tunnels.insert(key, Arc::downgrade(&t));
                Ok(Some(t))
            }
            None => Ok(None),
        }
    }

    pub fn session(&self, id: &str) -> Result<Arc<Session>, Fail> {
        let s = self.sessions.lock().unwrap().get(id).cloned();
        let Some(s) = s else {
            return fail(404, "no such session (closed after 30 minutes idle?)");
        };
        s.touch();
        Ok(s)
    }

    pub fn list(&self) -> Value {
        Value::Array(
            self.sessions
                .lock()
                .unwrap()
                .values()
                .map(|s| s.describe())
                .collect(),
        )
    }

    pub fn close(&self, id: &str) -> Result<Value, Fail> {
        match self.sessions.lock().unwrap().remove(id) {
            Some(_) => Ok(json!({ "closed": id })),
            None => fail(404, "no such session"),
        }
    }

    pub fn query(&self, id: &str, req: QueryReq) -> Result<Value, Fail> {
        let s = self.session(id)?;
        let sql = &req.sql;
        let clamp = |o: usize| sql::utf16_to_byte(sql, o);
        let range = if req.all {
            (0, sql.len())
        } else if let Some((a, b)) = req.selection.filter(|(a, b)| a != b) {
            (clamp(a.min(b)), clamp(a.max(b)))
        } else {
            match sql::at(sql, clamp(req.cursor.unwrap_or(0))) {
                Some(r) => r,
                None => return fail(400, "nothing to run"),
            }
        };
        let limit = req.limit.unwrap_or(500).clamp(1, 10_000);
        let out =
            s.pg.lock()
                .unwrap()
                .run(sql, range, req.all, limit, req.offset);
        if out.error.as_ref().is_some_and(|e| e.closed) {
            self.sessions.lock().unwrap().remove(id);
        }
        Ok(serde_json::to_value(out)?)
    }

    pub fn cancel(&self, id: &str) -> Result<Value, Fail> {
        let s = self.session(id)?;
        let (token, tls) = s.cancel.clone();
        token.cancel_query(tls)?;
        Ok(json!({ "cancelled": id }))
    }

    pub fn mode(&self, id: &str, req: ModeReq) -> Result<Value, Fail> {
        let s = self.session(id)?;
        check_write(&s.name, s.prod, req.write, req.confirm.as_deref())?;
        s.pg.lock().unwrap().set_readonly(!req.write)?;
        s.readonly.store(!req.write, Ordering::SeqCst);
        Ok(s.describe())
    }

    pub fn catalog(&self, id: &str) -> Result<Value, Fail> {
        let s = self.session(id)?;
        Ok(s.pg.lock().unwrap().catalog()?)
    }

    pub fn table(&self, id: &str, schema: &str, name: &str) -> Result<Value, Fail> {
        let s = self.session(id)?;
        Ok(s.pg.lock().unwrap().table(schema, name)?)
    }

    /// Closes sessions idle for longer than `SESSION_IDLE` or whose connection dropped.
    pub fn sweep(&self) {
        self.sessions.lock().unwrap().retain(|_, s| {
            let idle = s.last_used.lock().unwrap().elapsed() < SESSION_IDLE;
            let alive = s.pg.try_lock().map(|pg| !pg.is_closed()).unwrap_or(true);
            idle && alive
        });
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}
