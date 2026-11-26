//! Port-forwards started on demand and stopped when furet is done with them.

use anyhow::{Result, bail};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(30);

/// The (local, remote) ports a tunnel command forwards: kubectl's `18123:8123`, ssh's
/// `-L [addr:]18123:host:8123`.
pub fn forwards(cmd: &str) -> Vec<(u16, u16)> {
    cmd.split_whitespace()
        .map(|w| w.trim_matches(['"', '\'']))
        .filter_map(|w| {
            let parts: Vec<&str> = w.split(':').collect();
            let (local, remote) = match parts.as_slice() {
                [l, r] => (*l, *r),
                [l, _, r] => (*l, *r),
                [_, l, _, r] => (*l, *r),
                _ => return None,
            };
            Some((local.parse().ok()?, remote.parse().ok()?))
        })
        .collect()
}

pub fn is_up(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok())
}

type Log = Arc<Mutex<VecDeque<String>>>;

pub struct Tunnel {
    child: Child,
    log: Log,
    record: Option<std::path::PathBuf>,
}

/// What furet records about a tunnel it started, in its private state folder, so another furet process
/// (the engine, a `furet tunnel` in a terminal) can tell its own tunnels from anything else on a port.
#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    /// The connection definition it was started for: `<connections file>#<name>`.
    owner: String,
    pgid: i32,
}

fn record_path(host: &str, port: u16) -> std::path::PathBuf {
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    crate::engine::server::dir()
        .join("tunnels")
        .join(format!("{host}-{port}.json"))
}

fn alive(pgid: i32) -> bool {
    pgid > 0 && unsafe { libc::kill(pgid, 0) } == 0
}

/// Who holds `port`, when `lsof` can tell.
fn listener(port: u16) -> Option<String> {
    let out = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let pid = text.lines().find_map(|l| l.strip_prefix('p'))?;
    let cmd = text
        .lines()
        .find_map(|l| l.strip_prefix('c'))
        .unwrap_or("?");
    Some(format!("pid {pid} ({cmd})"))
}

/// A connection definition as people read it: `stg in ~/src/api`.
fn label(owner: &str) -> String {
    match owner.rsplit_once('#') {
        Some((file, name)) => format!("{name} ({file})"),
        None => owner.to_string(),
    }
}

fn drain(from: impl Read + Send + 'static, log: Log) {
    std::thread::spawn(move || {
        for line in BufReader::new(from).lines().map_while(Result::ok) {
            let mut l = log.lock().unwrap();
            if l.len() == 20 {
                l.pop_front();
            }
            l.push_back(line);
        }
    });
}

