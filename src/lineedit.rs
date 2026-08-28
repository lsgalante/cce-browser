//! A one-line text field: the text, a caret, and a selection.
//!
//! Written as the shared editor the chrome should use everywhere. The URL bar
//! still has its own copy of this logic welded into `BrowserApp` (59 call
//! sites); migrating it is a mechanical change worth doing on its own rather
//! than folded into a feature, so for now this backs the dialog fields only.

use cce_ui::widget::{ElementState, Key, KeyEvent, NamedKey};

/// What a keystroke meant, beyond editing the text.
#[derive(Debug, PartialEq)]
pub enum EditOutcome {
    /// Nothing structural — redraw and carry on.
    Edited,
    /// Enter: the caller commits.
    Submit,
    /// Escape: the caller cancels.
    Cancel,
    /// Not ours (a chord the chrome owns).
    Ignored,
}

#[derive(Default)]
pub struct LineEdit {
    pub text: String,
    pub cursor: usize,
    /// Normalized (start < end). Any edit replaces or drops it.
    pub selection: Option<(usize, usize)>,
    /// Render as bullets. Set for password fields.
    pub masked: bool,
}

fn prev_boundary(s: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if s.is_char_boundary(j) {
            return j;
        }
    }
    0
}

fn next_boundary(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() {
        j += 1;
        if s.is_char_boundary(j) {
            return j;
        }
    }
    s.len()
}

impl LineEdit {
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self { cursor: text.len(), text, ..Self::default() }
    }

    pub fn masked() -> Self {
        Self { masked: true, ..Self::default() }
    }

    /// What to draw. Never returns the password itself.
    pub fn display(&self) -> String {
        if self.masked {
            "\u{2022}".repeat(self.text.chars().count())
        } else {
            self.text.clone()
        }
    }

    pub fn select_all(&mut self) {
        self.cursor = self.text.len();
        self.selection = (self.cursor > 0).then_some((0, self.cursor));
    }

    fn take_selection(&mut self) -> bool {
        match self.selection.take() {
            Some((a, b)) if a < b && b <= self.text.len() => {
                self.text.replace_range(a..b, "");
                self.cursor = a;
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent) -> EditOutcome {
        if event.state != ElementState::Pressed {
            return EditOutcome::Ignored;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => return EditOutcome::Submit,
            Key::Named(NamedKey::Escape) => return EditOutcome::Cancel,
            Key::Named(NamedKey::Backspace) => {
                if !self.take_selection() && self.cursor > 0 {
                    let prev = prev_boundary(&self.text, self.cursor);
                    self.text.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                }
            }
            Key::Named(NamedKey::Delete) => {
                if !self.take_selection() && self.cursor < self.text.len() {
                    let next = next_boundary(&self.text, self.cursor);
                    self.text.replace_range(self.cursor..next, "");
                }
            }
            // Arrows collapse a selection to the edge they move toward.
            Key::Named(NamedKey::ArrowLeft) => {
                self.cursor = match self.selection.take() {
                    Some((a, _)) => a,
                    None => prev_boundary(&self.text, self.cursor),
                };
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.cursor = match self.selection.take() {
                    Some((_, b)) => b,
                    None => next_boundary(&self.text, self.cursor),
                };
            }
            Key::Named(NamedKey::Home) => {
                self.selection = None;
                self.cursor = 0;
            }
            Key::Named(NamedKey::End) => {
                self.selection = None;
                self.cursor = self.text.len();
            }
            Key::Character(c) if event.ctrl => match c.as_str() {
                "a" => self.select_all(),
                "u" => {
                    self.text.clear();
                    self.cursor = 0;
                    self.selection = None;
                }
                // Copy and cut are deliberately absent on a masked field:
                // a password should not leave through the clipboard by a
                // chord the user may not have meant. Paste is allowed, since
                // that is how password managers hand one over.
                "v" => {
                    if let Some(t) = cce_ui::widget::clipboard::read_from_clipboard() {
                        let flat: String = t.chars().filter(|c| !c.is_control()).collect();
                        if !flat.is_empty() {
                            self.take_selection();
                            self.text.insert_str(self.cursor, &flat);
                            self.cursor += flat.len();
                        }
                    }
                }
                "c" | "x" if !self.masked => {
                    if let Some((a, b)) = self.selection.filter(|&(a, b)| a < b) {
                        cce_ui::widget::clipboard::copy_to_clipboard(&self.text[a..b]);
                        if c == "x" {
                            self.take_selection();
                        }
                    }
                }
                _ => return EditOutcome::Ignored,
            },
            _ => {
                let insert = match (&event.text, &event.logical_key) {
                    (Some(t), _) if !event.ctrl && !t.chars().any(char::is_control) => {
                        Some(t.clone())
                    }
                    (None, Key::Named(NamedKey::Space)) => Some(" ".to_string()),
                    (None, Key::Character(c)) if !event.ctrl => Some(c.clone()),
                    _ => return EditOutcome::Ignored,
                };
                if let Some(t) = insert {
                    self.take_selection();
                    self.text.insert_str(self.cursor, &t);
                    self.cursor += t.len();
                }
            }
        }
        EditOutcome::Edited
    }
}
