//! Lists that keep their scroll position between frames and show a
//! scrollbar when they hold more rows than fit.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState};

/// Scroll position of one list, kept from frame to frame (the frame only
/// reads the [`App`](crate::tui::app::App), hence `Cell`).
#[derive(Debug, Clone, Default)]
pub struct Scroll {
    offset: Cell<usize>,
    page: Cell<usize>,
    /// Where the rows were drawn (for the mouse).
    rows: Cell<Rect>,
}

impl Scroll {
    /// Rows the last frame showed (at least 1): the step of PgUp/PgDn.
    pub fn page(&self) -> usize {
        self.page.get().max(1)
    }

    /// The row (of all rows) at a screen position, if the last frame drew
    /// one there.
    pub fn row_at(&self, column: u16, row: u16) -> Option<usize> {
        let rows = self.rows.get();
        let inside = column >= rows.x
            && column < rows.x + rows.width
            && row >= rows.y
            && row < rows.y + rows.height;
        inside.then(|| self.offset.get() + usize::from(row - rows.y))
    }

    /// True if the last frame drew the list under this position.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.row_at(column, row).is_some()
    }
}

/// What surrounds the rows of a list.
#[derive(Debug, Clone, Copy)]
pub struct Chrome {
    /// A border around it.
    pub bordered: bool,
    /// A header row above the rows.
    pub header: bool,
}

/// A bordered table with a header, the usual case.
pub const BOXED: Chrome = Chrome {
    bordered: true,
    header: true,
};

/// Draws `table` with the remembered scroll position, `selected`
/// highlighted, and a scrollbar on the right if `total` rows do not fit.
pub fn show(
    frame: &mut Frame,
    table: Table<'_>,
    area: Rect,
    scroll: &Scroll,
    selected: Option<usize>,
    total: usize,
    chrome: Chrome,
) {
    let border = u16::from(chrome.bordered);
    let top = border + u16::from(chrome.header);
    let visible = usize::from(area.height.saturating_sub(top + border));
    let mut state = TableState::default()
        .with_offset(scroll.offset.get().min(total.saturating_sub(visible)))
        .with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
    scroll.offset.set(state.offset());
    scroll.page.set(visible);
    let shown = total.saturating_sub(state.offset()).min(visible);
    scroll.rows.set(Rect {
        x: area.x + border,
        y: area.y + top,
        width: area.width.saturating_sub(2 * border),
        height: u16::try_from(shown).unwrap_or_default(),
    });
    scrollbar(frame, area, top, border, total, state.offset(), visible);
}

/// A scrollbar on the right edge of `area` (inside its border rows) if
/// `total` lines do not fit into `visible`.
pub fn scrollbar(
    frame: &mut Frame,
    area: Rect,
    top: u16,
    bottom: u16,
    total: usize,
    offset: usize,
    visible: usize,
) {
    if total <= visible || visible == 0 {
        return;
    }
    let track = Rect {
        y: area.y + top,
        height: area.height.saturating_sub(top + bottom),
        ..area
    };
    let mut state = ScrollbarState::new(total.saturating_sub(visible) + 1)
        .position(offset)
        .viewport_content_length(visible);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        track,
        &mut state,
    );
}
