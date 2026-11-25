//! The HTTP side of `dbx serve`: 127.0.0.1 only, a random token on every request, one server per user.

use super::{Engine, Fail, ModeReq, OpenReq, QueryReq, fail};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BODY_MAX: u64 = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Info {
    pub url: String,
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
    /// Which binary is serving: a reinstall (same version, new build) restarts it.
    #[serde(default)]
    pub build: String,
}

/// This binary's identity: its modification time and size.
fn build_id() -> String {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            format!("{t}-{}", m.len())
        })
        .unwrap_or_default()
}

pub(crate) fn dir() -> PathBuf {
    std::env::var_os("DBX_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| common::scope::state_home().join("dbx"))
}

fn info_file() -> PathBuf {
    dir().join("serve.json")
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom is readable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn private_dir() -> Result<PathBuf> {
    let d = dir();
    fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
    fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
    Ok(d)
}

fn write_info(info: &Info) -> Result<()> {
    let path = info_file();
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string(info)?.as_bytes())?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn read_info() -> Option<Info> {
    serde_json::from_str(&fs::read_to_string(info_file()).ok()?).ok()
}

/// A tiny HTTP client for talking to a running server.
fn call(info: &Info, method: &str, path: &str) -> Option<(u16, String)> {
    let addr = format!("127.0.0.1:{}", info.port).parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        info.port, info.token
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).ok()?;
    let status = resp.split_whitespace().nth(1)?.parse().ok()?;
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// The running server's version and build, if it answers.
fn running(info: &Info) -> Option<(String, String)> {
    let (status, body) = call(info, "GET", "/health")?;
    if status != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&body).ok()?;
    Some((
        v["version"].as_str()?.to_string(),
        v["build"].as_str().unwrap_or_default().to_string(),
    ))
}

fn current(info: &Info) -> bool {
    running(info).is_some_and(|(v, b)| v == VERSION && b == build_id())
}

pub fn stop() -> bool {
    read_info().is_some_and(|info| {
        call(&info, "POST", "/shutdown").is_some_and(|(status, _)| status == 200)
    })
}

/// Prints the running server's address and token, starting one in the background if needed.
/// Whether a dbx engine holds the lock: only your processes can open it (0700 folder), so a
/// `serve.json` without a holder is stale, whatever answers on its port.
fn engine_running() -> bool {
    let Ok(lock) = fs::OpenOptions::new()
        .write(true)
        .open(dir().join("serve.lock"))
    else {
        return false;
    };
    let free = unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&lock),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } == 0;
    if free {
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_UN) };
    }
    !free
}

pub fn ensure() -> Result<Info> {
    if !engine_running() {
        let _ = fs::remove_file(info_file());
    }
    if let Some(info) = read_info() {
        match running(&info) {
            Some(_) if current(&info) => return Ok(info),
            Some(_) => {
                call(&info, "POST", "/shutdown");
                wait_for(|| running(&info).is_none(), "the old server to stop")?;
            }
            None => {}
        }
    }
    let d = private_dir()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(d.join("serve.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("cannot start dbx serve")?;
    let mut found = None;
    wait_for(
        || {
            found = read_info().filter(|i| engine_running() && current(i));
            found.is_some()
        },
        "dbx serve to start",
    )?;
    Ok(found.unwrap())
}

fn wait_for(mut done: impl FnMut() -> bool, what: &str) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if done() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "timed out waiting for {what} (see {})",
        dir().join("serve.log").display()
    )
}

/// Runs the server in the foreground until it's idle for `idle` with no session open, or told to stop.
pub fn serve(idle: Duration) -> Result<()> {
    let d = private_dir()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(d.join("serve.lock"))?;
    if unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&lock),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        eprintln!("dbx serve is already running");
        return Ok(());
    }
    let _ = crate::panel::PANEL.refresh();
    crate::update::watch();
    let server =
        Arc::new(Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("cannot listen: {e}"))?);
    let port = server
        .server_addr()
        .to_ip()
        .context("not an IP listener")?
        .port();
    let info = Info {
        url: format!("http://127.0.0.1:{port}"),
        port,
        token: random_hex(32),
        pid: std::process::id(),
        version: VERSION.into(),
        build: build_id(),
    };
    write_info(&info)?;
    crate::tunnel::remove_on_signal(&info_file());
    eprintln!(
        "dbx serve {VERSION} listening on {} (pid {})",
        info.url, info.pid
    );

    let engine = Arc::new(Engine::default());
    *engine.last_request.lock().unwrap() = Some(Instant::now());
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (engine, server, stop) = (engine.clone(), server.clone(), stop.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(20));
                engine.sweep();
                let quiet = engine
                    .last_request
                    .lock()
                    .unwrap()
                    .is_none_or(|t| t.elapsed() > idle);
                if (quiet && engine.session_count() == 0) || stop.load(Ordering::SeqCst) {
                    stop.store(true, Ordering::SeqCst);
                    server.unblock();
                    return;
                }
            }
        });
    }
    let info = Arc::new(info);
    for req in server.incoming_requests() {
        let (engine, info, flag, server) =
            (engine.clone(), info.clone(), stop.clone(), server.clone());
        std::thread::spawn(move || {
            *engine.last_request.lock().unwrap() = Some(Instant::now());
            if respond(&engine, &info, req) {
                flag.store(true, Ordering::SeqCst);
                server.unblock();
            }
        });
        if stop.load(Ordering::SeqCst) {
            break;
        }
    }
    // Request threads may still hold sessions: stop the tunnels by process group as well.
    engine.close_all();
    crate::tunnel::stop_all();
    if read_info().is_some_and(|i| i.pid == std::process::id()) {
        let _ = fs::remove_file(info_file());
    }
    eprintln!("dbx serve stopped");
    Ok(())
}

