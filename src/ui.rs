use std::{
    fs::OpenOptions,
    io::{self, BufWriter, Write},
    sync::{
        Arc, Mutex,
        mpsc::{self, TryRecvError},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
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

use crate::{PickerDocument, PickerEngine, PickerItem, PickerResult};

const EVENT_INTERVAL: Duration = Duration::from_millis(16);
/// Large enough to hold a full-screen frame so each draw is one write.
const TERMINAL_BUFFER_BYTES: usize = 64 * 1024;
const ACCENT: Color = Color::Rgb(196, 167, 231);
const MUTED: Color = Color::Rgb(110, 106, 134);
const SUBTLE: Color = Color::Rgb(144, 140, 170);
const TEXT: Color = Color::Rgb(224, 222, 244);
const SUCCESS: Color = Color::Rgb(49, 116, 143);

/// Adapter-provided lazy preview loader.
pub trait PreviewProvider: Send + Sync + 'static {
    fn load(&self, item: &PickerItem) -> Result<Option<String>>;
}

/// Runs a picker in the current terminal.
///
/// `Ok(None)` means the user cancelled. Terminal state is restored even when
/// rendering or input handling fails.
pub fn run_picker(document: PickerDocument) -> Result<Option<PickerResult>> {
    run_picker_with_handler(document, |_| Ok(None))
}

/// Runs a picker and lets an adapter handle accepted actions before the UI exits.
///
/// Returning a replacement document keeps the picker open, preserves the query,
/// and atomically swaps the candidate set. Returning `None` accepts the result.
///
/// Alternate actions are delivered even when no row is highlighted, so
/// row-independent actions such as refreshes keep working on an empty list.
/// Such results have an empty `selected` list; returning `None` for them
/// leaves the picker open instead of producing an empty final result.
pub fn run_picker_with_handler<F>(
    document: PickerDocument,
    handler: F,
) -> Result<Option<PickerResult>>
where
    F: FnMut(&PickerResult) -> Result<Option<PickerDocument>>,
{
    run_picker_runtime(document, handler, || Ok(None))
}

/// Runs a picker with both accepted-action handling and asynchronous document updates.
///
/// The update callback is polled between frames. This lets adapters show the
/// terminal immediately, discover candidates on a worker thread, and replace
/// the loading document without blocking keyboard input or rendering.
pub fn run_picker_runtime<F, U>(
    document: PickerDocument,
    handler: F,
    updates: U,
) -> Result<Option<PickerResult>>
where
    F: FnMut(&PickerResult) -> Result<Option<PickerDocument>>,
    U: FnMut() -> Result<Option<PickerDocument>>,
{
    run_picker_runtime_with_preview(document, handler, updates, None)
}

/// Runs a picker with an optional lazy preview provider.
pub fn run_picker_runtime_with_preview<F, U>(
    document: PickerDocument,
    mut handler: F,
    mut updates: U,
    preview_provider: Option<Arc<dyn PreviewProvider>>,
) -> Result<Option<PickerResult>>
where
    F: FnMut(&PickerResult) -> Result<Option<PickerDocument>>,
    U: FnMut() -> Result<Option<PickerDocument>>,
{
    run_core(
        document,
        preview_provider,
        None,
        || Ok(updates()?.map(Update::Document)),
        |result| {
            Ok(match handler(result)? {
                Some(document) => Dispatch::Replace(document),
                None => Dispatch::Finish,
            })
        },
    )
}

/// An adapter whose slow actions can run without freezing the picker.
///
/// The picker keeps rendering and accepting query edits, navigation, and
/// cancellation while a background action runs; other actions and Enter are
/// ignored until it finishes. The returned status is shown in the footer.
pub trait ActionHandler: Send + 'static {
    /// Applies an action; see [`run_picker_with_handler`] for the meaning of
    /// the returned document.
    fn handle_result(&mut self, result: &PickerResult) -> Result<Option<PickerDocument>>;

    /// Returns a status message when `result` should run on a worker thread,
    /// or `None` to run it inline. Actions that need the terminal, such as
    /// launching an editor, must run inline.
    fn background_status(&self, _result: &PickerResult) -> Option<String> {
        None
    }
}

