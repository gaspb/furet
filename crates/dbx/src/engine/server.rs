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
}

fn dir() -> PathBuf {
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

fn running_version(info: &Info) -> Option<String> {
    let (status, body) = call(info, "GET", "/health")?;
    if status != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&body).ok()?;
    v["version"].as_str().map(String::from)
}

pub fn stop() -> bool {
    read_info().is_some_and(|info| {
        call(&info, "POST", "/shutdown").is_some_and(|(status, _)| status == 200)
    })
}

/// Prints the running server's address and token, starting one in the background if needed.
pub fn ensure() -> Result<Info> {
    if let Some(info) = read_info() {
        match running_version(&info) {
            Some(v) if v == VERSION => return Ok(info),
            Some(_) => {
                call(&info, "POST", "/shutdown");
                wait_for(
                    || running_version(&info).is_none(),
                    "the old server to stop",
                )?;
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
            found = read_info().filter(|i| running_version(i).as_deref() == Some(VERSION));
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
    };
    write_info(&info)?;
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
    let read = req.as_reader().take(BODY_MAX).read_to_string(&mut body);

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
        match (&method, segments.as_slice()) {
            (Method::Get, ["health"]) => Ok(
                json!({ "version": VERSION, "pid": info.pid, "sessions": engine.session_count() }),
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
            (Method::Get, ["sessions"]) => Ok(engine.list()),
            (Method::Post, ["sessions"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<OpenReq>(v).map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|r| engine.open(r)),
            (Method::Delete, ["sessions", id]) => engine.close(id),
            (Method::Post, ["sessions", id, "query"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<QueryReq>(v).map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|r| engine.query(id, r)),
            (Method::Post, ["sessions", id, "cancel"]) => engine.cancel(id),
            (Method::Post, ["sessions", id, "mode"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<ModeReq>(v).map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|r| engine.mode(id, r)),
            (Method::Post, ["sessions", id, "apply"]) => parse(&body)
                .and_then(|v| {
                    serde_json::from_value::<super::ApplyReq>(v)
                        .map_err(|e| Fail(400, e.to_string()))
                })
                .and_then(|r| engine.apply(id, r)),
            (Method::Get, ["sessions", id, "catalog"]) => engine.catalog(id),
            (Method::Get, ["sessions", id, "table"]) => {
                param("schema").and_then(|s| param("name").and_then(|n| engine.table(id, &s, &n)))
            }
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
