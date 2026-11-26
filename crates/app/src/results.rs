//! A result set in the virtualized data table: values coloured by type, null in italics, the
//! type (and where a foreign key points) under each column name. Foreign-key cells show the row
//! they point at on hover, and Cmd/Ctrl+click follows them.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::*;
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct Ref {
    pub schema: String,
    pub table: String,
    pub column: String,
    #[serde(default, skip_serializing)]
    pub guessed: bool,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Col {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub refs: Option<Ref>,
    pub origin: Option<Ref>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Set {
    pub columns: Vec<Col>,
    pub rows: Vec<Vec<Option<String>>>,
    #[serde(default)]
    pub more: bool,
    pub affected: Option<u64>,
}

pub type Follow = Rc<dyn Fn(usize, usize, &mut Window, &mut App)>;

#[derive(Default)]
pub struct Results {
    pub set: Set,
    /// Per foreign-key column: each value's row in a few words (`None`: no such row).
    pub summaries: HashMap<usize, HashMap<String, Option<String>>>,
    pub on_follow: Option<Follow>,
}

impl Results {
    pub fn summary(&self, row: usize, col: usize) -> Option<String> {
        let v = self.set.rows.get(row)?.get(col)?.as_ref()?;
        let table = &self.set.columns[col].refs.as_ref()?.table;
        Some(match self.summaries.get(&col)?.get(v)? {
            Some(s) => s.clone(),
            None => format!("no {table} row {v}"),
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Number,
    Time,
    Json,
    Bool,
    Text,
}

fn kind(ty: &str) -> Kind {
    let t = ty.trim_start_matches("Nullable(");
    if t == "bool" || t.starts_with("Bool") {
        Kind::Bool
    } else if [
        "int", "float", "numeric", "oid", "money", "UInt", "Int", "Float", "Decimal",
    ]
    .iter()
    .any(|p| t.starts_with(p))
    {
        Kind::Number
    } else if ["timestamp", "date", "time", "interval", "Date"]
        .iter()
        .any(|p| t.starts_with(p))
    {
        Kind::Time
    } else if t.starts_with("json") || t.starts_with("JSON") {
        Kind::Json
    } else {
        Kind::Text
    }
}

pub fn is_number(ty: Option<&str>) -> bool {
    ty.map(kind) == Some(Kind::Number)
}

/// `→ tenants`, or `→ tenants?` when only the column's name says so.
pub fn arrow(r: &Ref) -> String {
    format!("→ {}{}", r.table, if r.guessed { "?" } else { "" })
}

impl TableDelegate for Results {
    fn columns_count(&self, _: &App) -> usize {
        self.set.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.set.rows.len()
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let c = &self.set.columns[ix];
        let longest = self
            .set
            .rows
            .iter()
            .take(200)
            .map(|r| r[ix].as_deref().map_or(4, str::len))
            .max()
            .unwrap_or(4);
        let under =
            c.ty.as_deref().map_or(0, str::len) + c.refs.as_ref().map_or(0, |r| r.table.len() + 5);
        let width = (longest.max(c.name.len() + 2).max(under + 2).min(48) as f32) * 7.5 + 24.;
        let col = Column::new(format!("c{ix}"), c.name.clone())
            .width(px(width))
            .resizable(true);
        if is_number(c.ty.as_deref()) {
            col.text_right()
        } else {
            col
        }
    }

    fn render_th(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let c = &self.set.columns[ix];
        let under = [c.ty.clone(), c.refs.as_ref().map(arrow)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(c.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(under),
            )
    }

    fn render_td(
        &mut self,
        row: usize,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let c = &self.set.columns[ix];
        let v = self.set.rows[row][ix].clone();
        let k = c.ty.as_deref().map_or(Kind::Text, kind);
        let (text, color) = match (&v, k) {
            (None, _) => ("null".to_string(), theme.muted_foreground),
            (Some(v), Kind::Bool) => {
                let yes = v == "t" || v == "true";
                (
                    if yes { "true" } else { "false" }.into(),
                    if yes { theme.success } else { theme.danger },
                )
            }
            (Some(v), _) if c.refs.is_some() => (v.clone(), theme.blue),
            (Some(v), Kind::Number) => (v.clone(), theme.info),
            (Some(v), Kind::Time) => (v.clone(), theme.cyan),
            (Some(v), Kind::Json) => (v.clone(), theme.magenta),
            (Some(v), Kind::Text) => (v.clone(), theme.foreground),
        };
        let one_line: String = text
            .chars()
            .take(300)
            .collect::<String>()
            .replace('\n', " ⏎ ");
        let link = c.refs.is_some() && v.is_some();
        let tip = if link { self.summary(row, ix) } else { None };
        let follow = self.on_follow.clone().filter(|_| link);
        div()
            .id(("cell", row * 4096 + ix))
            .truncate()
            .font_family(theme.mono_font_family.clone())
            .text_color(color)
            .when(v.is_none(), |e| e.italic())
            .when(link, |e| e.underline())
            .when_some(tip, |e, tip| {
                e.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .when_some(follow, |e, follow| {
                e.on_mouse_down(MouseButton::Left, move |ev, window, cx| {
                    if ev.modifiers.secondary(cx) {
                        cx.stop_propagation();
                        follow(row, ix, window, cx);
                    }
                })
            })
            .child(one_line)
    }

    fn cell_text(&self, row: usize, ix: usize, _: &App) -> String {
        self.set.rows[row][ix].clone().unwrap_or_default()
    }
}

const LABELS: [&str; 11] = [
    "name",
    "title",
    "label",
    "display_name",
    "full_name",
    "username",
    "email",
    "slug",
    "code",
    "number",
    "reference",
];
const DETAILS: [&str; 7] = [
    "status", "state", "plan", "type", "kind", "currency", "email",
];

/// A row in a few words: its name-like field, then a status-like one.
pub fn summarize(columns: &[Col], row: &[Option<String>]) -> String {
    let at = |name: &str, skip: Option<usize>| {
        columns
            .iter()
            .position(|c| c.name == name)
            .filter(|&i| Some(i) != skip && row[i].is_some())
    };
    let text = columns.iter().enumerate().position(|(i, c)| {
        c.ty.as_deref()
            .is_some_and(|t| t.contains("text") || t.contains("char") || t.contains("String"))
            && row[i].as_ref().is_some_and(|v| v.len() < 60)
    });
    let main = LABELS.iter().find_map(|n| at(n, None)).or(text);
    let detail = DETAILS.iter().find_map(|n| at(n, main));
    [main, detail]
        .into_iter()
        .flatten()
        .filter_map(|i| row[i].as_deref())
        .map(|v| v.chars().take(60).collect::<String>())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `customers #8` for a row found by its id, `orders · customer_id = 8` for rows found by a column.
pub fn step_label(table: &str, column: &str, value: &str) -> String {
    let uuid = value.len() > 13
        && value.as_bytes()[8] == b'-'
        && value[..8].bytes().all(|b| b.is_ascii_hexdigit());
    let v = if uuid {
        format!("{}…", &value[..8])
    } else {
        value.to_string()
    };
    if column == "id" {
        format!("{table} #{v}")
    } else {
        format!("{table} · {column} = {v}")
    }
}

#[cfg(test)]
mod tests {
    use super::{Col, step_label, summarize};

    fn col(name: &str, ty: &str) -> Col {
        Col {
            name: name.into(),
            ty: Some(ty.into()),
            refs: None,
            origin: None,
        }
    }

    #[test]
    fn summarizes_by_name_then_status() {
        let cols = [col("id", "uuid"), col("plan", "text"), col("name", "text")];
        let row = [
            Some("x".into()),
            Some("pro".into()),
            Some("Tenant 1".into()),
        ];
        assert_eq!(summarize(&cols, &row), "Tenant 1 · pro");
    }

    #[test]
    fn labels_shorten_uuids() {
        assert_eq!(
            step_label("tenants", "id", "4554e28f-8a16-41a2-a2a2-28ef19968696"),
            "tenants #4554e28f…"
        );
        assert_eq!(
            step_label("orders", "customer_id", "8"),
            "orders · customer_id = 8"
        );
    }
}
