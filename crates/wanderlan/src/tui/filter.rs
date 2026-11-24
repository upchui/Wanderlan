//! A text filter for a list (`/`): every word must occur in one of the
//! fields of a row, case-insensitive. Enter keeps it, Esc clears it.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::input::TextInput;
use super::theme;

/// Width of the input box.
const WIDTH: u16 = 30;

/// The filter of one list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// The text.
    pub input: TextInput,
    /// Keys go into the filter.
    pub editing: bool,
}

impl Filter {
    /// True if it hides rows (or is being typed).
    pub fn is_shown(&self) -> bool {
        self.editing || !self.input.value().trim().is_empty()
    }

    /// Starts typing.
    pub fn start(&mut self) {
        self.editing = true;
    }

    /// True if every word of the filter occurs in one of `fields`.
    pub fn matches(&self, fields: &[&str]) -> bool {
        let fields: Vec<String> = fields.iter().map(|f| f.to_lowercase()).collect();
        self.input
            .value()
            .split_whitespace()
            .map(str::to_lowercase)
            .all(|word| fields.iter().any(|f| f.contains(&word)))
    }

    /// A key while typing: Enter keeps the filter, Esc clears it.
    pub fn handle_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Enter => self.editing = false,
            KeyCode::Esc => {
                self.editing = false;
                self.input.set("");
            }
            _ => {
                self.input.handle(key);
            }
        }
    }

    /// Draws the filter line (and the cursor while typing).
    pub fn draw(&self, frame: &mut ratatui::Frame, area: Rect, shown: usize, total: usize) {
        let (text, cursor) = self.input.window(usize::from(WIDTH - 1));
        let style = if self.editing {
            theme::focused()
        } else {
            theme::field()
        };
        let hint = if self.editing {
            "  Enter keeps, Esc clears"
        } else {
            "  / changes it"
        };
        let line = Line::from(vec![
            Span::styled(" Filter ", theme::dim()),
            Span::styled(format!("{text:<width$}", width = usize::from(WIDTH)), style),
            Span::styled(format!("  {shown} of {total}"), theme::dim()),
            Span::styled(hint, theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        if self.editing {
            let x = area.x + 8 + u16::try_from(cursor).unwrap_or_default();
            frame.set_cursor_position(Position::new(x, area.y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_must_match_some_field() {
        let mut f = Filter::default();
        f.input.set("lab 10.8");
        assert!(f.matches(&["lab1", "10.8.1.1"]));
        assert!(!f.matches(&["lab1", "192.168.1.1"]));
        f.input.set("LAB");
        assert!(f.matches(&["lab1"]), "case does not matter");
        f.input.set("");
        assert!(f.matches(&["anything"]));
    }
}
