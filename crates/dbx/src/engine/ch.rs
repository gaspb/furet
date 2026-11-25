//! One ClickHouse connection held by the engine for a panel, over ClickHouse's HTTP interface: a
//! server-side session (so `USE` and `SET` last), one request per statement, `readonly=2` while
//! read-only.

use super::pg::{BUDGET, CELL_MAX, Column, Output, ResultSet, SqlError, WRAP_PREFIX, truncate};
use super::sql::{self, Dialect};
use crate::conn::Resolved;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FORMAT: &str = "TabSeparatedWithNamesAndTypes";
const SESSION_TIMEOUT: &str = "1800";
const SYNTAX_ERROR: i64 = 62;
const READONLY: i64 = 164;
const INVALID_SESSION_TIMEOUT: i64 = 374;
const NAMED_IN_MESSAGE: [i64; 5] = [16, 46, 47, 60, 81];
const HIDDEN: &str = "('system', 'INFORMATION_SCHEMA', 'information_schema')";

pub struct Ch {
    agent: ureq::Agent,
    base: String,
    user: String,
    password: Option<String>,
    database: Option<String>,
    session: String,
    session_timeout: bool,
    /// The user's own settings make it read-only: nothing lifts that, and `readonly` can't be sent.
    pub server_readonly: bool,
    readonly: AtomicBool,
    /// ClickHouse takes one request at a time per session.
    lock: Mutex<()>,
    running: Mutex<Option<String>>,
    cancelled: Mutex<Option<String>>,
    pub server_version: String,
}

#[derive(Debug, Default, PartialEq)]
struct Failure {
    code: Option<i64>,
    message: String,
    detail: Option<String>,
    /// 1-based byte offset in the text sent.
    position: Option<usize>,
    /// What the message names (an identifier, a table…), to point at it.
    named: Option<String>,
}

type Records = Vec<HashMap<String, String>>;

