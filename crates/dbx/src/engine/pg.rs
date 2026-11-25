//! One Postgres connection held by the engine for a panel: queries, paging, catalog, cancel.

use super::sql;
use crate::conn::Resolved;
use anyhow::{Context, Result};
use postgres::{CancelToken, Client, Config, SimpleQueryMessage};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres_rustls::MakeRustlsConnect;

pub(super) const CELL_MAX: usize = 4000;
pub(super) const BUDGET: usize = 6 * 1024 * 1024;
const READONLY_OPTION: &str = "-c default_transaction_read_only=on";
pub(super) const WRAP_PREFIX: &str = "select * from (\n";

pub struct Pg {
    client: Client,
    cancel: CancelToken,
    tls: MakeRustlsConnect,
    pub server_version: String,
    /// Single-column foreign keys by (table oid, column number), read once per catalog.
    fks: Option<HashMap<(u32, i16), Ref>>,
    /// Table columns by (table oid, column number), as results meet them.
    origins: HashMap<(u32, i16), Ref>,
}

#[derive(Serialize, Debug)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    /// The row this column's value points at, when it comes from a foreign key column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs: Option<Ref>,
    /// The table column it is a copy of, when it is one (not an expression).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<Ref>,
}

/// A row as text, as Postgres sends it (null as None).
pub type TextRow = Vec<Option<String>>;

/// A referenced column: `select * from schema.table where column = <value>` finds the row.
#[derive(Serialize, Debug, Clone)]
pub struct Ref {
    pub schema: String,
    pub table: String,
    pub column: String,
}

#[derive(Serialize, Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows past the limit exist.
    pub more: bool,
    /// Rows changed, for a statement that returns none.
    pub affected: Option<u64>,
}

#[derive(Serialize, Debug)]
pub struct SqlError {
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub code: Option<String>,
    /// UTF-16 offset in the request's SQL, for the editor.
    pub position: Option<usize>,
    /// The connection is gone; open a new session.
    pub closed: bool,
}

#[derive(Serialize, Debug)]
pub struct Output {
    pub results: Vec<ResultSet>,
    pub error: Option<SqlError>,
    pub elapsed_ms: u64,
    /// UTF-16 range of what ran.
    pub ran: (usize, usize),
    /// Ran as a page (`limit`/`offset` applied by the engine): ask for the next with `offset`.
    pub paged: bool,
}

#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// libpq's sslmodes: only verify-ca/verify-full check the certificate (port-forwards
/// reach the server as localhost, which no certificate names).
fn tls(sslmode: &str) -> MakeRustlsConnect {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions");
    let config = if sslmode.starts_with("verify") {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth()
    };
    MakeRustlsConnect::new(config)
}

