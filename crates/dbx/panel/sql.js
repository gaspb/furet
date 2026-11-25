// Completion candidates for the console: keywords, the catalog's tables, and the columns of
// the tables the query names (or of `alias.` when the word follows one).

const KEYWORDS = `select from where and or not in is null like ilike between exists join left right inner
outer full cross on using group by order having limit offset distinct as case when then else end
insert into values update set delete returning create table view index alter drop add column
primary key references default unique check with union all intersect except asc desc nulls first
last true false count sum avg min max coalesce now interval explain analyze begin commit rollback
truncate cascade grant revoke jsonb json text integer bigint numeric boolean timestamptz date uuid`
  .split(/\s+/);

/** The identifier (with an optional `qualifier.`) ending at `offset` (UTF-16, as the editor reports). */
export function wordAt(text, offset) {
  const before = text.slice(0, offset);
  const m = /([A-Za-z_][\w$]*\.)?([A-Za-z_][\w$]*)?$/.exec(before);
  const qualifier = m?.[1] ? m[1].slice(0, -1) : null;
  const word = m?.[2] ?? "";
  return { word, qualifier, start: offset - word.length };
}

function inQuoteOrComment(text, offset) {
  let quote = null;
  for (let i = 0; i < offset; i++) {
    const c = text[i];
    if (quote === "-") { if (c === "\n") quote = null; continue; }
    if (quote) { if (c === quote) quote = null; continue; }
    if (c === "'" || c === '"' || c === "`") quote = c;
    else if (c === "-" && text[i + 1] === "-") quote = "-";
  }
  return quote !== null;
}

/** Tables named after FROM/JOIN/UPDATE/INTO, with their aliases. */
function referenced(text, tables) {
  const out = new Map();
  const re = /\b(?:from|join|update|into)\s+((?:"[^"]+"|`[^`]+`|[\w$]+)(?:\.(?:"[^"]+"|`[^`]+`|[\w$]+))?)(?:\s+(?:as\s+)?([A-Za-z_][\w$]*))?/gi;
  for (const m of text.matchAll(re)) {
    const parts = m[1].replace(/["`]/g, "").split(".");
    const name = parts.pop(), schema = parts.pop();
    const t = tables.find((x) => x.name === name && (!schema || x.schema === schema));
    if (!t) continue;
    out.set(name.toLowerCase(), t);
    const alias = m[2]?.toLowerCase();
    if (alias && !KEYWORDS.includes(alias)) out.set(alias, t);
  }
  return out;
}

/** How well `label` matches `w` (lowercase): 0 prefix, 1 a word part's prefix, 2 inside, 3 in order; null none. */
export function matchTier(label, w) {
  const l = label.toLowerCase();
  if (!w) return 0;
  if (l.startsWith(w)) return 0;
  if (label.split(/_|(?=[A-Z])/).some((p) => p.toLowerCase().startsWith(w))) return 1;
  if (l.includes(w)) return 2;
  if (w.length < 2) return null;
  let i = 0;
  for (const c of l) if (c === w[i]) i++;
  return i === w.length ? 3 : null;
}

