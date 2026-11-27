//! View 4: learned MAC addresses, filterable and sortable.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};

use super::{list, titled};
use crate::tui::macs::{MacState, entry_site};
use crate::tui::{format, theme};

/// Width of the filter input.
const FILTER_WIDTH: u16 = 24;

pub fn draw(frame: &mut Frame, state: &MacState, scroll: &list::Scroll, area: Rect) {
    let [filter, table] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
    let visible = state.visible();
    let (text, cursor) = state.filter.window(usize::from(FILTER_WIDTH - 1));
    let input_style = if state.editing {
        theme::focused()
    } else {
        theme::field()
    };
    let order = if state.descending { "desc" } else { "asc" };
    let line = Line::from(vec![
        Span::styled(" Filter ", theme::dim()),
        Span::styled(
            format!("{text:<width$}", width = usize::from(FILTER_WIDTH)),
            input_style,
        ),
        Span::styled("  sorted by ", theme::dim()),
        Span::raw(format!("{} ({order})", state.sort.name())),
        Span::styled(
            format!("  {} of {} entries", visible.len(), state.entries.len()),
            theme::dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), filter);
    if state.editing {
        let x = filter.x + 8 + u16::try_from(cursor).unwrap_or_default();
        frame.set_cursor_position(Position::new(x, filter.y));
    }

    let block = titled(" Learned MAC addresses ");
    if let Some(e) = &state.error {
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {e}"), theme::bad())).block(block),
            table,
        );
        return;
    }
    if !state.loaded {
        frame.render_widget(
            Paragraph::new(Line::styled(" Loading ...", theme::dim())).block(block),
            table,
        );
        return;
    }
    let header = Row::new(vec!["MAC", "VLAN", "PORT", "SITE", "AGE"]).style(theme::header());
    let rows: Vec<Row> = visible
        .iter()
        .map(|e| {
            Row::new(vec![
                Cell::from(e.mac.clone()),
                Cell::from(e.vlan.map_or_else(|| "-".into(), |v| v.to_string())),
                Cell::from(e.port.clone()),
                Cell::from(entry_site(e).unwrap_or("this site").to_string()),
                Cell::from(e.age_secs.map_or_else(|| "-".into(), format::age)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(17),
        Constraint::Length(5),
        Constraint::Length(15),
        Constraint::Length(12),
        Constraint::Min(5),
    ];
    let total = rows.len();
    list::show(
        frame,
        Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(theme::selected()),
        table,
        scroll,
        (!visible.is_empty()).then_some(state.selected),
        total,
        list::BOXED,
    );
}
