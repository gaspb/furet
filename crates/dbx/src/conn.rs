//! Turning a configured connection into what a client needs: secrets read, URL, env vars, argv.

use crate::config::{Conn, Entry, Kind, Layer, Value};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub kind: Kind,
    pub host: String,
    pub port: u16,
    /// ClickHouse's HTTP port (8123, or 8443 with `secure=1`); 0 for Postgres.
    pub http_port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub params: Vec<(String, String)>,
    pub readonly: bool,
}

const PG_READONLY: &str = "-c default_transaction_read_only=on";

pub fn read_value(v: &Value, base: &Path) -> Result<String> {
    match v {
        Value::Plain(s) => Ok(s.clone()),
        Value::Env { env } => std::env::var(env).with_context(|| format!("${env} is not set")),
        Value::Keychain { keychain } => crate::store::keychain_get(keychain),
        Value::Dotenv { dotenv, key } => {
            let path = common::scope::expand(dotenv, base);
            let text = fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            dotenv_get(&text, key)
                .with_context(|| format!("{key} is not set in {}", path.display()))
        }
        Value::Cmd { cmd } => {
            let out = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(base)
                .stdin(Stdio::inherit())
                .stderr(Stdio::inherit())
                .output()
                .with_context(|| format!("cannot run `{cmd}`"))?;
            if !out.status.success() {
                bail!("`{cmd}` failed ({})", out.status);
            }
            let s = String::from_utf8(out.stdout).context("command output is not UTF-8")?;
            Ok(s.trim_end_matches(['\n', '\r']).to_string())
        }
    }
}

fn dotenv_get(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim_start();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        if let Some(inner) = v
            .strip_prefix('"')
            .and_then(|r| r.rsplit_once('"'))
            .map(|(a, _)| a)
        {
            return Some(inner.replace("\\n", "\n").replace("\\\"", "\""));
        }
        if let Some(inner) = v
            .strip_prefix('\'')
            .and_then(|r| r.rsplit_once('\''))
            .map(|(a, _)| a)
        {
            return Some(inner.to_string());
        }
        let v = v.split_once(" #").map_or(v, |(a, _)| a).trim_end();
        return Some(v.to_string());
    }
    None
}

fn kind_of_scheme(scheme: &str) -> Option<Kind> {
    match scheme {
        "postgres" | "postgresql" => Some(Kind::Postgres),
        "clickhouse" => Some(Kind::Clickhouse),
        _ => None,
    }
}

fn default_port(kind: Kind) -> u16 {
    match kind {
        Kind::Postgres => 5432,
        Kind::Clickhouse => 9000,
    }
}

fn decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

pub fn resolve(entry: &Entry, layer: &Layer) -> Result<Resolved> {
    let c = &entry.conn;
    if let Some(why) = &c.unreachable {
        bail!("{}: {why}", entry.name);
    }
    let read = |v: &Option<Value>| v.as_ref().map(|v| read_value(v, &layer.base)).transpose();
    let mut r = Resolved {
        kind: Kind::Postgres,
        host: String::new(),
        port: 0,
        http_port: 0,
        user: None,
        password: None,
        database: None,
        params: Vec::new(),
        readonly: c.readonly(),
    };
    let mut kind = c.kind;
    if let Some(raw) = read(&c.url)? {
        let u = Url::parse(&raw).with_context(|| format!("{}: invalid url", entry.name))?;
        kind = kind.or_else(|| kind_of_scheme(u.scheme()));
        r.host = u
            .host_str()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .to_string();
        r.port = u.port().unwrap_or(0);
        r.user = Some(decode(u.username())).filter(|s| !s.is_empty());
        r.password = u.password().map(decode);
        r.database = Some(decode(u.path().trim_start_matches('/'))).filter(|s| !s.is_empty());
        r.params = u
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    }
    let Some(kind) = kind else {
        bail!(
            "{}: set `kind = \"postgres\"` or `\"clickhouse\"` (or use a postgres:// or clickhouse:// url)",
            entry.name
        );
    };
    r.kind = kind;
    if let Some(h) = read(&c.host)? {
        r.host = h;
    }
    if let Some(p) = c.port {
        r.port = p;
    }
    if let Some(u) = read(&c.user)? {
        r.user = Some(u);
    }
    if let Some(p) = read(&c.password)? {
        r.password = Some(p);
    }
    if let Some(d) = read(&c.database)? {
        r.database = Some(d);
    }
    if r.host.is_empty() {
        r.host = "localhost".into();
    }
    if r.port == 0 {
        r.port = default_port(kind);
    }
    if kind == Kind::Clickhouse {
        let from_url = r
            .params
            .iter()
            .position(|(k, _)| k == "http_port")
            .map(|i| r.params.remove(i).1)
            .and_then(|p| p.parse().ok());
        let secure = r.param("secure").is_some_and(|v| v == "1" || v == "true");
        r.http_port = c
            .http_port
            .or(from_url)
            .unwrap_or(if secure { 8443 } else { 8123 });
    }
    Ok(r)
}

