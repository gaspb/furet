//! The same `dbx serve` engine as the CLI and the herdr panel: started with `dbx serve --ensure`,
//! asked over HTTP on 127.0.0.1 with its token. Blocking calls, made off the UI thread.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Command;

#[derive(Deserialize, Clone, Debug)]
pub struct Engine {
    pub url: String,
    pub token: String,
}

impl Engine {
    /// The running engine, started if it isn't (`DBX_BIN` picks another `dbx`).
    pub fn ensure() -> Result<Engine> {
        let bin = std::env::var("DBX_BIN").unwrap_or_else(|_| "dbx".into());
        let out = Command::new(&bin)
            .args(["serve", "--ensure"])
            .output()
            .with_context(|| format!("cannot run {bin}: is dbx installed?"))?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(serde_json::from_slice(&out.stdout)?)
    }

    pub fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let url = format!("{}{path}", self.url);
        let auth = format!("Bearer {}", self.token);
        let mut res = match (method, body) {
            ("POST", Some(b)) => agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(serde_json::to_string(&b)?)?,
            ("DELETE", _) => agent.delete(&url).header("Authorization", &auth).call()?,
            _ => agent.get(&url).header("Authorization", &auth).call()?,
        };
        let ok = res.status().is_success();
        let v: Value =
            serde_json::from_str(&res.body_mut().read_to_string()?).unwrap_or(Value::Null);
        if !ok {
            bail!("{}", v["error"].as_str().unwrap_or("dbx serve refused"));
        }
        Ok(v)
    }
}

fn q(cwd: &str) -> String {
    format!("cwd={}", urlencode(cwd))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// What the app asks of the engine, scoped to the repo it was opened in.
#[derive(Clone)]
pub struct Client {
    pub engine: Engine,
    pub cwd: String,
}

impl Client {
    pub fn connections(&self) -> Result<Value> {
        self.engine
            .call("GET", &format!("/connections?{}", q(&self.cwd)), None)
    }

    pub fn open(&self, name: &str) -> Result<Value> {
        self.engine.call(
            "POST",
            &format!("/sessions?{}", q(&self.cwd)),
            Some(json!({ "cwd": self.cwd, "name": name })),
        )
    }

    pub fn catalog(&self, session: &str) -> Result<Value> {
        self.engine.call(
            "GET",
            &format!("/sessions/{session}/catalog?{}", q(&self.cwd)),
            None,
        )
    }

    /// `cursor`: a UTF-16 offset (what the engine counts in).
    pub fn query(&self, session: &str, sql: &str, cursor: usize, all: bool) -> Result<Value> {
        self.engine.call(
            "POST",
            &format!("/sessions/{session}/query?{}", q(&self.cwd)),
            Some(json!({ "sql": sql, "cursor": cursor, "all": all, "limit": 1000, "by": "app" })),
        )
    }
}