type LoadFn<H> = Box<dyn FnOnce(&mut H) -> Result<PickerDocument> + Send>;

/// A document load that runs on a worker thread after the picker appears.
pub struct BackgroundLoad<H> {
    /// Footer status shown while loading.
    pub status: String,
    /// Produces the first real document.
    pub load: LoadFn<H>,
}

impl<H> BackgroundLoad<H> {
    pub fn new(
        status: impl Into<String>,
        load: impl FnOnce(&mut H) -> Result<PickerDocument> + Send + 'static,
    ) -> Self {
        Self {
            status: status.into(),
            load: Box::new(load),
        }
    }
}

/// Runs a picker whose adapter may perform slow work off the render thread.
///
/// `document` is shown immediately. When `initial_load` is provided it runs
/// on a worker thread and replaces that placeholder document when finished.
pub fn run_picker_with_actions<H: ActionHandler>(
    document: PickerDocument,
    handler: H,
    initial_load: Option<BackgroundLoad<H>>,
    preview_provider: Option<Arc<dyn PreviewProvider>>,
) -> Result<Option<PickerResult>> {
    let handler = Arc::new(Mutex::new(handler));
    let (sender, receiver) = mpsc::channel::<JobOutcome>();
    let mut pending = false;

    let initial_busy = initial_load.map(|load| {
        let handler = Arc::clone(&handler);
        let sender = sender.clone();
        thread::spawn(move || {
            let outcome = (load.load)(&mut lock_handler(&handler)).map(Some);
            let _ = sender.send(JobOutcome {
                result: None,
                outcome,
            });
        });
        pending = true;
        load.status
    });

    let poll_handler = Arc::clone(&handler);
    let pending = std::cell::Cell::new(pending);
    run_core(
        document,
        preview_provider,
        initial_busy,
        || {
            if !pending.get() {
                return Ok(None);
            }
            let job = match receiver.try_recv() {
                Ok(job) => job,
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    bail!("picker background worker exited without a result")
                }
            };
            pending.set(false);
            Ok(Some(match (job.outcome?, job.result) {
                (Some(document), _) => Update::Document(document),
                (None, Some(result)) if !result.selected.is_empty() => Update::Finish(result),
                (None, _) => Update::Idle,
            }))
        },
        |result| {
            let status = lock_handler(&poll_handler).background_status(result);
            let Some(status) = status else {
                return Ok(match lock_handler(&poll_handler).handle_result(result)? {
                    Some(document) => Dispatch::Replace(document),
                    None => Dispatch::Finish,
                });
            };
            let handler = Arc::clone(&poll_handler);
            let sender = sender.clone();
            let result = result.clone();
            thread::spawn(move || {
                let outcome = lock_handler(&handler).handle_result(&result);
                let _ = sender.send(JobOutcome {
                    result: Some(result),
                    outcome,
                });
            });
            pending.set(true);
            Ok(Dispatch::Busy(status))
        },
    )
}

fn lock_handler<H>(handler: &Mutex<H>) -> std::sync::MutexGuard<'_, H> {
    // A panicked worker already reported failure through the channel; the
    // adapter state is still the best information available.
    handler
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct JobOutcome {
    /// The action that started the job, or `None` for an initial load.
    result: Option<PickerResult>,
    outcome: Result<Option<PickerDocument>>,
}

/// Asynchronous changes delivered to a running picker.
enum Update {
    /// Replace the candidate document and clear any busy status.
    Document(PickerDocument),
    /// Close the picker with this accepted result.
    Finish(PickerResult),
    /// Background work finished without changing the document.
    Idle,
}

/// How the runtime should continue after an action.
enum Dispatch {
    Replace(PickerDocument),
    Finish,
    Busy(String),
}

enum Exit {
    Cancel,
    Action(PickerResult),
    Finish(PickerResult),
}

