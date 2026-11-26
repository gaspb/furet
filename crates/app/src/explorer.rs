//! The window: connections → schemas → tables on the left, a SQL editor over the results on the
//! right. Cmd/Ctrl+Enter runs the statement under the cursor, with Shift everything; a table in
//! the tree shows its first rows. Foreign keys lead to the rows they point at, and the inspector
//! to the rows pointing back: each step goes on a trail, and Esc goes back.

use crate::engine::{Client, Engine};
use crate::results::{self, Ref, Results, Set};
use gpui_kit::base::actions::Cancel;
use gpui_kit::base::{TreeEvent, TreeItem, TreeState};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::tree::tree;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

actions!(furet_app, [RunStatement, RunAll, Follow, ToggleInspector]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-enter", RunStatement, Some("FuretConsole")),
        KeyBinding::new("ctrl-enter", RunStatement, Some("FuretConsole")),
        KeyBinding::new("cmd-shift-enter", RunAll, Some("FuretConsole")),
        KeyBinding::new("ctrl-shift-enter", RunAll, Some("FuretConsole")),
        KeyBinding::new("cmd-enter", Follow, Some("FuretResults")),
        KeyBinding::new("ctrl-enter", Follow, Some("FuretResults")),
        KeyBinding::new("space", ToggleInspector, Some("FuretResults")),
    ]);
}

enum Referrers {
    Loading,
    Found(Vec<Value>),
    Failed(String),
}

/// What the results showed before a step: put back as it was by Esc or a crumb.
struct Step {
    set: Set,
    summaries: HashMap<usize, HashMap<String, Option<String>>>,
    here: Option<(Ref, String)>,
    sel: Option<(usize, usize)>,
}

struct Conn {
    name: String,
    stage: String,
    kind: String,
    endpoint: String,
    prod: bool,
}

/// How the last run went, for the bar above the results.
enum Outcome {
    Rows {
        rows: usize,
        more: bool,
        ms: u64,
    },
    Affected {
        rows: u64,
        ms: u64,
    },
    Failed {
        message: String,
        detail: Option<String>,
        ms: u64,
    },
}

