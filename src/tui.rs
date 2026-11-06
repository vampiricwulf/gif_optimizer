//! Interactive screen shown when the program starts without arguments:
//! pick the options, run, and watch the progress.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::output::{Event, FileResult, Kind, Output, Sink};
use crate::progress::Stage;
use crate::{format_size, Options, LOG_FILE, OUTPUT_DIR, WORK_DIR};

const MAX_JOBS: usize = 64;

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Exact,
    Replace,
    Recursive,
    Gifsicle,
    Log,
    Force,
    Jobs,
    Run,
}

const ITEMS: [Item; 8] = [
    Item::Exact,
    Item::Replace,
    Item::Recursive,
    Item::Gifsicle,
    Item::Log,
    Item::Force,
    Item::Jobs,
    Item::Run,
];
const RUN: usize = ITEMS.len() - 1;
/// A faint background for the selected row, from the 256-color palette so
/// it does not depend on how the theme maps the 16 basic colors.
const SELECTED_BG: Color = Color::Indexed(237);

/// A clickable region from the last draw: clicking it selects `select`
/// (if any) and then acts as if `key` was pressed.
#[derive(Clone, Copy)]
struct Hit {
    area: Rect,
    select: Option<usize>,
    key: Option<KeyCode>,
}

/// A footer entry: a key chip with a description, or a plain note.
enum Hint {
    Key(&'static str, &'static str, Option<usize>, Option<KeyCode>),
    Note(&'static str),
}

/// Keeps mouse reporting on while alive, also when unwinding from a panic.
struct MouseCapture;

impl MouseCapture {
    fn enable() -> std::io::Result<Self> {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
}

/// A file being processed.
struct Active {
    index: usize,
    name: String,
    stage: Stage,
    done: usize,
    total: usize,
}

impl Active {
    /// Rough fraction of the file's work that is done. Reading is quick
    /// compared to optimizing, and gifsicle is only a final touch.
    fn fraction(&self) -> f64 {
        let part = if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).min(1.0)
        };
        match self.stage {
            Stage::Reading => 0.0,
            Stage::Optimizing => 0.9 * part,
            Stage::Gifsicle => 0.9 + 0.1 * part,
        }
    }

    fn label(&self) -> String {
        match (self.stage, self.total) {
            (Stage::Reading, _) => format!("reading, {} frames", self.done),
            (stage, 0) => stage.label().to_string(),
            (stage, total) => format!(
                "{} {:>3}%",
                stage.label(),
                (self.done * 100 / total).min(100)
            ),
        }
    }
}

/// A run in progress or just finished.
struct RunView {
    rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
    lines: Vec<(Kind, String)>,
    total: Option<usize>,
    /// Files that finished, successfully or not.
    done: usize,
    failed: usize,
    /// Files cancelled by a stop request.
    stopped: usize,
    /// Files in progress, by index.
    active: Vec<Active>,
    bytes_in: u64,
    bytes_out: u64,
    /// `Some(ok)` once the run is over.
    finished: Option<bool>,
    /// First visible log line, or `None` to follow the end.
    scroll: Option<usize>,
    log_height: usize,
}

struct App {
    options: Options,
    selected: usize,
    /// Asking to confirm a run that replaces originals.
    confirming: bool,
    run: Option<RunView>,
    base_dir: String,
    quit: bool,
    /// Clickable regions, topmost last.
    hits: Vec<Hit>,
}

/// Runs the interactive screen and returns the process exit code.
pub fn main() -> i32 {
    let mut app = App::new();
    let result = ratatui::run(|terminal| {
        let _mouse = MouseCapture::enable()?;
        app.event_loop(terminal)
    });
    match result {
        Ok(()) => i32::from(app.run.is_some_and(|r| r.finished == Some(false))),
        Err(e) => {
            eprintln!("Terminal error: {e}");
            1
        }
    }
}

