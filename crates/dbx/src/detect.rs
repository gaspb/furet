//! Databases running in Docker, found from `docker inspect`: usable without configuring anything.
//! Credentials come from the container's environment each time; nothing is written anywhere.

use crate::config::{Conn, Kind, Value};
use serde_json::Value as Json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    pub conn: Conn,
    /// Started by this repo's compose file (or one of its worktrees').
    pub here: bool,
}

/// What Docker had, and why it couldn't be asked when it couldn't.
#[derive(Debug, Default)]
pub struct Detection {
    pub found: Vec<Found>,
    pub problem: Option<String>,
}

/// The docker CLI: on PATH, else where Docker Desktop, Homebrew, OrbStack, Rancher Desktop or
/// Colima put it (an app started from the Dock or a launcher has a minimal PATH).
fn docker_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DBX_DOCKER_BIN") {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("docker"))
            .find(|f| f.is_file())
    }) {
        return Some(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    [
        PathBuf::from("/usr/local/bin/docker"),
        PathBuf::from("/opt/homebrew/bin/docker"),
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin/docker"),
        home.join(".orbstack/bin/docker"),
        home.join(".rd/bin/docker"),
        home.join(".docker/bin/docker"),
        PathBuf::from("/usr/bin/docker"),
        PathBuf::from("/snap/bin/docker"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// `docker …` with a deadline, so a stuck daemon can't stall the panel.
fn docker(bin: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    // Credential helpers and contexts (Docker Desktop, OrbStack…) live next to the CLI.
    if let (Some(dir), Some(path)) = (bin.parent(), std::env::var_os("PATH")) {
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        dirs.push(dir.to_path_buf());
        if let Ok(joined) = std::env::join_paths(dirs) {
            cmd.env("PATH", joined);
        }
    }
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(e) => return Err(e.to_string()),
            Ok(None) if start.elapsed() > Duration::from_secs(3) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Docker didn't answer within 3 s".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let read = |r: Option<&mut dyn std::io::Read>| {
        let mut s = String::new();
        if let Some(r) = r {
            let _ = r.read_to_string(&mut s);
        }
        s
    };
    let out = read(child.stdout.as_mut().map(|r| r as &mut dyn std::io::Read));
    if !status.success() {
        let err = read(child.stderr.as_mut().map(|r| r as &mut dyn std::io::Read));
        let err = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        return Err(
            if err.contains("Cannot connect to the Docker daemon")
                || err.contains("Is the docker daemon running")
                || err.contains("failed to connect to the docker API")
            {
                "Docker isn't running".into()
            } else {
                err
            },
        );
    }
    Ok(out)
}

pub fn enabled() -> bool {
    !matches!(
        std::env::var("DBX_DOCKER").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// The running Postgres and ClickHouse containers; `roots` are the folders that count as this repo.
pub fn running(roots: &[&Path]) -> Detection {
    if !enabled() {
        return Detection::default();
    }
    let Some(bin) = docker_bin() else {
        return Detection {
            found: Vec::new(),
            problem: Some("docker not found (set DBX_DOCKER_BIN to its path)".into()),
        };
    };
    let ids = match docker(&bin, &["ps", "-q"]) {
        Ok(ids) => ids,
        Err(e) => {
            return Detection {
                found: Vec::new(),
                problem: Some(e),
            };
        }
    };
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Detection::default();
    }
    let mut args = vec!["inspect"];
    args.extend(&ids);
    let list = match docker(&bin, &args).map(|t| serde_json::from_str::<Json>(&t)) {
        Ok(Ok(Json::Array(list))) => list,
        Ok(_) => {
            return Detection {
                found: Vec::new(),
                problem: Some("unexpected docker inspect output".into()),
            };
        }
        Err(e) => {
            return Detection {
                found: Vec::new(),
                problem: Some(e),
            };
        }
    };
    let mut found: Vec<Found> = list.iter().filter_map(|c| self::found(c, roots)).collect();
    found.sort_by(|a, b| b.here.cmp(&a.here).then(a.name.cmp(&b.name)));
    Detection {
        found,
        problem: None,
    }
}

/// Postgres or ClickHouse, from what only a server has: the port its image exposes (5432; 8123
/// for ClickHouse) or Postgres' `PGDATA`. Apps that connect to one carry `POSTGRES_*`/`CLICKHOUSE_*`
/// too, and some images are named after one (postgrest), so neither counts; a name only does when
/// the image exposes no ports at all.
fn kind_of(c: &Json, env: &[(&str, &str)]) -> Option<Kind> {
    let image = c["Config"]["Image"].as_str().unwrap_or("").to_lowercase();
    let ports = c["Config"]["ExposedPorts"].as_object();
    let exposes = |p: &str| ports.is_some_and(|m| m.contains_key(p));
    let none_exposed = ports.is_none_or(|m| m.is_empty());
    let named = |names: &[&str]| none_exposed && names.iter().any(|n| image.contains(n));
    if exposes("5432/tcp")
        || env.iter().any(|(k, _)| *k == "PGDATA")
        || named(&["postgres", "postgis", "timescale"])
    {
        Some(Kind::Postgres)
    } else if exposes("8123/tcp") || named(&["clickhouse"]) {
        Some(Kind::Clickhouse)
    } else {
        None
    }
}

fn found(c: &Json, roots: &[&Path]) -> Option<Found> {
    let image = c["Config"]["Image"].as_str()?;
    let env: Vec<(&str, &str)> = c["Config"]["Env"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str()?.split_once('='))
                .collect()
        })
        .unwrap_or_default();
    let kind = kind_of(c, &env)?;
    let var = |k: &str| {
        env.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
            .filter(|v| !v.is_empty())
    };
    let labels = &c["Config"]["Labels"];
    let label = |k: &str| labels[k].as_str().map(String::from);
    let (inner, user, password, database) = match kind {
        Kind::Postgres => {
            let user = var("POSTGRES_USER").unwrap_or_else(|| "postgres".into());
            let db = var("POSTGRES_DB").unwrap_or_else(|| user.clone());
            (5432, user, var("POSTGRES_PASSWORD"), db)
        }
        Kind::Clickhouse => {
            let user = var("CLICKHOUSE_USER").unwrap_or_else(|| "default".into());
            (
                9000,
                user,
                var("CLICKHOUSE_PASSWORD"),
                var("CLICKHOUSE_DB").unwrap_or_else(|| "default".into()),
            )
        }
    };
    let container = c["Name"].as_str()?.trim_start_matches('/').to_string();
    let workdir = label("com.docker.compose.project.working_dir");
    let here = workdir.as_deref().is_some_and(|w| {
        let w = Path::new(w)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(w).to_path_buf());
        roots.iter().any(|r| w.starts_with(r))
    });
    let service = label("com.docker.compose.service");
    let name = match (&service, here) {
        (Some(s), true) => s.clone(),
        _ => container.clone(),
    };
    let project = label("com.docker.compose.project");
    let mut note = match (&project, here) {
        (Some(p), true) => format!("docker · {image} · this repo's compose ({p})"),
        (Some(p), false) => format!("docker · {image} · compose project {p}"),
        (None, _) => format!("docker · {image}"),
    };
    let address = address(c, inner);
    let unreachable = address.is_none().then(|| format!("port {inner} isn't published, so it can't be reached from this machine: publish it (ports: [\"{inner}:{inner}\"])"));
    if address.is_none() {
        note.push_str(&format!(" · port {inner} not published"));
    }
    let (host, port) = address.unwrap_or_else(|| ("localhost".into(), inner));
    Some(Found {
        name,
        here,
        conn: Conn {
            kind: Some(kind),
            host: Some(Value::Plain(host)),
            port: Some(port),
            user: Some(Value::Plain(user)),
            password: password.map(Value::Plain),
            database: Some(Value::Plain(database)),
            stage: Some("local".into()),
            note: Some(note),
            unreachable,
            ..Conn::default()
        },
    })
}

/// Where the container's `inner` port can be reached: its published port, else (Linux only) its
/// address on the Docker network.
fn address(c: &Json, inner: u16) -> Option<(String, u16)> {
    let published = c["NetworkSettings"]["Ports"][format!("{inner}/tcp")]
        .as_array()
        .and_then(|b| {
            b.iter().find_map(|x| {
                let port = x["HostPort"].as_str()?.parse().ok()?;
                let ip = x["HostIp"].as_str().unwrap_or("");
                let host = if ip.is_empty() || ip == "0.0.0.0" || ip == "::" {
                    "localhost".to_string()
                } else {
                    ip.to_string()
                };
                Some((host, port))
            })
        });
    if published.is_some() || !cfg!(target_os = "linux") {
        return published;
    }
    c["NetworkSettings"]["Networks"]
        .as_object()?
        .values()
        .find_map(|n| {
            n["IPAddress"]
                .as_str()
                .filter(|ip| !ip.is_empty())
                .map(|ip| (ip.to_string(), inner))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn container(image: &str, env: &[&str], ports: Json, labels: Json) -> Json {
        let exposed: serde_json::Map<String, Json> = ports
            .as_object()
            .map(|m| m.keys().map(|k| (k.clone(), json!({}))).collect())
            .unwrap_or_default();
        json!({
            "Name": "/proj-db-1",
            "Config": { "Image": image, "Env": env, "Labels": labels, "ExposedPorts": exposed },
            "NetworkSettings": { "Ports": ports, "Networks": { "bridge": { "IPAddress": "172.17.0.5" } } },
        })
    }

    #[test]
    fn reads_postgres_from_compose() {
        let c = container(
            "postgres:16-alpine",
            &[
                "POSTGRES_USER=app",
                "POSTGRES_PASSWORD=pw",
                "POSTGRES_DB=shop",
                "PATH=/usr/bin",
            ],
            json!({ "5432/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "5433" }] }),
            json!({ "com.docker.compose.service": "db", "com.docker.compose.project": "proj",
                    "com.docker.compose.project.working_dir": "/src/proj" }),
        );
        let f = found(&c, &[Path::new("/src/proj")]).unwrap();
        assert_eq!((f.name.as_str(), f.here), ("db", true));
        assert_eq!(f.conn.host, Some(Value::Plain("localhost".into())));
        assert_eq!(f.conn.port, Some(5433));
        assert_eq!(f.conn.password, Some(Value::Plain("pw".into())));
        assert_eq!(f.conn.database, Some(Value::Plain("shop".into())));
        let elsewhere = found(&c, &[Path::new("/src/other")]).unwrap();
        assert_eq!(
            (elsewhere.name.as_str(), elsewhere.here),
            ("proj-db-1", false)
        );
    }

    #[test]
    fn defaults_and_other_images() {
        let c = container("clickhouse/clickhouse-server:24", &[], json!({}), json!({}));
        let f = found(&c, &[]).unwrap();
        assert_eq!(f.conn.kind, Some(Kind::Clickhouse));
        assert_eq!(f.conn.user, Some(Value::Plain("default".into())));
        if cfg!(target_os = "linux") {
            assert_eq!(
                (f.conn.host, f.conn.port),
                (Some(Value::Plain("172.17.0.5".into())), Some(9000))
            );
        }
        assert!(found(&container("redis:7", &[], json!({}), json!({})), &[]).is_none());
        let pg = found(
            &container("postgis/postgis:16", &[], json!({}), json!({})),
            &[],
        )
        .unwrap();
        assert_eq!(pg.conn.user, Some(Value::Plain("postgres".into())));
        assert_eq!(pg.conn.password, None);
    }

    /// The containers from a real `docker ps`: two compose projects, a plain one, Redpanda next to them.
    #[test]
    fn meteroid_setup() {
        let repo = "/src/meteroid-ent2";
        let compose = |project: &str, service: &str, dir: &str| {
            json!({
                "com.docker.compose.project": project, "com.docker.compose.service": service,
                "com.docker.compose.project.working_dir": dir,
            })
        };
        let published = |inner: u16, host: u16| json!({ format!("{inner}/tcp"): [{ "HostIp": "0.0.0.0", "HostPort": host.to_string() }] });
        let mut db = container(
            "ghcr.io/meteroid-oss/meteroid-postgres:18.3-standard",
            &["POSTGRES_PASSWORD=secret"],
            published(5432, 15432),
            compose("meteroid-ent2", "meteroid-db", repo),
        );
        db["Name"] = json!("/meteroid-ent2-meteroid-db-1");
        let f = found(&db, &[Path::new(repo)]).unwrap();
        assert_eq!(
            (f.name.as_str(), f.here, f.conn.port),
            ("meteroid-db", true, Some(15432))
        );
        let mut ch = container(
            "clickhouse/clickhouse-server:25.6.2-alpine",
            &[],
            json!({
                "8123/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "18123" }],
                "9000/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "19000" }],
                "9009/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "19009" }],
            }),
            compose("meteroid-ent2", "clickhouse", repo),
        );
        ch["Name"] = json!("/meteroid-ent2-clickhouse-1");
        assert_eq!(
            found(&ch, &[Path::new(repo)]).unwrap().conn.port,
            Some(19000)
        );
        let mut test_pg = container(
            "ghcr.io/meteroid-oss/meteroid-postgres:18.3-standard",
            &[],
            published(5432, 55432),
            json!({}),
        );
        test_pg["Name"] = json!("/meteroid-test-pg");
        assert_eq!(
            found(&test_pg, &[Path::new(repo)]).unwrap().name,
            "meteroid-test-pg"
        );
        let redpanda = container(
            "docker.redpanda.com/redpandadata/console:v2.3.1",
            &[],
            published(8080, 18085),
            json!({}),
        );
        assert!(found(&redpanda, &[]).is_none());
    }

    #[test]
    fn recognises_custom_images_by_port_or_env_and_flags_unpublished_ones() {
        let mut custom = container("acme/db:1", &[], json!({}), json!({}));
        custom["Config"]["ExposedPorts"] = json!({ "5432/tcp": {} });
        let f = found(&custom, &[]).unwrap();
        assert_eq!(f.conn.kind, Some(Kind::Postgres));
        assert_eq!(f.conn.unreachable.is_some(), !cfg!(target_os = "linux"));
        let mut app = container(
            "meteroid/api:latest",
            &[
                "POSTGRES_HOST=meteroid-db",
                "POSTGRES_PASSWORD=x",
                "CLICKHOUSE_URL=http://clickhouse:8123",
            ],
            json!({}),
            json!({}),
        );
        app["Config"]["ExposedPorts"] = json!({ "8080/tcp": {} });
        assert!(
            found(&app, &[]).is_none(),
            "an app that connects to a database isn't one"
        );
        let mut postgrest = container("postgrest/postgrest:v12", &[], json!({}), json!({}));
        postgrest["Config"]["ExposedPorts"] = json!({ "3000/tcp": {} });
        assert!(found(&postgrest, &[]).is_none());
        let mut ch = container("acme/analytics:2", &[], json!({}), json!({}));
        ch["Config"]["ExposedPorts"] = json!({ "8123/tcp": {}, "9000/tcp": {} });
        assert_eq!(found(&ch, &[]).unwrap().conn.kind, Some(Kind::Clickhouse));
    }
}