pub struct Explorer {
    client: Option<Client>,
    cwd: String,
    conns: Vec<Conn>,
    sessions: HashMap<String, String>,
    catalogs: HashMap<String, Value>,
    expanded: HashSet<String>,
    current: Option<String>,
    tree: Entity<TreeState>,
    editor: Entity<EditorState>,
    results: Entity<TableState<Results>>,
    status: SharedString,
    error: Option<SharedString>,
    busy: bool,
    outcome: Option<Outcome>,
    /// Whether each open connection is read-only, as the engine last said.
    modes: HashMap<String, bool>,
    /// Typing a prod connection's name to allow writes.
    confirm: Option<Entity<InputState>>,
    /// The connection the shown results came from.
    shown: Option<String>,
    trail: Vec<Step>,
    /// Where the shown rows were found, when they're a step away from the query's own.
    here: Option<(Ref, String)>,
    sel: Option<(usize, usize)>,
    inspector: bool,
    referrers: HashMap<usize, Referrers>,
    /// Bumped whenever the results change, so late answers for older ones are dropped.
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl Explorer {
    pub fn new(cwd: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
        });
        editor.update(cx, |e, cx| e.set_value("select 1;", window, cx));
        let weak = cx.weak_entity();
        let follow: results::Follow = Rc::new(move |row, col, window, cx| {
            let _ = weak.update(cx, |this, cx| this.follow(row, col, window, cx));
        });
        let delegate = Results {
            on_follow: Some(follow),
            ..Results::default()
        };
        let results = cx.new(|cx| TableState::new(delegate, window, cx).cell_selectable(true));
        let picked = cx.subscribe_in(
            &results,
            window,
            |this, _, event: &TableEvent, window, cx| match *event {
                TableEvent::SelectCell(row, col) => {
                    this.select(row, col.saturating_sub(results::NUMBERS), window, cx)
                }
                TableEvent::SelectRow(row) => {
                    let col = this.sel.map_or(0, |(_, c)| c);
                    this.select(row, col, window, cx)
                }
                TableEvent::DoubleClickedCell(row, col) if col >= results::NUMBERS => {
                    this.follow(row, col - results::NUMBERS, window, cx)
                }
                _ => {}
            },
        );
        let subscription = cx.subscribe_in(
            &tree,
            window,
            |this, _, event: &TreeEvent, window, cx| match event {
                TreeEvent::Expanded(id) => {
                    this.expanded.insert(id.to_string());
                    if let Some(name) = id.strip_prefix("c:") {
                        this.connect(name.to_string(), window, cx);
                    }
                }
                TreeEvent::Collapsed(id) => {
                    this.expanded.remove(id.as_ref());
                }
            },
        );
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            crate::look::follow_system(window, cx)
        });
        let mut this = Self {
            client: None,
            cwd,
            conns: Vec::new(),
            sessions: HashMap::new(),
            catalogs: HashMap::new(),
            expanded: HashSet::new(),
            current: None,
            tree,
            editor,
            results,
            status: "Starting the furet engine…".into(),
            error: None,
            busy: false,
            outcome: None,
            modes: HashMap::new(),
            confirm: None,
            shown: None,
            trail: Vec::new(),
            here: None,
            sel: None,
            inspector: true,
            referrers: HashMap::new(),
            generation: 0,
            _subscriptions: vec![subscription, picked, appearance],
        };
        this.load(window, cx);
        this
    }

    /// Runs `work` off the UI thread with the client, then `done` back on it.
    fn background<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Client) -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, anyhow::Result<T>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { work(&client) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                done(this, result, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let client = Client {
                        engine: Engine::ensure()?,
                        cwd,
                    };
                    let conns = client.connections()?;
                    anyhow::Ok((client, conns))
                })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                match found {
                    Ok((client, conns)) => {
                        this.client = Some(client);
                        this.conns = conns["connections"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|c| Conn {
                                name: c["name"].as_str().unwrap_or("").into(),
                                stage: c["stage"].as_str().unwrap_or("").into(),
                                kind: c["kind"].as_str().unwrap_or("postgres").into(),
                                endpoint: c["endpoint"].as_str().unwrap_or("").into(),
                                prod: matches!(c["stage"].as_str(), Some("prod" | "production")),
                            })
                            .collect();
                        this.status = format!("{} connections", this.conns.len()).into();
                        this.rebuild_tree(cx);
                    }
                    Err(e) => this.error = Some(format!("{e:#}").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        let items: Vec<TreeItem> = self
            .conns
            .iter()
            .map(|c| {
                let id = format!("c:{}", c.name);
                let mut item =
                    TreeItem::new(id.clone(), c.name.clone()).expanded(self.expanded.contains(&id));
                match self.catalogs.get(&c.name) {
                    Some(cat) => {
                        let tables = cat["tables"].as_array().cloned().unwrap_or_default();
                        for s in cat["schemas"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|s| s.as_str())
                        {
                            let sid = format!("s:{}:{s}", c.name);
                            let children: Vec<TreeItem> = tables
                                .iter()
                                .filter(|t| t["schema"].as_str() == Some(s))
                                .map(|t| {
                                    let name = t["name"].as_str().unwrap_or("");
                                    TreeItem::new(
                                        format!("t:{}:{s}:{name}", c.name),
                                        name.to_string(),
                                    )
                                })
                                .collect();
                            if children.is_empty() {
                                continue;
                            }
                            item = item.child(
                                TreeItem::new(sid.clone(), s.to_string())
                                    .expanded(self.expanded.contains(&sid))
                                    .children(children),
                            );
                        }
                    }
                    // A placeholder child, so the connection can be opened (and then load).
                    None => item = item.child(TreeItem::new(format!("l:{}", c.name), "…")),
                }
                item
            })
            .collect();
        self.tree.update(cx, |t, cx| t.set_items(items, cx));
    }

    fn connect(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.current = Some(name.clone());
        if self.catalogs.contains_key(&name) {
            return;
        }
        let known = self.sessions.get(&name).cloned();
        self.status = format!("Connecting to {name}…").into();
        self.background(
            window,
            cx,
            move |c| {
                let (id, readonly) = match known {
                    Some(id) => (id, None),
                    None => {
                        let s = c.open(&name)?;
                        (
                            s["id"].as_str().unwrap_or("").to_string(),
                            s["readonly"].as_bool(),
                        )
                    }
                };
                let cat = c.catalog(&id)?;
                Ok((name, id, cat, readonly))
            },
            |this, r, _, cx| match r {
                Ok((name, id, cat, readonly)) => {
                    if let Some(ro) = readonly {
                        this.modes.insert(name.clone(), ro);
                    }
                    this.status = format!(
                        "{name} · {} tables",
                        cat["tables"].as_array().map_or(0, Vec::len)
                    )
                    .into();
                    this.sessions.insert(name.clone(), id);
                    this.catalogs.insert(name, cat);
                    this.error = None;
                    this.rebuild_tree(cx);
                }
                Err(e) => this.error = Some(format!("{e:#}").into()),
            },
        );
    }

    fn clicked(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let parts: Vec<&str> = id.splitn(4, ':').collect();
        match parts.as_slice() {
            ["c", name] | ["s", name, ..] => self.current = Some(name.to_string()),
            ["t", name, schema, table] => {
                self.current = Some(name.to_string());
                let sql = format!("select * from \"{schema}\".\"{table}\" limit 200;");
                self.editor.update(cx, |e, cx| e.set_value(sql, window, cx));
                self.run(false, window, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    fn run(&mut self, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.current.clone() else {
            self.error = Some("Pick a connection in the tree first".into());
            cx.notify();
            return;
        };
        if self.busy {
            return;
        }
        let (sql, cursor) = {
            let e = self.editor.read(cx);
            let text = e.value().to_string();
            let at = e.cursor().min(text.len());
            let utf16 = text[..at].encode_utf16().count();
            (text, utf16)
        };
        let known = self.sessions.get(&name).cloned();
        self.busy = true;
        self.status = format!("Running on {name}…").into();
        self.background(
            window,
            cx,
            move |c| {
                let id = match known {
                    Some(id) => id,
                    None => c.open(&name)?["id"].as_str().unwrap_or("").to_string(),
                };
                Ok((name, id.clone(), c.query(&id, &sql, cursor, all)?))
            },
            |this, r, window, cx| {
                this.busy = false;
                match r {
                    Ok((name, id, out)) => {
                        this.sessions.insert(name.clone(), id);
                        if let Some(ro) = out["readonly"].as_bool() {
                            this.modes.insert(name.clone(), ro);
                        }
                        let sets: Vec<Set> =
                            serde_json::from_value(out["results"].clone()).unwrap_or_default();
                        let ms = out["elapsed_ms"].as_u64().unwrap_or(0);
                        this.error = None;
                        this.status = format!("Ran on {name}").into();
                        let failed = out["error"]["message"].as_str().map(|m| Outcome::Failed {
                            message: m.to_string(),
                            detail: ["detail", "hint"]
                                .iter()
                                .filter_map(|k| out["error"][k].as_str())
                                .map(String::from)
                                .reduce(|a, b| format!("{a}\n{b}")),
                            ms,
                        });
                        this.outcome = failed;
                        if let Some(set) = sets.into_iter().last() {
                            if this.outcome.is_none() {
                                this.outcome = Some(match set.affected {
                                    Some(rows) if set.columns.is_empty() => {
                                        Outcome::Affected { rows, ms }
                                    }
                                    _ => Outcome::Rows {
                                        rows: set.rows.len(),
                                        more: set.more,
                                        ms,
                                    },
                                });
                            }
                            this.trail.clear();
                            this.here = None;
                            this.shown = Some(name);
                            this.show(set, HashMap::new(), None, window, cx);
                        }
                    }
                    Err(e) => this.error = Some(format!("{e:#}").into()),
                }
            },
        );
    }

    fn conn(&self, name: &str) -> Option<&Conn> {
        self.conns.iter().find(|c| c.name == name)
    }

    fn session(&self) -> Option<String> {
        self.sessions.get(self.shown.as_ref()?).cloned()
    }

    /// Puts a result set on screen, then looks up what its foreign keys point at.
    fn show(
        &mut self,
        set: Set,
        summaries: HashMap<usize, HashMap<String, Option<String>>>,
        sel: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        self.referrers.clear();
        self.sel = sel;
        self.results.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.set = set;
            d.summaries = summaries;
            t.refresh(cx);
            if let Some((row, col)) = sel {
                t.set_selected_cell(row, col + results::NUMBERS, cx);
            }
        });
        self.prefetch(window, cx);
        if let Some((row, _)) = sel {
            self.load_referrers(row, window, cx);
        }
        cx.notify();
    }

    fn prefetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        let d = self.results.read(cx).delegate();
        let wanted: Vec<(usize, Ref, Vec<String>)> = d
            .set
            .columns
            .iter()
            .enumerate()
            .filter(|(i, _)| !d.summaries.contains_key(i))
            .filter_map(|(i, c)| {
                let mut values: Vec<String> =
                    d.set.rows.iter().filter_map(|r| r[i].clone()).collect();
                values.sort();
                values.dedup();
                Some((i, c.refs.clone()?, values)).filter(|(_, _, v)| !v.is_empty())
            })
            .collect();
        let generation = self.generation;
        for (col, target, values) in wanted {
            let session = session.clone();
            self.background(
                window,
                cx,
                move |c| {
                    let got: Set = serde_json::from_value(c.lookup(
                        &session,
                        &serde_json::to_value(&target)?,
                        None,
                        &values,
                    )?)?;
                    let at = got.columns.iter().position(|x| x.name == target.column);
                    let mut found: HashMap<String, Option<String>> =
                        values.into_iter().map(|v| (v, None)).collect();
                    for r in &got.rows {
                        if let Some(Some(v)) = at.map(|i| &r[i]) {
                            let words = results::summarize(&got.columns, r);
                            let words = if words.is_empty() {
                                format!("{} #{v}", target.table)
                            } else {
                                words
                            };
                            found.insert(v.clone(), Some(words));
                        }
                    }
                    Ok(found)
                },
                move |this, r, _, cx| {
                    if let (Ok(found), true) = (r, this.generation == generation) {
                        this.results.update(cx, |t, cx| {
                            t.delegate_mut().summaries.insert(col, found);
                            t.refresh(cx);
                        });
                    }
                },
            );
        }
    }

    fn select(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.sel = Some((row, col));
        self.load_referrers(row, window, cx);
        cx.notify();
    }

    /// Who points at a row, once per row: for each table its columns come from.
    fn load_referrers(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.inspector || self.referrers.contains_key(&row) {
            return;
        }
        let Some(session) = self.session() else {
            return;
        };
        let set = &self.results.read(cx).delegate().set;
        let Some(values) = set.rows.get(row) else {
            return;
        };
        let mut tables: Vec<(String, String, serde_json::Map<String, Value>)> = Vec::new();
        for (c, v) in set.columns.iter().zip(values) {
            let Some(o) = &c.origin else { continue };
            let at = match tables
                .iter()
                .position(|(s, t, _)| *s == o.schema && *t == o.table)
            {
                Some(at) => at,
                None => {
                    tables.push((o.schema.clone(), o.table.clone(), Default::default()));
                    tables.len() - 1
                }
            };
            tables[at].2.insert(o.column.clone(), serde_json::json!(v));
        }
        self.referrers.insert(row, Referrers::Loading);
        let generation = self.generation;
        self.background(
            window,
            cx,
            move |c| {
                let mut found = Vec::new();
                for (schema, table, values) in tables {
                    let out = c.referrers(&session, &schema, &table, Value::Object(values))?;
                    found.extend(out["referrers"].as_array().cloned().unwrap_or_default());
                }
                Ok(found)
            },
            move |this, r, _, _| {
                if this.generation == generation {
                    let state = match r {
                        Ok(found) => Referrers::Found(found),
                        Err(e) => Referrers::Failed(format!("{e:#}")),
                    };
                    this.referrers.insert(row, state);
                }
            },
        );
    }

    fn follow(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let set = &self.results.read(cx).delegate().set;
        let target = set.columns.get(col).and_then(|c| c.refs.clone());
        let value = set.rows.get(row).and_then(|r| r[col].clone());
        match (target, value) {
            (Some(target), Some(value)) => self.go(target, value, window, cx),
            (Some(_), None) => self.say("null points at nothing", cx),
            _ => {}
        }
    }

    /// One step on: the rows of `target.table` whose `target.column` is `value`.
    fn go(&mut self, target: Ref, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        let label = results::step_label(&target.table, &target.column, &value);
        self.status = format!("Opening {label}…").into();
        self.background(
            window,
            cx,
            move |c| {
                let got = c.lookup(&session, &serde_json::to_value(&target)?, Some(&value), &[])?;
                Ok((serde_json::from_value::<Set>(got)?, target, value))
            },
            move |this, r, window, cx| match r {
                Ok((set, target, value)) => {
                    let col = set
                        .columns
                        .iter()
                        .position(|c| c.name == target.column)
                        .unwrap_or(0);
                    let sel = (!set.rows.is_empty()).then_some((0, col));
                    this.status = format!(
                        "{label} · {} row{}",
                        set.rows.len(),
                        if set.rows.len() == 1 { "" } else { "s" }
                    )
                    .into();
                    let d = this.results.read(cx).delegate();
                    this.trail.push(Step {
                        set: d.set.clone(),
                        summaries: d.summaries.clone(),
                        here: this.here.take(),
                        sel: this.sel,
                    });
                    this.here = Some((target, value));
                    this.error = None;
                    this.show(set, HashMap::new(), sel, window, cx);
                    this.focus_results(window, cx);
                }
                Err(e) => this.error = Some(format!("{e:#}").into()),
            },
        );
    }

    /// Back one step, or to step `to` of the trail.
    fn back(&mut self, to: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let at = match to {
            Some(at) if at < self.trail.len() => at,
            None if !self.trail.is_empty() => self.trail.len() - 1,
            _ => return,
        };
        let step = self.trail.drain(at..).next().unwrap();
        self.here = step.here;
        self.status = self
            .label()
            .unwrap_or_else(|| "Back to the results".into())
            .into();
        self.show(step.set, step.summaries, step.sel, window, cx);
        self.focus_results(window, cx);
    }

    fn focus_results(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.results.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn label(&self) -> Option<String> {
        let (r, v) = self.here.as_ref()?;
        Some(results::step_label(&r.table, &r.column, v))
    }

    /// The current step as a query in the editor, to take it further by hand.
    fn as_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((r, v)) = &self.here else { return };
        let sql = format!(
            "select * from \"{}\".\"{}\" where \"{}\" = '{}';",
            r.schema,
            r.table,
            r.column,
            v.replace('\'', "''")
        );
        self.editor.update(cx, |e, cx| e.set_value(sql, window, cx));
    }

    fn say(&mut self, text: &str, cx: &mut Context<Self>) {
        self.status = text.to_string().into();
        cx.notify();
    }
}

impl Explorer {
    /// The results, with the trail above them and the inspector beside them.
    fn results_area(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        if self.results.read(cx).delegate().set.columns.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(icon("furet/table.svg", theme.muted_foreground).size(px(28.)))
                .child(match &self.outcome {
                    Some(Outcome::Failed { .. }) => "Nothing to show",
                    Some(Outcome::Affected { .. }) => "The statement returned no rows",
                    _ => "Pick a table on the left, or write a query and press ⌘/Ctrl+↵",
                })
                .into_any_element();
        }
        let table = v_flex()
            .size_full()
            .when(!self.trail.is_empty(), |e| e.child(self.trail_bar(cx)))
            .child(
                div().flex_1().min_h_0().child(
                    DataTable::new(&self.results)
                        .with_size(gpui_kit::component::Size::Size(px(24.)))
                        .header_row_height(px(38.)),
                ),
            );
        let body = match self.sel.filter(|_| self.inspector) {
            Some((row, _)) => h_resizable("furet-results")
                .child(resizable_panel().child(table))
                .child(
                    resizable_panel()
                        .size(px(340.))
                        .child(self.inspector(row, cx)),
                )
                .into_any_element(),
            None => table.into_any_element(),
        };
        div()
            .size_full()
            .key_context("FuretResults")
            .bg(theme.background)
            .on_action(cx.listener(|this, _: &Follow, window, cx| {
                if let Some((row, col)) = this.sel {
                    this.follow(row, col, window, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &Cancel, window, cx| {
                if this.trail.is_empty() {
                    cx.propagate();
                } else {
                    this.back(None, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, window, cx| {
                this.inspector = !this.inspector;
                if let Some((row, _)) = this.sel {
                    this.load_referrers(row, window, cx);
                }
                cx.notify();
            }))
            .child(body)
            .into_any_element()
    }

    fn trail_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let crumb = |id: SharedString, text: String, current: bool| {
            div()
                .id(id)
                .px_1p5()
                .rounded_sm()
                .flex_none()
                .text_xs()
                .text_color(if current {
                    theme.foreground
                } else {
                    theme.blue
                })
                .when(current, |e| e.font_weight(FontWeight::SEMIBOLD))
                .when(!current, |e| {
                    e.cursor_pointer().hover(|s| s.bg(theme.muted))
                })
                .child(text)
        };
        let mut labels: Vec<String> = self
            .trail
            .iter()
            .skip(1)
            .filter_map(|s| s.here.as_ref())
            .map(|(r, v)| results::step_label(&r.table, &r.column, v))
            .collect();
        labels.extend(self.label());
        let mut bar = h_flex()
            .h(px(30.))
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                crumb("furet-back".into(), "← Back".into(), false)
                    .on_click(cx.listener(|this, _, window, cx| this.back(None, window, cx))),
            )
            .child(div().w(px(8.)))
            .child(
                crumb("furet-crumb-0".into(), "query".into(), false)
                    .on_click(cx.listener(|this, _, window, cx| this.back(Some(0), window, cx))),
            );
        let last = labels.len().saturating_sub(1);
        for (i, label) in labels.into_iter().enumerate() {
            bar = bar
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("›"),
                )
                .child(
                    crumb(format!("furet-crumb-{}", i + 1).into(), label, i == last).when(
                        i != last,
                        |e| {
                            e.on_click(cx.listener(move |this, _, window, cx| {
                                this.back(Some(i + 1), window, cx)
                            }))
                        },
                    ),
                );
        }
        bar.child(div().flex_1()).child(
            crumb("furet-as-sql".into(), "As SQL".into(), false)
                .on_click(cx.listener(|this, _, window, cx| this.as_sql(window, cx))),
        )
    }

    /// The selected row: its fields, where its keys point, and what points at it.
    fn inspector(&self, row: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let d = self.results.read(cx).delegate();
        let set = &d.set;
        let table = set
            .columns
            .iter()
            .find_map(|c| c.origin.as_ref().map(|o| o.table.clone()))
            .unwrap_or_else(|| "row".into());
        let mut list = v_flex()
            .id("furet-inspector")
            .size_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{table} · row {}", row + 1)),
            );
        let Some(values) = set.rows.get(row) else {
            return list;
        };
        for (i, (c, v)) in set.columns.iter().zip(values).enumerate() {
            let link = c.refs.clone().zip(v.clone()).map(|(r, v)| {
                let about = d.summary(row, i);
                let text = [
                    Some(results::arrow(&r)),
                    about,
                    r.guessed.then(|| "guessed from the name".to_string()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                (r, v, text)
            });
            list = list.child(
                v_flex()
                    .px_3()
                    .py_1()
                    .child(
                        h_flex()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(div().flex_1().child(c.name.clone()))
                            .child(c.ty.clone().unwrap_or_default()),
                    )
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_size(theme.mono_font_size)
                            .text_color(if v.is_none() {
                                theme.muted_foreground
                            } else {
                                theme.foreground
                            })
                            .when(v.is_none(), |e| e.italic())
                            .child(v.clone().unwrap_or_else(|| "null".into())),
                    )
                    .when_some(link, |e, (target, value, text)| {
                        e.child(
                            div()
                                .id(("furet-insp-go", i))
                                .text_xs()
                                .text_color(theme.blue)
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.go(target.clone(), value.clone(), window, cx)
                                }))
                                .child(text),
                        )
                    }),
            );
        }
        list = list.child(
            div()
                .px_3()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child("REFERENCED BY"),
        );
        match self.referrers.get(&row) {
            None | Some(Referrers::Loading) => {
                list = list.child(
                    div()
                        .px_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("…"),
                )
            }
            Some(Referrers::Failed(e)) => {
                list = list.child(
                    div()
                        .px_3()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(e.clone()),
                )
            }
            Some(Referrers::Found(found)) if found.is_empty() => {
                list = list.child(
                    div()
                        .px_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Nothing points at this row"),
                )
            }
            Some(Referrers::Found(found)) => {
                for (i, x) in found.iter().enumerate() {
                    let count = x["count"].as_u64().unwrap_or(0);
                    let target = Ref {
                        schema: x["schema"].as_str().unwrap_or("").into(),
                        table: x["table"].as_str().unwrap_or("").into(),
                        column: x["column"].as_str().unwrap_or("").into(),
                        guessed: false,
                    };
                    let value = x["value"].as_str().unwrap_or("").to_string();
                    let name = format!(
                        "{}.{}{}",
                        target.table,
                        target.column,
                        if x["guessed"].as_bool() == Some(true) {
                            " (by name)"
                        } else {
                            ""
                        }
                    );
                    let more = if x["more"].as_bool() == Some(true) {
                        "+"
                    } else {
                        ""
                    };
                    let rows = match count {
                        0 => "none".to_string(),
                        1 => "1 row →".to_string(),
                        n => format!("{n}{more} rows →"),
                    };
                    list = list.child(
                        h_flex()
                            .id(("furet-insp-ref", i))
                            .px_3()
                            .py_0p5()
                            .text_xs()
                            .when(count > 0, |e| {
                                e.cursor_pointer().hover(|s| s.bg(theme.muted)).on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.go(target.clone(), value.clone(), window, cx)
                                    }),
                                )
                            })
                            .child(div().flex_1().truncate().child(name))
                            .child(
                                div()
                                    .text_color(if count > 0 {
                                        theme.blue
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(rows),
                            ),
                    );
                }
            }
        }
        list
    }
}