impl Tunnel {
    /// Starts `cmd` for the connection `owner` (`<file>#<name>`) and waits for `host:port`.
    /// A port that already answers is used only when it's this connection's tunnel started by
    /// another furet process (Ok(None)); anything else on it is refused, so credentials only ever go
    /// through a tunnel started for them.
    pub fn ensure(
        cmd: &str,
        host: &str,
        port: u16,
        base: &Path,
        owner: &str,
    ) -> Result<Option<Tunnel>> {
        let path = record_path(host, port);
        if is_up(host, port) {
            let record: Option<Record> = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            match record {
                Some(r) if alive(r.pgid) && r.owner == owner => return Ok(None),
                Some(r) if alive(r.pgid) => bail!(
                    "{host}:{port} is already forwarded for {}: give this connection's tunnel another local port",
                    label(&r.owner)
                ),
                _ => bail!(
                    "{host}:{port} is already in use by another program{}; furet only connects through a tunnel it \
                     started for this connection: stop it, or use another local port in the tunnel and the url",
                    listener(port)
                        .map(|l| format!(" ({l})"))
                        .unwrap_or_default()
                ),
            }
        }
        eprintln!(
            "{}",
            crate::paint("2", &format!("furet: starting tunnel: {cmd}"))
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(base)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let log: Log = Arc::default();
        drain(child.stdout.take().unwrap(), log.clone());
        drain(child.stderr.take().unwrap(), log.clone());
        register(child.id() as i32);
        let mut t = Tunnel {
            child,
            log,
            record: None,
        };
        let start = Instant::now();
        loop {
            if is_up(host, port) {
                let record = Record {
                    owner: owner.to_string(),
                    pgid: t.child.id() as i32,
                };
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                    let _ = std::fs::set_permissions(
                        dir,
                        std::os::unix::fs::PermissionsExt::from_mode(0o700),
                    );
                }
                if crate::store::write_private(&path, &serde_json::to_string(&record)?).is_ok() {
                    remove_on_signal(&path);
                    t.record = Some(path);
                }
                return Ok(Some(t));
            }
            if let Some(status) = t.child.try_wait()? {
                std::thread::sleep(Duration::from_millis(100));
                bail!(
                    "tunnel exited ({status}) before {host}:{port} was reachable{}",
                    t.output()
                );
            }
            if start.elapsed() > START_TIMEOUT {
                let other: Vec<String> = forwards(cmd)
                    .into_iter()
                    .filter(|(local, _)| *local != port && is_up(host, *local))
                    .map(|(local, remote)| format!("{local} (→ {remote})"))
                    .collect();
                let hint = if other.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; the tunnel is up on {}: the connection's port (or ClickHouse's http_port) \
                         should be that local port",
                        other.join(", ")
                    )
                };
                bail!(
                    "{host}:{port} still unreachable after {}s{hint}{}",
                    START_TIMEOUT.as_secs(),
                    t.output()
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn output(&self) -> String {
        let l = self.log.lock().unwrap();
        if l.is_empty() {
            String::new()
        } else {
            format!(
                ":\n  {}",
                l.iter().cloned().collect::<Vec<_>>().join("\n  ")
            )
        }
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        Ok(self.child.wait()?)
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let pgid = self.child.id() as i32;
        unregister(pgid);
        if let Some(path) = &self.record {
            let ours = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| serde_json::from_str::<Record>(&t).ok())
                .is_some_and(|r| r.pgid == pgid);
            if ours {
                let _ = std::fs::remove_file(path);
            }
        }
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        for _ in 0..20 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

extern "C" fn ignore(_: libc::c_int) {}

/// Runs a child in the foreground. Ctrl-C and a closed pane go to the child; furet outlives it to
/// stop its tunnels. (A handler, unlike SIG_IGN, is reset to default in the child on exec.)
pub fn run_foreground(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP];
    let mut old = [0 as libc::sighandler_t; 3];
    for (i, s) in SIGNALS.iter().enumerate() {
        old[i] = unsafe { libc::signal(*s, ignore as *const () as libc::sighandler_t) };
    }
    let status = cmd.status();
    for (i, s) in SIGNALS.iter().enumerate() {
        unsafe { libc::signal(*s, old[i]) };
    }
    status
}

/// Process groups of running tunnels, so a signal that ends furet can stop them first.
static GROUPS: [AtomicI32; 16] = [const { AtomicI32::new(0) }; 16];

fn register(pgid: i32) {
    for slot in &GROUPS {
        if slot
            .compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
}

fn unregister(pgid: i32) {
    for slot in &GROUPS {
        let _ = slot.compare_exchange(pgid, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// Files to remove if a signal ends furet, as C strings the handler can use.
static RECORDS: [std::sync::atomic::AtomicPtr<libc::c_char>; 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 16];

/// A file the signal handler removes on the way out (tunnel records, the engine's serve.json).
pub fn remove_on_signal(path: &Path) {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    let raw = c.into_raw();
    for slot in &RECORDS {
        if slot
            .compare_exchange(
                std::ptr::null_mut(),
                raw,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return;
        }
    }
    drop(unsafe { std::ffi::CString::from_raw(raw) });
}

extern "C" fn stop_tunnels_and_die(sig: libc::c_int) {
    for slot in &RECORDS {
        let p = slot.load(Ordering::SeqCst);
        if !p.is_null() {
            unsafe { libc::unlink(p) };
        }
    }
    for slot in &GROUPS {
        let pgid = slot.load(Ordering::SeqCst);
        if pgid > 0 {
            unsafe { libc::kill(-pgid, libc::SIGTERM) };
        }
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Stops every tunnel this process started (on the way out, whoever still holds them).
pub fn stop_all() {
    for slot in &GROUPS {
        let pgid = slot.swap(0, Ordering::SeqCst);
        if pgid > 0 {
            unsafe { libc::kill(-pgid, libc::SIGTERM) };
        }
    }
}

/// Ctrl-C, a closed pane or a kill stops furet's tunnels instead of leaving them running.
pub fn stop_tunnels_on_exit_signals() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe { libc::signal(sig, stop_tunnels_and_die as *const () as libc::sighandler_t) };
    }
}
