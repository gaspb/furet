//! Port-forwards started on demand and stopped when dbx is done with them.

use anyhow::{Result, bail};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Starts `cmd` unless `host:port` already answers (another pane's tunnel, say), then waits for it.
    pub fn ensure(cmd: &str, host: &str, port: u16, base: &Path) -> Result<Option<Tunnel>> {
        if is_up(host, port) {
            return Ok(None);
        }
        eprintln!(
            "{}",
            crate::paint("2", &format!("dbx: starting tunnel: {cmd}"))
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
        let mut t = Tunnel { child, log };
        let start = Instant::now();
        loop {
            if is_up(host, port) {
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
                bail!(
                    "{host}:{port} still unreachable after {}s{}",
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

/// Runs a child in the foreground. Ctrl-C and a closed pane go to the child; dbx outlives it to
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
