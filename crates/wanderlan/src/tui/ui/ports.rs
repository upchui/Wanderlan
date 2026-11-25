//! The window to choose trunk ports: the interfaces of this host, the
//! chosen ones ticked and numbered (1 is the main port).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table};

use super::{list, titled};
use crate::tui::app::App;
use crate::tui::format;
use crate::tui::ports::{PortPicker, PortRow};
use crate::tui::theme;

/// Width of the window.
const WIDTH: u16 = 76;
/// Text in front of the "other" field.
const OTHER: &str = " other: ";
/// Width of the "other" field.
const OTHER_WIDTH: usize = 16;

fn table_row(picker: &PortPicker, row: &PortRow) -> Row<'static> {
    let position = picker.position(&row.name);
    let mark = if position.is_some() { "[x]" } else { "[ ]" };
    let number = position.map_or_else(String::new, |p| (p + 1).to_string());
    let (state, mtu) = if row.missing {
        (String::new(), String::new())
    } else {
        (
            if row.up { "up" } else { "down" }.to_string(),
            row.mtu.to_string(),
        )
    };
    // The tunnels that run over it: a warning, in its own colour.
    let note = match (&row.blocked, &row.warning, &row.note, position) {
        (Some(why), ..) => Cell::from(why.clone()),
        (None, Some(warning), ..) => Cell::from(warning.clone()).style(theme::warn()),
        (None, None, Some(note), _) => Cell::from(note.clone()),
        (None, None, None, Some(0)) => Cell::from("main port"),
        (None, None, None, _) => Cell::from(""),
    };
    let style = match (&row.blocked, position) {
        // Chosen although it cannot be: the draft has it, Space takes it out.
        (Some(_), Some(_)) => theme::bad(),
        (Some(_), None) => theme::dim(),
        _ if row.missing => theme::warn(),
        _ => ratatui::style::Style::new(),
    };
    Row::new(vec![
        Cell::from(mark),
        Cell::from(number),
        Cell::from(row.name.clone()),
        Cell::from(row.kind.clone()),
        Cell::from(state),
        Cell::from(mtu),
        note,
    ])
    .style(style)
}

/// Draws the window over `area`.
pub fn draw(frame: &mut Frame, app: &App, picker: &PortPicker, area: Rect) {
    let Some(draft) = &app.draft else { return };
    let rows = picker.rows(draft, &app.interfaces);
    let width = area.width.min(WIDTH);
    let text_width = usize::from(width.saturating_sub(4));
    // Below the list: what the choice means, then what is wrong with it.
    let mut info: Vec<Line> = Vec::new();
    for part in format::wrap(&picker.summary(draft), text_width) {
        info.push(Line::styled(format!(" {part}"), theme::dim()));
    }
    for effect in picker.effects(draft) {
        for part in format::wrap(&effect, text_width) {
            info.push(Line::raw(format!(" {part}")));
        }
    }
    if let Some(warning) = picker.warning(&app.interfaces) {
        for part in format::wrap(&warning, text_width) {
            info.push(Line::styled(format!(" {part}"), theme::warn()));
        }
    }
    let problem = picker
        .error
        .clone()
        .or_else(|| {
            app.interfaces_error
                .as_ref()
                .map(|e| format!("Cannot read the interfaces of this host: {e}"))
        })
        .or_else(|| picker.apply(draft).err());
    if let Some(problem) = problem {
        for part in format::wrap(&problem, text_width) {
            info.push(Line::styled(format!(" {part}"), theme::bad()));
        }
    }
    let info_height = u16::try_from(info.len()).unwrap_or(u16::MAX);
    let table_height = u16::try_from(rows.len().max(1) + 1).unwrap_or(u16::MAX);
    let height = table_height + 2 + info_height + 2;
    let popup = area.centered(Constraint::Length(width), Constraint::Max(height));
    frame.render_widget(Clear, popup);
    let block = titled(format!(" {} ", picker.title()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [table_area, other_area, _, info_area] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(info_height),
    ])
    .areas(inner);
    // Nothing to list: one line of text instead of the table. Until the
    // list is there, the chosen ports would look missing.
    let loading = !app.interfaces_loaded && app.interfaces_error.is_none();
    let rows = if loading { Vec::new() } else { rows };
    let empty = if loading {
        Some((
            " Loading the interfaces of this host ...".to_string(),
            theme::dim(),
        ))
    } else if rows.is_empty() {
        Some((
            " No interfaces found; type a name below.".to_string(),
            theme::dim(),
        ))
    } else {
        None
    };
    if let Some((text, style)) = empty {
        frame.render_widget(Paragraph::new(Line::styled(text, style)), table_area);
    }
    let table_rows: Vec<Row> = rows.iter().map(|r| table_row(picker, r)).collect();
    let total = table_rows.len();
    if total > 0 {
        list::show(
            frame,
            Table::new(
                table_rows,
                [
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(15),
                    Constraint::Length(9),
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Min(10),
                ],
            )
            .header(
                Row::new(vec!["", "#", "INTERFACE", "KIND", "STATE", "MTU", "NOTE"])
                    .style(theme::header()),
            )
            .row_highlight_style(theme::selected()),
            table_area,
            &app.scrolls.ports,
            (picker.cursor < rows.len()).then_some(picker.cursor),
            total,
            list::Chrome {
                bordered: false,
                header: true,
            },
        );
    }
    // A name that is not (yet) on this host.
    let focused = !loading && picker.cursor >= rows.len();
    let (shown, col) = picker.manual.window(OTHER_WIDTH - 1);
    let field_style = if focused {
        theme::focused()
    } else {
        theme::field()
    };
    let label_style = if focused { theme::bold() } else { theme::dim() };
    let hint = if focused {
        "  Enter adds it"
    } else {
        "  an interface that is not there yet"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(OTHER, label_style),
            Span::styled(format!("{shown:<OTHER_WIDTH$}"), field_style),
            Span::styled(hint, theme::dim()),
        ])),
        other_area,
    );
    if focused {
        let x = other_area.x + u16::try_from(OTHER.len() + col).unwrap_or_default();
        frame.set_cursor_position(Position::new(x, other_area.y));
    }
    frame.render_widget(Paragraph::new(info), info_area);
}
