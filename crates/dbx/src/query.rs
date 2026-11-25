//! `dbx query <name> "<sql>"`: SQL through the engine, so read-only rules, tunnels and the history
//! are the panel's own. Prints a table (or `--json` / `--csv`); no client program needed. Made for
//! agents as much as for you: what they run shows in the panel's history with who ran it.

use crate::engine::{history, server};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;

pub struct Args<'a> {
    pub name: &'a str,
    pub sql: &'a str,
    pub write: bool,
    pub confirm: Option<String>,
    pub format: Format,
    pub limit: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Csv,
}

/// Who is asking: an agent's environment says so (Claude Code, Codex, Gemini, Cursor, Aider).
pub fn who() -> String {
    let set = |k: &str| std::env::var_os(k).is_some();
    let agent = if set("CLAUDECODE") || set("CLAUDE_CODE_ENTRYPOINT") {
        Some("claude-code")
    } else if set("CODEX_SANDBOX") || set("CODEX_HOME") {
        Some("codex")
    } else if set("GEMINI_CLI") {
        Some("gemini")
    } else if set("CURSOR_AGENT") {
        Some("cursor")
    } else if set("AIDER_MODEL") {
        Some("aider")
    } else {
        None
    };
    agent.map_or("cli".into(), |a| format!("agent ({a})"))
}

pub fn saved_sql(cwd: &Path, name: &str) -> Result<String> {
    Ok(history::read_saved(cwd, name)?.sql)
}

pub fn request(
    info: &server::Info,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let url = format!("{}{path}", info.url);
    let auth = format!("Bearer {}", info.token);
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
    let v: Value = serde_json::from_str(&res.body_mut().read_to_string()?).unwrap_or(Value::Null);
    if !ok {
        bail!("{}", v["error"].as_str().unwrap_or("dbx serve refused"));
    }
    Ok(v)
}

/// Runs the statements and prints them; 1 when one fails.
pub fn run(cwd: &Path, a: Args) -> Result<i32> {
    let info = server::ensure()?;
    let cwd_text = cwd.to_string_lossy();
    let q = |p: &str| {
        format!(
            "{p}?cwd={}",
            url::form_urlencoded::byte_serialize(cwd_text.as_bytes()).collect::<String>()
        )
    };
    let session = request(
        &info,
        "POST",
        &q("/sessions"),
        Some(json!({ "cwd": cwd, "name": a.name, "write": a.write, "confirm": a.confirm })),
    )?;
    let id = session["id"].as_str().context("no session id")?.to_string();
    let out = request(
        &info,
        "POST",
        &q(&format!("/sessions/{id}/query")),
        Some(json!({ "sql": a.sql, "all": true, "limit": a.limit, "by": who() })),
    );
    let _ = request(&info, "DELETE", &q(&format!("/sessions/{id}")), None);
    let out = out?;
    print(&out, a.format);
    Ok(if out["error"].is_null() { 0 } else { 1 })
}

fn cell(v: &Value) -> String {
    v.as_str().map_or("null".into(), String::from)
}

fn print(out: &Value, format: Format) {
    let sets = out["results"].as_array().cloned().unwrap_or_default();
    match format {
        Format::Json => {
            let sets: Vec<Value> = sets
                .iter()
                .map(|s| {
                    let cols: Vec<String> = s["columns"].as_array().into_iter().flatten()
                        .map(|c| c["name"].as_str().unwrap_or("").to_string()).collect();
                    let rows: Vec<Value> = s["rows"].as_array().into_iter().flatten()
                        .map(|r| Value::Object(cols.iter().cloned().zip(r.as_array().cloned().unwrap_or_default()).collect()))
                        .collect();
                    json!({ "columns": cols, "rows": rows, "more": s["more"], "affected": s["affected"] })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "results": sets, "error": out["error"] }))
                    .unwrap_or_default()
            );
        }
        Format::Csv | Format::Table => {
            for s in &sets {
                let cols: Vec<String> = s["columns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| c["name"].as_str().unwrap_or("").to_string())
                    .collect();
                let rows: Vec<Vec<String>> = s["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| r.as_array().into_iter().flatten().map(cell).collect())
                    .collect();
                if cols.is_empty() {
                    println!("{} rows affected", s["affected"].as_u64().unwrap_or(0));
                } else if format == Format::Csv {
                    let esc = |v: &str| {
                        if v.contains([',', '"', '\n']) {
                            format!("\"{}\"", v.replace('"', "\"\""))
                        } else {
                            v.to_string()
                        }
                    };
                    println!(
                        "{}",
                        cols.iter().map(|c| esc(c)).collect::<Vec<_>>().join(",")
                    );
                    for r in &rows {
                        println!("{}", r.iter().map(|c| esc(c)).collect::<Vec<_>>().join(","));
                    }
                } else {
                    table(&cols, &rows, s["more"].as_bool().unwrap_or(false));
                }
            }
            if let Some(e) = out["error"].as_object() {
                eprintln!(
                    "error: {}",
                    e.get("message").and_then(Value::as_str).unwrap_or("failed")
                );
                for k in ["detail", "hint"] {
                    if let Some(v) = e.get(k).and_then(Value::as_str) {
                        eprintln!("{k}: {v}");
                    }
                }
            }
        }
    }
}

fn table(cols: &[String], rows: &[Vec<String>], more: bool) {
    const MAX: usize = 60;
    let show = |v: &str| {
        let one: String = v.replace('\n', "⏎");
        if one.chars().count() > MAX {
            format!("{}…", one.chars().take(MAX - 1).collect::<String>())
        } else {
            one
        }
    };
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| show(c)).collect())
        .collect();
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([c.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join(" │ ")
            .trim_end()
            .to_string()
    };
    println!("{}", line(cols));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("─┼─")
    );
    for r in &rows {
        println!("{}", line(r));
    }
    println!(
        "({} row{}{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        if more { ", more: raise --limit" } else { "" }
    );
}
