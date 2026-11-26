//! Copying a row and what it needs from one Postgres connection to another (stg → local, say):
//! the rows its foreign keys point at, all the way up, and optionally the rows pointing at it.
//! Parents go in first, rows already there are left alone, generated columns are skipped,
//! personal data can be masked, and sequences move past the copied ids.

use super::pg::{Pg, quote_ident, quote_literal};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

const MAX_ROWS: usize = 2000;
const CHILDREN: usize = 200;

#[derive(Debug, Clone)]
pub struct Row {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub values: Vec<Option<String>>,
}

#[derive(Serialize, Debug, Default)]
pub struct Outcome {
    /// Rows per table, in the order they go in.
    pub plan: Vec<(String, usize)>,
    pub inserted: u64,
    pub existing: u64,
    pub masked: usize,
}

fn rel(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(table))
}

/// `'"schema"."table"'::regclass`, for catalog queries.
fn regclass(schema: &str, table: &str) -> String {
    format!("{}::regclass", quote_literal(&rel(schema, table)))
}

struct Catalog<'a> {
    pg: &'a mut Pg,
    keys: HashMap<String, Vec<String>>,
    parents: HashMap<String, Vec<(String, String, String, String)>>,
}

impl Catalog<'_> {
    fn primary_key(&mut self, schema: &str, table: &str) -> Result<Vec<String>> {
        let k = rel(schema, table);
        if let Some(v) = self.keys.get(&k) {
            return Ok(v.clone());
        }
        let (_, rows) = self.pg.text_rows(&format!(
            "select a.attname from pg_index i
             join pg_attribute a on a.attrelid = i.indrelid and a.attnum = any(i.indkey)
             where i.indrelid = {} and i.indisprimary order by a.attnum",
            regclass(schema, table)
        ))?;
        let cols: Vec<String> = rows.into_iter().filter_map(|r| r[0].clone()).collect();
        self.keys.insert(k, cols.clone());
        Ok(cols)
    }

    /// (column, referenced schema, table, column) for each single-column foreign key.
    fn foreign_keys(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<Vec<(String, String, String, String)>> {
        let k = rel(schema, table);
        if let Some(v) = self.parents.get(&k) {
            return Ok(v.clone());
        }
        let (_, rows) = self.pg.text_rows(&format!(
            "select a.attname, n.nspname, r.relname, f.attname
             from pg_constraint c
             join pg_attribute a on a.attrelid = c.conrelid and a.attnum = c.conkey[1]
             join pg_class r on r.oid = c.confrelid
             join pg_namespace n on n.oid = r.relnamespace
             join pg_attribute f on f.attrelid = c.confrelid and f.attnum = c.confkey[1]
             where c.contype = 'f' and cardinality(c.conkey) = 1 and c.conrelid = {}",
            regclass(schema, table)
        ))?;
        let fks: Vec<_> = rows
            .into_iter()
            .filter_map(|r| Some((r[0].clone()?, r[1].clone()?, r[2].clone()?, r[3].clone()?)))
            .collect();
        self.parents.insert(k, fks.clone());
        Ok(fks)
    }

    fn rows_where(
        &mut self,
        schema: &str,
        table: &str,
        column: &str,
        value: &str,
        limit: usize,
    ) -> Result<Vec<Row>> {
        let (columns, rows) = self.pg.text_rows(&format!(
            "select * from {} where {} = {} limit {limit}",
            rel(schema, table),
            quote_ident(column),
            quote_literal(value)
        ))?;
        Ok(rows
            .into_iter()
            .map(|values| Row {
                schema: schema.into(),
                table: table.into(),
                columns: columns.clone(),
                values,
            })
            .collect())
    }

    fn identity(&mut self, row: &Row) -> Result<String> {
        let key = self.primary_key(&row.schema, &row.table)?;
        let parts: Vec<String> = if key.is_empty() {
            row.values
                .iter()
                .map(|v| v.clone().unwrap_or_default())
                .collect()
        } else {
            key.iter()
                .map(|k| {
                    row.columns
                        .iter()
                        .position(|c| c == k)
                        .and_then(|i| row.values[i].clone())
                        .unwrap_or_default()
                })
                .collect()
        };
        Ok(format!(
            "{}\u{1}{}",
            rel(&row.schema, &row.table),
            parts.join("\u{1}")
        ))
    }
}

