use std::{
    fs::OpenOptions,
    io::{self, Write},
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{PickerDocument, PickerEngine, PickerResult};

const EVENT_INTERVAL: Duration = Duration::from_millis(16);
const ACCENT: Color = Color::Rgb(196, 167, 231);
const MUTED: Color = Color::Rgb(110, 106, 134);
const SUBTLE: Color = Color::Rgb(144, 140, 170);
const TEXT: Color = Color::Rgb(224, 222, 244);
const SUCCESS: Color = Color::Rgb(49, 116, 143);

/// Runs a picker in the current terminal.
///
/// `Ok(None)` means the user cancelled. Terminal state is restored even when
/// rendering or input handling fails.
pub fn run_picker(document: PickerDocument) -> Result<Option<PickerResult>> {
    document.validate().map_err(anyhow::Error::msg)?;

    let mut engine = PickerEngine::new(document.items, document.options.multi);
    engine.set_query(document.options.initial_query.clone());

    let mut terminal = TerminalSession::new()?;
    let mut app = PickerApp::new(engine, document.options);
    app.run(&mut terminal.terminal)
}

type TerminalWriter = Box<dyn Write>;
type PickerTerminal = Terminal<CrosstermBackend<TerminalWriter>>;

struct TerminalSession {
    terminal: PickerTerminal,
}

impl TerminalSession {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut writer = terminal_writer();
        if let Err(error) = execute!(writer, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error).context("enter alternate screen");
        }

        let backend = CrosstermBackend::new(writer);
        match Terminal::new(backend) {
            Ok(mut terminal) => match terminal.clear() {
                Ok(()) => Ok(Self { terminal }),
                Err(error) => {
                    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
                    let _ = disable_raw_mode();
                    Err(error).context("clear terminal")
                }
            },
            Err(error) => {
                let mut writer = terminal_writer();
                let _ = execute!(writer, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error).context("initialize terminal")
            }
        }
    }
}