impl App {
    fn new() -> Self {
        Self {
            // Exact is on whenever Replace is turned on, so turning it off
            // again is an explicit choice to allow look-alike merging.
            options: Options {
                lossy: true,
                ..Options::default()
            },
            selected: RUN,
            confirming: false,
            run: None,
            base_dir: crate::base_dir()
                .map_or_else(|e| format!("{e:#}"), |d| d.display().to_string()),
            quit: false,
            hits: Vec::new(),
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        while !self.quit {
            if let Some(run) = &mut self.run {
                run.receive();
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    // Windows also reports key releases; act on presses only.
                    TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                        let ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        self.on_key(if ctrl_c { KeyCode::Char('q') } else { key.code });
                    }
                    TermEvent::Mouse(mouse) => self.on_mouse(mouse.kind, mouse.column, mouse.row),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyCode) {
        if let Some(run) = &mut self.run {
            if run.finished.is_none() {
                match key {
                    KeyCode::Char('q') | KeyCode::Esc => run.stop.store(true, Ordering::Relaxed),
                    _ => run.scroll_key(key),
                }
            } else {
                match key {
                    KeyCode::Char('q') => self.quit = true,
                    KeyCode::Enter | KeyCode::Esc => self.run = None,
                    _ => run.scroll_key(key),
                }
            }
            return;
        }
        if self.confirming {
            self.confirming = false;
            if matches!(key, KeyCode::Char('y' | 'Y')) {
                self.start();
            }
            return;
        }
        let item = ITEMS[self.selected];
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = (self.selected + ITEMS.len() - 1) % ITEMS.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.selected = (self.selected + 1) % ITEMS.len();
            }
            KeyCode::Left | KeyCode::Char('-') if item == Item::Jobs => {
                self.options.jobs = self.options.jobs.saturating_sub(1).max(1);
            }
            KeyCode::Right | KeyCode::Char('+') if item == Item::Jobs => {
                self.options.jobs = (self.options.jobs + 1).min(MAX_JOBS);
            }
            KeyCode::Enter if item == Item::Run => {
                if self.options.replace {
                    self.confirming = true;
                } else {
                    self.start();
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(flag) = self.flag(item) {
                    *flag = !*flag;
                }
                if item == Item::Replace && self.options.replace {
                    self.options.exact = true;
                }
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position::new(column, row);
                if let Some(hit) = self.hits.iter().rev().find(|h| h.area.contains(pos)) {
                    let hit = *hit;
                    if let Some(i) = hit.select {
                        self.selected = i;
                    }
                    if let Some(key) = hit.key {
                        self.on_key(key);
                    }
                }
            }
            MouseEventKind::ScrollUp => self.on_wheel(-3),
            MouseEventKind::ScrollDown => self.on_wheel(3),
            _ => {}
        }
    }

    fn on_wheel(&mut self, lines: isize) {
        match &mut self.run {
            Some(run) => run.scroll_by(lines),
            None if !self.confirming => {
                self.selected = self.selected.saturating_add_signed(lines.signum()).min(RUN);
            }
            None => {}
        }
    }

    fn flag(&mut self, item: Item) -> Option<&mut bool> {
        let o = &mut self.options;
        Some(match item {
            Item::Exact => &mut o.exact,
            Item::Replace => &mut o.replace,
            Item::Recursive => &mut o.recursive,
            Item::Gifsicle => &mut o.gifsicle,
            Item::Log => &mut o.log,
            Item::Force => &mut o.force,
            Item::Jobs | Item::Run => return None,
        })
    }

    fn start(&mut self) {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let options = self.options.clone();
        let worker_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut out = Output::new(Sink::Channel(tx));
            if options.log {
                if let Err(e) = crate::open_log(&mut out, &options) {
                    out.error(format!("Not logging: {e:#}"));
                }
            }
            let ok = crate::run_reported(&options, &out, &worker_stop);
            out.event(Event::Finished { ok });
        });
        self.run = Some(RunView::new(rx, stop));
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.hits.clear();
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        frame.render_widget(
            Line::from(vec![
                Span::from(" GIF Optimizer ").bold().black().on_cyan(),
                Span::from(format!(" {}", self.base_dir)).cyan(),
            ]),
            header,
        );
        const SCROLL: Hint = Hint::Key("↑↓ PgUp PgDn wheel", "scroll", None, None);
        let q = Some(KeyCode::Char('q'));
        let hints = match &mut self.run {
            Some(run) => {
                run.draw(frame, body);
                match run.finished {
                    None if run.stop.load(Ordering::Relaxed) => {
                        vec![Hint::Note("Stopping..."), SCROLL]
                    }
                    None => vec![Hint::Key("q", "stop", None, q), SCROLL],
                    Some(_) => vec![
                        Hint::Key("Enter", "back to options", None, Some(KeyCode::Enter)),
                        Hint::Key("q", "quit", None, q),
                        SCROLL,
                    ],
                }
            }
            None => {
                self.draw_settings(frame, body);
                if self.confirming {
                    vec![
                        Hint::Key("y", "replace originals", None, Some(KeyCode::Char('y'))),
                        Hint::Key("n", "cancel", None, Some(KeyCode::Char('n'))),
                    ]
                } else {
                    vec![
                        Hint::Key("↑↓", "select", None, None),
                        Hint::Key("Space/click", "toggle", None, None),
                        Hint::Key("←→", "jobs", None, None),
                        Hint::Key("Enter", "run", Some(RUN), Some(KeyCode::Enter)),
                        Hint::Key("q", "quit", None, q),
                    ]
                }
            }
        };
        self.draw_footer(frame, footer, &hints);
    }

    fn draw_footer(&mut self, frame: &mut Frame, area: Rect, hints: &[Hint]) {
        let mut spans = Vec::new();
        let mut x = area.x;
        for hint in hints {
            let start = spans.len();
            match *hint {
                Hint::Key(key, desc, select, code) => {
                    spans.push(Span::from(format!(" {key} ")).black().on_gray());
                    spans.push(Span::from(format!(" {desc} ")).dim());
                    let width: u16 = spans[start..].iter().map(|s| s.width() as u16).sum();
                    if code.is_some() {
                        self.hits.push(Hit {
                            area: Rect::new(x, area.y, width, 1).intersection(area),
                            select,
                            key: code,
                        });
                    }
                }
                Hint::Note(text) => spans.push(Span::from(format!("{text} ")).yellow()),
            }
            spans.push(Span::from(" "));
            x = x.saturating_add(spans[start..].iter().map(|s| s.width() as u16).sum());
        }
        frame.render_widget(Line::from(spans), area);
    }

    fn item_spans(&self, item: Item) -> Vec<Span<'static>> {
        let o = &self.options;
        let toggle = |on: bool, label: String| {
            let check = if on {
                Span::from("[x]").green().bold()
            } else {
                Span::from("[ ]").gray()
            };
            vec![check, Span::from(format!(" {label}"))]
        };
        match item {
            Item::Exact => toggle(o.exact, "Exact (lossless)".into()),
            Item::Replace => {
                let mut spans = toggle(o.replace, "Replace originals".into());
                if o.replace {
                    spans[1] = spans[1].clone().yellow().bold();
                }
                spans
            }
            Item::Recursive => toggle(o.recursive, "Include subfolders".into()),
            Item::Gifsicle => toggle(o.gifsicle, "Use gifsicle".into()),
            Item::Log => toggle(o.log, format!("Write {LOG_FILE}")),
            Item::Force => toggle(o.force, "Redo up-to-date GIFs".into()),
            // The spans at index 1 and 3 are the -/+ buttons; see `draw_settings`.
            Item::Jobs => vec![
                Span::from("    Parallel jobs: "),
                Span::from("[-]").cyan().bold(),
                Span::from(format!(" {} ", o.jobs)).bold(),
                Span::from("[+]").cyan().bold(),
            ],
            Item::Run => vec![Span::from("    "), Span::from("► Run").green().bold()],
        }
    }

    fn draw_settings(&mut self, frame: &mut Frame, area: Rect) {
        let [list, help] = Layout::vertical([
            Constraint::Length(ITEMS.len() as u16 + 2),
            Constraint::Min(0),
        ])
        .areas(area);
        let block = Block::bordered().title(" Options ".bold());
        let inner = block.inner(list);
        let mut lines = Vec::new();
        let mut selected_row = Rect::default();
        for (i, &item) in ITEMS.iter().enumerate() {
            let spans = self.item_spans(item);
            let y = inner.y.saturating_add(i as u16);
            let row = Rect::new(inner.x, y, inner.width, 1).intersection(inner);
            let key = match item {
                Item::Jobs => None,
                Item::Run => Some(KeyCode::Enter),
                _ => Some(KeyCode::Char(' ')),
            };
            self.hits.push(Hit {
                area: row,
                select: Some(i),
                key,
            });
            if item == Item::Jobs {
                let mut x = inner.x.saturating_add(1);
                for (k, span) in spans.iter().enumerate() {
                    let width = span.width() as u16;
                    if k == 1 || k == 3 {
                        self.hits.push(Hit {
                            area: Rect::new(x, y, width, 1).intersection(row),
                            select: Some(i),
                            key: Some(if k == 1 {
                                KeyCode::Left
                            } else {
                                KeyCode::Right
                            }),
                        });
                    }
                    x = x.saturating_add(width);
                }
            }
            // The marker must stay one column wide; the jobs buttons'
            // click areas above assume it.
            lines.push(if i == self.selected {
                let spans = spans
                    .into_iter()
                    .map(|s| if s.style.fg.is_none() { s.cyan() } else { s });
                selected_row = row;
                Line::from([vec![Span::from("▶").cyan()], spans.collect()].concat()).bold()
            } else {
                Line::from([vec![Span::from(" ")], spans].concat())
            });
        }
        frame.render_widget(Paragraph::new(lines).block(block), list);
        // Fill the whole row, not just its text; only the background changes.
        frame
            .buffer_mut()
            .set_style(selected_row, Style::new().bg(SELECTED_BG));

        let item = ITEMS[self.selected];
        let help_text = Paragraph::new(self.help(item)).wrap(Wrap { trim: true });
        let help_text = if item == Item::Replace {
            help_text.yellow()
        } else {
            help_text
        };
        frame.render_widget(
            help_text.block(
                Block::bordered()
                    .title(" Help ")
                    .border_style(Style::new().dark_gray()),
            ),
            help,
        );
        if self.confirming {
            self.draw_confirm(frame, area);
        }
    }

    fn draw_confirm(&mut self, frame: &mut Frame, area: Rect) {
        // Clicking anywhere outside the buttons cancels.
        self.hits.push(Hit {
            area,
            select: None,
            key: Some(KeyCode::Char('n')),
        });
        let popup = centered(area, 56, 6);
        let block = Block::bordered()
            .title(" Replace originals ".bold())
            .yellow();
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let [text, buttons] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Overwrite originals where the result is smaller?"),
                Line::from("This cannot be undone.").red().bold(),
            ])
            .wrap(Wrap { trim: true }),
            text,
        );
        let yes = Span::from(" Yes, replace (y) ").bold().black().on_yellow();
        let no = Span::from(" Cancel (n) ").black().on_gray();
        let [yes_area, _, no_area] = Layout::horizontal([
            Constraint::Length(yes.width() as u16),
            Constraint::Length(3),
            Constraint::Length(no.width() as u16),
        ])
        .flex(Flex::Center)
        .areas(buttons);
        frame.render_widget(yes, yes_area);
        frame.render_widget(no, no_area);
        self.hits.push(Hit {
            area: yes_area,
            select: None,
            key: Some(KeyCode::Char('y')),
        });
    }

    fn help(&self, item: Item) -> String {
        match item {
            Item::Exact => "Merge only pixel-identical frames, so the result is lossless. \
                 When off, frames that differ only by dithering noise may also be merged: \
                 the result looks the same but is not pixel-identical."
                .to_string(),
            Item::Replace => format!(
                "Overwrite each original in place, only when the result is smaller, \
                 keeping its modified time. This cannot be undone{}. \
                 When off, results are written to {OUTPUT_DIR}/.",
                if self.options.exact {
                    ""
                } else {
                    ", and Exact is off, so look-alike frames may be merged"
                }
            ),
            Item::Recursive => format!(
                "Also process GIFs in subfolders. {OUTPUT_DIR}/ and {WORK_DIR}/ are skipped, \
                 and symlinked folders are not followed."
            ),
            Item::Gifsicle => "If gifsicle is installed, try its -O3 pass on each result. \
                 It is kept only when smaller and verified like our own output."
                .to_string(),
            Item::Log => format!(
                "Append everything shown during the run to {LOG_FILE} next to the executable."
            ),
            Item::Force => format!(
                "Process every GIF again. When off, a GIF is skipped if {OUTPUT_DIR}/ already \
                 has a result that is newer than the GIF."
            ),
            Item::Jobs => format!(
                "How many GIFs to process at the same time. Use ←/→ or click -/+ to change. \
                 Default: one per CPU core ({}).",
                Options::default_jobs()
            ),
            Item::Run => format!("Optimize the GIFs in {}.", self.base_dir),
        }
    }
}