const TABLE_CONTEXT = /\b(from|join|into|update|table)\s+(?:[\w$"`]+\.)?[\w$]*$/i;

/**
 * Completions for the word at `offset`, best first: prefix matches, then a word part's, then
 * anywhere, then letters in order. `explicit` (Ctrl+Space) lists candidates with nothing typed.
 * After FROM/JOIN/INTO/UPDATE tables come first, elsewhere the columns of the tables in the query.
 */
export function complete(text, offset, catalog, { explicit = false, max = 200 } = {}) {
  const { word, qualifier, start } = wordAt(text, offset);
  if (inQuoteOrComment(text, offset)) return { start, items: [] };
  if (!word && !qualifier && !explicit) return { start, items: [] };
  const tables = catalog?.tables ?? [];
  const refs = referenced(text, tables);
  const w = word.toLowerCase();
  const wantTables = TABLE_CONTEXT.test(text.slice(0, offset));
  const rank = { column: wantTables ? 2 : 0, table: wantTables ? 0 : 1, view: wantTables ? 0 : 1, "materialized view": wantTables ? 0 : 1, schema: 2, keyword: 3 };
  const seen = new Set(), items = [];
  const add = (label, kind, detail) => {
    if (seen.has(label) || label.toLowerCase() === w) return;
    const tier = matchTier(label, w);
    if (tier === null) return;
    seen.add(label);
    items.push({ label, kind, detail, tier, order: items.length });
  };
  if (qualifier) {
    const q = qualifier.toLowerCase();
    const t = refs.get(q) ?? tables.find((x) => x.name.toLowerCase() === q);
    if (t) for (const c of t.columns) add(c.name, "column", c.type);
    for (const x of tables.filter((x) => x.schema.toLowerCase() === q)) add(x.name, x.kind ?? "table", `${x.rows} rows`);
  } else {
    for (const t of new Set(refs.values())) for (const c of t.columns) add(c.name, "column", `${t.name} · ${c.type}`);
    for (const t of tables) add(t.name, t.kind ?? "table", t.schema);
    for (const s of catalog?.schemas ?? []) add(s, "schema", "");
    if (w || !wantTables) for (const k of KEYWORDS) add(k, "keyword", "");
  }
  items.sort((a, b) => a.tier - b.tier || (rank[a.kind] ?? 1) - (rank[b.kind] ?? 1) || a.order - b.order);
  return { start, items: items.slice(0, max).map(({ label, kind, detail }) => ({ label, kind, detail })) };
}

/** Line and column (1-based) of a UTF-16 offset. */
export function lineCol(text, offset) {
  const before = text.slice(0, offset);
  const line = before.split("\n").length;
  return { line, col: offset - before.lastIndexOf("\n") };
}

export function quoteIdent(s) {
  return /^[a-z_][a-z0-9_$]*$/.test(s) ? s : `"${s.replaceAll('"', '""')}"`;
}

export function csv(columns, rows) {
  const cell = (v) => (v === null ? "" : /[",\n]/.test(v) ? `"${v.replaceAll('"', '""')}"` : v);
  return [columns.map((c) => cell(c.name)), ...rows.map((r) => r.map(cell))].map((r) => r.join(",")).join("\n");
}

export function json(columns, rows) {
  return JSON.stringify(rows.map((r) => Object.fromEntries(columns.map((c, i) => [c.name, r[i]]))), null, 2);
}

// Types with no `=` in Postgres: their old value can't be checked before an update.
const NO_EQUALITY = /^(json|xml|point|line|lseg|box|path|polygon|circle)(\[\])?$/;

/** A Postgres string literal that means the same whatever `standard_conforming_strings` says. */
export function literal(v) {
  if (v === null) return "NULL";
  const quoted = v.replaceAll("'", "''");
  return v.includes("\\") ? `E'${quoted.replaceAll("\\", "\\\\")}'` : `'${quoted}'`;
}

/**
 * The statements for a table view's pending changes: one UPDATE per edited row (only if its
 * edited columns still hold what was loaded) and one DELETE per row marked for deletion,
 * each found by its primary key. `pending`: Map "row:col" → value; `deleted`: Set of rows.
 */
export function changeSql(rel, set, pk, pending, deleted) {
  const cols = set.columns;
  const pkIdx = pk.map((n) => cols.findIndex((c) => c.name === n));
  if (!pk.length || pkIdx.includes(-1)) throw new Error("editing needs the table's primary key in the result");
  const where = (r) => pkIdx.map((j, k) => `${quoteIdent(pk[k])} = ${literal(set.rows[r][j])}`);
  const byRow = new Map();
  for (const [key, v] of pending) {
    const [r, c] = key.split(":").map(Number);
    if (!byRow.has(r)) byRow.set(r, []);
    byRow.get(r).push([c, v]);
  }
  const out = [];
  for (const [r, cells] of [...byRow].sort((a, b) => a[0] - b[0])) {
    if (deleted.has(r)) continue;
    const assign = cells.map(([c, v]) => `${quoteIdent(cols[c].name)} = ${literal(v)}`).join(", ");
    const unchanged = cells.filter(([c]) => !NO_EQUALITY.test(cols[c].type ?? ""))
      .map(([c]) => `${quoteIdent(cols[c].name)} IS NOT DISTINCT FROM ${literal(set.rows[r][c])}`);
    out.push({ row: r, sql: `UPDATE ${rel} SET ${assign} WHERE ${[...where(r), ...unchanged].join(" AND ")}` });
  }
  for (const r of [...deleted].sort((a, b) => a - b)) out.push({ row: r, sql: `DELETE FROM ${rel} WHERE ${where(r).join(" AND ")}` });
  return out;
}