fn stage_color(stage: &str, cx: &App) -> Hsla {
    let t = cx.theme();
    match stage {
        "local" | "dev" | "test" => t.success,
        "prod" | "production" => t.danger,
        "stg" | "staging" | "preprod" => t.warning,
        _ => t.info,
    }
}

fn kind_icon(kind: &str) -> (&'static str, Hsla) {
    match kind {
        "clickhouse" => ("furet/clickhouse.svg", rgb(0xd4a20b).into()),
        _ => ("furet/postgres.svg", rgb(0x4169e1).into()),
    }
}

fn icon(path: &'static str, color: Hsla) -> Svg {
    svg().path(path).size(px(14.)).flex_none().text_color(color)
}

/// The stage, in its colour, as the herdr panel shows it.
fn pill(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .flex_none()
        .child(text.into())
}

fn rows_label(n: i64) -> String {
    match n {
        n if n < 0 => String::new(),
        n if n < 1000 => n.to_string(),
        n if n < 1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        n => format!("{:.1}M", n as f64 / 1e6),
    }
}

impl Explorer {
    fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.current.clone() else {
            return;
        };
        let readonly = self.modes.get(&name).copied().unwrap_or(true);
        let prod = self.conn(&name).is_some_and(|c| c.prod);
        if readonly && prod {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(format!("Type {name} to allow writes"))
            });
            let sub = cx.subscribe_in(
                &input,
                window,
                |this, input, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => {
                        let typed = input.read(cx).value().to_string();
                        if let Some(name) = this.current.clone() {
                            this.set_mode(name, true, Some(typed), window, cx);
                        }
                    }
                    InputEvent::Blur => {
                        this.confirm = None;
                        cx.notify();
                    }
                    _ => {}
                },
            );
            self._subscriptions.push(sub);
            input.update(cx, |i, cx| i.focus(window, cx));
            self.confirm = Some(input);
            cx.notify();
            return;
        }
        self.set_mode(name, readonly, None, window, cx);
    }

    fn set_mode(
        &mut self,
        name: String,
        write: bool,
        confirm: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let known = self.sessions.get(&name).cloned();
        self.background(
            window,
            cx,
            move |c| {
                let id = match known {
                    Some(id) => id,
                    None => c.open(&name)?["id"].as_str().unwrap_or("").to_string(),
                };
                let out = c.mode(&id, write, confirm.as_deref())?;
                Ok((name, id, out["readonly"].as_bool().unwrap_or(!write)))
            },
            |this, r, _, _| match r {
                Ok((name, id, readonly)) => {
                    this.confirm = None;
                    this.status = format!(
                        "{name} is {}",
                        if readonly { "read-only" } else { "read-write" }
                    )
                    .into();
                    this.sessions.insert(name.clone(), id);
                    this.modes.insert(name, readonly);
                }
                Err(e) => this.error = Some(format!("{e:#}").into()),
            },
        );
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let conns: HashMap<String, (String, String, String)> = self
            .conns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    (c.stage.clone(), c.kind.clone(), c.endpoint.clone()),
                )
            })
            .collect();
        let sizes: HashMap<String, i64> = self
            .catalogs
            .iter()
            .flat_map(|(conn, cat)| {
                cat["tables"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |t| {
                        (
                            format!(
                                "t:{conn}:{}:{}",
                                t["schema"].as_str().unwrap_or(""),
                                t["name"].as_str().unwrap_or("")
                            ),
                            t["rows"].as_i64().unwrap_or(-1),
                        )
                    })
            })
            .collect();
        let repo = std::path::Path::new(&self.cwd)
            .file_name()
            .map_or(self.cwd.clone(), |n| n.to_string_lossy().into_owned());
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .h(px(44.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(icon("furet/database.svg", theme.muted_foreground))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Databases"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(repo),
                    ),
            )
            .child(
                tree(&self.tree, move |ix, entry, selected, _, cx| {
                    let item = entry.item();
                    let id = item.id.to_string();
                    let t = cx.theme().clone();
                    let view = view.clone();
                    let depth = entry.depth();
                    let chevron = if !entry.is_folder() {
                        div().w(px(12.)).flex_none().into_any_element()
                    } else {
                        icon(
                            if entry.is_expanded() {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            },
                            t.muted_foreground,
                        )
                        .size(px(12.))
                        .into_any_element()
                    };
                    let row = h_flex().w_full().gap_2().items_center().child(chevron);
                    let row = match id.split(':').next() {
                        Some("c") => {
                            let (stage, kind, endpoint) =
                                conns.get(item.label.as_ref()).cloned().unwrap_or_default();
                            let (path, color) = kind_icon(&kind);
                            row.child(icon(path, color))
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .flex_none()
                                        .child(item.label.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(t.muted_foreground)
                                        .child(endpoint),
                                )
                                .when(!stage.is_empty(), |e| {
                                    e.child(pill(stage.clone(), stage_color(&stage, cx)))
                                })
                        }
                        Some("s") => row
                            .child(icon(
                                if entry.is_expanded() {
                                    "icons/folder-open.svg"
                                } else {
                                    "icons/folder.svg"
                                },
                                t.muted_foreground,
                            ))
                            .child(div().flex_1().truncate().child(item.label.clone())),
                        Some("t") => {
                            row.child(icon("furet/table.svg", t.muted_foreground))
                                .child(div().flex_1().truncate().child(item.label.clone()))
                                .child(div().text_xs().text_color(t.muted_foreground).child(
                                    sizes.get(&id).map_or(String::new(), |n| rows_label(*n)),
                                ))
                        }
                        _ => row.child(
                            div()
                                .text_xs()
                                .italic()
                                .text_color(t.muted_foreground)
                                .child("connecting…"),
                        ),
                    };
                    ListItem::new(ix)
                        .selected(selected)
                        .h(px(24.))
                        .pl(px(6. + depth as f32 * 14.))
                        .pr_2()
                        .child(row)
                        .on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.clicked(&id, window, cx));
                        })
                })
                .flex_1(),
            )
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let current = self.current.clone().unwrap_or_default();
        let conn = self.conn(&current);
        let readonly = self.modes.get(&current).copied();
        let button = |id: &'static str, label: &'static str, primary: bool| {
            h_flex()
                .id(id)
                .h(px(26.))
                .px_2p5()
                .gap_1p5()
                .items_center()
                .rounded(px(6.))
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(primary, |e| {
                    e.bg(theme.primary)
                        .text_color(theme.primary_foreground)
                        .hover(|s| s.bg(theme.primary_hover))
                })
                .when(!primary, |e| {
                    e.border_1()
                        .border_color(theme.border)
                        .hover(|s| s.bg(theme.secondary_hover))
                })
                .when(primary, |e| {
                    e.child(icon("furet/play.svg", theme.primary_foreground))
                })
                .child(label)
        };
        h_flex()
            .h(px(44.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .when_some(conn, |e, c| {
                let (path, color) = kind_icon(&c.kind);
                e.child(icon(path, color))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(c.name.clone()),
                    )
                    .when(!c.stage.is_empty(), |e| {
                        e.child(pill(c.stage.clone(), stage_color(&c.stage, cx)))
                    })
            })
            .when(conn.is_none(), |e| {
                e.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("Pick a connection on the left"),
                )
            })
            .when_some(readonly, |e, ro| {
                let (path, label, color) = if ro {
                    ("furet/lock.svg", "read-only", theme.success)
                } else {
                    ("furet/lock-open.svg", "read-write", theme.danger)
                };
                e.child(
                    h_flex()
                        .id("furet-mode")
                        .gap_1()
                        .px_2()
                        .h(px(22.))
                        .items_center()
                        .rounded(px(5.))
                        .text_xs()
                        .text_color(color)
                        .border_1()
                        .border_color(color.opacity(0.35))
                        .cursor_pointer()
                        .hover(|s| s.bg(color.opacity(0.08)))
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_mode(window, cx)))
                        .child(icon(path, color).size(px(12.)))
                        .child(label),
                )
            })
            .when_some(self.confirm.clone(), |e, input| {
                e.child(div().w(px(220.)).child(Input::new(&input).small()))
            })
            .child(div().flex_1())
            .when(self.busy, |e| {
                e.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("running…"),
                )
            })
            .child(
                button("furet-run-all", "Run all", false)
                    .on_click(cx.listener(|this, _, window, cx| this.run(true, window, cx))),
            )
            .child(
                button("furet-run", "Run", true)
                    .on_click(cx.listener(|this, _, window, cx| this.run(false, window, cx))),
            )
    }

    /// Rows and time, or what went wrong, above the results.
    fn outcome_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let plural = |n: u64| if n == 1 { "" } else { "s" };
        let bar = || {
            h_flex()
                .h(px(30.))
                .px_3()
                .gap_3()
                .items_center()
                .text_xs()
                .border_b_1()
                .border_color(theme.border)
        };
        Some(match self.outcome.as_ref()? {
            Outcome::Rows { rows, more, ms } => bar()
                .child(div().font_weight(FontWeight::MEDIUM).child(format!(
                    "{rows}{} row{}",
                    if *more { "+" } else { "" },
                    plural(*rows as u64)
                )))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("{ms} ms")),
                )
                .when(*more, |e| {
                    e.child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child("first 1000 shown"),
                    )
                })
                .into_any_element(),
            Outcome::Affected { rows, ms } => bar()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("{rows} row{} affected", plural(*rows))),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("{ms} ms")),
                )
                .into_any_element(),
            Outcome::Failed {
                message,
                detail,
                ms,
            } => v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .bg(theme.danger.opacity(0.08))
                .border_b_1()
                .border_color(theme.danger.opacity(0.3))
                .child(
                    h_flex()
                        .gap_2()
                        .text_color(theme.danger)
                        .font_weight(FontWeight::MEDIUM)
                        .child(message.clone())
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{ms} ms")),
                        ),
                )
                .when_some(detail.clone(), |e, d| {
                    e.child(div().text_xs().text_color(theme.muted_foreground).child(d))
                })
                .into_any_element(),
        })
    }
}

