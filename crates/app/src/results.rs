//! A result set in the virtualized data table: values coloured by type, null in italics, the
//! type (and where a foreign key points) under each column name.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::prelude::*;
use gpui_kit::*;
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Ref {
    pub table: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Col {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub refs: Option<Ref>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Set {
    pub columns: Vec<Col>,
    pub rows: Vec<Vec<Option<String>>>,
    #[serde(default)]
    pub more: bool,
    pub affected: Option<u64>,
}

#[derive(Default)]
pub struct Results {
    pub set: Set,
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
        let width = (longest
            .max(c.name.len() + 2)
            .max(c.ty.as_deref().map_or(0, str::len) + 2)
            .min(48) as f32)
            * 7.5
            + 24.;
        let col = Column::new(format!("c{ix}"), c.name.clone())
            .width(px(width))
            .resizable(true);
        if c.ty.as_deref().map(kind) == Some(Kind::Number) {
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
        let ty = match (&c.ty, &c.refs) {
            (Some(t), Some(r)) => format!("{t} → {}", r.table),
            (Some(t), None) => t.clone(),
            (None, _) => String::new(),
        };
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
                    .child(ty),
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
            (Some(v), _) if c.refs.is_some() => (v.clone(), theme.link),
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
        div()
            .truncate()
            .font_family(theme.mono_font_family.clone())
            .text_color(color)
            .when(v.is_none(), |e| e.italic())
            .when(c.refs.is_some() && v.is_some(), |e| e.underline())
            .child(one_line)
    }

    fn cell_text(&self, row: usize, ix: usize, _: &App) -> String {
        self.set.rows[row][ix].clone().unwrap_or_default()
    }
}
