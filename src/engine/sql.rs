//! Just enough SQL lexing to split a script into statements and tell a query from the rest.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    /// Backtick identifiers, and backslash escapes in every quote.
    Clickhouse,
}

/// Byte ranges of the statements in `sql`, trimmed, without their `;`. Quotes, comments and
/// `$tag$` bodies don't split.
pub fn statements(sql: &str) -> Vec<(usize, usize)> {
    statements_in(sql, Dialect::Postgres)
}

pub fn statements_in(sql: &str, dialect: Dialect) -> Vec<(usize, usize)> {
    let ch = dialect == Dialect::Clickhouse;
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => i = skip_quoted(b, i, ch),
            b'`' if ch => i = skip_quoted(b, i, true),
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i),
            b'$' => i = skip_dollar(sql, i),
            b';' => {
                push(sql, start, i, &mut out);
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    push(sql, start, b.len(), &mut out);
    out
}

fn push(sql: &str, start: usize, end: usize, out: &mut Vec<(usize, usize)>) {
    let s = &sql[start..end];
    if strip_comments(s).trim().is_empty() {
        return;
    }
    let lead = s.len() - s.trim_start().len();
    let trail = s.len() - s.trim_end().len();
    out.push((start + lead, end - trail));
}

fn skip_quoted(b: &[u8], i: usize, backslash: bool) -> usize {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

fn skip_block_comment(b: &[u8], i: usize) -> usize {
    let mut depth = 0;
    let mut j = i;
    while j + 1 < b.len() {
        match (b[j], b[j + 1]) {
            (b'/', b'*') => {
                depth += 1;
                j += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return j;
                }
            }
            _ => j += 1,
        }
    }
    b.len()
}

/// `$tag$ … $tag$` (a tag is empty or an identifier); a lone `$1` is a parameter.
fn skip_dollar(sql: &str, i: usize) -> usize {
    let rest = &sql[i + 1..];
    let tag_len = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let tag = &rest[..tag_len];
    if rest.as_bytes().get(tag_len) != Some(&b'$') || tag.starts_with(|c: char| c.is_ascii_digit())
    {
        return i + 1;
    }
    let delim = format!("${tag}$");
    let body = i + delim.len();
    sql[body..]
        .find(&delim)
        .map_or(sql.len(), |p| body + p + delim.len())
}

fn strip_comments(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' && b.get(i + 1) == Some(&b'-') {
            i = b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |p| i + p);
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(b, i);
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// The statement's first keyword, lowercased (`select`, `with`, `insert`…).
pub fn verb(stmt: &str) -> String {
    let s = strip_comments(stmt);
    let s = s.trim_start().trim_start_matches('(');
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// A statement that only reads rows and can be wrapped in `select * from (…) limit`.
pub fn pageable(stmt: &str) -> bool {
    let lower = strip_comments(stmt).to_lowercase();
    let reads = match verb(stmt).as_str() {
        "select" | "values" | "table" => true,
        // A CTE may hide a data-modifying statement.
        "with" => !["insert", "update", "delete", "merge"]
            .iter()
            .any(|w| contains_word(&lower, w)),
        _ => false,
    };
    reads
        && !contains_word(&lower, "into")
        && !lower.contains(" for update")
        && !lower.contains(" for share")
}

/// Would turn a read-only Postgres session writable (`set default_transaction_read_only`,
/// `begin read write`, `reset all`, dynamic SQL…): refused while it's read-only.
pub fn lifts_readonly(stmt: &str) -> bool {
    let lower = strip_comments(stmt).to_lowercase();
    let flat = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    [
        "transaction_read_only",
        "read write",
        "session characteristics",
        "set_config",
        "reset all",
        "discard all",
    ]
    .iter()
    .any(|w| flat.contains(w))
        || statements(stmt)
            .iter()
            .any(|&(a, b)| verb(&stmt[a..b]) == "do")
}

fn contains_word(s: &str, w: &str) -> bool {
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|t| t == w)
}

pub fn utf16_to_byte(s: &str, offset: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= offset {
            return i;
        }
        units += c.len_utf16();
    }
    s.len()
}

pub fn byte_to_utf16(s: &str, byte: usize) -> usize {
    s[..byte.min(s.len())].chars().map(char::len_utf16).sum()
}

