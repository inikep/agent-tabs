use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::commands::Template;

/// The main prompt box's own commands, by name; never sent to the harnesses.
static BUILTINS: std::sync::LazyLock<[Template; 3]> = std::sync::LazyLock::new(|| {
    let builtin = |usage: &str, description: &str| Template {
        usage: usage.into(),
        description: vec![description.into()],
    };
    [
        builtin("/exit", "Stop all harnesses and quit."),
        builtin("/help", "Show what each command does."),
        builtin("/quit", "Stop all harnesses and quit."),
    ]
});

#[derive(Debug, PartialEq, Eq)]
pub enum ComposerAction {
    None,
    Cancel,
    Submit { text: String, targets: Vec<usize> },
}

#[derive(Debug, Clone)]
pub struct Composer {
    text: String,
    cursor: usize,
    pub targets: Vec<bool>,
    history: Vec<String>,
    history_pos: Option<usize>,
    // None after dismissing the popup, completing a name, or recalling history.
    command_selection: Option<usize>,
}

impl Composer {
    pub fn new(count: usize) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            targets: vec![false; count],
            history: Vec::new(),
            history_pos: None,
            command_selection: Some(0),
        }
    }

    pub fn open(&mut self, targets: &[usize]) {
        self.targets.iter_mut().for_each(|t| *t = false);
        for &i in targets {
            if let Some(t) = self.targets.get_mut(i) {
                *t = true;
            }
        }
        self.history_pos = None;
        self.command_selection = Some(0);
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Sent prompts, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_pos = None;
    }

    pub fn selected(&self) -> Vec<usize> {
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(i, _)| i)
            .collect()
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(t) = self.targets.get_mut(index) {
            *t = !*t;
        }
    }

    pub fn clear(&mut self) {
        self.command_selection = Some(0);
        self.text.clear();
        self.cursor = 0;
        self.history_pos = None;
    }

    pub fn toggle_all(&mut self) {
        let all = self.targets.iter().all(|t| *t);
        self.targets.iter_mut().for_each(|t| *t = !all);
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.command_selection = Some(0);
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    pub fn command_prefix(&self) -> Option<&str> {
        self.command_selection?;
        (self.text.starts_with('/') && !self.text.contains(char::is_whitespace))
            .then_some(self.text())
    }

    pub fn matching_commands<'a>(
        prefix: &str,
        templates: &'a [Template],
        main: bool,
    ) -> Vec<&'a Template> {
        let builtin = |t: &Template| main && BUILTINS.iter().any(|b| b.usage == t.usage);
        let mut matches: Vec<_> = templates
            .iter()
            .filter(|t| t.name().starts_with(prefix) && !builtin(t))
            .collect();
        for b in BUILTINS
            .iter()
            .filter(|b| main && b.name().starts_with(prefix))
        {
            let index = matches.partition_point(|t| t.name() < b.name());
            matches.insert(index, b);
        }
        matches
    }

    pub fn select_command(&mut self, index: usize) {
        if let Some(selected) = &mut self.command_selection {
            *selected = index;
        }
    }

    pub fn command_index(&self, matches: &[&Template]) -> usize {
        self.command_selection
            .unwrap_or_default()
            .min(matches.len().saturating_sub(1))
    }

    /// Returns false for Enter on an argument-free command so the caller
    /// submits the completed name through the usual prompt handler.
    pub fn handle_command_key(&mut self, key: &KeyEvent, matches: &[&Template]) -> bool {
        let selected = self.command_index(matches);
        let Some(command) = matches.get(selected) else {
            return false;
        };
        match key.code {
            KeyCode::Up => self.command_selection = Some(selected.saturating_sub(1)),
            KeyCode::Down => self.command_selection = Some((selected + 1).min(matches.len() - 1)),
            KeyCode::Esc => self.command_selection = None,
            KeyCode::Tab | KeyCode::Enter => {
                let name = command.name();
                let takes_args = name != command.usage;
                self.clear();
                self.insert_str(name);
                if takes_args {
                    self.insert_str(" ");
                }
                self.command_selection = None;
                return takes_args || key.code == KeyCode::Tab;
            }
            _ => return false,
        }
        true
    }

    fn delete_range(&mut self, start: usize, end: usize) {
        if start < end {
            self.text.replace_range(start..end, "");
            self.cursor = start;
            self.command_selection = Some(0);
        }
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor)
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }

    fn column(&self) -> usize {
        self.text[self.line_start()..self.cursor].chars().count()
    }

    fn move_to_column(&mut self, start: usize, column: usize) {
        let end = self.text[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or(self.text.len());
        self.cursor = self.text[start..end]
            .char_indices()
            .nth(column)
            .map(|(i, _)| start + i)
            .unwrap_or(end);
    }

    fn move_up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let column = self.column();
        let prev_start = self.text[..start - 1]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.move_to_column(prev_start, column);
        true
    }

    fn move_down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let column = self.column();
        self.move_to_column(end + 1, column);
        true
    }

    fn recall(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_pos, older) {
            (None, true) => Some(self.history.len() - 1),
            (None, false) => None,
            (Some(0), true) => Some(0),
            (Some(i), true) => Some(i - 1),
            (Some(i), false) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), false) => None,
        };
        self.command_selection = None;
        self.history_pos = next;
        self.text = next.map(|i| self.history[i].clone()).unwrap_or_default();
        self.cursor = self.text.len();
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> ComposerAction {
        if key.kind == KeyEventKind::Release {
            return ComposerAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => return ComposerAction::Cancel,
            KeyCode::Enter if alt || shift => self.insert_str("\n"),
            KeyCode::Char('j') if ctrl => self.insert_str("\n"),
            KeyCode::Enter => {
                let targets = self.selected();
                if self.text.trim().is_empty() || targets.is_empty() {
                    return ComposerAction::None;
                }
                let text = std::mem::take(&mut self.text);
                self.cursor = 0;
                if self.history.last() != Some(&text) {
                    self.history.push(text.clone());
                }
                self.history_pos = None;
                return ComposerAction::Submit { text, targets };
            }
            KeyCode::Char(c) if alt && c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap() as usize;
                if n == 0 {
                    self.toggle_all();
                } else {
                    self.toggle(n - 1);
                }
            }
            KeyCode::Char('a') if ctrl => self.toggle_all(),
            KeyCode::Char('u') if ctrl => {
                let start = self.line_start();
                self.delete_range(start, self.cursor);
            }
            KeyCode::Char('k') if ctrl => {
                let end = self.line_end();
                self.delete_range(self.cursor, end);
            }
            KeyCode::Char('w') if ctrl => {
                let before = &self.text[..self.cursor];
                let trimmed = before.trim_end_matches(|c: char| c.is_whitespace());
                let start = trimmed
                    .rfind(|c: char| c.is_whitespace())
                    .map(|i| i + 1)
                    .unwrap_or(0);
                self.delete_range(start, self.cursor);
            }
            KeyCode::Char(c) if !ctrl => {
                let mut buf = [0u8; 4];
                self.insert_str(c.encode_utf8(&mut buf));
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let prev = self.prev_boundary();
                self.delete_range(prev, self.cursor);
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                let next = self.next_boundary();
                self.delete_range(self.cursor, next);
            }
            KeyCode::Left => self.cursor = self.prev_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Up => {
                if !self.move_up() && (self.text.is_empty() || self.history_pos.is_some()) {
                    self.recall(true);
                }
            }
            KeyCode::Down => {
                if !self.move_down() && self.history_pos.is_some() {
                    self.recall(false);
                }
            }
            _ => {}
        }
        ComposerAction::None
    }

    pub fn set_cursor_line_col(&mut self, line: usize, col: usize) {
        let start = self
            .text
            .split_inclusive('\n')
            .take(line)
            .map(str::len)
            .sum::<usize>()
            .min(self.text.len());
        self.move_to_column(start, col);
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        let line = self.text[..self.cursor].matches('\n').count();
        (line, self.column())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(c: &mut Composer, code: KeyCode, mods: KeyModifiers) -> ComposerAction {
        c.handle_key(&KeyEvent::new(code, mods))
    }

    fn typed(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            press(c, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    #[test]
    fn command_filter_requires_a_single_slash_word() {
        let mut c = Composer::new(1);
        for text in [
            "", "review", "see /r", " /r", "/r ", "/r\n", "/r\t", "/r\u{a0}",
        ] {
            c.clear();
            c.insert_str(text);
            assert_eq!(c.command_prefix(), None, "{text:?}");
        }
        let templates = crate::commands::list(&Default::default());
        for (prefix, names) in [
            ("/r", vec!["/review", "/review-full"]),
            ("/rev", vec!["/review", "/review-full"]),
            ("/review-f", vec!["/review-full"]),
            ("/unknown", vec![]),
        ] {
            c.clear();
            c.insert_str(prefix);
            let matches =
                Composer::matching_commands(c.command_prefix().unwrap(), &templates, false);
            assert_eq!(matches.iter().map(|t| t.name()).collect::<Vec<_>>(), names);
        }
        assert_eq!(
            Composer::matching_commands("/he", &templates, true)[0].name(),
            "/help"
        );
        assert!(Composer::matching_commands("/he", &templates, false).is_empty());
        for (prefix, names) in [("/e", vec!["/exit"]), ("/q", vec!["/quit"])] {
            let matches = Composer::matching_commands(prefix, &templates, true);
            assert_eq!(matches.iter().map(|t| t.name()).collect::<Vec<_>>(), names);
            assert!(Composer::matching_commands(prefix, &templates, false).is_empty());
        }
    }

    #[test]
    fn command_selection_resets_on_edit_and_completes_the_selected_name() {
        let templates = crate::commands::list(&Default::default());
        let mut c = Composer::new(1);
        c.insert_str("/r");
        let choose = |c: &mut Composer, code| {
            let matches =
                Composer::matching_commands(c.command_prefix().unwrap(), &templates, true);
            c.handle_command_key(&KeyEvent::new(code, KeyModifiers::NONE), &matches)
        };
        assert!(choose(&mut c, KeyCode::Down));
        c.insert_str("e");
        assert!(choose(&mut c, KeyCode::Enter));
        assert_eq!(c.text(), "/review ");
        assert!(c.command_prefix().is_none());

        c.clear();
        c.insert_str("/h");
        assert!(choose(&mut c, KeyCode::Tab));
        assert_eq!(c.text(), "/help");
        assert!(c.command_prefix().is_none());
    }

    #[test]
    fn submit_returns_text_and_targets() {
        let mut c = Composer::new(4);
        c.open(&[0, 2]);
        typed(&mut c, "hi");
        assert_eq!(
            press(&mut c, KeyCode::Enter, KeyModifiers::NONE),
            ComposerAction::Submit {
                text: "hi".into(),
                targets: vec![0, 2]
            }
        );
        assert_eq!(c.text(), "");
    }

    #[test]
    fn empty_text_or_no_targets_does_not_submit() {
        let mut c = Composer::new(2);
        c.open(&[0]);
        assert_eq!(
            press(&mut c, KeyCode::Enter, KeyModifiers::NONE),
            ComposerAction::None
        );
        typed(&mut c, "x");
        c.toggle(0);
        assert_eq!(
            press(&mut c, KeyCode::Enter, KeyModifiers::NONE),
            ComposerAction::None
        );
    }

    #[test]
    fn multiline_editing() {
        let mut c = Composer::new(1);
        c.open(&[0]);
        typed(&mut c, "ab");
        press(&mut c, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut c, "cd");
        press(&mut c, KeyCode::Char('j'), KeyModifiers::CONTROL);
        typed(&mut c, "ef");
        assert_eq!(c.text(), "ab\ncd\nef");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.cursor_line_col(), (1, 2));
        press(&mut c, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(c.text(), "ab\nc\nef");
        press(&mut c, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(c.text(), "ab\n\nef");
    }

    #[test]
    fn unicode_cursor_movement() {
        let mut c = Composer::new(1);
        typed(&mut c, "zażółć");
        press(&mut c, KeyCode::Left, KeyModifiers::NONE);
        press(&mut c, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(c.text(), "zażóć");
    }

    #[test]
    fn target_toggles() {
        let mut c = Composer::new(3);
        c.open(&[1]);
        press(&mut c, KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!(c.selected(), [0, 1]);
        press(&mut c, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(c.selected(), [0, 1, 2]);
        press(&mut c, KeyCode::Char('0'), KeyModifiers::ALT);
        assert!(c.selected().is_empty());
    }

    #[test]
    fn up_does_not_replace_text_being_edited() {
        let mut c = Composer::new(1);
        c.open(&[0]);
        typed(&mut c, "first");
        press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        typed(&mut c, "a long draft");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "a long draft");
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "a long draft");
    }

    #[test]
    fn set_cursor_line_col_moves_within_lines() {
        let mut c = Composer::new(1);
        typed(&mut c, "ab");
        press(&mut c, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut c, "cdef");
        c.set_cursor_line_col(1, 2);
        assert_eq!(c.cursor_line_col(), (1, 2));
        c.set_cursor_line_col(0, 9);
        assert_eq!(c.cursor_line_col(), (0, 2));
    }

    #[test]
    fn history_recall() {
        let mut c = Composer::new(1);
        c.open(&[0]);
        typed(&mut c, "first");
        press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        typed(&mut c, "second");
        press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "second");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "first");
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "second");
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "");
    }

    #[test]
    fn set_history_is_recalled_newest_first() {
        let mut c = Composer::new(1);
        c.open(&[0]);
        c.set_history(vec!["old".into(), "new".into()]);
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "new");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "old");
        assert_eq!(c.history(), ["old", "new"]);
    }

    #[test]
    fn paste_normalizes_line_endings() {
        let mut c = Composer::new(1);
        c.insert_str("a\r\nb\rc");
        assert_eq!(c.text(), "a\nb\nc");
    }
}