impl Pg {
    pub fn connect(r: &Resolved) -> Result<Pg> {
        let mut c = Config::new();
        let user = r
            .user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "postgres".into());
        c.host(&r.host)
            .port(r.port)
            .user(&user)
            .dbname(r.database.as_deref().unwrap_or(&user))
            .application_name("dbx")
            .connect_timeout(Duration::from_secs(10));
        if let Some(pw) = &r.password {
            c.password(pw);
        }
        if r.readonly {
            c.options(READONLY_OPTION);
        }
        let sslmode = r
            .params
            .iter()
            .find(|(k, _)| k == "sslmode")
            .map(|(_, v)| v.as_str())
            .unwrap_or("prefer");
        c.ssl_mode(match sslmode {
            "disable" | "allow" => postgres::config::SslMode::Disable,
            "prefer" => postgres::config::SslMode::Prefer,
            _ => postgres::config::SslMode::Require,
        });
        let tls = tls(sslmode);
        let mut client = c
            .connect(tls.clone())
            .map_err(|e| anyhow::anyhow!(describe(&e)))
            .with_context(|| format!("cannot connect to {}:{}", r.host, r.port))?;
        let server_version = client
            .query_one("select current_setting('server_version')", &[])
            .map(|row| row.get::<_, String>(0))
            .unwrap_or_default();
        let cancel = client.cancel_token();
        Ok(Pg {
            client,
            cancel,
            tls,
            server_version,
            fks: None,
            origins: HashMap::new(),
        })
    }

    pub fn cancel_token(&self) -> (CancelToken, MakeRustlsConnect) {
        (self.cancel.clone(), self.tls.clone())
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Runs `statements` in one transaction, each expected to change exactly one row; anything
    /// else rolls the whole batch back. Returns the index and message of the one that failed.
    pub fn apply(&mut self, statements: &[String]) -> std::result::Result<usize, (usize, String)> {
        let mut tx = self.client.transaction().map_err(|e| (0, describe(&e)))?;
        for (i, stmt) in statements.iter().enumerate() {
            match tx.execute(stmt.as_str(), &[]) {
                Ok(1) => {}
                Ok(0) => {
                    return Err((
                        i,
                        "the row changed or is gone since it was loaded; nothing was applied"
                            .into(),
                    ));
                }
                Ok(n) => return Err((i, format!("it would change {n} rows; nothing was applied"))),
                Err(e) => return Err((i, format!("{}; nothing was applied", describe(&e)))),
            }
        }
        tx.commit().map_err(|e| (statements.len(), describe(&e)))?;
        Ok(statements.len())
    }

    pub fn set_readonly(&mut self, on: bool) -> Result<()> {
        let v = if on { "on" } else { "off" };
        self.client
            .batch_execute(&format!("set default_transaction_read_only = {v}"))?;
        Ok(())
    }

    /// What the server says now, not what was asked: a statement may have changed it.
    pub fn is_readonly(&mut self) -> Option<bool> {
        let rows = self
            .client
            .simple_query("show default_transaction_read_only")
            .ok()?;
        rows.iter().find_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => r.get(0).map(|v| v == "on"),
            _ => None,
        })
    }

    /// Runs `sql[range]` (byte range). A lone read-only statement runs as a page of `limit` rows.
    pub fn run(
        &mut self,
        sql: &str,
        range: (usize, usize),
        all: bool,
        limit: usize,
        offset: usize,
    ) -> Output {
        let start = Instant::now();
        let text = &sql[range.0..range.1];
        let single = !all || sql::statements(text).len() == 1;
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
        if single && sql::pageable(text) {
            match self.page(text, limit, offset) {
                Some(Ok(set)) => {
                    out.results.push(set);
                    out.paged = true;
                }
                Some(Err(e)) => {
                    let closed = self.client.is_closed();
                    out.error = Some(sql_error(
                        &e,
                        sql,
                        range.0,
                        WRAP_PREFIX.chars().count(),
                        closed,
                    ));
                }
                None => return self.run_as_typed(sql, range, limit, out, start),
            }
            out.elapsed_ms = start.elapsed().as_millis() as u64;
            return out;
        }
        self.run_as_typed(sql, range, limit, out, start)
    }

    fn run_as_typed(
        &mut self,
        sql: &str,
        range: (usize, usize),
        limit: usize,
        mut out: Output,
        start: Instant,
    ) -> Output {
        let text = &sql[range.0..range.1];
        match self.client.simple_query(text) {
            Ok(messages) => out.results = collect(messages, limit),
            Err(e) => {
                let closed = self.client.is_closed();
                out.error = Some(sql_error(&e, sql, range.0, 0, closed));
            }
        }
        out.elapsed_ms = start.elapsed().as_millis() as u64;
        out
    }

    /// `None` when the statement can't be wrapped (it then runs as typed); preparing runs nothing.
    fn page(
        &mut self,
        stmt: &str,
        limit: usize,
        offset: usize,
    ) -> Option<Result<ResultSet, postgres::Error>> {
        let wrapped = format!(
            "{WRAP_PREFIX}{stmt}\n) as dbx_page limit {} offset {offset}",
            limit + 1
        );
        let prepared = self.client.prepare(&wrapped).ok()?;
        let fks = self.foreign_keys();
        let keys: Vec<Option<(u32, i16)>> = prepared
            .columns()
            .iter()
            .map(|c| c.table_oid().zip(c.column_id()))
            .collect();
        self.learn_origins(&keys);
        let columns = prepared
            .columns()
            .iter()
            .zip(&keys)
            .map(|(c, k)| Column {
                name: c.name().into(),
                ty: Some(c.type_().name().into()),
                refs: k.and_then(|k| fks.get(&k).cloned()),
                origin: k.and_then(|k| self.origins.get(&k).cloned()),
            })
            .collect();
        drop(prepared);
        Some(self.client.simple_query(&wrapped).map(|messages| {
            let mut set = collect(messages, limit)
                .into_iter()
                .next()
                .unwrap_or_default();
            set.columns = columns;
            set
        }))
    }

    /// Rows as text, whole, with their column names (for copying between databases).
    pub fn text_rows(&mut self, sql: &str) -> Result<(Vec<String>, Vec<TextRow>)> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        for m in self.client.simple_query(sql)? {
            match m {
                SimpleQueryMessage::RowDescription(c) => {
                    columns = c.iter().map(|x| x.name().to_string()).collect()
                }
                SimpleQueryMessage::Row(r) => {
                    rows.push((0..r.len()).map(|i| r.get(i).map(String::from)).collect())
                }
                _ => {}
            }
        }
        Ok((columns, rows))
    }

    /// Runs one statement; how many rows it changed.
    pub fn execute(&mut self, sql: &str) -> Result<u64> {
        let mut n = 0;
        for m in self.client.simple_query(sql)? {
            if let SimpleQueryMessage::CommandComplete(c) = m {
                n = c;
            }
        }
        Ok(n)
    }

    /// Names the table columns results come from (once per column per session).
    fn learn_origins(&mut self, keys: &[Option<(u32, i16)>]) {
        let unknown: Vec<(u32, i16)> = keys
            .iter()
            .flatten()
            .filter(|k| !self.origins.contains_key(k))
            .copied()
            .collect();
        if unknown.is_empty() {
            return;
        }
        let oids: Vec<u32> = unknown.iter().map(|k| k.0).collect();
        let nums: Vec<i16> = unknown.iter().map(|k| k.1).collect();
        let Ok(rows) = self.client.query(
            "select a.attrelid, a.attnum, n.nspname::text, c.relname::text, a.attname::text
             from unnest($1::oid[], $2::int2[]) as k(rel, num)
             join pg_attribute a on a.attrelid = k.rel and a.attnum = k.num
             join pg_class c on c.oid = a.attrelid
             join pg_namespace n on n.oid = c.relnamespace",
            &[&oids, &nums],
        ) else {
            return;
        };
        for r in rows {
            let key = (r.get::<_, u32>(0), r.get::<_, i16>(1));
            let origin = Ref {
                schema: r.get(2),
                table: r.get(3),
                column: r.get(4),
            };
            self.origins.insert(key, origin);
        }
    }

    /// The database's single-column foreign keys (read once, and again with the catalog).
    fn foreign_keys(&mut self) -> HashMap<(u32, i16), Ref> {
        if let Some(fks) = &self.fks {
            return fks.clone();
        }
        let rows = self.client.query(
            "select c.conrelid, c.conkey[1], n.nspname::text, r.relname::text, a.attname::text
             from pg_constraint c
             join pg_class r on r.oid = c.confrelid
             join pg_namespace n on n.oid = r.relnamespace
             join pg_attribute a on a.attrelid = c.confrelid and a.attnum = c.confkey[1]
             where c.contype = 'f' and cardinality(c.conkey) = 1",
            &[],
        );
        let fks: HashMap<(u32, i16), Ref> = rows
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        let key = (r.get::<_, u32>(0), r.get::<_, i16>(1));
                        let target = Ref {
                            schema: r.get(2),
                            table: r.get(3),
                            column: r.get(4),
                        };
                        (key, target)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.fks = Some(fks.clone());
        fks
    }

    /// The row `table.column = value` points at, with its own columns' references.
    pub fn lookup(&mut self, target: &Ref, value: &str, limit: usize) -> Result<ResultSet> {
        let stmt = format!(
            "select * from {}.{} where {} = {}",
            quote_ident(&target.schema),
            quote_ident(&target.table),
            quote_ident(&target.column),
            quote_literal(value)
        );
        match self.page(&stmt, limit, 0) {
            Some(r) => Ok(r?),
            None => anyhow::bail!("cannot look up {}.{}", target.schema, target.table),
        }
    }

    /// The single-column foreign keys that point at `schema.table`, and how many rows each has
    /// pointing at `row` (counted up to 1000; `more` past that).
    pub fn referrers(
        &mut self,
        schema: &str,
        table: &str,
        row: &HashMap<String, Option<String>>,
    ) -> Result<Vec<serde_json::Value>> {
        let incoming = self.client.query(
            "select n.nspname::text, s.relname::text, a.attname::text, t.attname::text
             from pg_constraint c
             join pg_class s on s.oid = c.conrelid
             join pg_namespace n on n.oid = s.relnamespace
             join pg_attribute a on a.attrelid = c.conrelid and a.attnum = c.conkey[1]
             join pg_attribute t on t.attrelid = c.confrelid and t.attnum = c.confkey[1]
             where c.contype = 'f' and cardinality(c.conkey) = 1
               and c.confrelid = (quote_ident($1) || '.' || quote_ident($2))::regclass
             order by s.relname, a.attname",
            &[&schema, &table],
        )?;
        let mut out = Vec::new();
        for r in incoming {
            let (src_schema, src_table, column, target): (String, String, String, String) =
                (r.get(0), r.get(1), r.get(2), r.get(3));
            let Some(Some(value)) = row.get(&target) else {
                continue;
            };
            let counted = self.client.query_one(
                &format!(
                    "select count(*) from (select 1 from {}.{} where {} = {} limit 1001) x",
                    quote_ident(&src_schema),
                    quote_ident(&src_table),
                    quote_ident(&column),
                    quote_literal(value)
                ),
                &[],
            )?;
            let count: i64 = counted.get(0);
            out.push(serde_json::json!({
                "schema": src_schema, "table": src_table, "column": column, "value": value,
                "count": count.min(1000), "more": count > 1000,
            }));
        }
        Ok(out)
    }

    /// The rows several values point at, in one query (a page's worth of hover cards).
    pub fn lookup_many(&mut self, target: &Ref, values: &[String]) -> Result<ResultSet> {
        let list: Vec<String> = values.iter().take(500).map(|v| quote_literal(v)).collect();
        if list.is_empty() {
            return Ok(ResultSet::default());
        }
        let stmt = format!(
            "select * from {}.{} where {} in ({})",
            quote_ident(&target.schema),
            quote_ident(&target.table),
            quote_ident(&target.column),
            list.join(", ")
        );
        match self.page(&stmt, list.len(), 0) {
            Some(r) => Ok(r?),
            None => anyhow::bail!("cannot look up {}.{}", target.schema, target.table),
        }
    }

    pub fn catalog(&mut self) -> Result<serde_json::Value> {
        self.fks = None;
        let schemas: Vec<String> = self
            .client
            .query(
                "select nspname::text from pg_namespace
                 where nspname not in ('pg_catalog', 'information_schema') and nspname not like 'pg\\_toast%'
                   and nspname not like 'pg\\_temp%'
                 order by nspname = current_schema() desc, nspname",
                &[],
            )?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let rows = self.client.query(
            "select n.nspname::text, c.relname::text,
                    case c.relkind when 'r' then 'table' when 'p' then 'table' when 'v' then 'view'
                         when 'm' then 'materialized view' when 'f' then 'foreign table' end,
                    greatest(c.reltuples, 0)::bigint,
                    obj_description(c.oid, 'pg_class'),
                    coalesce((select json_agg(json_build_object(
                                 'name', a.attname, 'type', format_type(a.atttypid, a.atttypmod),
                                 'nullable', not a.attnotnull,
                                 'pk', exists (select 1 from pg_index i where i.indrelid = c.oid and i.indisprimary
                                                                         and a.attnum = any(i.indkey)))
                               order by a.attnum)
                              from pg_attribute a where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped),
                             '[]')::text
             from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where c.relkind in ('r', 'p', 'v', 'm', 'f') and not c.relispartition
               and n.nspname not in ('pg_catalog', 'information_schema') and n.nspname not like 'pg\\_toast%'
               and n.nspname not like 'pg\\_temp%'
             order by 1, 2
             limit 5000",
            &[],
        )?;
        let tables: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let columns: serde_json::Value = serde_json::from_str(&r.get::<_, String>(5))
                    .unwrap_or(serde_json::Value::Array(vec![]));
                serde_json::json!({
                    "schema": r.get::<_, String>(0),
                    "name": r.get::<_, String>(1),
                    "kind": r.get::<_, Option<String>>(2),
                    "rows": r.get::<_, i64>(3),
                    "comment": r.get::<_, Option<String>>(4),
                    "columns": columns,
                })
            })
            .collect();
        Ok(serde_json::json!({ "schemas": schemas, "tables": tables }))
    }

    pub fn table(&mut self, schema: &str, name: &str) -> Result<serde_json::Value> {
        let rel = format!("{}.{}", quote_ident(schema), quote_ident(name));
        let head = self.client.query_one(
            "select c.oid::bigint, c.relkind::text, greatest(c.reltuples, 0)::bigint,
                    pg_total_relation_size(c.oid)::bigint, obj_description(c.oid, 'pg_class'),
                    case when c.relkind in ('v', 'm') then pg_get_viewdef(c.oid, true) end
             from pg_class c where c.oid = $1::text::regclass",
            &[&rel],
        )?;
        let oid: i64 = head.get(0);
        let oid = oid as u32;
        let columns: Vec<serde_json::Value> = self
            .client
            .query(
                "select a.attname::text, format_type(a.atttypid, a.atttypmod), not a.attnotnull,
                        pg_get_expr(d.adbin, d.adrelid), col_description(a.attrelid, a.attnum)
                 from pg_attribute a left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
                 where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped order by a.attnum",
                &[&oid],
            )?
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.get::<_, String>(0), "type": r.get::<_, String>(1), "nullable": r.get::<_, bool>(2),
                    "default": r.get::<_, Option<String>>(3), "comment": r.get::<_, Option<String>>(4),
                })
            })
            .collect();
        let indexes: Vec<serde_json::Value> = self
            .client
            .query(
                "select i.indexrelid::regclass::text, pg_get_indexdef(i.indexrelid), i.indisprimary, i.indisunique
                 from pg_index i where i.indrelid = $1 order by i.indisprimary desc, 1",
                &[&oid],
            )?
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.get::<_, String>(0), "definition": r.get::<_, String>(1),
                    "primary": r.get::<_, bool>(2), "unique": r.get::<_, bool>(3),
                })
            })
            .collect();
        let constraints: Vec<serde_json::Value> = self
            .client
            .query(
                "select conname::text, contype::text, pg_get_constraintdef(oid) from pg_constraint
                 where conrelid = $1 order by contype, conname",
                &[&oid],
            )?
            .iter()
            .map(|r| {
                let kind = match r.get::<_, String>(1).as_str() {
                    "p" => "primary key",
                    "f" => "foreign key",
                    "u" => "unique",
                    "c" => "check",
                    "x" => "exclusion",
                    _ => "other",
                };
                serde_json::json!({ "name": r.get::<_, String>(0), "kind": kind, "definition": r.get::<_, String>(2) })
            })
            .collect();
        let primary_key: Vec<String> = self
            .client
            .query(
                "select a.attname::text from pg_index i
                 join lateral unnest(i.indkey) with ordinality k(attnum, n) on true
                 join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
                 where i.indrelid = $1 and i.indisprimary order by k.n",
                &[&oid],
            )?
            .iter()
            .map(|r| r.get(0))
            .collect();
        Ok(serde_json::json!({
            "primary_key": primary_key,
            "schema": schema, "name": name, "kind": head.get::<_, String>(1),
            "rows": head.get::<_, i64>(2), "bytes": head.get::<_, i64>(3),
            "comment": head.get::<_, Option<String>>(4), "view_definition": head.get::<_, Option<String>>(5),
            "columns": columns, "indexes": indexes, "constraints": constraints,
        }))
    }
}