/// The start row(s), what they point at (all the way up), and with `children` the rows pointing
/// at them, in an order where every row comes after the rows it needs.
pub fn collect(
    src: &mut Pg,
    schema: &str,
    table: &str,
    column: &str,
    value: &str,
    children: bool,
) -> Result<Vec<Row>> {
    let mut cat = Catalog {
        pg: src,
        keys: HashMap::new(),
        parents: HashMap::new(),
    };
    let start = cat.rows_where(schema, table, column, value, 50)?;
    if start.is_empty() {
        bail!("no {schema}.{table} row with {column} = {value}");
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in &start {
        visit(&mut cat, row.clone(), &mut seen, &mut out, 0)?;
    }
    if children {
        let (_, incoming) = cat.pg.text_rows(&format!(
            "select n.nspname, s.relname, a.attname, t.attname
             from pg_constraint c
             join pg_class s on s.oid = c.conrelid
             join pg_namespace n on n.oid = s.relnamespace
             join pg_attribute a on a.attrelid = c.conrelid and a.attnum = c.conkey[1]
             join pg_attribute t on t.attrelid = c.confrelid and t.attnum = c.confkey[1]
             where c.contype = 'f' and cardinality(c.conkey) = 1 and c.confrelid = {}",
            regclass(schema, table)
        ))?;
        for parent in &start {
            for fk in &incoming {
                let (Some(cs), Some(ct), Some(cc), Some(target)) = (&fk[0], &fk[1], &fk[2], &fk[3])
                else {
                    continue;
                };
                let Some(Some(v)) = parent
                    .columns
                    .iter()
                    .position(|c| c == target)
                    .map(|i| parent.values[i].clone())
                else {
                    continue;
                };
                for child in cat.rows_where(cs, ct, cc, &v, CHILDREN)? {
                    visit(&mut cat, child, &mut seen, &mut out, 0)?;
                }
            }
        }
    }
    Ok(out)
}

fn visit(
    cat: &mut Catalog,
    row: Row,
    seen: &mut HashSet<String>,
    out: &mut Vec<Row>,
    depth: usize,
) -> Result<()> {
    if !seen.insert(cat.identity(&row)?) {
        return Ok(());
    }
    if out.len() >= MAX_ROWS || depth > 30 {
        bail!("that's more than {MAX_ROWS} rows; copy something smaller");
    }
    for (col, ps, pt, pc) in cat.foreign_keys(&row.schema, &row.table)? {
        let Some(Some(v)) = row
            .columns
            .iter()
            .position(|c| *c == col)
            .map(|i| row.values[i].clone())
        else {
            continue;
        };
        for parent in cat.rows_where(&ps, &pt, &pc, &v, 1)? {
            visit(cat, parent, seen, out, depth + 1)?;
        }
    }
    out.push(row);
    Ok(())
}

/// Rows per table, in the order they'd go in.
pub fn plan(rows: &[Row]) -> Vec<(String, usize)> {
    let mut plan: Vec<(String, usize)> = Vec::new();
    for r in rows {
        let name = format!("{}.{}", r.schema, r.table);
        match plan.iter_mut().find(|(n, _)| *n == name) {
            Some((_, n)) => *n += 1,
            None => plan.push((name, 1)),
        }
    }
    plan
}

const PEOPLE: [&str; 9] = [
    "user",
    "customer",
    "contact",
    "member",
    "person",
    "people",
    "employee",
    "lead",
    "subscriber",
];

/// A stand-in for personal data, the same for the same value (so equal emails stay equal). `name`
/// counts only in tables of people (a company's name stays).
pub fn mask(table: &str, column: &str, value: &str) -> Option<String> {
    let c = column.to_lowercase();
    let person = PEOPLE.iter().any(|p| table.to_lowercase().contains(p));
    let mut h: u64 = 0xcbf29ce484222325;
    for b in value.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let tag = format!("{:08x}", h as u32);
    if c.contains("email") {
        Some(format!("user-{tag}@example.com"))
    } else if c.contains("phone") || c.contains("mobile") {
        Some(format!("+1555{:07}", h % 10_000_000))
    } else if [
        "first_name",
        "last_name",
        "full_name",
        "display_name",
        "contact_name",
        "firstname",
        "lastname",
    ]
    .contains(&c.as_str())
        || (person && c == "name")
    {
        Some(format!("Person {tag}"))
    } else if c.contains("address") || c.contains("street") {
        Some("1 Example Street".into())
    } else if ["iban", "ssn", "tax_id", "vat_number", "card_number"]
        .iter()
        .any(|k| c.contains(k))
    {
        Some(format!("XXXX{tag}"))
    } else {
        None
    }
}

/// Inserts the rows in one transaction; rows already there stay as they are.
pub fn insert(dst: &mut Pg, rows: &[Row], masking: bool) -> Result<Outcome> {
    let mut out = Outcome {
        plan: plan(rows),
        ..Outcome::default()
    };
    let mut targets: HashMap<String, (Vec<String>, bool)> = HashMap::new();
    dst.execute("begin")?;
    let result = (|| -> Result<()> {
        for r in rows {
            let key = rel(&r.schema, &r.table);
            if !targets.contains_key(&key) {
                let (_, cols) = dst
                    .text_rows(&format!(
                        "select attname, attgenerated <> '', attidentity = 'a' from pg_attribute
                         where attrelid = {} and attnum > 0 and not attisdropped",
                        regclass(&r.schema, &r.table)
                    ))
                    .with_context(|| {
                        format!("{key} isn't in the target database (same migrations?)")
                    })?;
                let writable = cols
                    .iter()
                    .filter(|c| c[1].as_deref() != Some("t"))
                    .filter_map(|c| c[0].clone())
                    .collect();
                let always = cols.iter().any(|c| c[2].as_deref() == Some("t"));
                targets.insert(key.clone(), (writable, always));
            }
            let (writable, always) = &targets[&key];
            let mut names = Vec::new();
            let mut values = Vec::new();
            for (c, v) in r.columns.iter().zip(&r.values) {
                if !writable.contains(c) {
                    continue;
                }
                names.push(quote_ident(c));
                values.push(match v {
                    None => "NULL".to_string(),
                    Some(v) => match masking.then(|| mask(&r.table, c, v)).flatten() {
                        Some(m) => {
                            out.masked += 1;
                            quote_literal(&m)
                        }
                        None => quote_literal(v),
                    },
                });
            }
            let n = dst.execute(&format!(
                "insert into {key} ({}){} values ({}) on conflict do nothing",
                names.join(", "),
                if *always {
                    " overriding system value"
                } else {
                    ""
                },
                values.join(", ")
            ))?;
            if n == 0 {
                out.existing += 1
            } else {
                out.inserted += n
            }
        }
        for key in targets.keys() {
            let (_, seq) = dst.text_rows(&format!(
                "select a.attname, pg_get_serial_sequence({0}, a.attname) from pg_index i
                 join pg_attribute a on a.attrelid = i.indrelid and a.attnum = any(i.indkey)
                 where i.indrelid = {0}::regclass and i.indisprimary",
                quote_literal(key)
            ))?;
            for s in seq {
                if let (Some(col), Some(sequence)) = (&s[0], &s[1]) {
                    dst.execute(&format!(
                        "select setval({}, greatest((select coalesce(max({}), 0) from {key}), 1))",
                        quote_literal(sequence),
                        quote_ident(col)
                    ))?;
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            dst.execute("commit")?;
            Ok(out)
        }
        Err(e) => {
            let _ = dst.execute("rollback");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_personal_data_consistently() {
        assert_eq!(
            mask("t", "email", "a@b.c"),
            mask("t", "billing_email", "a@b.c")
        );
        assert!(
            mask("t", "email", "a@b.c")
                .unwrap()
                .ends_with("@example.com")
        );
        assert_ne!(mask("t", "email", "a@b.c"), mask("t", "email", "x@y.z"));
        assert!(
            mask("t", "phone_number", "0612")
                .unwrap()
                .starts_with("+1555")
        );
        assert_eq!(mask("orgs", "name", "Acme"), None, "company names stay");
        assert!(
            mask("customers", "name", "Ada")
                .unwrap()
                .starts_with("Person ")
        );
        assert_eq!(mask("t", "status", "paid"), None);
    }

    #[test]
    fn plans_in_order_with_counts() {
        let row = |t: &str| Row {
            schema: "s".into(),
            table: t.into(),
            columns: vec![],
            values: vec![],
        };
        let p = plan(&[row("orgs"), row("customers"), row("orders"), row("orders")]);
        assert_eq!(
            p,
            vec![
                ("s.orgs".into(), 1),
                ("s.customers".into(), 1),
                ("s.orders".into(), 2)
            ]
        );
    }
}
