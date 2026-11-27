//! The window to choose the uplink of a breakout: the default route and the
//! interfaces of this box, the one in the form marked.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};

use super::{list, titled};
use crate::tui::app::App;
use crate::tui::format;
use crate::tui::theme;
use crate::tui::uplinks::{UplinkPicker, UplinkRow};

/// Width of the window.
const WIDTH: u16 = 100;
/// Text in front of the "other" field.
const OTHER: &str = " other: ";
/// Width of the "other" field.
const OTHER_WIDTH: usize = 16;

fn table_row(current: &str, row: &UplinkRow) -> Row<'static> {
    let mark = if row.value() == current {
        "(•)"
    } else {
        "( )"
    };
    let name = row
        .name
        .clone()
        .unwrap_or_else(|| "default route".to_string());
    let state = match (&row.name, row.up) {
        (None, _) => "",
        (Some(_), true) => "up",
        (Some(_), false) => "down",
    };
    let note = match (&row.problem, &row.note) {
        (Some(problem), _) => Cell::from(problem.clone()).style(theme::warn()),
        (None, Some(note)) => Cell::from(note.clone()),
        (None, None) => Cell::from(""),
    };
    Row::new(vec![
        Cell::from(mark),
        Cell::from(name),
        Cell::from(state),
        Cell::from(row.addresses.clone()),
        Cell::from(row.routes.clone()),
        note,
    ])
}

/// Draws the window over `area`.
pub fn draw(frame: &mut Frame, app: &App, picker: &UplinkPicker, area: Rect) {
    let Some(draft) = &app.draft else { return };
    let rows = picker.rows(draft, app.status.as_ref());
    let cursor = picker.cursor(&rows);
    let current = picker.current();
    let width = area.width.min(WIDTH);
    let text_width = usize::from(width.saturating_sub(4));
    let mut info: Vec<Line> = Vec::new();
    let what = if picker.own {
        "Where the public traffic of the VLAN leaves this box. The default route follows the box's own; an interface needs a default route of its own (any metric) or is a point-to-point link."
            .to_string()
    } else {
        format!(
            "The interfaces of {} are known only on its box: its default route, or type the name of one.",
            picker.site_name
        )
    };
    for part in format::wrap(&what, text_width) {
        info.push(Line::styled(format!(" {part}"), theme::dim()));
    }
    let problem = picker.error.clone().or_else(|| {
        rows.get(cursor)
            .and_then(|r| r.problem.clone())
            .map(|p| format!("{}: {p} (it can still be chosen)", r_name(&rows, cursor)))
    });
    if let Some(problem) = problem {
        let style = if picker.error.is_some() {
            theme::bad()
        } else {
            theme::warn()
        };
        for part in format::wrap(&problem, text_width) {
            info.push(Line::styled(format!(" {part}"), style));
        }
    }
    let info_height = u16::try_from(info.len()).unwrap_or(u16::MAX);
    let table_height = u16::try_from(rows.len() + 1).unwrap_or(u16::MAX);
    let height = table_height + 2 + info_height + 2;
    let popup = area.centered(Constraint::Length(width), Constraint::Max(height));
    super::clear_around(frame, popup);
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
    let total = rows.len();
    // As wide as their content (the addresses and routes at most so wide).
    let widest = |f: &dyn Fn(&UplinkRow) -> usize, header: &str, most: usize| {
        let w = rows
            .iter()
            .map(f)
            .max()
            .unwrap_or(0)
            .max(header.len())
            .min(most);
        u16::try_from(w).unwrap_or(u16::MAX)
    };
    let name_w = widest(
        &|r| r.name.as_deref().map_or("default route".len(), str::len),
        "INTERFACE",
        15,
    );
    // What is left after the mark, the name, the state, the gaps and the
    // scrollbar goes to the addresses and routes, the rest to the note.
    let rest = inner
        .width
        .saturating_sub(3 + name_w + 5 + 5 + 1)
        .saturating_sub(12);
    let addr_w = widest(&|r| r.addresses.chars().count(), "ADDRESSES", 28).min(rest * 11 / 20);
    let route_w =
        widest(&|r| r.routes.chars().count(), "DEFAULT ROUTE", 26).min(rest.saturating_sub(addr_w));
    list::show(
        frame,
        Table::new(
            rows.iter()
                .map(|r| table_row(&current, r))
                .collect::<Vec<_>>(),
            [
                Constraint::Length(3),
                Constraint::Length(name_w),
                Constraint::Length(5),
                Constraint::Length(addr_w),
                Constraint::Length(route_w),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new(vec![
                "",
                "INTERFACE",
                "STATE",
                "ADDRESSES",
                "DEFAULT ROUTE",
                "NOTE",
            ])
            .style(theme::header()),
        )
        .row_highlight_style(theme::selected()),
        table_area,
        &app.scrolls.uplinks,
        (cursor < rows.len()).then_some(cursor),
        total,
        list::Chrome {
            bordered: false,
            header: true,
        },
    );
    // A name that is not (yet) on the box.
    let focused = cursor >= rows.len();
    let (shown, col) = picker.manual.window(OTHER_WIDTH - 1);
    let field_style = if focused {
        theme::focused()
    } else {
        theme::field()
    };
    let label_style = if focused { theme::bold() } else { theme::dim() };
    let hint = if focused {
        "  Enter takes it"
    } else {
        "  an interface that is not listed"
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

/// The name of the row at `index`, for messages.
fn r_name(rows: &[UplinkRow], index: usize) -> String {
    rows.get(index)
        .and_then(|r| r.name.clone())
        .unwrap_or_else(|| "the default route".to_string())
}