fn terminal_writer() -> TerminalWriter {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map(|tty| Box::new(tty) as TerminalWriter)
        .unwrap_or_else(|_| Box::new(io::stderr()))
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

struct PickerApp {
    engine: PickerEngine,
    options: crate::PickerOptions,
    query_cursor: usize,
}

impl PickerApp {
    fn new(engine: PickerEngine, options: crate::PickerOptions) -> Self {
        let query_cursor = options.initial_query.chars().count();
        Self {
            engine,
            options,
            query_cursor,
        }
    }

    fn run(&mut self, terminal: &mut PickerTerminal) -> Result<Option<PickerResult>> {
        loop {
            self.engine.tick(2);
            terminal
                .draw(|frame| self.render(frame))
                .context("render picker")?;

            if !event::poll(EVENT_INTERVAL).context("poll terminal input")? {
                continue;
            }

            match event::read().context("read terminal input")? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let Some(outcome) = self.handle_key(key) {
                        return Ok(outcome);
                    }
                }
                Event::Paste(text) => self.insert_text(&text),
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Option<PickerResult>> {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
        {
            return Some(None);
        }

        if let Some(action) = self.action_for_key(key) {
            return Some(self.engine.result(action));
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => Some(self.engine.result("accept")),
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.engine.move_previous();
                None
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.engine.move_next();
                None
            }
            (KeyCode::PageUp, _) => {
                self.engine.move_page(-1, self.options.max_visible);
                None
            }
            (KeyCode::PageDown, _) => {
                self.engine.move_page(1, self.options.max_visible);
                None
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.engine
                    .move_page(-1, self.options.max_visible.saturating_div(2).max(1));
                None
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.engine
                    .move_page(1, self.options.max_visible.saturating_div(2).max(1));
                None
            }
            (KeyCode::Tab, _) if self.options.multi => {
                if self.engine.toggle_current() {
                    self.engine.move_next();
                }
                None
            }
            (KeyCode::Left, _) | (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                self.query_cursor = self.query_cursor.saturating_sub(1);
                None
            }
            (KeyCode::Right, _) | (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                self.query_cursor = (self.query_cursor + 1).min(self.query_len());
                None
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.query_cursor = 0;
                None
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.query_cursor = self.query_len();
                None
            }
            (KeyCode::Backspace, _) => {
                self.remove_before_cursor();
                None
            }
            (KeyCode::Delete, _) => {
                self.remove_at_cursor();
                None
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                self.remove_word_before_cursor();
                None
            }
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                self.truncate_query_at_cursor();
                None
            }
            (KeyCode::Char(character), modifiers)
                if !modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.insert_text(&character.to_string());
                None
            }
            _ => None,
        }
    }

    fn action_for_key(&self, key: KeyEvent) -> Option<String> {
        let key = canonical_key(key)?;
        self.options
            .actions
            .iter()
            .find(|action| action.key.eq_ignore_ascii_case(&key))
            .map(|action| action.id.clone())
    }

    fn query_len(&self) -> usize {
        self.engine.query().chars().count()
    }

    fn insert_text(&mut self, text: &str) {
        let byte_index = char_to_byte_index(self.engine.query(), self.query_cursor);
        let mut query = self.engine.query().to_owned();
        query.insert_str(byte_index, text);
        self.query_cursor += text.chars().count();
        self.engine.set_query(query);
    }

    fn remove_before_cursor(&mut self) {
        if self.query_cursor == 0 {
            return;
        }
        let mut query = self.engine.query().to_owned();
        let start = char_to_byte_index(&query, self.query_cursor - 1);
        let end = char_to_byte_index(&query, self.query_cursor);
        query.replace_range(start..end, "");
        self.query_cursor -= 1;
        self.engine.set_query(query);
    }

    fn remove_at_cursor(&mut self) {
        if self.query_cursor >= self.query_len() {
            return;
        }
        let mut query = self.engine.query().to_owned();
        let start = char_to_byte_index(&query, self.query_cursor);
        let end = char_to_byte_index(&query, self.query_cursor + 1);
        query.replace_range(start..end, "");
        self.engine.set_query(query);
    }

    fn remove_word_before_cursor(&mut self) {
        if self.query_cursor == 0 {
            return;
        }
        let characters: Vec<char> = self.engine.query().chars().collect();
        let mut start = self.query_cursor;
        while start > 0 && characters[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !characters[start - 1].is_whitespace() {
            start -= 1;
        }

        let mut query = self.engine.query().to_owned();
        let start_byte = char_to_byte_index(&query, start);
        let end_byte = char_to_byte_index(&query, self.query_cursor);
        query.replace_range(start_byte..end_byte, "");
        self.query_cursor = start;
        self.engine.set_query(query);
    }

    fn truncate_query_at_cursor(&mut self) {
        let mut query = self.engine.query().to_owned();
        query.truncate(char_to_byte_index(&query, self.query_cursor));
        self.engine.set_query(query);
    }

    fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let title_height = usize::from(self.options.title.is_some()) as u16;
        let footer_height = 1;
        let layout = Layout::vertical([
            Constraint::Length(title_height),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(area);

        if let Some(title) = &self.options.title {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    title,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )))
                .alignment(ratatui::layout::Alignment::Center),
                layout[0],
            );
        }

        self.render_query(frame, layout[1]);
        self.render_content(frame, layout[2]);
        self.render_footer(frame, layout[3]);
    }

    fn render_query(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let query = self.engine.query();
        let mut spans = vec![Span::styled(
            &self.options.prompt,
            Style::default().fg(ACCENT),
        )];
        if query.is_empty() {
            spans.push(Span::styled(
                &self.options.placeholder,
                Style::default().fg(MUTED),
            ));
        } else {
            spans.push(Span::styled(query, Style::default().fg(TEXT)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);

        let prefix_width = self.options.prompt.width();
        let cursor_prefix: String = query.chars().take(self.query_cursor).collect();
        let cursor_x = inner
            .x
            .saturating_add((prefix_width + cursor_prefix.width()) as u16)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, inner.y));
    }

    fn render_content(&self, frame: &mut Frame<'_>, area: Rect) {
        let preview = self
            .engine
            .current_item()
            .and_then(|item| item.preview.as_deref());
        let areas = if preview.is_some() && area.width >= 80 {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(100), Constraint::Percentage(0)])
                .split(area)
        };

        self.render_list(frame, areas[0]);
        if let Some(preview) = preview.filter(|_| areas[1].width > 0) {
            let block = Block::default()
                .title(" Preview ")
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(MUTED))
                .padding(Padding::horizontal(1));
            frame.render_widget(
                Paragraph::new(preview)
                    .style(Style::default().fg(SUBTLE))
                    .wrap(Wrap { trim: false })
                    .block(block),
                areas[1],
            );
        }
    }

    fn render_list(&self, frame: &mut Frame<'_>, area: Rect) {
        let visible = self
            .options
            .max_visible
            .min(area.height.saturating_sub(1) as usize)
            .max(1);
        let count = self.engine.match_count();
        let start = self
            .engine
            .cursor()
            .saturating_sub(visible / 2)
            .min(count.saturating_sub(visible));
        let end = (start + visible).min(count);

        let mut rows = Vec::with_capacity(end.saturating_sub(start).max(1));
        for index in start..end {
            let Some(item) = self.engine.matched_item(index) else {
                continue;
            };
            let current = index == self.engine.cursor();
            let marker = if self.options.multi {
                if self.engine.is_selected(&item.id) {
                    Span::styled("✓ ", Style::default().fg(SUCCESS))
                } else {
                    Span::raw("  ")
                }
            } else if current {
                Span::styled("› ", Style::default().fg(ACCENT))
            } else {
                Span::raw("  ")
            };
            let label_style = if current {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            let mut line = vec![marker, Span::styled(&item.label, label_style)];
            if let Some(description) = &item.description {
                line.push(Span::styled(
                    format!("  {description}"),
                    Style::default().fg(MUTED),
                ));
            }
            rows.push(ListItem::new(Line::from(line)));
        }

        if rows.is_empty() {
            rows.push(ListItem::new(Line::from(Span::styled(
                "  No matching items",
                Style::default().fg(MUTED),
            ))));
        }

        frame.render_widget(List::new(rows), area);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut parts = Vec::new();
        if let Some(footer) = &self.options.footer {
            parts.push(footer.clone());
        }
        if self.options.multi {
            parts.push("tab select".to_owned());
        }
        parts.extend(
            self.options
                .actions
                .iter()
                .map(|action| format!("{} {}", action.key, action.label)),
        );
        parts.push("enter accept".to_owned());
        parts.push("esc cancel".to_owned());

        let count = format!(
            "{}/{}",
            self.engine
                .cursor()
                .saturating_add(1)
                .min(self.engine.match_count()),
            self.engine.match_count()
        );
        let available = area.width.saturating_sub(count.width() as u16 + 1) as usize;
        let hints = truncate_to_width(&parts.join(" · "), available);
        let gap = area
            .width
            .saturating_sub((hints.width() + count.width()) as u16) as usize;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(hints, Style::default().fg(MUTED)),
                Span::raw(" ".repeat(gap)),
                Span::styled(count, Style::default().fg(MUTED)),
            ])),
            area,
        );
    }
}