impl Ch {
    pub fn connect(r: &Resolved) -> Result<Ch> {
        let secure = r
            .params
            .iter()
            .any(|(k, v)| k == "secure" && (v == "1" || v == "true"));
        let host = if r.host.contains(':') {
            format!("[{}]", r.host)
        } else {
            r.host.clone()
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // Like the Postgres connections: straight to the server or its tunnel, never a web proxy.
            .proxy(None)
            .timeout_global(None)
            .timeout_connect(Some(Duration::from_secs(10)))
            .build()
            .into();
        let mut ch = Ch {
            agent,
            base: format!(
                "{}://{host}:{}/",
                if secure { "https" } else { "http" },
                r.http_port
            ),
            user: r.user.clone().unwrap_or_else(|| "default".into()),
            password: r.password.clone(),
            database: r.database.clone(),
            session: super::server::random_hex(16),
            session_timeout: true,
            server_readonly: false,
            // Asked without `readonly`, to learn what the server itself imposes.
            readonly: AtomicBool::new(false),
            lock: Mutex::new(()),
            running: Mutex::new(None),
            cancelled: Mutex::new(None),
            server_version: String::new(),
        };
        let where_ = format!("{}:{} (ClickHouse's HTTP port)", r.host, r.http_port);
        let first = match ch.records("select version() as v, getSetting('readonly') as ro", &[]) {
            Err(f) if f.code.is_none() => {
                return Err(anyhow!(f.message)).with_context(|| format!("cannot reach {where_}"));
            }
            other => other.map_err(|f| anyhow!(f.message))?,
        };
        let first = first.into_iter().next().unwrap_or_default();
        ch.server_version = field(&first, "v");
        ch.server_readonly = first.get("ro").is_some_and(|v| v != "0");
        ch.readonly.store(r.readonly, Ordering::SeqCst);
        let use_db = ch
            .database
            .as_deref()
            .map(|d| format!("use {}", quote_ident(d)));
        let setup = use_db.as_deref().unwrap_or("select 1");
        match ch.exec(setup, true, &[], None) {
            Err(f) if f.code == Some(INVALID_SESSION_TIMEOUT) => {
                ch.session_timeout = false;
                ch.exec(setup, true, &[], None)
                    .map_err(|f| anyhow!(f.message))?;
            }
            other => {
                other.map_err(|f| anyhow!(f.message))?;
            }
        }
        Ok(ch)
    }

    pub fn set_readonly(&self, on: bool) -> Result<()> {
        if !on && self.server_readonly {
            bail!("{} is read-only on the server itself", self.user);
        }
        self.readonly.store(on, Ordering::SeqCst);
        Ok(())
    }

    /// Kills the statement running in the session, if any.
    pub fn cancel(&self) -> Result<()> {
        let id = self.running.lock().unwrap().clone();
        if let Some(id) = id {
            *self.cancelled.lock().unwrap() = Some(id.clone());
            self.kill(&id).map_err(|f| anyhow!(f.message))?;
        }
        Ok(())
    }

    fn kill(&self, id: &str) -> Result<ResultSet, Failure> {
        self.exec(
            "kill query where query_id = {id:String} async",
            false,
            &[("id", id)],
            None,
        )
    }

    /// Runs the statements in `sql[range]` in order, stopping at the first error. A lone
    /// read-only statement runs as a page of `limit` rows.
    pub fn run(&self, sql: &str, range: (usize, usize), limit: usize, offset: usize) -> Output {
        let start = Instant::now();
        let mut out = Output {
            results: Vec::new(),
            error: None,
            elapsed_ms: 0,
            ran: (
                sql::byte_to_utf16(sql, range.0),
                sql::byte_to_utf16(sql, range.1),
            ),
            paged: false,
        };
        let stmts: Vec<(usize, usize)> =
            sql::statements_in(&sql[range.0..range.1], Dialect::Clickhouse)
                .into_iter()
                .map(|(s, e)| (range.0 + s, range.0 + e))
                .collect();
        let mut todo = &stmts[..];
        if let [(s, e)] = stmts[..]
            && sql::pageable(&sql[s..e])
        {
            let wrapped = format!(
                "{WRAP_PREFIX}{}\n) limit {} offset {offset}",
                &sql[s..e],
                limit + 1
            );
            match self.exec(&wrapped, true, &[], Some(limit)) {
                Ok(set) => {
                    out.results.push(set);
                    out.paged = true;
                    todo = &[];
                }
                // Not a query that can be wrapped (a FORMAT clause…): run it as typed.
                Err(f) if f.code == Some(SYNTAX_ERROR) => {}
                Err(f) => {
                    out.error = Some(self.sql_error(f, sql, (s, e), WRAP_PREFIX.len()));
                    todo = &[];
                }
            }
        }
        for &(s, e) in todo {
            match self.exec(&sql[s..e], true, &[], Some(limit)) {
                Ok(set) => out.results.push(set),
                Err(f) => {
                    out.error = Some(self.sql_error(f, sql, (s, e), 0));
                    break;
                }
            }
        }
        out.elapsed_ms = start.elapsed().as_millis() as u64;
        out
    }

    /// One statement. In the session it can be cancelled; `limit` caps the rows kept (and cells'
    /// length), `None` keeps everything (the engine's own queries).
    fn exec(
        &self,
        stmt: &str,
        in_session: bool,
        params: &[(&str, &str)],
        limit: Option<usize>,
    ) -> Result<ResultSet, Failure> {
        let id = super::server::random_hex(16);
        let mut query: Vec<(String, String)> = vec![
            ("query_id".into(), id.clone()),
            ("default_format".into(), FORMAT.into()),
            // Held back until this much is ready: an error in the first rows still gets a status.
            ("buffer_size".into(), (8 << 20).to_string()),
        ];
        let _turn = if in_session {
            let turn = self.lock.lock().unwrap();
            query.push(("session_id".into(), self.session.clone()));
            if self.session_timeout {
                query.push(("session_timeout".into(), SESSION_TIMEOUT.into()));
            }
            *self.running.lock().unwrap() = Some(id.clone());
            Some(turn)
        } else {
            if let Some(db) = &self.database {
                query.push(("database".into(), db.clone()));
            }
            None
        };
        if self.readonly.load(Ordering::SeqCst) && !self.server_readonly {
            query.push(("readonly".into(), "2".into()));
        }
        for (k, v) in params {
            query.push((format!("param_{k}"), v.to_string()));
        }
        let mut out = self.send(&query, stmt, limit, &id);
        if in_session {
            *self.running.lock().unwrap() = None;
            // The server may drop the connection rather than say so.
            if self.cancelled.lock().unwrap().take().as_ref() == Some(&id)
                && let Err(f) = &mut out
            {
                f.message = "Query was cancelled.".into();
            }
        }
        out
    }

    fn send(
        &self,
        query: &[(String, String)],
        stmt: &str,
        limit: Option<usize>,
        id: &str,
    ) -> Result<ResultSet, Failure> {
        let mut req = self
            .agent
            .post(&self.base)
            .query_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .header("X-ClickHouse-User", &self.user);
        if let Some(pw) = &self.password {
            req = req.header("X-ClickHouse-Key", pw);
        }
        let resp = req.send(stmt).map_err(|e| Failure {
            message: e.to_string(),
            ..Failure::default()
        })?;
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        };
        let format = header("X-ClickHouse-Format");
        let tag = header("X-ClickHouse-Exception-Tag");
        let written = header("X-ClickHouse-Summary")
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["written_rows"].as_str()?.parse().ok());
        let status = resp.status().as_u16();
        let mut body = resp.into_body();
        if status != 200 {
            let text = body
                .with_config()
                .limit(1 << 20)
                .lossy_utf8(true)
                .read_to_string()
                .unwrap_or_default();
            return Err(parse_failure(&text));
        }
        let mut reader = BufReader::new(body.into_reader());
        let (set, finished) = read_result(&mut reader, format.as_deref(), tag.as_deref(), limit)?;
        if !finished && !drained(&mut reader) {
            drop(reader);
            let _ = self.kill(id);
        }
        Ok(set.unwrap_or(ResultSet {
            affected: Some(written.unwrap_or(0)),
            ..ResultSet::default()
        }))
    }

    fn sql_error(&self, f: Failure, sql: &str, (s, e): (usize, usize), shift: usize) -> SqlError {
        let stmt = &sql[s..e];
        let byte = f
            .position
            .map(|p| p.saturating_sub(1 + shift).min(stmt.len()))
            .or_else(|| f.named.as_deref().and_then(|n| locate(stmt, n)));
        let position = byte.map(|b| {
            let mut b = s + b;
            while !sql.is_char_boundary(b) {
                b -= 1;
            }
            sql::byte_to_utf16(sql, b)
        });
        let hint = (f.code == Some(READONLY) && self.readonly.load(Ordering::SeqCst)).then(|| {
            "this connection is read-only: switch it to read-write to change data".to_string()
        });
        SqlError {
            message: f.message,
            detail: f.detail,
            hint,
            code: f.code.map(|c| c.to_string()),
            position,
            closed: false,
        }
    }

    /// The engine's own queries, outside the session: rows as column → value, nulls left out.
    fn records(&self, sql: &str, params: &[(&str, &str)]) -> Result<Records, Failure> {
        let set = self.exec(sql, false, params, None)?;
        Ok(set
            .rows
            .into_iter()
            .map(|row| {
                set.columns
                    .iter()
                    .zip(row)
                    .filter_map(|(c, v)| Some((c.name.clone(), v?)))
                    .collect()
            })
            .collect())
    }

    pub fn catalog(&self) -> Result<Value> {
        let visible = |col: &str| format!("({col} not in {HIDDEN} or {col} = currentDatabase())");
        let q = |sql: String| self.records(&sql, &[]).map_err(|f| anyhow!(f.message));
        let schemas: Vec<String> = q(format!(
            "select name from system.databases where {} order by name = currentDatabase() desc, name",
            visible("name")
        ))?
        .into_iter()
        .filter_map(|mut r| r.remove("name"))
        .collect();
        let mut columns: HashMap<(String, String), Vec<Value>> = HashMap::new();
        for r in q(format!(
            "select database, table, name, type, is_in_primary_key from system.columns
             where {} order by database, table, position limit 200000",
            visible("database")
        ))? {
            columns
                .entry((field(&r, "database"), field(&r, "table")))
                .or_default()
                .push(json!({
                    "name": field(&r, "name"), "type": field(&r, "type"),
                    "pk": r.get("is_in_primary_key").is_some_and(|v| v == "1"),
                }));
        }
        let tables: Vec<Value> = q(format!(
            "select database, name, engine, total_rows, comment from system.tables
             where {} and not is_temporary and not startsWith(name, '.inner')
             order by database, name limit 5000",
            visible("database")
        ))?
        .into_iter()
        .map(|r| {
            let key = (field(&r, "database"), field(&r, "name"));
            json!({
                "schema": key.0, "name": key.1, "kind": kind_of(&field(&r, "engine")),
                "rows": r.get("total_rows").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0),
                "comment": r.get("comment").filter(|c| !c.is_empty()),
                "columns": columns.remove(&key).unwrap_or_default(),
            })
        })
        .collect();
        Ok(json!({ "schemas": schemas, "tables": tables }))
    }

    pub fn table(&self, db: &str, name: &str) -> Result<Value> {
        let at = [("db", db), ("name", name)];
        let q = |sql: &str| self.records(sql, &at).map_err(|f| anyhow!(f.message));
        let Some(t) =
            q("select * from system.tables where database = {db:String} and name = {name:String}")?
                .into_iter()
                .next()
        else {
            bail!("no table {db}.{name}");
        };
        let number = |k: &str| t.get(k).and_then(|v| v.parse::<u64>().ok());
        let columns: Vec<Value> = q(
            "select name, type, default_kind, default_expression, comment, compression_codec
             from system.columns where database = {db:String} and table = {name:String} order by position",
        )?
        .into_iter()
        .map(|c| {
            let default = c.get("default_expression").filter(|v| !v.is_empty());
            json!({
                "name": field(&c, "name"), "type": field(&c, "type"),
                "default": default,
                "default_kind": default.and(c.get("default_kind")).map(|k| k.to_lowercase()),
                "comment": c.get("comment").filter(|v| !v.is_empty()),
                "codec": c.get("compression_codec").filter(|v| !v.is_empty()),
            })
        })
        .collect();
        let indexes: Vec<Value> = q(
            "select * from system.data_skipping_indices where database = {db:String} and table = {name:String}",
        )?
        .into_iter()
        .map(|x| {
            let ty = x.get("type_full").or(x.get("type")).cloned().unwrap_or_default();
            json!({
                "name": field(&x, "name"),
                "definition": format!("{} TYPE {ty} GRANULARITY {}", field(&x, "expr"), field(&x, "granularity")),
                "primary": false, "unique": false,
            })
        })
        .collect();
        let keys: Vec<Value> = [
            ("partition key", "partition_key"),
            ("sorting key", "sorting_key"),
            ("primary key", "primary_key"),
            ("sampling key", "sampling_key"),
        ]
        .into_iter()
        .filter_map(|(kind, k)| {
            let v = t.get(k).filter(|v| !v.is_empty())?;
            Some(json!({ "kind": kind, "definition": v }))
        })
        .collect();
        let engine = t
            .get("engine_full")
            .filter(|v| !v.is_empty())
            .or(t.get("engine"))
            .cloned();
        Ok(json!({
            "dialect": "clickhouse",
            "schema": db, "name": name, "kind": kind_of(&field(&t, "engine")), "engine": engine,
            "rows": number("total_rows"), "bytes": number("total_bytes"),
            "comment": t.get("comment").filter(|v| !v.is_empty()),
            "create": t.get("create_table_query"),
            "keys": keys, "columns": columns, "indexes": indexes, "constraints": [],
            "primary_key": [], "view_definition": null,
        }))
    }
}

