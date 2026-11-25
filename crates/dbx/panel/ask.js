// Asking an agent (Claude Code, Codex… in a pane of this space): you type the question, the panel
// adds what the agent needs (connection and stage, the SQL, the error, the tables involved, how to
// query on its own with `dbx query`) and sends it with `hc send`, then shows the agent.
// Row values only go along from local/dev/test connections.
import { div } from "gpui-kit";
import { h_flex, Input } from "gpui-base";
import { agents, hc, run } from "herdr";
import { statementAt } from "./sql.js";

const SAFE = ["local", "dev", "test"];

/** The agents worth asking, this space's first, idle ones before busy ones. */
function candidates(view) {
  const all = agents();
  const here = all.filter((a) => a.space === view.spaceId);
  const rank = (a) => (a.status === "idle" || a.status === "done" ? 0 : 1) * 10 + (a.focused ? 0 : 1);
  return (here.length ? here : all).slice().sort((a, b) => rank(a) - rank(b) || b.seq - a.seq);
}

/** `about`: "statement", "error", "result", "table". */
export function openAsk(view, tab, about, cx) {
  const list = candidates(view);
  if (!list.length) { view.flash("No agent in this space: start one (Claude Code, Codex…) in a pane", cx); return; }
  tab.ask = { about, agents: list, at: 0, sending: false };
  const src = view.source(tab.conn);
  if (!src.catalog && !src.loading) view.refreshCatalog(tab.conn, cx);
  view.askInput.set_value("");
  view.askInput.focus();
  cx.notify();
}

export function closeAsk(view, tab, cx) {
  tab.ask = null;
  cx.notify();
}

const fence = (s) => "```sql\n" + s.trim() + "\n```";

function tablesIn(view, tab, sql) {
  const tables = view.source(tab.conn).catalog?.tables ?? [];
  const named = tables.filter((x) => new RegExp(`\\b${x.name}\\b`, "i").test(sql));
  if (tab.kind === "table" && !named.some((x) => x.name === tab.table)) {
    const own = tables.find((x) => x.schema === tab.schema && x.name === tab.table);
    if (own) named.unshift(own);
  }
  return named.slice(0, 8).map((x) => `- ${x.schema}.${x.name}: ${(x.columns ?? []).map((c) => `${c.name} ${c.type}`).join(", ")}`);
}

/** The question, then what the agent needs to answer it. */
export function context(view, tab, question) {
  const c = view.conn(tab.conn) ?? {}, about = tab.ask.about;
  const stage = c.stage ?? "?", readonly = tab.session?.readonly ?? view.source(tab.conn).session?.readonly ?? c.readonly;
  const set = tab.results?.[tab.picked ?? 0];
  let sql = "";
  if (tab.kind === "console" && tab.editor) {
    const sel = tab.editor.selected_range(), text = tab.editor.value();
    sql = sel.start !== sel.end ? text.slice(Math.min(sel.start, sel.end), Math.max(sel.start, sel.end)) : statementAt(text, sel.start).text;
  } else if (tab.kind === "table") {
    sql = `select * from ${tab.schema}.${tab.table}${tab.where ? ` where ${tab.where}` : ""}${tab.order ? ` order by ${tab.order}` : ""}`;
  }
  if (tab.step) sql = tab.step.sql;
  const parts = [question.trim() || "Help me with this.", "",
    `Context from the dbx panel: connection \`${tab.conn}\` (${c.kind ?? "postgres"}, stage ${stage}${readonly ? ", read-only" : ""}).`];
  if (sql) parts.push("", "SQL:", fence(sql));
  if (tab.error?.message && (about === "error" || about === "statement")) {
    parts.push("", `Error: ${tab.error.message}${tab.error.detail ? `\nDetail: ${tab.error.detail}` : ""}${tab.error.hint ? `\nHint: ${tab.error.hint}` : ""}`);
  }
  if (set?.columns.length && about !== "error") {
    parts.push("", `Result: ${set.rows.length}${set.more ? "+" : ""} rows; columns ${set.columns.map((x) => `${x.name} ${x.type ?? ""}`.trim()).join(", ")}.`);
    if (SAFE.includes(stage)) {
      const rows = set.rows.slice(0, 10).map((r) => r.map((v) => (v === null ? "null" : v.length > 80 ? v.slice(0, 80) + "…" : v)).join(" | "));
      parts.push("First rows:", "```", ...rows, "```");
    } else parts.push(`(Row values left out: ${stage} data. Query it yourself if you need them.)`);
  }
  const tables = tablesIn(view, tab, sql);
  if (tables.length) parts.push("", "Tables:", ...tables);
  parts.push("", `You can run queries yourself: \`dbx query ${tab.conn} "select …"\` (add --json; read-only rules apply; \`dbx --skill\` for the rest).`);
  return parts.join("\n");
}

export function sendAsk(view, tab, cx) {
  const a = tab.ask;
  if (!a || a.sending) return;
  const agent = a.agents[a.at], text = context(view, tab, view.askInput.value());
  a.sending = true;
  cx.spawn(async (cx) => {
    try {
      const r = await hc(["send", agent.address, text]);
      if (r.code !== 0) throw new Error(r.stderr.trim() || `hc send exited with ${r.code}`);
      tab.ask = null;
      run("agent.focus", { terminal: agent.address });
      view.flash(`Sent to ${agent.app ?? agent.title} (${agent.address})`, cx);
    } catch (e) { a.sending = false; view.flash(String(e.message ?? e), cx); }
    cx.notify();
  });
  cx.notify();
}

export function askBar(view, tab, t) {
  const a = tab.ask, agent = a.agents[a.at];
  const name = `${agent.app ?? agent.title ?? "agent"} · ${agent.tab_name ?? agent.address}`;
  const what = { statement: "about this statement", error: "about this error", result: "about these rows", table: `about ${tab.table ?? "this table"}` }[a.about];
  return h_flex().h(40).flex_none().px(10).gap(8).items_center().bg(t.raised).border_b(1).border_color(t.border).key_context("DbxAsk")
    .child(div().text_size(12).text_color(t.muted).child("Ask"))
    .child(div().id(`dbx-ask-agent-${tab.id}`).px(8).py(2).rounded(5).text_size(12).text_color(t.accent).cursor_pointer().hover((s) => s.bg(t.hover))
      .on_click((_e, cx) => { a.at = (a.at + 1) % a.agents.length; cx.notify(); })
      .child(a.agents.length > 1 ? `${name} ▾` : name))
    .child(div().text_size(12).text_color(t.muted).child(what))
    .child(h_flex().h(28).flex_1().min_w(120).px(8).items_center().rounded(6).border_1().border_color(t.accent).bg(t.background)
      .child(Input.new(view.askInput).flex_1().h_full().text_size(12)))
    .child(div().text_size(11).text_color(t.dim).child(a.sending ? "sending…" : "Enter · Esc"));
}