/// The statement holding `cursor` (a byte offset), or the last one before it.
pub fn at(sql: &str, cursor: usize, dialect: Dialect) -> Option<(usize, usize)> {
    let all = statements_in(sql, dialect);
    all.iter()
        .copied()
        .find(|&(s, e)| cursor >= s && cursor <= e + 1)
        .or_else(|| all.iter().copied().rev().find(|&(_, e)| e <= cursor))
        .or_else(|| all.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        statements(sql)
            .into_iter()
            .map(|(s, e)| &sql[s..e])
            .collect()
    }

    #[test]
    fn splits_on_semicolons_outside_quotes_comments_and_bodies() {
        let sql = "select ';' as a; -- x;y\nselect \"a;b\" from t;\n/* c; /* nested; */ */ select 1;\n\
                   create function f() returns int as $body$ select 1; $body$ language sql; select $1";
        assert_eq!(
            texts(sql),
            vec![
                "select ';' as a",
                "-- x;y\nselect \"a;b\" from t",
                "/* c; /* nested; */ */ select 1",
                "create function f() returns int as $body$ select 1; $body$ language sql",
                "select $1",
            ]
        );
        assert!(texts("  ; -- only a comment\n").is_empty());
    }

    #[test]
    fn splits_clickhouse_quoting() {
        let sql = "select `a;b`, 'it\\'s;' from t; select \"x\\\";\" from `t;2`";
        let ranges = statements_in(sql, Dialect::Clickhouse);
        let texts: Vec<&str> = ranges.iter().map(|&(s, e)| &sql[s..e]).collect();
        assert_eq!(
            texts,
            vec![
                "select `a;b`, 'it\\'s;' from t",
                "select \"x\\\";\" from `t;2`"
            ]
        );
        assert_eq!(statements("select 'a\\'; select 2").len(), 2);
    }

    #[test]
    fn knows_which_statements_only_read() {
        assert!(pageable("-- hi\nSELECT * FROM users"));
        assert!(pageable("with x as (select 1) select * from x"));
        assert!(pageable("(select 1) union (select 2)"));
        assert!(!pageable(
            "with d as (delete from t returning *) select * from d"
        ));
        assert!(!pageable("select * into t2 from t"));
        assert!(!pageable("select * from t for update"));
        assert!(!pageable("update t set a = 1"));
        assert!(!pageable("explain select 1"));
    }

    #[test]
    fn spots_what_would_lift_read_only() {
        for sql in [
            "SET default_transaction_read_only = off",
            "set session characteristics as transaction read write",
            "begin; set transaction\n  READ   WRITE; insert into t values (1)",
            "start transaction isolation level serializable, read write",
            "reset all",
            "select set_config('default_transaction_' || 'read_only', 'off', false)",
            "do $$ begin execute 'x'; end $$",
        ] {
            assert!(lifts_readonly(sql), "{sql}");
        }
        assert!(!lifts_readonly("select * from users; begin read only"));
        assert!(!lifts_readonly("show transaction isolation level"));
    }

    #[test]
    fn finds_the_statement_under_the_cursor() {
        let sql = "select 1;\n\nselect 2;\nselect 3";
        assert_eq!(
            at(sql, 3, Dialect::Postgres).map(|(s, e)| &sql[s..e]),
            Some("select 1")
        );
        assert_eq!(
            at(sql, 9, Dialect::Postgres).map(|(s, e)| &sql[s..e]),
            Some("select 1")
        );
        assert_eq!(
            at(sql, 10, Dialect::Postgres).map(|(s, e)| &sql[s..e]),
            Some("select 1")
        );
        assert_eq!(
            at(sql, 13, Dialect::Postgres).map(|(s, e)| &sql[s..e]),
            Some("select 2")
        );
        assert_eq!(
            at(sql, sql.len(), Dialect::Postgres).map(|(s, e)| &sql[s..e]),
            Some("select 3")
        );
    }

    #[test]
    fn converts_js_offsets() {
        let s = "é😀x";
        assert_eq!(utf16_to_byte(s, 3), 6);
        assert_eq!(byte_to_utf16(s, 6), 3);
    }
}
