//! Databases running in Docker, found from `docker inspect`: usable without configuring anything.
//! Credentials come from the container's environment each time; nothing is written anywhere.

use crate::config::{Conn, Kind, Value};
use serde_json::Value as Json;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    pub conn: Conn,
    /// Started by this repo's compose file (or one of its worktrees').
    pub here: bool,
}

/// `docker …` with a deadline, so a stuck daemon can't stall the panel; None when docker isn't there.
fn docker(args: &[&str]) -> Option<String> {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if start.elapsed() > Duration::from_secs(3) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    Some(out)
}

pub fn enabled() -> bool {
    !matches!(
        std::env::var("DBX_DOCKER").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// The running Postgres and ClickHouse containers; `roots` are the folders that count as this repo.
pub fn running(roots: &[&Path]) -> Vec<Found> {
    if !enabled() {
        return Vec::new();
    }
    let Some(ids) = docker(&["ps", "-q"]) else {
        return Vec::new();
    };
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["inspect"];
    args.extend(&ids);
    let Some(text) = docker(&args) else {
        return Vec::new();
    };
    let Ok(Json::Array(list)) = serde_json::from_str::<Json>(&text) else {
        return Vec::new();
    };
    let mut out: Vec<Found> = list.iter().filter_map(|c| found(c, roots)).collect();
    out.sort_by(|a, b| b.here.cmp(&a.here).then(a.name.cmp(&b.name)));
    out
}

fn kind_of(image: &str) -> Option<Kind> {
    let image = image.rsplit('/').next().unwrap_or(image).to_lowercase();
    if [
        "postgres",
        "postgis",
        "timescaledb",
        "pgvector",
        "supabase/postgres",
        "pgduckdb",
    ]
    .iter()
    .any(|k| image.contains(k))
    {
        Some(Kind::Postgres)
    } else if image.contains("clickhouse") {
        Some(Kind::Clickhouse)
    } else {
        None
    }
}

fn found(c: &Json, roots: &[&Path]) -> Option<Found> {
    let image = c["Config"]["Image"].as_str()?;
    let kind = kind_of(image)?;
    let env: Vec<(&str, &str)> = c["Config"]["Env"]
        .as_array()?
        .iter()
        .filter_map(|e| e.as_str()?.split_once('='))
        .collect();
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
    let (host, port) = address(c, inner)?;
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
    let note = match (&project, here) {
        (Some(p), true) => format!("docker · {image} · this repo's compose ({p})"),
        (Some(p), false) => format!("docker · {image} · compose project {p}"),
        (None, _) => format!("docker · {image}"),
    };
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
        json!({
            "Name": "/proj-db-1",
            "Config": { "Image": image, "Env": env, "Labels": labels },
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
}
