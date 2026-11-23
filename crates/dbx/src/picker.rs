//! The connection list shown by a bare `dbx`: type to filter, Enter to open.

use crate::Row;
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row as TableRow, Table, TableState};

pub enum Action {
    Open { name: String, write: bool },
    Edit,
    Refresh,
    Quit,
}

struct Picker<'a> {
    label: &'a str,
    rows: &'a [Row],
    filter: String,
    state: TableState,
    message: Option<String>,
}

fn stage_style(stage: &str) -> Style {
    match stage {
        "local" | "dev" | "test" => Style::new().fg(Color::Green),
        "prod" | "production" => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        "stg" | "staging" | "preprod" => Style::new().fg(Color::Yellow),
        _ => Style::new().fg(Color::Blue),
    }
}

impl<'a> Picker<'a> {
    fn visible(&self) -> Vec<&'a Row> {
        let f = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                f.split_whitespace().all(|w| {
                    [
                        r.name.as_str(),
                        r.stage.as_str(),
                        r.kind.unwrap_or(""),
                        r.endpoint.as_str(),
                        r.source.as_str(),
                    ]
                    .iter()
                    .any(|s| s.to_lowercase().contains(w))
                })
            })
            .collect()
    }

    fn draw(&mut self, f: &mut Frame) {
        let [head, input, body, msg, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Fill(1),
            Constraint::Length(if self.message.is_some() { 2 } else { 0 }),
            Constraint::Length(1),
        ])
        .areas(f.area());

        f.render_widget(
            Line::from(vec![
                Span::from(" dbx ").bold(),
                Span::from(self.label).dim(),
            ]),
            head,
        );
        f.render_widget(
            Line::from(vec![
                Span::from(" › ").dim(),
                Span::from(self.filter.as_str()),
            ]),
            input,
        );
        f.set_cursor_position((input.x + 3 + self.filter.chars().count() as u16, input.y));

        let visible = self.visible();
        let rows: Vec<TableRow> = visible
            .iter()
            .map(|r| {
                let tunnel = match r.tunnel_up {
                    Some(true) => Span::from("⇄").green(),
                    Some(false) => Span::from("⇄").dim(),
                    None => Span::from(" "),
                };
                let source = if r.source == "this repo" {
                    String::new()
                } else {
                    r.source.clone()
                };
                TableRow::new(vec![
                    Cell::from(tunnel),
                    Cell::from(r.name.as_str()).bold(),
                    Cell::from(Span::styled(r.stage.as_str(), stage_style(&r.stage))),
                    Cell::from(r.kind.unwrap_or("?")).dim(),
                    Cell::from(r.endpoint.as_str()),
                    Cell::from(if r.readonly { "ro" } else { "rw" }).dim(),
                    Cell::from(source).dim(),
                ])
            })
            .collect();
        let empty = rows.is_empty();
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(
                    visible
                        .iter()
                        .map(|r| r.name.len() as u16)
                        .max()
                        .unwrap_or(4)
                        .max(4),
                ),
                Constraint::Length(
                    visible
                        .iter()
                        .map(|r| r.stage.len() as u16)
                        .max()
                        .unwrap_or(5)
                        .max(5),
                ),
                Constraint::Length(10),
                Constraint::Fill(1),
                Constraint::Length(2),
                Constraint::Fill(1),
            ],
        )
        .column_spacing(2)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸");
        if empty {
            let text = if self.rows.is_empty() {
                "no connections here yet — ^E to add some (or `dbx add`, `dbx include <other clone>`)"
            } else {
                "nothing matches"
            };
            f.render_widget(Paragraph::new(format!("  {text}")).dim(), body);
        } else {
            f.render_stateful_widget(table, body, &mut self.state);
        }
        if let Some(m) = &self.message {
            f.render_widget(Paragraph::new(format!(" {m}")).red(), msg);
        }
        f.render_widget(
            Line::from(
                " ↵ open · ^W open read-write · ^E edit connections · ^R refresh · esc quit",
            )
            .dim(),
            foot,
        );
    }
}

pub fn run(label: &str, rows: &[Row], message: Option<String>) -> Result<Action> {
    let mut p = Picker {
        label,
        rows,
        filter: String::new(),
        state: TableState::default(),
        message,
    };
    p.state.select(Some(0));
    let mut terminal = ratatui::init();
    let result = (|| -> Result<Action> {
        loop {
            terminal.draw(|f| p.draw(f))?;
            let Event::Key(k) = event::read()? else {
                continue;
            };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            let n = p.visible().len();
            let selected = |p: &Picker| {
                p.state
                    .selected()
                    .and_then(|i| p.visible().get(i).map(|r| r.name.clone()))
            };
            match k.code {
                KeyCode::Esc => return Ok(Action::Quit),
                KeyCode::Char('c' | 'q') if ctrl => return Ok(Action::Quit),
                KeyCode::Char('e') if ctrl => return Ok(Action::Edit),
                KeyCode::Char('r') if ctrl => return Ok(Action::Refresh),
                KeyCode::Char('u') if ctrl => p.filter.clear(),
                KeyCode::Enter | KeyCode::Char('w') if k.code == KeyCode::Enter || ctrl => {
                    if let Some(name) = selected(&p) {
                        return Ok(Action::Open {
                            name,
                            write: k.code != KeyCode::Enter,
                        });
                    }
                }
                KeyCode::Up => p
                    .state
                    .select(Some(p.state.selected().unwrap_or(0).saturating_sub(1))),
                KeyCode::Down => p.state.select(Some(
                    (p.state.selected().unwrap_or(0) + 1).min(n.saturating_sub(1)),
                )),
                KeyCode::Char('p') if ctrl => p
                    .state
                    .select(Some(p.state.selected().unwrap_or(0).saturating_sub(1))),
                KeyCode::Char('n') if ctrl => p.state.select(Some(
                    (p.state.selected().unwrap_or(0) + 1).min(n.saturating_sub(1)),
                )),
                KeyCode::Backspace => {
                    p.filter.pop();
                    p.state.select(Some(0));
                }
                KeyCode::Char(c) if !ctrl => {
                    p.filter.push(c);
                    p.state.select(Some(0));
                }
                _ => {}
            }
            p.message = None;
        }
    })();
    ratatui::restore();
    result
}
