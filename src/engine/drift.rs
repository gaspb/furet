//! Comparing two Postgres schemas (stg against prod, local against stg): which tables, columns,
//! indexes and constraints are missing or different, and the SQL that would bring the right side
//! in line with the left. furet only shows that SQL; running it is a migration's job.

use super::pg::{Pg, quote_ident};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Table {
        kind: String,
    },
    Column {
        ty: String,
        not_null: bool,
        default: Option<String>,
    },
    Index {
        def: String,
    },
    Constraint {
        def: String,
    },
}

#[derive(Serialize, Debug)]
pub struct Difference {
    /// table, column, index or constraint.
    pub kind: &'static str,
    /// `shop.orders`, `shop.orders.status`, `shop.orders_status_idx`…
    pub object: String,
    pub left: Option<String>,
    pub right: Option<String>,
    /// What would make the right side like the left (commented out when it would drop something).
    pub fix: String,
}

#[derive(Serialize, Debug)]
pub struct Drift {
    pub differences: Vec<Difference>,
    /// Objects that are the same on both sides.
    pub same: usize,
}

type Key = (String, String, String);

const USER_SCHEMAS: &str = "n.nspname not in ('pg_catalog', 'information_schema')
    and n.nspname not like 'pg\\_toast%' and n.nspname not like 'pg\\_temp%'";

fn snapshot(pg: &mut Pg) -> Result<BTreeMap<(String, Key), Item>> {
    let mut out = BTreeMap::new();
    let text = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().unwrap_or_default();
    let (_, tables) = pg.text_rows(&format!(
        "select n.nspname, c.relname, c.relkind::text from pg_class c join pg_namespace n on n.oid = c.relnamespace
         where c.relkind in ('r', 'p', 'v', 'm') and {USER_SCHEMAS}"
    ))?;
    for r in tables {
        let kind = match text(&r, 2).as_str() {
            "v" => "view",
            "m" => "materialized view",
            _ => "table",
        }
        .to_string();
        out.insert(
            ("table".into(), (text(&r, 0), text(&r, 1), String::new())),
            Item::Table { kind },
        );
    }
    let (_, columns) = pg.text_rows(&format!(
        "select n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull::text,
                pg_get_expr(d.adbin, d.adrelid)
         from pg_attribute a join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace
         left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
         where c.relkind in ('r', 'p') and a.attnum > 0 and not a.attisdropped and {USER_SCHEMAS}"
    ))?;
    for r in columns {
        out.insert(
            ("column".into(), (text(&r, 0), text(&r, 1), text(&r, 2))),
            Item::Column {
                ty: text(&r, 3),
                not_null: text(&r, 4) == "true",
                default: r[5].clone(),
            },
        );
    }
    let (_, indexes) = pg.text_rows(&format!(
        "select n.nspname, ct.relname, ci.relname, pg_get_indexdef(i.indexrelid)
         from pg_index i join pg_class ci on ci.oid = i.indexrelid join pg_class ct on ct.oid = i.indrelid
         join pg_namespace n on n.oid = ci.relnamespace
         where not exists (select 1 from pg_constraint k where k.conindid = i.indexrelid) and {USER_SCHEMAS}"
    ))?;
    for r in indexes {
        out.insert(
            ("index".into(), (text(&r, 0), text(&r, 1), text(&r, 2))),
            Item::Index { def: text(&r, 3) },
        );
    }
    let (_, constraints) = pg.text_rows(&format!(
        "select n.nspname, c.relname, k.conname, pg_get_constraintdef(k.oid)
         from pg_constraint k join pg_class c on c.oid = k.conrelid join pg_namespace n on n.oid = c.relnamespace
         where k.contype in ('p', 'u', 'f', 'c', 'x') and {USER_SCHEMAS}"
    ))?;
    for r in constraints {
        out.insert(
            ("constraint".into(), (text(&r, 0), text(&r, 1), text(&r, 2))),
            Item::Constraint { def: text(&r, 3) },
        );
    }
    Ok(out)
}