fn run_core<P, D>(
    document: PickerDocument,
    preview_provider: Option<Arc<dyn PreviewProvider>>,
    initial_busy: Option<String>,
    mut poll: P,
    mut dispatch: D,
) -> Result<Option<PickerResult>>
where
    P: FnMut() -> Result<Option<Update>>,
    D: FnMut(&PickerResult) -> Result<Dispatch>,
{
    document.validate().map_err(anyhow::Error::msg)?;

    let mut engine = PickerEngine::new(document.items, document.options.multi);
    engine.set_query(document.options.initial_query.clone());

    let mut terminal = TerminalSession::new()?;
    let mut app = PickerApp::new(engine, document.options, preview_provider);
    app.busy = initial_busy;
    loop {
        let result = match app.run(&mut terminal.terminal, &mut poll)? {
            Exit::Cancel => return Ok(None),
            Exit::Finish(result) => return Ok(Some(result)),
            Exit::Action(result) => result,
        };
        match dispatch(&result)? {
            Dispatch::Replace(replacement) => app.replace_document(replacement)?,
            Dispatch::Finish if result.selected.is_empty() => {}
            Dispatch::Finish => return Ok(Some(result)),
            Dispatch::Busy(status) => app.busy = Some(status),
        }
    }
}

/// Benchmark support: renders frames of a picker into an in-memory backend.
///
/// Not part of the stable API.
#[doc(hidden)]
pub struct FrameBench {
    app: PickerApp,
    terminal: Terminal<ratatui::backend::TestBackend>,
}

#[doc(hidden)]
impl FrameBench {
    pub fn new(
        document: PickerDocument,
        width: u16,
        height: u16,
        preview: Option<Arc<dyn PreviewProvider>>,
    ) -> Result<Self> {
        document.validate().map_err(anyhow::Error::msg)?;
        let mut engine = PickerEngine::new(document.items, document.options.multi);
        engine.settle();
        let app = PickerApp::new(engine, document.options, preview);
        let terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
        Ok(Self { app, terminal })
    }

    /// Sets a static preview shown beside the list, as a provider would.
    pub fn set_preview(&mut self, preview: String) {
        self.app.preview_loading = false;
        self.app.dynamic_preview = Some(preview);
    }

    pub fn engine(&mut self) -> &mut PickerEngine {
        &mut self.app.engine
    }