fn field(r: &HashMap<String, String>, k: &str) -> String {
    r.get(k).cloned().unwrap_or_default()
}

fn kind_of(engine: &str) -> &'static str {
    match engine {
        "View" | "LiveView" | "WindowView" => "view",
        "MaterializedView" => "materialized view",
        "Dictionary" => "dictionary",
        _ => "table",
    }
}

pub fn quote_ident(s: &str) -> String {
    format!("`{}`", s.replace('\\', "\\\\").replace('`', "\\`"))
}

/// Reads what's left of a result that was cut short, if it's little: then nothing needs killing.
fn drained(reader: &mut impl Read) -> bool {
    let mut rest = reader.take(1 << 20);
    std::io::copy(&mut rest, &mut std::io::sink()).is_ok_and(|n| n < 1 << 20)
}

/// A response body: `None` for a statement that returns nothing; true when it was read to the end.
fn read_result(
    reader: &mut impl BufRead,
    format: Option<&str>,
    tag: Option<&str>,
    limit: Option<usize>,
) -> Result<(Option<ResultSet>, bool), Failure> {
    let tsv = format == Some(FORMAT);
    let mut set: Option<ResultSet> = None;
    let mut size = 0;
    let mut blank = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).map_err(|e| Failure {
            message: format!("the response was cut off: {e}"),
            ..Failure::default()
        })?;
        if n == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if (tag.is_some() && line == b"__exception__")
            || (tag.is_none() && line.starts_with(b"Code: ") && contains(&line, b"DB::Exception"))
        {
            let mut rest = String::from_utf8_lossy(&line).into_owned() + "\n";
            let _ = reader.read_to_string(&mut rest);
            return Err(parse_failure(&exception_text(&rest, tag)));
        }
        let Some(s) = set.as_mut() else {
            let mut first = ResultSet::default();
            if tsv {
                let mut types = Vec::new();
                let _ = reader.read_until(b'\n', &mut types);
                if types.last() == Some(&b'\n') {
                    types.pop();
                }
                let mut types = split(&types);
                first.columns = split(&line)
                    .map(|v| Column {
                        name: unescape(v).unwrap_or_default(),
                        ty: types.next().and_then(unescape),
                        refs: None,
                    })
                    .collect();
            } else {
                first.columns = vec![Column {
                    name: format.unwrap_or("result").to_string(),
                    ty: None,
                    refs: None,
                }];
                first
                    .rows
                    .push(vec![Some(String::from_utf8_lossy(&line).into_owned())]);
            }
            set = Some(first);
            continue;
        };
        // An empty line may only be what precedes an exception.
        if line.is_empty() && !blank {
            blank = true;
            continue;
        }
        let mut rows = Vec::new();
        if std::mem::take(&mut blank) {
            rows.push(Vec::new());
        }
        rows.push(line.clone());
        for raw in rows {
            if let Some(limit) = limit
                && (s.rows.len() >= limit || size > BUDGET)
            {
                s.more = true;
                return Ok((set, false));
            }
            let cells: Vec<Option<String>> = if tsv {
                split(&raw)
                    .zip(&s.columns)
                    .map(|(v, c)| match c.ty.as_deref() {
                        Some(ty) if composite(ty) => Some(String::from_utf8_lossy(v).into_owned()),
                        _ => unescape(v),
                    })
                    .collect()
            } else {
                vec![Some(String::from_utf8_lossy(&raw).into_owned())]
            };
            let cells = match limit {
                Some(_) => cells
                    .into_iter()
                    .map(|c| {
                        c.map(|v| {
                            size += v.len().min(CELL_MAX) + 8;
                            truncate(&v)
                        })
                    })
                    .collect(),
                None => cells,
            };
            s.rows.push(cells);
        }
    }
    if blank && let Some(s) = set.as_mut() {
        s.rows.push(vec![Some(String::new()); s.columns.len()]);
    }
    Ok((set, true))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn split(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(|&b| b == b'\t')
}