/// A string literal that means the same whatever `standard_conforming_strings` says.
pub fn quote_literal(s: &str) -> String {
    let quoted = s.replace('\'', "''");
    if s.contains('\\') {
        format!("E'{}'", quoted.replace('\\', "\\\\"))
    } else {
        format!("'{quoted}'")
    }
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn collect(messages: Vec<SimpleQueryMessage>, limit: usize) -> Vec<ResultSet> {
    let mut sets = Vec::new();
    let mut open: Option<ResultSet> = None;
    let mut size = 0;
    for m in messages {
        match m {
            SimpleQueryMessage::RowDescription(cols) => {
                open = Some(ResultSet {
                    columns: cols
                        .iter()
                        .map(|c| Column {
                            name: c.name().into(),
                            ty: None,
                            refs: None,
                            origin: None,
                        })
                        .collect(),
                    ..Default::default()
                });
            }
            SimpleQueryMessage::Row(row) => {
                let set = open.get_or_insert_with(ResultSet::default);
                if set.rows.len() >= limit || size > BUDGET {
                    set.more = true;
                    continue;
                }
                let cells: Vec<Option<String>> = (0..row.len())
                    .map(|i| {
                        row.get(i).map(|v| {
                            size += v.len().min(CELL_MAX) + 8;
                            truncate(v)
                        })
                    })
                    .collect();
                set.rows.push(cells);
            }
            SimpleQueryMessage::CommandComplete(n) => match open.take() {
                Some(set) => sets.push(set),
                None => sets.push(ResultSet {
                    affected: Some(n),
                    ..Default::default()
                }),
            },
            _ => {}
        }
    }
    sets.extend(open);
    sets
}

pub(super) fn truncate(v: &str) -> String {
    if v.len() <= CELL_MAX {
        return v.to_string();
    }
    let mut end = CELL_MAX;
    while !v.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &v[..end])
}

fn describe(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_string(),
        None => {
            let mut s = e.to_string();
            let mut src = std::error::Error::source(e);
            while let Some(inner) = src {
                s = format!("{s}: {inner}");
                src = inner.source();
            }
            s
        }
    }
}

fn sql_error(e: &postgres::Error, sql: &str, base: usize, shift: usize, closed: bool) -> SqlError {
    let db = e.as_db_error();
    let position = db.and_then(|d| match d.position() {
        Some(postgres::error::ErrorPosition::Original(p)) => {
            let stmt = &sql[base..];
            let byte = stmt
                .char_indices()
                .nth((*p as usize).saturating_sub(1 + shift))
                .map_or(stmt.len(), |(i, _)| i);
            Some(sql::byte_to_utf16(sql, base + byte))
        }
        _ => None,
    });
    SqlError {
        message: describe(e),
        detail: db.and_then(|d| d.detail().map(String::from)),
        hint: db.and_then(|d| d.hint().map(String::from)),
        code: db.map(|d| d.code().code().to_string()),
        position,
        closed,
    }
}
