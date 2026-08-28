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

/// Also used by the chrome's text elision.
pub fn prev_boundary(s: &str, i: usize) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;
    use cce_ui::widget::{ElementState, Key, NamedKey};

    fn ev(key: Key, ctrl: bool) -> KeyEvent {
        let text = match &key {
            Key::Character(c) if !ctrl => Some(c.clone()),
            _ => None,
        };
        KeyEvent {
            state: ElementState::Pressed,
            logical_key: key,
            text,
            repeat: false,
            ctrl,
            shift: false,
            alt: false,
        }
    }
    fn ch(c: &str) -> KeyEvent {
        ev(Key::Character(c.into()), false)
    }
    fn ctrl(c: &str) -> KeyEvent {
        ev(Key::Character(c.into()), true)
    }
    fn named(n: NamedKey) -> KeyEvent {
        ev(Key::Named(n), false)
    }

    fn typed(e: &mut LineEdit, s: &str) {
        for c in s.chars() {
            e.handle_key(&ch(&c.to_string()));
        }
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut e = LineEdit::default();
        typed(&mut e, "abc");
        assert_eq!(e.text, "abc");
        assert_eq!(e.cursor, 3);
    }

    /// The URL bar's defining behaviour: entering it selects everything, so
    /// the next keystroke replaces the address rather than appending to it.
    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut e = LineEdit::with_text("https://example.com");
        e.select_all();
        typed(&mut e, "x");
        assert_eq!(e.text, "x");
        assert_eq!(e.selection, None);
    }

    #[test]
    fn ctrl_a_selects_all_and_ctrl_u_clears() {
        let mut e = LineEdit::with_text("abc");
        e.handle_key(&ctrl("a"));
        assert_eq!(e.selection, Some((0, 3)));
        e.handle_key(&ctrl("u"));
        assert_eq!(e.text, "");
        assert_eq!(e.selection, None);
    }

    #[test]
    fn backspace_deletes_a_selection_whole_or_one_char() {
        let mut e = LineEdit::with_text("abc");
        e.select_all();
        e.handle_key(&named(NamedKey::Backspace));
        assert_eq!(e.text, "");

        let mut e = LineEdit::with_text("abc");
        e.handle_key(&named(NamedKey::Backspace));
        assert_eq!(e.text, "ab");
    }

    /// Arrows collapse to the edge they move toward rather than stepping from
    /// the caret — otherwise Left after a select-all lands in the wrong place.
    #[test]
    fn arrows_collapse_a_selection_to_its_edge() {
        let mut e = LineEdit::with_text("abc");
        e.select_all();
        e.handle_key(&named(NamedKey::ArrowLeft));
        assert_eq!((e.cursor, e.selection), (0, None));

        e.select_all();
        e.handle_key(&named(NamedKey::ArrowRight));
        assert_eq!((e.cursor, e.selection), (3, None));
    }

    #[test]
    fn enter_and_escape_are_reported_not_swallowed() {
        let mut e = LineEdit::with_text("x");
        assert_eq!(e.handle_key(&named(NamedKey::Enter)), EditOutcome::Submit);
        assert_eq!(e.handle_key(&named(NamedKey::Escape)), EditOutcome::Cancel);
    }

    /// Multi-byte text must not be split mid-character.
    #[test]
    fn caret_moves_by_character_not_byte() {
        let mut e = LineEdit::with_text("é1");
        e.handle_key(&named(NamedKey::Home));
        e.handle_key(&named(NamedKey::ArrowRight));
        assert_eq!(e.cursor, 2, "é is two bytes");
        e.handle_key(&named(NamedKey::Backspace));
        assert_eq!(e.text, "1");
    }

    /// A password must not leave through a chord the user may not have meant.
    #[test]
    fn a_masked_field_hides_its_text_and_refuses_copy() {
        let mut e = LineEdit::masked();
        typed(&mut e, "hunter2");
        assert_eq!(e.display(), "•".repeat(7));
        assert_ne!(e.display(), e.text);
        e.select_all();
        assert_eq!(e.handle_key(&ctrl("c")), EditOutcome::Ignored);
        assert_eq!(e.handle_key(&ctrl("x")), EditOutcome::Ignored);
        assert_eq!(e.text, "hunter2", "cut must not have removed it");
    }
}