impl RunView {
    fn new(rx: Receiver<Event>, stop: Arc<AtomicBool>) -> Self {
        Self {
            rx,
            stop,
            lines: Vec::new(),
            total: None,
            done: 0,
            failed: 0,
            stopped: 0,
            active: Vec::new(),
            bytes_in: 0,
            bytes_out: 0,
            finished: None,
            scroll: None,
            log_height: 0,
        }
    }

    fn receive(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Event::Text(lines)) => {
                    for (kind, text) in lines {
                        self.lines
                            .extend(text.lines().map(|l| (kind, l.to_string())));
                        if text.is_empty() {
                            self.lines.push((kind, String::new()));
                        }
                    }
                }
                Ok(Event::Started { total }) => self.total = Some(total),
                Ok(Event::FileStarted { index, name }) => {
                    let at = self.active.partition_point(|a| a.index < index);
                    self.active.insert(
                        at,
                        Active {
                            index,
                            name,
                            stage: Stage::Reading,
                            done: 0,
                            total: 0,
                        },
                    );
                }
                Ok(Event::Progress {
                    index,
                    stage,
                    done,
                    total,
                }) => {
                    if let Some(a) = self.active.iter_mut().find(|a| a.index == index) {
                        (a.stage, a.done, a.total) = (stage, done, total);
                    }
                }
                Ok(Event::FileDone { index, result }) => {
                    self.active.retain(|a| a.index != index);
                    match result {
                        FileResult::Done(a, b) => {
                            self.done += 1;
                            self.bytes_in += a;
                            self.bytes_out += b;
                        }
                        FileResult::Failed => {
                            self.done += 1;
                            self.failed += 1;
                        }
                        FileResult::Stopped => self.stopped += 1,
                    }
                }
                Ok(Event::Finished { ok }) => self.finished = Some(ok),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.finished.is_none() {
                        self.lines
                            .push((Kind::Error, "The run ended unexpectedly.".to_string()));
                        self.finished = Some(false);
                    }
                    self.active.clear();
                    break;
                }
            }
        }
    }

    /// Fraction of the run that is done, counting files in progress partly.
    fn ratio(&self) -> f64 {
        let ratio = match self.total {
            None => 0.0,
            Some(0) => 1.0,
            Some(total) => {
                let partial: f64 = self.active.iter().map(Active::fraction).sum();
                (self.done as f64 + partial) / total as f64
            }
        };
        ratio.clamp(0.0, 1.0)
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.log_height)
    }

    fn scroll_to(&mut self, top: usize) {
        self.scroll = (top < self.max_scroll()).then_some(top);
    }

    fn scroll_by(&mut self, lines: isize) {
        let top = self.scroll.unwrap_or(self.max_scroll());
        self.scroll_to(top.saturating_add_signed(lines));
    }

    fn scroll_key(&mut self, key: KeyCode) {
        let page = self.log_height.max(1) as isize;
        match key {
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown => self.scroll_by(page),
            KeyCode::Home => self.scroll_to(0),
            KeyCode::End => self.scroll_to(usize::MAX),
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let shown = self.active.len().min(area.height as usize / 3);
        let active_height = if shown == 0 { 0 } else { shown as u16 + 2 };
        let [gauge, active, log] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(active_height),
            Constraint::Min(0),
        ])
        .areas(area);

        let mut label = match self.total {
            None => "Looking for GIFs...".to_string(),
            Some(total) => format!("{}/{total} GIFs", self.done),
        };
        if self.bytes_in > 0 {
            label += &format!(
                ", {} -> {} (saved {})",
                format_size(self.bytes_in),
                format_size(self.bytes_out),
                format_size(self.bytes_in.saturating_sub(self.bytes_out))
            );
        }
        if self.failed > 0 {
            label += &format!(", {} failed", self.failed);
        }
        if self.stopped > 0 {
            label += &format!(", {} stopped", self.stopped);
        }
        let (title, style) = match self.finished {
            None => (" Running ", Style::new().cyan()),
            Some(true) => (" Done ", Style::new().green()),
            Some(false) => (" Finished with errors ", Style::new().red()),
        };
        frame.render_widget(
            Gauge::default()
                .block(Block::bordered().title(title.bold()).border_style(style))
                .gauge_style(style)
                .ratio(self.ratio())
                .label(label),
            gauge,
        );

        if shown > 0 {
            const BAR: usize = 20;
            let lines: Vec<Line> = self.active[..shown]
                .iter()
                .map(|a| {
                    let filled = (a.fraction() * BAR as f64).round() as usize;
                    Line::from(vec![
                        Span::from("█".repeat(filled)).cyan(),
                        Span::from("░".repeat(BAR - filled)).dark_gray(),
                        Span::from(format!(" {:<18} ", a.label())).dim(),
                        Span::from(a.name.clone()),
                    ])
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .title(" In progress ")
                        .border_style(Style::new().dark_gray()),
                ),
                active,
            );
        }

        let block = Block::bordered()
            .title(" Output ")
            .border_style(Style::new().dark_gray());
        self.log_height = block.inner(log).height as usize;
        let top = self.scroll.unwrap_or(self.max_scroll());
        let lines: Vec<Line> = self
            .lines
            .iter()
            .skip(top)
            .take(self.log_height)
            .map(|(kind, text)| Line::styled(text.as_str(), log_style(*kind)))
            .collect();
        frame.render_widget(Paragraph::new(lines).block(block), log);
    }
}