/// Arrays, tuples and maps are written as literals, which TabSeparated doesn't escape again.
fn composite(ty: &str) -> bool {
    let mut base = ty;
    while let Some(inner) = ["Nullable(", "LowCardinality("]
        .iter()
        .find_map(|w| base.strip_prefix(w))
    {
        base = inner;
    }
    ["Array(", "Tuple(", "Map(", "Nested("]
        .iter()
        .any(|p| base.starts_with(p))
        || [
            "Point",
            "Ring",
            "LineString",
            "MultiLineString",
            "Polygon",
            "MultiPolygon",
        ]
        .contains(&base)
}

/// A TabSeparated field: `\N` is null, backslash escapes undone.
fn unescape(v: &[u8]) -> Option<String> {
    if v == b"\\N" {
        return None;
    }
    let mut out = Vec::with_capacity(v.len());
    let mut i = 0;
    while i < v.len() {
        if v[i] == b'\\' && i + 1 < v.len() {
            out.push(match v[i + 1] {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'0' => 0,
                b'b' => 8,
                b'f' => 12,
                b'a' => 7,
                b'v' => 11,
                c => c,
            });
            i += 2;
        } else {
            out.push(v[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// The message inside an exception block written after a result had started:
/// `__exception__\n<tag>\n<message>\n<size> <tag>\n__exception__`.
fn exception_text(rest: &str, tag: Option<&str>) -> String {
    let Some(tag) = tag else {
        return rest.to_string();
    };
    let body = rest
        .split_once('\n')
        .map_or(rest, |(_, b)| b)
        .trim_start_matches(tag)
        .trim_start_matches(['\r', '\n']);
    let end = body.rfind(&format!(" {tag}")).unwrap_or(body.len());
    let body = &body[..end];
    body[..body.rfind('\n').unwrap_or(body.len())].to_string()
}

/// `Code: 62. DB::Exception: Syntax error: failed at position 8 (frm) (line 1, col 8): …
/// Expected one of: …. (SYNTAX_ERROR) (version 24.8.1)` → message, detail, position.
fn parse_failure(text: &str) -> Failure {
    let text = text.trim();
    let (code, rest) = match text
        .strip_prefix("Code: ")
        .and_then(|r| r.split_once(". "))
        .and_then(|(c, r)| Some((c.parse::<i64>().ok()?, r)))
    {
        Some((c, r)) => (Some(c), r),
        None => (None, text),
    };
    let mut msg = rest
        .strip_prefix("DB::Exception: ")
        .unwrap_or(rest)
        .to_string();
    if let Some(i) = msg.rfind(" (version ") {
        msg.truncate(i);
    }
    if let Some(i) = msg.rfind(" (")
        && msg[i + 2..].strip_suffix(')').is_some_and(|n| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        })
    {
        msg.truncate(i);
    }
    if let Some(i) = msg.find(" In scope ") {
        msg.truncate(i);
    }
    let mut f = Failure {
        code,
        ..Failure::default()
    };
    if let Some(i) = msg.find("failed at position ") {
        let after = &msg[i + "failed at position ".len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        f.position = digits.parse().ok();
        let token = after[digits.len()..]
            .trim_start()
            .strip_prefix('(')
            .and_then(|t| t.split_once(')'))
            .map(|(t, _)| t);
        let expected = msg.find("Expected one of:").map(|j| msg[j..].to_string());
        let hint = msg.find("Maybe you meant: ").map(|j| {
            msg[j..]
                .split_once(". ")
                .map_or(&msg[j..], |(h, _)| h)
                .to_string()
                + "."
        });
        f.detail = [hint, expected]
            .into_iter()
            .flatten()
            .reduce(|a, b| format!("{a} {b}"));
        msg = format!(
            "{}{}",
            msg[..i].trim_end_matches([' ', ':']),
            token.map_or(String::new(), |t| format!(" at {t}"))
        );
    }
    if code.is_some_and(|c| NAMED_IN_MESSAGE.contains(&c)) {
        f.named = ['`', '\''].iter().find_map(|&q| {
            let (_, after) = msg.split_once(q)?;
            Some(after.split_once(q)?.0.to_string())
        });
    }
    f.message = msg.trim().to_string();
    f
}

/// Where `name` (or its last `.` part) appears as a whole word in `stmt`.
fn locate(stmt: &str, name: &str) -> Option<usize> {
    let word = |s: &str, n: &str| {
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        s.match_indices(n).map(|(i, _)| i).find(|&i| {
            !s[..i].chars().next_back().is_some_and(ident)
                && !s[i + n.len()..].chars().next().is_some_and(ident)
        })
    };
    word(stmt, name).or_else(|| {
        let (_, last) = name.rsplit_once('.')?;
        word(stmt, last)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_errors() {
        let f = parse_failure(
            "Code: 62. DB::Exception: Syntax error: failed at position 30 (system) (line 2, col 14): system.one ) limit 1. Maybe you meant: FROM. Expected one of: token, Comma, FROM. (SYNTAX_ERROR) (version 26.10.4.207 (official build))\n",
        );
        assert_eq!(f.code, Some(62));
        assert_eq!(f.position, Some(30));
        assert_eq!(f.message, "Syntax error at system");
        assert_eq!(
            f.detail.as_deref(),
            Some("Maybe you meant: FROM. Expected one of: token, Comma, FROM.")
        );
        let f = parse_failure(
            "Code: 47. DB::Exception: Unknown expression identifier `nope`. In scope SELECT nope FROM system.one. (UNKNOWN_IDENTIFIER) (version 26.10.4.207 (official build))",
        );
        assert_eq!(f.message, "Unknown expression identifier `nope`.");
        assert_eq!(f.named.as_deref(), Some("nope"));
        let f = parse_failure(
            "Code: 164. DB::Exception: default: Cannot execute query in readonly mode. (READONLY) (version 26.10.4.207 (official build))",
        );
        assert_eq!(
            (f.code, f.message.as_str()),
            (Some(164), "default: Cannot execute query in readonly mode.")
        );
        assert_eq!(
            parse_failure("Connection refused").message,
            "Connection refused"
        );
    }

    #[test]
    fn points_at_what_the_message_names() {
        let stmt = "select a.id, nope\nfrom t a";
        assert_eq!(locate(stmt, "nope"), Some(13));
        assert_eq!(locate(stmt, "a.nope"), Some(13));
        assert_eq!(locate("select nopes, nope", "nope"), Some(14));
        assert_eq!(locate(stmt, "zzz"), None);
    }

    #[test]
    fn reads_tab_separated_results() {
        let body = "a\tb\tc\nUInt64\tArray(String)\tNullable(String)\n1\t['x','y\\'z\\t']\t\\N\n2\t[]\tline\\nbreak\\ttab\\\\\n";
        let (set, done) = read_result(&mut body.as_bytes(), Some(FORMAT), None, Some(10)).unwrap();
        let set = set.unwrap();
        assert!(done && !set.more);
        assert_eq!(
            set.columns
                .iter()
                .map(|c| (c.name.as_str(), c.ty.as_deref().unwrap()))
                .collect::<Vec<_>>(),
            vec![
                ("a", "UInt64"),
                ("b", "Array(String)"),
                ("c", "Nullable(String)")
            ]
        );
        assert_eq!(
            set.rows,
            vec![
                vec![Some("1".into()), Some("['x','y\\'z\\t']".into()), None],
                vec![
                    Some("2".into()),
                    Some("[]".into()),
                    Some("line\nbreak\ttab\\".into())
                ],
            ]
        );
        let (set, done) = read_result(&mut body.as_bytes(), Some(FORMAT), None, Some(1)).unwrap();
        assert!(!done && set.unwrap().more);
        let (set, _) = read_result(&mut "".as_bytes(), None, None, Some(1)).unwrap();
        assert!(set.is_none());
        let (set, _) =
            read_result(&mut "x\nString\n\n\n".as_bytes(), Some(FORMAT), None, None).unwrap();
        assert_eq!(set.unwrap().rows, vec![vec![Some(String::new())]; 2]);
        let (set, _) = read_result(
            &mut "{\n  \"a\": 1\n}\n".as_bytes(),
            Some("JSON"),
            None,
            None,
        )
        .unwrap();
        let set = set.unwrap();
        assert_eq!((set.columns[0].name.as_str(), set.rows.len()), ("JSON", 3));
    }

    #[test]
    fn reads_an_exception_after_rows() {
        let body = "n\tx\nUInt64\tUInt8\n1\t0\n\n__exception__\ntagxyz\nCode: 395. DB::Exception: Value passed to 'throwIf' function is non-zero. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 26.10.4.207 (official build))\n120 tagxyz\n__exception__\n";
        let f =
            read_result(&mut body.as_bytes(), Some(FORMAT), Some("tagxyz"), Some(10)).unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str()),
            (Some(395), "Value passed to 'throwIf' function is non-zero.")
        );
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_ident("we`ird"), "`we\\`ird`");
    }
}