fn char_to_byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map_or(value.len(), |(index, _)| index)
}

fn truncate_to_width(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }

    let ellipsis = "…";
    let target = width.saturating_sub(ellipsis.width());
    let mut output = String::new();
    for character in value.chars() {
        let mut candidate = output.clone();
        candidate.push(character);
        if candidate.width() > target {
            break;
        }
        output.push(character);
    }
    output.push_str(ellipsis);
    output
}

fn canonical_key(key: KeyEvent) -> Option<String> {
    let mut modifiers = Vec::with_capacity(3);
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers.push("shift");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        modifiers.push("super");
    }

    let code = match key.code {
        KeyCode::Char(character) => character.to_ascii_lowercase().to_string(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => "backtab".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::F(number) => format!("f{number}"),
        _ => return None,
    };

    if modifiers.is_empty() {
        Some(code)
    } else {
        Some(format!("{}-{code}", modifiers.join("-")))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEvent;

    use super::*;

    #[test]
    fn converts_character_positions_to_byte_offsets() {
        assert_eq!(char_to_byte_index("aéb", 0), 0);
        assert_eq!(char_to_byte_index("aéb", 1), 1);
        assert_eq!(char_to_byte_index("aéb", 2), 3);
        assert_eq!(char_to_byte_index("aéb", 3), 4);
    }

    #[test]
    fn formats_modified_keys_for_action_lookup() {
        assert_eq!(
            canonical_key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::CONTROL)),
            Some("ctrl-y".to_owned())
        );
        assert_eq!(
            canonical_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT)),
            Some("alt-o".to_owned())
        );
    }

    #[test]
    fn truncation_respects_visible_width() {
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("界界界", 5), "界界…");
    }
}