/// Colors a line of run output by what it reports.
fn log_style(kind: Kind) -> Style {
    let style = Style::new();
    match kind {
        Kind::Plain => style,
        Kind::Heading => style.cyan().bold(),
        Kind::Detail => style.dim(),
        Kind::Good => style.green(),
        Kind::Warning => style.yellow(),
        Kind::Error => style.red(),
        Kind::Emphasis => style.bold(),
        Kind::Total => style.green().bold(),
    }
}

/// A `width` x `height` rectangle in the middle of `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn settings_toggle_and_confirm_replace() {
        let mut app = App::new();
        app.selected = 1;
        app.on_key(KeyCode::Char(' '));
        assert!(app.options.replace);
        assert!(app.options.exact, "replacing turns on Exact");
        assert!(app.options.lossless());
        app.selected = 0;
        app.on_key(KeyCode::Char(' '));
        assert!(!app.options.lossless(), "Exact can be turned off again");
        assert!(render(&mut app, 80, 24).contains("[x] Replace originals"));

        app.selected = ITEMS.len() - 1;
        app.on_key(KeyCode::Enter);
        assert!(app.confirming, "replacing must be confirmed");
        assert!(render(&mut app, 80, 24).contains("cannot be undone"));
        render(&mut app, 10, 3);
        app.on_key(KeyCode::Char('n'));
        assert!(!app.confirming && app.run.is_none());

        app.selected = 6;
        app.on_key(KeyCode::Left);
        for _ in 0..MAX_JOBS {
            app.on_key(KeyCode::Left);
        }
        assert_eq!(app.options.jobs, 1);
    }

    #[test]
    fn run_view_shows_progress_and_scrolls() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new();
        app.run = Some(RunView::new(rx, Arc::new(AtomicBool::new(false))));
        let out = Output::new(Sink::Channel(tx));
        out.event(Event::Started { total: 3 });
        for i in 0..50 {
            out.line(format!("line {i}\n  detail"));
        }
        for index in 0..3 {
            out.event(Event::FileStarted {
                index,
                name: format!("file{index}.gif"),
            });
        }
        out.event(Event::Progress {
            index: 1,
            stage: Stage::Optimizing,
            done: 50,
            total: 100,
        });
        out.event(Event::FileDone {
            index: 0,
            result: FileResult::Done(2048, 1024),
        });
        let run = app.run.as_mut().unwrap();
        run.receive();
        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("optimizing  50%") && screen.contains("file1.gif"));
        assert!(
            !screen.contains("file0.gif"),
            "finished files leave the list"
        );
        // One file done plus 45% of another, out of three.
        let ratio = app.run.as_ref().unwrap().ratio();
        assert!((ratio - 1.45 / 3.0).abs() < 1e-9, "{ratio}");

        out.event(Event::FileDone {
            index: 1,
            result: FileResult::Failed,
        });
        out.event(Event::FileDone {
            index: 2,
            result: FileResult::Stopped,
        });
        drop(out);
        let run = app.run.as_mut().unwrap();
        run.receive();
        assert_eq!(run.finished, Some(false), "a lost worker counts as failed");
        assert_eq!(run.lines.len(), 101);
        assert_eq!(
            run.lines[100],
            (Kind::Error, "The run ended unexpectedly.".into())
        );
        assert!(run.active.is_empty());

        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("2/3 GIFs") && screen.contains("1 failed"));
        assert!(screen.contains("1 stopped") && !screen.contains("In progress"));
        assert!(screen.contains("line 49"), "follows the end");
        app.on_key(KeyCode::Home);
        assert!(render(&mut app, 80, 24).contains("line 0 "));
        app.on_key(KeyCode::End);
        assert_eq!(app.run.as_ref().unwrap().scroll, None);
        render(&mut app, 5, 2);
        app.on_key(KeyCode::Enter);
        assert!(app.run.is_none());
    }

    /// Where `text` starts on an 80x24 screen.
    fn find(app: &mut App, text: &str) -> (u16, u16) {
        let screen = render(app, 80, 24);
        let lines: Vec<String> = screen
            .chars()
            .collect::<Vec<_>>()
            .chunks(80)
            .map(|c| c.iter().collect())
            .collect();
        for (y, line) in lines.iter().enumerate() {
            if let Some(x) = line.find(text) {
                return (line[..x].chars().count() as u16, y as u16);
            }
        }
        panic!("{text:?} not on screen:\n{}", lines.join("\n"));
    }

    fn click(app: &mut App, (x, y): (u16, u16)) {
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
    }

    #[test]
    fn mouse_toggles_changes_jobs_and_confirms() {
        let mut app = App::new();
        app.options.jobs = 4;
        let pos = find(&mut app, "Include subfolders");
        click(&mut app, pos);
        assert!(app.options.recursive && app.selected == 2);

        let minus = find(&mut app, "[-]");
        click(&mut app, minus);
        assert_eq!((app.options.jobs, app.selected), (3, 6));
        let plus = find(&mut app, "[+]");
        click(&mut app, plus);
        click(&mut app, plus);
        assert_eq!(app.options.jobs, 5);
        // Clicking the jobs label only selects it.
        let label = find(&mut app, "Parallel jobs");
        click(&mut app, label);
        assert_eq!(app.options.jobs, 5);

        app.on_mouse(MouseEventKind::ScrollDown, 0, 0);
        assert_eq!(app.selected, RUN);
        app.on_mouse(MouseEventKind::ScrollDown, 0, 0);
        assert_eq!(app.selected, RUN, "the wheel stops at the last item");

        app.options.replace = true;
        let run = find(&mut app, "Run");
        click(&mut app, run);
        assert!(app.confirming);
        render(&mut app, 80, 24);
        click(&mut app, (0, 5));
        assert!(
            !app.confirming && app.run.is_none(),
            "clicking outside cancels"
        );

        let enter = find(&mut app, " Enter ");
        click(&mut app, enter);
        assert!(app.confirming, "the footer Enter chip runs");
        let yes = find(&mut app, "Yes, replace");
        assert!(app
            .hits
            .iter()
            .any(|h| h.area.contains(Position::new(yes.0, yes.1))
                && h.key == Some(KeyCode::Char('y'))));
    }

    #[test]
    fn selected_row_has_marker_and_faint_background() {
        let mut app = App::new();
        app.options.exact = true;
        app.selected = 0;
        let (x, y) = find(&mut app, "▶");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_bg = |cx: u16| buffer[(cx, y)].bg;
        assert_eq!((row_bg(x), row_bg(77)), (SELECTED_BG, SELECTED_BG));
        assert_eq!(buffer[(x + 1, y)].fg, Color::Green, "[x] keeps its color");
        assert_eq!(buffer[(x + 5, y)].fg, Color::Cyan, "label turns cyan");
        assert_ne!(buffer[(x, y + 1)].bg, SELECTED_BG);
    }

    #[test]
    fn wheel_scrolls_the_run_log() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new();
        app.run = Some(RunView::new(rx, Arc::new(AtomicBool::new(false))));
        let out = Output::new(Sink::Channel(tx));
        for i in 0..50 {
            out.line(format!("line {i}"));
        }
        app.run.as_mut().unwrap().receive();
        render(&mut app, 80, 24);
        let max = app.run.as_ref().unwrap().max_scroll();
        app.on_mouse(MouseEventKind::ScrollUp, 0, 0);
        assert_eq!(app.run.as_ref().unwrap().scroll, Some(max - 3));
        app.on_mouse(MouseEventKind::ScrollDown, 0, 0);
        assert_eq!(app.run.as_ref().unwrap().scroll, None, "back to following");
        let stop = find(&mut app, " q ");
        click(&mut app, stop);
        assert!(app.run.as_ref().unwrap().stop.load(Ordering::Relaxed));
    }

    #[test]
    fn log_lines_are_colored_by_kind() {
        let (tx, rx) = mpsc::channel();
        let mut run = RunView::new(rx, Arc::new(AtomicBool::new(false)));
        let out = Output::new(Sink::Channel(tx));
        out.block(vec![
            (Kind::Heading, "[1/2] a.gif".into()),
            (Kind::Good, "    size: 1 KB -> 0 KB".into()),
            (Kind::Warning, "    kept original file: bigger".into()),
        ]);
        out.error("broken\nstill broken");
        out.line("GIF Optimizer - x");
        run.receive();
        let colors: Vec<_> = run.lines.iter().map(|(k, _)| log_style(*k).fg).collect();
        assert_eq!(
            colors,
            [
                Some(Color::Cyan),
                Some(Color::Green),
                Some(Color::Yellow),
                Some(Color::Red),
                Some(Color::Red),
                None
            ]
        );
    }
}