fn rel(s: &str, t: &str) -> String {
    format!("{}.{}", quote_ident(s), quote_ident(t))
}

fn describe(item: &Item) -> String {
    match item {
        Item::Table { kind } => kind.clone(),
        Item::Column {
            ty,
            not_null,
            default,
        } => format!(
            "{ty}{}{}",
            if *not_null { " not null" } else { "" },
            default
                .as_ref()
                .map(|d| format!(" default {d}"))
                .unwrap_or_default()
        ),
        Item::Index { def } | Item::Constraint { def } => def.clone(),
    }
}

fn fix(kind: &str, (s, t, n): &Key, left: Option<&Item>, right: Option<&Item>) -> String {
    let table = rel(s, t);
    match (kind, left, right) {
        ("table", Some(_), None) => {
            format!("-- {table} is missing: run the migrations that create it")
        }
        ("table", None, Some(_)) => format!("-- only on the right: drop table {table};"),
        ("table", Some(_), Some(_)) => {
            format!("-- {table} is a different kind of relation on each side")
        }
        (
            "column",
            Some(Item::Column {
                ty,
                not_null,
                default,
            }),
            None,
        ) => format!(
            "alter table {table} add column {} {ty}{}{};",
            quote_ident(n),
            default
                .as_ref()
                .map(|d| format!(" default {d}"))
                .unwrap_or_default(),
            if *not_null { " not null" } else { "" }
        ),
        ("column", None, Some(_)) => format!(
            "-- only on the right: alter table {table} drop column {};",
            quote_ident(n)
        ),
        (
            "column",
            Some(Item::Column {
                ty,
                not_null,
                default,
            }),
            Some(Item::Column {
                ty: rty,
                not_null: rnn,
                default: rd,
            }),
        ) => {
            let col = quote_ident(n);
            let mut out = Vec::new();
            if ty != rty {
                out.push(format!("alter table {table} alter column {col} type {ty};"));
            }
            if not_null != rnn {
                out.push(format!(
                    "alter table {table} alter column {col} {} not null;",
                    if *not_null { "set" } else { "drop" }
                ));
            }
            if default != rd {
                out.push(match default {
                    Some(d) => format!("alter table {table} alter column {col} set default {d};"),
                    None => format!("alter table {table} alter column {col} drop default;"),
                });
            }
            out.join("\n")
        }
        ("index", Some(Item::Index { def }), None) => format!("{def};"),
        ("index", None, Some(_)) => format!("drop index {};", rel(s, n)),
        ("index", Some(Item::Index { def }), Some(_)) => {
            format!("drop index {};\n{def};", rel(s, n))
        }
        ("constraint", Some(Item::Constraint { def }), None) => {
            format!(
                "alter table {table} add constraint {} {def};",
                quote_ident(n)
            )
        }
        ("constraint", None, Some(_)) => {
            format!("alter table {table} drop constraint {};", quote_ident(n))
        }
        ("constraint", Some(Item::Constraint { def }), Some(_)) => format!(
            "alter table {table} drop constraint {0};\nalter table {table} add constraint {0} {def};",
            quote_ident(n)
        ),
        _ => String::new(),
    }
}