fn header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Answers one request; true when it asked the server to stop.
fn respond(engine: &Engine, info: &Info, mut req: Request) -> bool {
    let hosts = [
        format!("127.0.0.1:{}", info.port),
        format!("localhost:{}", info.port),
    ];
    let host_ok = header(&req, "Host").is_some_and(|h| hosts.iter().any(|x| x == h));
    let token_ok = header(&req, "Authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.as_bytes(), info.token.as_bytes()));
    let (url, method) = (req.url().to_string(), req.method().clone());
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let params: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let mut body = String::new();
    // Nothing of a request is read before it's authorised.
    let read = if host_ok && token_ok {
        req.as_reader().take(BODY_MAX).read_to_string(&mut body)
    } else {
        Ok(0)
    };

    let mut shutdown = false;
    let result: Result<Value, Fail> = if !host_ok || !token_ok {
        fail(401, "unauthorized")
    } else if read.is_err() {
        fail(400, "unreadable body")
    } else {
        let json = |b: &str| -> Result<Value, Fail> {
            serde_json::from_str(b).map_err(|e| Fail(400, format!("bad json: {e}")))
        };
        let parse = |b: &str| json(if b.trim().is_empty() { "{}" } else { b });
        let param = |k: &str| {
            params
                .get(k)
                .cloned()
                .ok_or_else(|| Fail(400, format!("missing ?{k}=")))
        };
        // Session routes act for the repo the caller says it's in, on that repo's sessions only.
        let root = || param("cwd").and_then(|c| engine.root_of(&c));
        match (&method, segments.as_slice()) {
            (Method::Get, ["health"]) => Ok(
                json!({ "version": VERSION, "build": info.build, "pid": info.pid, "sessions": engine.session_count() }),
            ),
            (Method::Post, ["shutdown"]) => {
                shutdown = true;
                Ok(json!({ "stopping": true }))
            }
            (Method::Get, ["connections"]) => param("cwd").and_then(|cwd| engine.connections(&cwd)),
            (Method::Get, ["connections", "draft"]) => {
                param("cwd").and_then(|c| param("name").and_then(|n| engine.draft(&c, &n)))
            }
            (Method::Post, ["connections", "disconnect"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<super::NameReq>(v)
                        .map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|r| engine.disconnect(r)),
            (Method::Post, ["connections", what @ ("test" | "save")]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<super::DraftReq>(v)
                        .map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|r| {
                    if *what == "test" {
                        engine.test(r)
                    } else {
                        engine.save(r)
                    }
                }),
            (Method::Delete, ["connections"]) => {
                param("cwd").and_then(|c| param("name").and_then(|n| engine.remove(&c, &n)))
            }
            (Method::Get, ["sessions"]) => root().map(|r| engine.list(&r)),
            (Method::Post, ["sessions"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<OpenReq>(v).map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|r| engine.open(r)),
            (Method::Delete, ["sessions", id]) => root().and_then(|r| engine.close(&r, id)),
            (Method::Post, ["sessions", id, "query"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<QueryReq>(v).map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|q| root().and_then(|r| engine.query(&r, id, q))),
            (Method::Post, ["sessions", id, "cancel"]) => {
                root().and_then(|r| engine.cancel(&r, id))
            }
            (Method::Post, ["sessions", id, "mode"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<ModeReq>(v).map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|m| root().and_then(|r| engine.mode(&r, id, m))),
            (Method::Post, ["sessions", id, "apply"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<super::ApplyReq>(v)
                        .map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|a| root().and_then(|r| engine.apply(&r, id, a))),
            (Method::Post, ["sessions", id, "lookup"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<super::LookupReq>(v)
                        .map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|l| root().and_then(|r| engine.lookup(&r, id, l))),
            (Method::Get, ["sessions", id, "catalog"]) => {
                root().and_then(|r| engine.catalog(&r, id))
            }
            (Method::Get, ["sessions", id, "table"]) => root().and_then(|r| {
                param("schema")
                    .and_then(|s| param("name").and_then(|n| engine.table(&r, id, &s, &n)))
            }),
            _ => fail(404, format!("no route for {method} {path}")),
        }
    };
    let (status, value) = match result {
        Ok(v) => (200, v),
        Err(Fail(status, msg)) => (status, json!({ "error": msg })),
    };
    let resp = Response::from_string(value.to_string())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
    let _ = req.respond(resp);
    shutdown
}