impl Render for Explorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let console = v_flex()
            .size_full()
            .key_context("FuretConsole")
            .on_action(
                cx.listener(|this, _: &RunStatement, window, cx| this.run(false, window, cx)),
            )
            .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run(true, window, cx)))
            .child(self.toolbar(cx))
            .child(
                v_resizable("furet-split")
                    .child(
                        resizable_panel().size(px(200.)).child(
                            div()
                                .size_full()
                                .pt_1()
                                .child(Editor::new(&self.editor).appearance(false).h_full()),
                        ),
                    )
                    .child(
                        resizable_panel().child(
                            v_flex()
                                .size_full()
                                .border_t_1()
                                .border_color(theme.border)
                                .children(self.outcome_bar(cx))
                                .child(div().flex_1().min_h_0().child(self.results_area(cx))),
                        ),
                    ),
            );
        let keys = "⌘/Ctrl+↵ run · ⇧ all · ⌘/Ctrl+click follow · Esc back · Space inspector";
        let status = h_flex()
            .h(px(24.))
            .px_3()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .text_size(px(11.5))
            .text_color(theme.muted_foreground)
            .child(div().flex_none().child(self.status.clone()))
            .when_some(self.error.clone(), |e, err| {
                e.child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_color(theme.danger)
                        .child(err),
                )
            })
            .when(self.error.is_none(), |e| e.child(div().flex_1()))
            .child(div().flex_none().child(keys));
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font_family.clone())
            .text_size(theme.font_size)
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("furet-main")
                        .child(resizable_panel().size(px(290.)).child(self.sidebar(cx)))
                        .child(resizable_panel().child(console)),
                ),
            )
            .child(status)
    }
}