pub fn compare(left: &mut Pg, right: &mut Pg, schema: Option<&str>) -> Result<Drift> {
    let (a, b) = (snapshot(left)?, snapshot(right)?);
    let wanted = |k: &(String, Key)| schema.is_none_or(|s| k.1.0 == s);
    let mut keys: Vec<&(String, Key)> = a.keys().chain(b.keys()).filter(|k| wanted(k)).collect();
    keys.sort();
    keys.dedup();
    // A table on one side only is one difference (its columns, indexes and constraints with it).
    let table_of = |k: &(String, Key)| {
        (
            "table".to_string(),
            (k.1.0.clone(), k.1.1.clone(), String::new()),
        )
    };
    let lonely = |k: &(String, Key)| {
        let t = table_of(k);
        k.0 != "table" && a.contains_key(&t) != b.contains_key(&t)
    };
    let mut differences = Vec::new();
    let mut same = 0;
    for k in keys {
        let (l, r) = (a.get(k), b.get(k));
        if l == r {
            same += 1;
            continue;
        }
        if lonely(k) {
            continue;
        }
        let (s, t, n) = &k.1;
        if k.0 == "table" && r.is_none() {
            let parts: Vec<_> = a
                .iter()
                .filter(|(x, _)| x.0 != "table" && x.1.0 == *s && x.1.1 == *t)
                .collect();
            differences.push(Difference {
                kind: "table",
                object: format!("{s}.{t}"),
                left: Some(format!(
                    "{} with {} columns",
                    l.map(describe).unwrap_or_default(),
                    parts.iter().filter(|(x, _)| x.0 == "column").count()
                )),
                right: None,
                fix: create_table(s, t, &parts),
            });
            continue;
        }
        let kind: &'static str = match k.0.as_str() {
            "table" => "table",
            "column" => "column",
            "index" => "index",
            _ => "constraint",
        };
        let object = match kind {
            "table" => format!("{s}.{t}"),
            "index" => format!("{s}.{n} (on {t})"),
            _ => format!("{s}.{t}.{n}"),
        };
        differences.push(Difference {
            kind,
            object,
            left: l.map(describe),
            right: r.map(describe),
            fix: fix(kind, &k.1, l, r),
        });
    }
    Ok(Drift { differences, same })
}

/// The `create table` (then its indexes) that would make a table the left side has.
fn create_table(s: &str, t: &str, parts: &[(&(String, Key), &Item)]) -> String {
    let mut lines: Vec<String> = parts
        .iter()
        .filter_map(|(k, i)| match i {
            Item::Column { .. } => Some(format!("{} {}", quote_ident(&k.1.2), describe(i))),
            _ => None,
        })
        .collect();
    lines.extend(parts.iter().filter_map(|(k, i)| match i {
        Item::Constraint { def } => Some(format!("constraint {} {def}", quote_ident(&k.1.2))),
        _ => None,
    }));
    let indexes: Vec<String> = parts
        .iter()
        .filter_map(|(_, i)| match i {
            Item::Index { def } => Some(format!("{def};")),
            _ => None,
        })
        .collect();
    let mut out = format!(
        "create table {} (\n  {}\n);",
        rel(s, t),
        lines.join(",\n  ")
    );
    for i in indexes {
        out.push('\n');
        out.push_str(&i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_fix_for_each_kind_of_difference() {
        let key = ("shop".to_string(), "orders".to_string(), "note".to_string());
        let col = |ty: &str, nn: bool| Item::Column {
            ty: ty.into(),
            not_null: nn,
            default: None,
        };
        assert_eq!(
            fix("column", &key, Some(&col("text", true)), None),
            "alter table \"shop\".\"orders\" add column \"note\" text not null;"
        );
        assert_eq!(
            fix(
                "column",
                &key,
                Some(&col("bigint", true)),
                Some(&col("integer", false))
            ),
            "alter table \"shop\".\"orders\" alter column \"note\" type bigint;\nalter table \"shop\".\"orders\" alter column \"note\" set not null;"
        );
        assert!(
            fix(
                "table",
                &key,
                None,
                Some(&Item::Table {
                    kind: "table".into()
                })
            )
            .starts_with("-- only on the right")
        );
        let idx = Item::Index {
            def: "CREATE INDEX x ON shop.orders USING btree (note)".into(),
        };
        assert_eq!(
            fix("index", &key, Some(&idx), None),
            "CREATE INDEX x ON shop.orders USING btree (note);"
        );
    }
}