    pub fn draw(&mut self) -> Result<()> {
        self.terminal
            .draw(|frame| self.app.render(frame))
            .context("render benchmark frame")?;
        Ok(())
    }
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
    // Crossterm emits many small escape sequences per frame; buffering turns
    // them into a single write instead of one syscall per cell run. Ratatui
    // flushes the backend after every draw, and `execute!` flushes as well.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map(|tty| Box::new(BufWriter::with_capacity(TERMINAL_BUFFER_BYTES, tty)) as TerminalWriter)
        .unwrap_or_else(|_| {
            Box::new(BufWriter::with_capacity(
                TERMINAL_BUFFER_BYTES,
                io::stderr(),
            ))
        })
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

struct PreviewRequest {
    generation: u64,
    item: PickerItem,
}

struct PreviewResponse {
    generation: u64,
    item_id: String,
    preview: Result<Option<String>>,
}

struct PreviewWorker {
    requests: mpsc::Sender<PreviewRequest>,
    responses: mpsc::Receiver<PreviewResponse>,
}

impl PreviewWorker {
    fn new(provider: Arc<dyn PreviewProvider>) -> Self {
        let (request_sender, request_receiver) = mpsc::channel::<PreviewRequest>();
        let (response_sender, response_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(mut request) = request_receiver.recv() {
                while let Ok(newer) = request_receiver.try_recv() {
                    request = newer;
                }
                let item_id = request.item.id.clone();
                let preview = provider.load(&request.item);
                if response_sender
                    .send(PreviewResponse {
                        generation: request.generation,
                        item_id,
                        preview,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            requests: request_sender,
            responses: response_receiver,
        }
    }
}

struct PickerApp {
    engine: PickerEngine,
    options: crate::PickerOptions,
    query_cursor: usize,
    preview_worker: Option<PreviewWorker>,
    preview_generation: u64,
    preview_item_id: Option<String>,
    dynamic_preview: Option<String>,
    preview_loading: bool,
    /// Status of running background work; actions are paused while set.
    busy: Option<String>,
}

impl PickerApp {
    fn new(
        engine: PickerEngine,
        options: crate::PickerOptions,
        preview_provider: Option<Arc<dyn PreviewProvider>>,
    ) -> Self {
        let query_cursor = options.initial_query.chars().count();
        Self {
            engine,
            options,
            query_cursor,
            preview_worker: preview_provider.map(PreviewWorker::new),
            preview_generation: 0,
            preview_item_id: None,
            dynamic_preview: None,
            preview_loading: false,
            busy: None,
        }
    }

    fn replace_document(&mut self, document: PickerDocument) -> Result<()> {
        document.validate().map_err(anyhow::Error::msg)?;
        self.engine
            .replace_items(document.items, document.options.multi);
        self.options = document.options;
        self.query_cursor = self.engine.query().chars().count();
        self.preview_item_id = None;
        self.dynamic_preview = None;
        self.preview_loading = false;
        Ok(())
    }

    fn run<P>(&mut self, terminal: &mut PickerTerminal, poll: &mut P) -> Result<Exit>
    where
        P: FnMut() -> Result<Option<Update>>,
    {
        let mut dirty = true;
        loop {
            match poll()? {
                Some(Update::Document(document)) => {
                    self.replace_document(document)?;
                    self.busy = None;
                    dirty = true;
                }
                Some(Update::Finish(result)) => return Ok(Exit::Finish(result)),
                Some(Update::Idle) => {
                    self.busy = None;
                    dirty = true;
                }
                None => {}
            }
            dirty |= self.engine.tick(2).changed;
            dirty |= self.poll_preview();
            dirty |= self.request_preview();
            // Idle frames are skipped entirely: no layout, no buffer diff, and
            // no terminal I/O unless something visible may have changed.
            if dirty {
                terminal
                    .draw(|frame| self.render(frame))
                    .context("render picker")?;
                dirty = false;
            }

            if !event::poll(EVENT_INTERVAL).context("poll terminal input")? {
                continue;
            }

            // Drain every queued event before rendering again so bursts such
            // as key repeat or fast typing cost one frame, not one per key.
            loop {
                match event::read().context("read terminal input")? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        if let Some(outcome) = self.handle_key(key) {
                            return Ok(match outcome {
                                Some(result) => Exit::Action(result),
                                None => Exit::Cancel,
                            });
                        }
                        dirty = true;
                    }
                    Event::Paste(text) => {
                        self.insert_text(&text);
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
                if !event::poll(Duration::ZERO).context("poll terminal input")? {
                    break;
                }
            }
        }
    }

    /// Requests a preview for the highlighted item. Returns whether preview
    /// state changed and needs to be redrawn.
    fn request_preview(&mut self) -> bool {
        let Some(worker) = &self.preview_worker else {
            return false;
        };
        let item = if self.options.preview_visible {
            self.engine.current_item()
        } else {
            None
        };
        let Some(item) = item else {
            let changed = self.preview_item_id.is_some()
                || self.dynamic_preview.is_some()
                || self.preview_loading;
            self.preview_item_id = None;
            self.dynamic_preview = None;
            self.preview_loading = false;
            return changed;
        };
        if self.preview_item_id.as_deref() == Some(item.id.as_str()) {
            return false;
        }

        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_item_id = Some(item.id.clone());
        self.dynamic_preview = None;
        self.preview_loading = true;
        let _ = worker.requests.send(PreviewRequest {
            generation: self.preview_generation,
            item: item.clone(),
        });
        true
    }

    /// Applies finished preview responses. Returns whether one was applied.
    fn poll_preview(&mut self) -> bool {
        let Some(worker) = &self.preview_worker else {
            return false;
        };
        let mut changed = false;
        while let Ok(response) = worker.responses.try_recv() {
            if response.generation != self.preview_generation
                || self.preview_item_id.as_deref() != Some(response.item_id.as_str())
            {
                continue;
            }
            self.preview_loading = false;
            self.dynamic_preview = match response.preview {
                Ok(preview) => preview,
                Err(error) => Some(format!("Preview unavailable: {error:#}")),
            };
            changed = true;
        }
        changed
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Option<PickerResult>> {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
        {
            return Some(None);
        }

        if let Some(action) = self.action_for_key(key) {
            if self.busy.is_some() {
                return None;
            }
            return Some(Some(self.engine.action_result(action)));
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) if self.busy.is_some() => None,
            // Enter with no matching row keeps the picker open rather than
            // silently exiting as though the user had cancelled.
            (KeyCode::Enter, _) => self.engine.result("accept").map(Some),
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
        let header_height = usize::from(self.options.header.is_some()) as u16;
        let footer_lines = self.footer_lines(area.width as usize);
        let footer_height = footer_lines.len() as u16;
        let layout = Layout::vertical([
            Constraint::Length(title_height),
            Constraint::Length(3),
            Constraint::Length(header_height),
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
        if let Some(header) = &self.options.header {
            frame.render_widget(
                Paragraph::new(header.as_str()).style(Style::default().fg(MUTED)),
                layout[2],
            );
        }
        self.render_content(frame, layout[3]);
        self.render_footer(frame, layout[4], &footer_lines);
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
        let cursor_prefix = &query[..char_to_byte_index(query, self.query_cursor)];
        let cursor_x = inner
            .x
            .saturating_add((prefix_width + cursor_prefix.width()) as u16)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, inner.y));
    }

    fn render_content(&self, frame: &mut Frame<'_>, area: Rect) {
        let preview = if !self.options.preview_visible {
            None
        } else if self.preview_worker.is_some() {
            if self.preview_loading {
                Some("Loading preview…")
            } else {
                self.dynamic_preview.as_deref()
            }
        } else {
            self.engine
                .current_item()
                .and_then(|item| item.preview.as_deref())
        };
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
            let visible = visible_preview(preview, block.inner(areas[1]).height);
            frame.render_widget(
                Paragraph::new(visible)
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
            let cursor = if current {
                Span::styled("› ", Style::default().fg(ACCENT))
            } else {
                Span::raw("  ")
            };
            let selection = if self.options.multi {
                if self.engine.is_matched_selected(index) {
                    Span::styled("✓ ", Style::default().fg(SUCCESS))
                } else {
                    Span::raw("  ")
                }
            } else {
                Span::raw("")
            };
            let label_style = if current {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            let mut line = vec![cursor, selection, Span::styled(&item.label, label_style)];
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

    fn footer_parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(busy) = &self.busy {
            parts.push(format!("⟳ {busy}"));
        }
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
        parts
    }

    fn footer_lines(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return vec![String::new()];
        }
        let count = format!(
            "{}/{}",
            self.engine
                .cursor()
                .saturating_add(1)
                .min(self.engine.match_count()),
            self.engine.match_count()
        );
        let count = truncate_to_width(&count, width);
        let first_capacity = width.saturating_sub(count.width() + 1);
        let mut lines = pack_footer_parts(&self.footer_parts(), first_capacity, width);
        let first = lines.first_mut().expect("footer always has a first line");
        let gap = width.saturating_sub(first.width() + count.width());
        first.push_str(&" ".repeat(gap));
        first.push_str(&count);
        lines
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, lines: &[String]) {
        let lines = lines
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(MUTED))))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// Returns the prefix of `preview` that can appear in `height` rows.
///
/// Each source line occupies at least one wrapped row, so lines past
/// `height` are never visible. Trimming them first keeps a 1,000-line preview
/// from being converted and wrapped in full on every frame.
fn visible_preview(preview: &str, height: u16) -> &str {
    let height = usize::from(height);
    if height == 0 {
        return "";
    }
    match preview.match_indices('\n').nth(height - 1) {
        Some((index, _)) => &preview[..index],
        None => preview,
    }
}

fn pack_footer_parts(parts: &[String], first_width: usize, full_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut capacity = first_width;
    if capacity == 0 {
        lines.push(String::new());
        capacity = full_width;
    }

    for part in parts {
        let separator = if current.is_empty() { "" } else { " · " };
        let candidate_width = current.width() + separator.width() + part.width();
        if !current.is_empty() && candidate_width > capacity {
            lines.push(current);
            current = String::new();
            capacity = full_width;
        }
        if !current.is_empty() {
            current.push_str(" · ");
        }
        current.push_str(&truncate_to_width(
            part,
            capacity.saturating_sub(current.width()),
        ));
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
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
    let mut used = 0;
    for character in value.chars() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > target {
            break;
        }
        used += character_width;
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
    fn preview_keeps_only_lines_that_fit() {
        assert_eq!(visible_preview("a\nb\nc\n", 2), "a\nb");
        assert_eq!(visible_preview("a\nb", 5), "a\nb");
        assert_eq!(visible_preview("a\nb", 0), "");
    }

    #[test]
    fn truncation_respects_visible_width() {
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("界界界", 5), "界界…");
    }

    #[test]
    fn footer_wraps_without_dropping_file_picker_hints() {
        let mut options = crate::PickerOptions {
            footer: Some("hidden off   ignored on".to_owned()),
            multi: true,
            ..Default::default()
        };
        options.actions = [
            ("toggle-hidden", "alt-.", "hidden"),
            ("toggle-ignored", "alt-i", "ignored"),
            ("toggle-preview", "alt-v", "preview"),
            ("copy-paths", "alt-y", "copy paths"),
            ("reveal", "alt-o", "reveal"),
        ]
        .into_iter()
        .map(|(id, key, label)| crate::PickerAction {
            id: id.to_owned(),
            key: key.to_owned(),
            label: label.to_owned(),
        })
        .collect();
        let mut engine = PickerEngine::new(
            vec![PickerItem {
                id: "file".to_owned(),
                label: "file.txt".to_owned(),
                description: None,
                search_text: None,
                preview: None,
                metadata: serde_json::Value::Null,
            }],
            true,
        );
        engine.settle();
        let app = PickerApp::new(engine, options, None);

        let lines = app.footer_lines(80);
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|line| line.width() <= 80));
        assert!(lines[0].ends_with("1/1"));
        let rendered = lines.join(" ");
        for hint in [
            "hidden off",
            "ignored on",
            "tab select",
            "alt-. hidden",
            "alt-i ignored",
            "alt-v preview",
            "alt-y copy paths",
            "alt-o reveal",
            "enter accept",
            "esc cancel",
        ] {
            assert!(rendered.contains(hint), "missing footer hint {hint:?}");
        }
    }

    #[test]
    fn empty_list_keeps_actions_available_and_ignores_enter() {
        let options = crate::PickerOptions {
            actions: vec![crate::PickerAction {
                id: "refresh".to_owned(),
                key: "ctrl-r".to_owned(),
                label: "refresh".to_owned(),
            }],
            ..Default::default()
        };
        let mut engine = PickerEngine::new(Vec::new(), false);
        engine.settle();
        let mut app = PickerApp::new(engine, options, None);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None,
            "Enter on an empty list must not exit the picker"
        );
        let outcome = app
            .handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .expect("action exits the input loop")
            .expect("action is not a cancellation");
        assert_eq!(outcome.action, "refresh");
        assert!(outcome.selected.is_empty());
    }

    #[test]
    fn busy_status_pauses_actions_but_not_editing() {
        let options = crate::PickerOptions {
            actions: vec![crate::PickerAction {
                id: "refresh".to_owned(),
                key: "ctrl-r".to_owned(),
                label: "refresh".to_owned(),
            }],
            ..Default::default()
        };
        let mut engine = PickerEngine::new(
            vec![PickerItem {
                id: "a".to_owned(),
                label: "alpha".to_owned(),
                description: None,
                search_text: None,
                preview: None,
                metadata: serde_json::Value::Null,
            }],
            false,
        );
        engine.settle();
        let mut app = PickerApp::new(engine, options, None);
        app.busy = Some("Refreshing".to_owned());

        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(app.engine.query(), "a");
        assert!(app.footer_lines(80)[0].starts_with("⟳ Refreshing"));
        assert_eq!(
            app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(None)
        );
    }
}
