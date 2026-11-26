//! The window: connections → schemas → tables on the left, a SQL editor over the results on the
//! right. Cmd/Ctrl+Enter runs the statement under the cursor, with Shift everything; a table in
//! the tree shows its first rows.

use crate::engine::{Client, Engine};
use crate::results::{Results, Set};
use gpui_kit::base::{TreeEvent, TreeItem, TreeState};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::tree::tree;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

actions!(furet_app, [RunStatement, RunAll]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-enter", RunStatement, Some("FuretConsole")),
        KeyBinding::new("ctrl-enter", RunStatement, Some("FuretConsole")),
        KeyBinding::new("cmd-shift-enter", RunAll, Some("FuretConsole")),
        KeyBinding::new("ctrl-shift-enter", RunAll, Some("FuretConsole")),
    ]);
}

struct Conn {
    name: String,
    stage: String,
    kind: String,
    endpoint: String,
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
        let results = cx.new(|cx| TableState::new(Results::default(), window, cx));
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
            _subscriptions: vec![subscription],
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
                            })
                            .collect();
                        this.status = format!(
                            "{} connections · {}",
                            this.conns.len(),
                            conns["root"].as_str().unwrap_or("")
                        )
                        .into();
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
                let id = match known {
                    Some(id) => id,
                    None => c.open(&name)?["id"].as_str().unwrap_or("").to_string(),
                };
                let cat = c.catalog(&id)?;
                Ok((name, id, cat))
            },
            |this, r, _, cx| match r {
                Ok((name, id, cat)) => {
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
            ["c", name] => self.current = Some(name.to_string()),
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
            |this, r, _, cx| {
                this.busy = false;
                match r {
                    Ok((name, id, out)) => {
                        this.sessions.insert(name.clone(), id);
                        let sets: Vec<Set> =
                            serde_json::from_value(out["results"].clone()).unwrap_or_default();
                        let ms = out["elapsed_ms"].as_u64().unwrap_or(0);
                        this.error = out["error"]["message"]
                            .as_str()
                            .map(|m| SharedString::from(m.to_string()));
                        if let Some(set) = sets.into_iter().last() {
                            let shown = match set.affected {
                                Some(n) if set.columns.is_empty() => format!("{n} rows affected"),
                                _ => format!(
                                    "{} row{}{}",
                                    set.rows.len(),
                                    if set.rows.len() == 1 { "" } else { "s" },
                                    if set.more { "+" } else { "" }
                                ),
                            };
                            this.status = format!("{name} · {shown} · {ms} ms").into();
                            this.results.update(cx, |t, cx| {
                                t.delegate_mut().set = set;
                                t.refresh(cx);
                            });
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

impl Render for Explorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let stages: HashMap<String, (String, String, String)> = self
            .conns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    (c.stage.clone(), c.kind.clone(), c.endpoint.clone()),
                )
            })
            .collect();
        let sidebar = v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .items_center()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Databases"),
            )
            .child(
                tree(&self.tree, move |ix, entry, selected, _, cx| {
                    let item = entry.item();
                    let id = item.id.to_string();
                    let view = view.clone();
                    let depth = entry.depth();
                    let (label, suffix): (String, Option<(String, Hsla)>) =
                        match id.split(':').next() {
                            Some("c") => {
                                let (stage, kind, endpoint) =
                                    stages.get(item.label.as_ref()).cloned().unwrap_or_default();
                                (
                                    format!("{}  {kind} · {endpoint}", item.label),
                                    Some((stage.clone(), stage_color(&stage, cx))),
                                )
                            }
                            _ => (item.label.to_string(), None),
                        };
                    ListItem::new(ix)
                        .selected(selected)
                        .pl(px(8. + depth as f32 * 14.))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(div().text_color(cx.theme().muted_foreground).child(
                                    if entry.is_folder() {
                                        if entry.is_expanded() { "▾" } else { "▸" }
                                    } else {
                                        "▦"
                                    },
                                ))
                                .child(div().flex_1().truncate().child(label))
                                .when_some(suffix, |e, (stage, color)| {
                                    e.child(div().text_xs().text_color(color).child(stage))
                                }),
                        )
                        .on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.clicked(&id, window, cx));
                        })
                })
                .flex_1(),
            );
        let current = self.current.clone().unwrap_or_default();
        let stage = self
            .conn(&current)
            .map(|c| c.stage.clone())
            .unwrap_or_default();
        let toolbar = h_flex()
            .h(px(36.))
            .px_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if current.is_empty() {
                        "No connection".to_string()
                    } else {
                        current.clone()
                    }),
            )
            .when(!stage.is_empty(), |e| {
                e.child(
                    div()
                        .text_xs()
                        .text_color(stage_color(&stage, cx))
                        .child(stage.clone()),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("⌘/Ctrl+Enter runs the statement · ⇧ runs all"),
            );
        let console = v_flex()
            .size_full()
            .key_context("FuretConsole")
            .on_action(
                cx.listener(|this, _: &RunStatement, window, cx| this.run(false, window, cx)),
            )
            .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run(true, window, cx)))
            .child(toolbar)
            .child(
                v_resizable("furet-split")
                    .child(
                        resizable_panel()
                            .size(px(220.))
                            .child(Editor::new(&self.editor).h_full()),
                    )
                    .child(resizable_panel().child(DataTable::new(&self.results))),
            );
        let status = h_flex()
            .h(px(26.))
            .px_3()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(self.status.clone()),
            )
            .when_some(self.error.clone(), |e, err| {
                e.child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_color(theme.danger)
                        .child(err),
                )
            });
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_sm()
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("furet-main")
                        .child(resizable_panel().size(px(300.)).child(sidebar))
                        .child(resizable_panel().child(console)),
                ),
            )
            .child(status)
    }
}