/// `host:port/db` from what's in the file, without running anything; `…` for values read elsewhere.
pub fn endpoint(c: &Conn) -> String {
    let from_url = c
        .url
        .as_ref()
        .and_then(|v| v.plain())
        .and_then(|u| Url::parse(u).ok());
    let field = |v: &Option<Value>| v.as_ref().map(|v| v.plain().unwrap_or("…").to_string());
    let host = field(&c.host)
        .or_else(|| {
            from_url
                .as_ref()
                .and_then(|u| u.host_str().map(String::from))
        })
        .or_else(|| {
            c.url
                .as_ref()
                .filter(|v| v.plain().is_none())
                .map(|_| "…".into())
        })
        .unwrap_or_else(|| "localhost".into());
    let port = c.port.or_else(|| from_url.as_ref().and_then(|u| u.port()));
    let db = field(&c.database).or_else(|| {
        from_url
            .as_ref()
            .map(|u| u.path().trim_start_matches('/').to_string())
            .filter(|s| !s.is_empty())
    });
    let mut s = host;
    if let Some(p) = port {
        s += &format!(":{p}");
    }
    if let Some(d) = db {
        s += &format!("/{d}");
    }
    s
}

/// The local port a tunnelled connection listens on, when it can be known without reading secrets.
pub fn local_port(c: &Conn) -> Option<u16> {
    let kind = c.kind()?;
    let from_url = c
        .url
        .as_ref()
        .and_then(|v| v.plain())
        .and_then(|u| Url::parse(u).ok());
    c.port
        .or_else(|| from_url.as_ref().and_then(|u| u.port()))
        .or(Some(default_port(kind)))
}

impl Resolved {
    pub fn url(&self) -> String {
        let scheme = self.kind.name();
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let mut u =
            Url::parse(&format!("{scheme}://{host}:{}", self.port)).expect("valid base url");
        if let Some(user) = &self.user {
            let _ = u.set_username(user);
        }
        if let Some(pw) = &self.password {
            let _ = u.set_password(Some(pw));
        }
        if let Some(db) = &self.database {
            u.set_path(db);
        }
        let mut params = self.params.clone();
        if self.readonly {
            match self.kind {
                Kind::Postgres => params.push(("options".into(), PG_READONLY.into())),
                Kind::Clickhouse => params.push(("readonly".into(), "1".into())),
            }
        }
        if !params.is_empty() {
            u.query_pairs_mut().extend_pairs(params);
        }
        u.to_string()
    }

    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Variables that point the usual clients and libraries at this connection.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        let mut set = |k: &str, v: Option<&str>| {
            if let Some(v) = v {
                env.push((k.to_string(), v.to_string()));
            }
        };
        let port = self.port.to_string();
        match self.kind {
            Kind::Postgres => {
                set("PGHOST", Some(&self.host));
                set("PGPORT", Some(&port));
                set("PGUSER", self.user.as_deref());
                set("PGPASSWORD", self.password.as_deref());
                set("PGDATABASE", self.database.as_deref());
                set("PGSSLMODE", self.param("sslmode"));
                if self.readonly {
                    let opts = match std::env::var("PGOPTIONS") {
                        Ok(o) if !o.trim().is_empty() => format!("{o} {PG_READONLY}"),
                        _ => PG_READONLY.to_string(),
                    };
                    set("PGOPTIONS", Some(&opts));
                }
            }
            Kind::Clickhouse => {
                set("CLICKHOUSE_HOST", Some(&self.host));
                set("CLICKHOUSE_PORT", Some(&port));
                set("CLICKHOUSE_USER", self.user.as_deref());
                set("CLICKHOUSE_PASSWORD", self.password.as_deref());
                set("CLICKHOUSE_DATABASE", self.database.as_deref());
                set("CLICKHOUSE_URL", Some(&self.url()));
            }
        }
        set("DATABASE_URL", Some(&self.url()));
        env
    }

    /// The command line for an interactive client: `custom` (with placeholders) or the kind's default.
    pub fn client(&self, custom: Option<&[String]>) -> Vec<String> {
        if let Some(argv) = custom {
            return argv.iter().map(|a| self.fill(a)).collect();
        }
        match self.kind {
            Kind::Postgres => vec!["psql".into()],
            Kind::Clickhouse => {
                let mut argv: Vec<String> =
                    if on_path("clickhouse-client") || !on_path("clickhouse") {
                        vec!["clickhouse-client".into()]
                    } else {
                        vec!["clickhouse".into(), "client".into()]
                    };
                argv.extend([
                    "--host".into(),
                    self.host.clone(),
                    "--port".into(),
                    self.port.to_string(),
                ]);
                if let Some(u) = &self.user {
                    argv.extend(["--user".into(), u.clone()]);
                }
                if let Some(d) = &self.database {
                    argv.extend(["--database".into(), d.clone()]);
                }
                if self
                    .param("secure")
                    .is_some_and(|v| v == "1" || v == "true")
                {
                    argv.push("--secure".into());
                }
                if self.readonly {
                    argv.push("--readonly=1".into());
                }
                argv
            }
        }
    }

    fn fill(&self, arg: &str) -> String {
        arg.replace("{url}", &self.url())
            .replace("{host}", &self.host)
            .replace("{port}", &self.port.to_string())
            .replace("{user}", self.user.as_deref().unwrap_or(""))
            .replace("{password}", self.password.as_deref().unwrap_or(""))
            .replace("{database}", self.database.as_deref().unwrap_or(""))
    }
}

pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Layer, Origin};
    use std::path::PathBuf;

    fn entry(toml_conn: &str) -> (Entry, Layer) {
        let conn: Conn = toml::from_str(toml_conn).unwrap();
        let layer = Layer {
            origin: Origin::Project,
            root: None,
            file: PathBuf::new(),
            base: std::env::temp_dir(),
        };
        (
            Entry {
                name: "t".into(),
                layer: 0,
                conn,
            },
            layer,
        )
    }

    #[test]
    fn fields_override_the_url() {
        let (e, l) = entry(
            r#"url = "postgresql://app:old@db:6000/app?sslmode=require"
               password = { cmd = "printf 'p@ss word'" }
               stage = "local""#,
        );
        let r = resolve(&e, &l).unwrap();
        assert_eq!(
            (r.host.as_str(), r.port, r.password.as_deref()),
            ("db", 6000, Some("p@ss word"))
        );
        assert_eq!(
            r.url(),
            "postgres://app:p%40ss%20word@db:6000/app?sslmode=require"
        );
        assert!(r.env().contains(&("PGSSLMODE".into(), "require".into())));
    }

    #[test]
    fn readonly_reaches_url_env_and_client() {
        let (e, l) = entry(
            r#"url = "postgres://app@localhost:15432/app"
                              stage = "stg""#,
        );
        let r = resolve(&e, &l).unwrap();
        assert!(
            r.url()
                .ends_with("?options=-c+default_transaction_read_only%3Don")
        );
        assert!(
            r.env()
                .iter()
                .any(|(k, v)| k == "PGOPTIONS" && v.ends_with(PG_READONLY))
        );

        let (e, l) = entry(
            r#"kind = "clickhouse"
                              user = "default"
                              stage = "prod""#,
        );
        let r = resolve(&e, &l).unwrap();
        assert_eq!((r.port, r.http_port), (9000, 8123));
        assert!(r.client(None).contains(&"--readonly=1".to_string()));
        assert_eq!(r.url(), "clickhouse://default@localhost:9000?readonly=1");
    }

    #[test]
    fn clickhouse_http_port_from_the_field_or_the_url() {
        let (e, l) = entry(r#"url = "clickhouse://default@localhost:19000/db?http_port=18123""#);
        let r = resolve(&e, &l).unwrap();
        assert_eq!((r.port, r.http_port), (19000, 18123));
        assert_eq!(r.url(), "clickhouse://default@localhost:19000/db");
        let (e, l) = entry(
            r#"url = "clickhouse://h/db?secure=1&http_port=1"
               http_port = 9443"#,
        );
        assert_eq!(resolve(&e, &l).unwrap().http_port, 9443);
        let (e, l) = entry(r#"url = "clickhouse://h/db?secure=1""#);
        assert_eq!(resolve(&e, &l).unwrap().http_port, 8443);
    }

    #[test]
    fn reads_dotenv_values() {
        let text = "# c\nexport A=1\nB = \"two words\" \nC='x#y' # note\nD=plain # note\n";
        assert_eq!(dotenv_get(text, "A").as_deref(), Some("1"));
        assert_eq!(dotenv_get(text, "B").as_deref(), Some("two words"));
        assert_eq!(dotenv_get(text, "C").as_deref(), Some("x#y"));
        assert_eq!(dotenv_get(text, "D").as_deref(), Some("plain"));
        assert_eq!(dotenv_get(text, "E"), None);
    }

    #[test]
    fn endpoint_never_runs_secrets() {
        let (e, _) = entry(
            r#"url = { cmd = "exit 1" }
                              kind = "postgres""#,
        );
        assert_eq!(endpoint(&e.conn), "…");
        let (e, _) = entry(r#"url = "postgres://u:p@localhost:15432/app""#);
        assert_eq!(endpoint(&e.conn), "localhost:15432/app");
        assert_eq!(local_port(&e.conn), Some(15432));
    }

    #[test]
    fn custom_clients_get_placeholders() {
        let (e, l) = entry(
            r#"url = "postgres://u@h:1/d"
                              stage = "local""#,
        );
        let r = resolve(&e, &l).unwrap();
        let argv = r.client(Some(&[
            "harlequin".into(),
            "-a".into(),
            "postgres".into(),
            "{url}".into(),
        ]));
        assert_eq!(argv[3], "postgres://u@h:1/d");
    }
}
