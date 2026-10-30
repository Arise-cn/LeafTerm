//! PTY sessions backed by Alacritty's terminal state machine.
//!
//! Output is parsed on the reader thread, not queued to the UI. A coalesced wake
//! tells the UI that a fresh visible-screen snapshot is available.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

pub const DEFAULT_BG: [u8; 3] = [12, 17, 20];
pub const DEFAULT_FG: [u8; 3] = [208, 218, 222];
const HISTORY_LINES: usize = 3_000;
const MAX_INPUT_BYTES: usize = 128 * 1024;
const MAX_OSC_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
enum OscState {
    #[default]
    Ground,
    Escape,
    Body(usize),
    Discard,
}

/// VTE's std-enabled OSC parser retains a Vec until a control string ends.
/// Limit only oversized OSC bodies; ordinary VT bytes (including UTF-8 split
/// across reads) reach the parser unchanged. No output buffer is retained here.
#[derive(Default)]
struct OscLimiter {
    state: OscState,
}

impl OscLimiter {
    fn advance<T: EventListener>(
        &mut self,
        parser: &mut Processor,
        term: &mut Term<T>,
        bytes: &[u8],
    ) {
        let mut start = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            self.state = match self.state {
                OscState::Ground if byte == 0x1b => OscState::Escape,
                OscState::Escape if byte == b']' => OscState::Body(0),
                OscState::Escape if matches!(byte, 0x18 | 0x1a | 0x20..=0x7e) => OscState::Ground,
                // VTE keeps its Escape state across executed C0 controls and
                // ignored DEL/high bytes. Otherwise ESC NUL ] would bypass us.
                OscState::Escape => OscState::Escape,
                OscState::Body(_) if byte == 0x1b => OscState::Escape,
                OscState::Body(_) if matches!(byte, 0x07 | 0x18 | 0x1a) => OscState::Ground,
                OscState::Body(count) if count == MAX_OSC_BYTES => {
                    parser.advance(term, &bytes[start..index]);
                    // CAN terminates the retained OSC prefix. Ignore its tail
                    // until the same delimiters that VTE recognizes, rather
                    // than letting the discarded payload become screen text.
                    parser.advance(term, b"\x18");
                    start = index + 1;
                    OscState::Discard
                }
                OscState::Body(count) => OscState::Body(count + 1),
                OscState::Discard if byte == 0x1b => {
                    start = index;
                    OscState::Escape
                }
                OscState::Discard if matches!(byte, 0x07 | 0x18 | 0x1a) => {
                    start = index + 1;
                    OscState::Ground
                }
                state => state,
            };
        }
        if self.state != OscState::Discard {
            parser.advance(term, &bytes[start..]);
        }
    }
}

#[derive(Clone, Debug)]
pub struct RenderCell {
    pub col: u16,
    pub row: u16,
    pub text: String,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub selected: bool,
}

#[derive(Clone, Debug)]
pub struct TerminalSnapshot {
    pub cells: Vec<RenderCell>,
    pub cols: u16,
    pub rows: u16,
    pub cursor: Option<(u16, u16)>,
}

struct Metadata {
    title: String,
    cwd: PathBuf,
    cwd_checked: Instant,
    size: WindowSize,
    error: Option<String>,
}

struct Shared {
    metadata: Mutex<Metadata>,
    input: mpsc::SyncSender<Vec<u8>>,
    dirty: AtomicBool,
    stopped: AtomicBool,
    exited: AtomicBool,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn changed(&self) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() > MAX_INPUT_BYTES {
            bail!("Input exceeds 128 KiB; paste a smaller block");
        }
        if self.exited.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire) {
            bail!("This shell has exited");
        }
        self.input.try_send(bytes).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => anyhow!("Terminal input is busy; please retry"),
            mpsc::TrySendError::Disconnected(_) => anyhow!("Terminal input is closed"),
        })
    }

    fn reply(&self, text: String) {
        if let Err(error) = self.send(text.into_bytes()) {
            self.metadata.lock().error = Some(error.to_string());
        }
    }
}

#[derive(Clone)]
struct Listener(Arc<Shared>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                // A title is chrome, not another copy of terminal history.
                self.0.metadata.lock().title = title.chars().take(256).collect();
            }
            Event::ResetTitle => self.0.metadata.lock().title = "Shell".into(),
            Event::PtyWrite(text) => self.0.reply(text),
            Event::ColorRequest(index, formatter) => {
                let [r, g, b] = palette(index);
                self.0.reply(formatter(Rgb { r, g, b }));
            }
            Event::TextAreaSizeRequest(formatter) => {
                let size = self.0.metadata.lock().size;
                self.0.reply(formatter(size));
            }
            // OS clipboard access is owned by the UI, not terminal output.
            Event::ClipboardLoad(_, _) | Event::ClipboardStore(_, _) => return,
            _ => {}
        }
        self.0.changed();
    }
}

#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct TerminalSession {
    term: Arc<Mutex<Term<Listener>>>,
    shared: Arc<Shared>,
    master: Box<dyn MasterPty + Send>,
    pid: Option<u32>,
    wait_thread: Option<std::thread::JoinHandle<()>>,
    // Startup shims must outlive the Shell that reads them.
    _shell_integration: Option<tempfile::TempDir>,
    mouse_position: (u16, u16),
    mouse_down: Option<u8>,
}

impl TerminalSession {
    pub fn spawn(
        cwd: &Path,
        cols: u16,
        rows: u16,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
        let mut command = CommandBuilder::new(&shell);
        command.arg("-l");
        let integration = crate::shell::configure(&mut command, &shell)?;
        let mut session = Self::spawn_command(cwd, cols, rows, command, wake)?;
        session._shell_integration = integration;
        Ok(session)
    }

    fn spawn_command(
        cwd: &Path,
        cols: u16,
        rows: u16,
        mut command: CommandBuilder,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let cols = cols.clamp(2, 1_000);
        let rows = rows.clamp(1, 500);
        let cwd = cwd.canonicalize().context("Open terminal directory")?;
        command.cwd(&cwd);
        // A GUI launched by an automation tool may inherit NO_COLOR=1. The
        // interactive PTY supports color; shell startup files and individual
        // commands can still opt out after the shell has started.
        command.env_remove("NO_COLOR");
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "LeafTerm");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Create pseudo-terminal")?;
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(std::io::Error::last_os_error()).context("Set nonblocking PTY");
            }
        }
        let mut child = pair.slave.spawn_command(command).context("Start shell")?;
        let pid = child.process_id();
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, rx) = mpsc::sync_channel(16);
        let shared = Arc::new(Shared {
            metadata: Mutex::new(Metadata {
                title: "Shell".into(),
                cwd,
                cwd_checked: Instant::now(),
                size: WindowSize {
                    num_lines: rows,
                    num_cols: cols,
                    cell_width: 0,
                    cell_height: 0,
                },
                error: None,
            }),
            input: tx,
            dirty: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            wake,
        });
        let config = Config {
            scrolling_history: HISTORY_LINES,
            kitty_keyboard: true,
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let term = Arc::new(Mutex::new(Term::new(
            config,
            &Size {
                cols: cols.into(),
                rows: rows.into(),
            },
            Listener(shared.clone()),
        )));

        let writer_shared = shared.clone();
        #[cfg(unix)]
        let writer_poll_fd = pair.master.as_raw_fd().and_then(duplicate_fd);
        std::thread::Builder::new()
            .name("leafterm-pty-input".into())
            .spawn(move || {
                write_input(
                    writer,
                    rx,
                    writer_shared,
                    #[cfg(unix)]
                    writer_poll_fd,
                );
            })?;

        // Poll a duplicated descriptor, whose lifetime belongs to the reader,
        // so a close in the UI cannot accidentally poll a reused descriptor.
        #[cfg(unix)]
        let poll_fd = pair.master.as_raw_fd().and_then(duplicate_fd);
        let reader_shared = shared.clone();
        let reader_term = term.clone();
        std::thread::Builder::new()
            .name("leafterm-pty-output".into())
            .spawn(move || {
                read_output(
                    reader,
                    reader_term,
                    reader_shared,
                    #[cfg(unix)]
                    poll_fd,
                );
            })?;

        let wait_shared = shared.clone();
        let wait_thread = std::thread::Builder::new()
            .name("leafterm-shell-wait".into())
            .spawn(move || {
                loop {
                    if wait_shared.stopped.load(Ordering::Acquire) {
                        // Only this thread owns/reaps Child. Its PID cannot be
                        // reused between this check, kill's grace period, and
                        // wait. Unlike clone_killer, Child::kill escalates an
                        // ignored SIGHUP to SIGKILL on Unix.
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => {}
                        Err(error) => {
                            wait_shared.metadata.lock().error =
                                Some(format!("Wait for shell: {error}"));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                wait_shared.exited.store(true, Ordering::Release);
                wait_shared.changed();
            })?;

        Ok(Self {
            term,
            shared,
            master: pair.master,
            pid,
            wait_thread: Some(wait_thread),
            _shell_integration: None,
            mouse_position: (0, 0),
            mouse_down: None,
        })
    }

    #[cfg(test)]
    pub fn child_pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn has_running_job(&self) -> bool {
        if self.is_exited() {
            return false;
        }
        #[cfg(unix)]
        if let (Some(foreground), Some(shell)) = (self.master.process_group_leader(), self.pid) {
            return foreground as u32 != shell;
        }
        false
    }

    pub fn is_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    /// Request all sessions to stop before dropping them, so application exit
    /// can give their child processes the termination grace period in parallel.
    pub fn shutdown(&self) {
        self.shared.stopped.store(true, Ordering::Release);
    }

    /// Window metadata reads must not acknowledge pending terminal rendering.
    pub fn title(&self) -> String {
        self.shared.metadata.lock().title.clone()
    }

    pub fn is_dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Acquire)
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared.metadata.lock().error.take()
    }

    pub fn cwd(&self) -> PathBuf {
        let mut metadata = self.shared.metadata.lock();
        if metadata.cwd_checked.elapsed() >= Duration::from_millis(500) {
            metadata.cwd_checked = Instant::now();
            #[cfg(unix)]
            let pid = self
                .master
                .process_group_leader()
                .map(|p| p as u32)
                .or(self.pid);
            #[cfg(not(unix))]
            let pid = self.pid;
            if let Some(path) = pid.and_then(process_cwd) {
                metadata.cwd = path;
            }
        }
        metadata.cwd.clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.clamp(2, 1_000);
        let rows = rows.clamp(1, 500);
        let mut term = self.term.lock();
        if term.columns() == cols as usize && term.screen_lines() == rows as usize {
            return Ok(());
        }
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        term.resize(Size {
            cols: cols.into(),
            rows: rows.into(),
        });
        self.shared.metadata.lock().size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 0,
            cell_height: 0,
        };
        self.shared.changed();
        Ok(())
    }

    pub fn input(&self, text: &str) -> Result<()> {
        let mode = *self.term.lock().mode();
        let bytes = if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
            text.chars()
                .map(|ch| format!("\x1b[{}u", ch as u32))
                .collect::<String>()
                .into_bytes()
        } else {
            text.as_bytes().to_vec()
        };
        self.send_input(bytes)
    }

    pub fn paste(&self, text: &str) -> Result<()> {
        if text.len() > MAX_INPUT_BYTES - 12 {
            bail!("Paste exceeds 128 KiB");
        }
        // Do not let embedded escape characters terminate a bracketed paste.
        let text = text.replace('\x1b', "");
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let text = if bracketed {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.send_input(text.into_bytes())
    }

    pub fn send_key(&self, key: &str, ctrl: bool, alt: bool, shift: bool) -> Result<()> {
        let mode = *self.term.lock().mode();
        if let Some(bytes) = key_bytes(key, ctrl, alt, shift, mode) {
            self.send_input(bytes)?;
        }
        Ok(())
    }

    fn send_input(&self, bytes: Vec<u8>) -> Result<()> {
        self.shared.send(bytes)?;
        let mut term = self.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        self.shared.changed();
        Ok(())
    }

    /// Positive lines scroll up. Fullscreen TUIs receive wheel events (when
    /// requested), or application-cursor keys for alternate-screen scrolling.
    pub fn scroll(&self, lines: i32) {
        let mode = *self.term.lock().mode();
        let count = lines.unsigned_abs().min(50);
        if mode.intersects(TermMode::MOUSE_MODE) {
            let button = if lines > 0 { 64 } else { 65 };
            for _ in 0..count {
                let _ = self.send_mouse(button, false, false, false, false);
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let key = if lines > 0 { "ArrowUp" } else { "ArrowDown" };
            for _ in 0..count {
                let _ = self.send_key(key, false, false, false);
            }
        } else {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            self.shared.changed();
        }
    }

    // These fields directly describe one terminal mouse-protocol event.
    #[allow(clippy::too_many_arguments)]
    pub fn mouse_button(
        &mut self,
        col: u16,
        row: u16,
        button: u8,
        pressed: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) -> Result<bool> {
        self.mouse_position = (col, row);
        if shift || !self.term.lock().mode().intersects(TermMode::MOUSE_MODE) {
            return Ok(false);
        }
        self.mouse_down = if pressed { Some(button) } else { None };
        self.send_mouse(button, !pressed, ctrl, alt, shift)?;
        Ok(true)
    }

    pub fn mouse_move(
        &mut self,
        col: u16,
        row: u16,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) -> Result<bool> {
        let position = (col, row);
        if self.mouse_position == position {
            return Ok(false);
        }
        self.mouse_position = position;
        let mode = *self.term.lock().mode();
        if shift
            || !(mode.contains(TermMode::MOUSE_MOTION)
                || (mode.contains(TermMode::MOUSE_DRAG) && self.mouse_down.is_some()))
        {
            return Ok(false);
        }
        self.send_mouse(self.mouse_down.unwrap_or(3) + 32, false, ctrl, alt, shift)?;
        Ok(true)
    }

    fn send_mouse(
        &self,
        button: u8,
        release: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) -> Result<()> {
        let mode = *self.term.lock().mode();
        let modifier = u8::from(shift) * 4 + u8::from(alt) * 8 + u8::from(ctrl) * 16;
        let (col, row) = self.mouse_position;
        let bytes = if mode.contains(TermMode::SGR_MOUSE) {
            format!(
                "\x1b[<{};{};{}{}",
                button + modifier,
                col + 1,
                row + 1,
                if release { 'm' } else { 'M' }
            )
            .into_bytes()
        } else if col < 223 && row < 223 {
            vec![
                0x1b,
                b'[',
                b'M',
                32 + if release { 3 } else { button } + modifier,
                col as u8 + 33,
                row as u8 + 33,
            ]
        } else {
            return Ok(());
        };
        self.shared.send(bytes)
    }

    pub fn focus(&self, focused: bool) {
        if self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
            let _ = self
                .shared
                .send(if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec());
        }
    }

    pub fn select_start(&self, col: u16, row: u16) {
        let mut term = self.term.lock();
        let point = viewport_point(&term, col, row);
        term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
        self.shared.changed();
    }

    pub fn select_update(&self, col: u16, row: u16) {
        let mut term = self.term.lock();
        let point = viewport_point(&term, col, row);
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, Side::Right);
        }
        self.shared.changed();
    }

    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
        self.shared.changed();
    }

    pub fn selected_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Search the retained grid without keeping a second copy of its text.
    pub fn find(&self, query: &str, backwards: bool) -> bool {
        if query.is_empty() {
            return false;
        }
        let mut term = self.term.lock();
        let first = -(term.history_size() as i32);
        let last = term.screen_lines() as i32 - 1;
        let current = term.selection.as_ref().and_then(|s| s.to_range(&term));
        let lines: Box<dyn Iterator<Item = i32>> = if backwards {
            Box::new((first..=last).rev())
        } else {
            Box::new(first..=last)
        };
        let mut fallback = None;
        let mut found = None;
        for row in lines {
            let mut text = String::new();
            let mut positions = Vec::new();
            for col in 0..term.columns() {
                let cell = &term.grid()[Line(row)][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                positions.push((text.len(), col));
                text.push(cell.c);
            }
            let matches: Vec<_> = text.match_indices(query).collect();
            let matches: Box<dyn Iterator<Item = _>> = if backwards {
                Box::new(matches.into_iter().rev())
            } else {
                Box::new(matches.into_iter())
            };
            for (start, matched) in matches {
                let col = positions
                    .iter()
                    .find(|(byte, _)| *byte == start)
                    .map(|(_, col)| *col)
                    .unwrap_or(0);
                let end = positions
                    .iter()
                    .take_while(|(byte, _)| *byte < start + matched.len())
                    .last()
                    .map(|(_, col)| *col)
                    .unwrap_or(col);
                let point = Point::new(Line(row), Column(col));
                let candidate = (point, Point::new(Line(row), Column(end)));
                fallback.get_or_insert(candidate);
                if current.is_none_or(|selection| {
                    if backwards {
                        point < selection.start
                    } else {
                        point > selection.end
                    }
                }) {
                    found = Some(candidate);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((start, end)) = found.or(fallback) else {
            return false;
        };
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        let desired_offset = (-start.line.0).max(0) as usize;
        let delta = desired_offset as i32 - term.grid().display_offset() as i32;
        term.scroll_display(Scroll::Delta(delta));
        self.shared.changed();
        true
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        let term = self.term.lock();
        // Clear while holding the grid lock: the reader's next mutation must
        // then schedule another wake, so no output is lost to a reset race.
        self.shared.dirty.store(false, Ordering::Release);
        let content = term.renderable_content();
        let mut cells = Vec::with_capacity(term.columns() * term.screen_lines());
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + content.display_offset as i32;
            if row < 0 || row >= term.screen_lines() as i32 {
                continue;
            }
            let cell = indexed.cell;
            let selected = content
                .selection
                .as_ref()
                .is_some_and(|s| s.contains(indexed.point));
            let mut fg = resolve_color(cell.fg, content.colors, cell.flags.contains(Flags::BOLD));
            let mut bg = resolve_color(cell.bg, content.colors, false);
            if cell.flags.contains(Flags::DIM) {
                fg = fg.map(|c| (c as f32 * 0.66) as u8);
            }
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            // Alacritty retains tabs for copying, but already resolved their
            // column positions. Shaping them again would shift ASCII runs
            // over independently positioned wide characters.
            let hidden = cell.c == '\t'
                || cell.flags.intersects(
                    Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
                );
            if !selected
                && bg == DEFAULT_BG
                && (hidden || cell.c == ' ')
                && !cell.flags.intersects(Flags::ALL_UNDERLINES)
            {
                continue;
            }
            let mut text = String::new();
            if !hidden {
                text.push(cell.c);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra.iter());
                }
            }
            cells.push(RenderCell {
                col: indexed.point.column.0 as u16,
                row: row as u16,
                text,
                fg,
                bg,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                selected,
            });
        }
        let cursor_row = content.cursor.point.line.0 + content.display_offset as i32;
        let cursor = if content.cursor.shape != CursorShape::Hidden
            && cursor_row >= 0
            && cursor_row < term.screen_lines() as i32
        {
            Some((content.cursor.point.column.0 as u16, cursor_row as u16))
        } else {
            None
        };
        TerminalSnapshot {
            cells,
            cols: term.columns() as u16,
            rows: term.screen_lines() as u16,
            cursor,
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.shutdown();
        // Do not let main return while child termination/reaping is pending.
        // The IO workers use nonblocking IO and observe the same stop flag.
        if let Some(thread) = self.wait_thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_input(
    mut writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
    #[cfg(unix)] poll_fd: Option<std::os::fd::OwnedFd>,
) {
    'input: while !shared.stopped.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(bytes) => {
                let mut offset = 0;
                while offset < bytes.len() {
                    if shared.stopped.load(Ordering::Acquire) {
                        break 'input;
                    }
                    match writer.write(&bytes[offset..]) {
                        Ok(0) => break 'input,
                        Ok(count) => offset += count,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            #[cfg(unix)]
                            if let Some(fd) = poll_fd.as_ref() {
                                use std::os::fd::AsRawFd;
                                let mut poll = libc::pollfd {
                                    fd: fd.as_raw_fd(),
                                    events: libc::POLLOUT,
                                    revents: 0,
                                };
                                unsafe {
                                    libc::poll(&mut poll, 1, 100);
                                }
                                continue;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => {
                            shared.metadata.lock().error = Some(format!("Write to shell: {error}"));
                            shared.changed();
                            break 'input;
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn read_output(
    mut reader: Box<dyn Read + Send>,
    term: Arc<Mutex<Term<Listener>>>,
    shared: Arc<Shared>,
    #[cfg(unix)] poll_fd: Option<std::os::fd::OwnedFd>,
) {
    let mut processor: Processor = Processor::new();
    let mut osc_limiter = OscLimiter::default();
    let mut buffer = [0u8; 8 * 1024];
    while !shared.stopped.load(Ordering::Acquire) {
        #[cfg(unix)]
        if let Some(fd) = poll_fd.as_ref() {
            use std::os::fd::AsRawFd;
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = processor
                .sync_timeout()
                .sync_timeout()
                .map(|deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(500) as i32
                })
                .unwrap_or(500);
            let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
            if processor
                .sync_timeout()
                .sync_timeout()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                processor.stop_sync(&mut *term.lock());
                shared.changed();
            }
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                osc_limiter.advance(&mut processor, &mut *term.lock(), &buffer[..count]);
                shared.changed();
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break, // A PTY commonly reports EIO when its slave closes.
        }
    }
    processor.stop_sync(&mut *term.lock());
    shared.changed();
}

#[cfg(unix)]
fn duplicate_fd(fd: std::os::fd::RawFd) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    (duplicate >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) })
}

fn viewport_point(term: &Term<Listener>, col: u16, row: u16) -> Point {
    Point::new(
        Line(
            (row as usize).min(term.screen_lines() - 1) as i32
                - term.grid().display_offset() as i32,
        ),
        Column((col as usize).min(term.columns() - 1)),
    )
}

fn key_bytes(key: &str, ctrl: bool, alt: bool, shift: bool, mode: TermMode) -> Option<Vec<u8>> {
    // winit uses a named key for the space bar on macOS. Normalize it before
    // encoding text keys so typed spaces retain the same modifier semantics.
    let key = if key == "Space" { " " } else { key };
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |suffix: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{suffix}")
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{suffix}")
        } else {
            format!("\x1b[{suffix}")
        }
    };
    let tilde = |number: u8| {
        if modifier > 1 {
            format!("\x1b[{number};{modifier}~")
        } else {
            format!("\x1b[{number}~")
        }
    };
    let kitty =
        mode.intersects(TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC);
    let mut sequence = match key {
        "ArrowUp" | "Up" => csi('A'),
        "ArrowDown" | "Down" => csi('B'),
        "ArrowRight" | "Right" => csi('C'),
        "ArrowLeft" | "Left" => csi('D'),
        "Home" => csi('H'),
        "End" => csi('F'),
        "Insert" => tilde(2),
        "Delete" => tilde(3),
        "PageUp" => tilde(5),
        "PageDown" => tilde(6),
        "F1" | "F2" | "F3" | "F4" => {
            let suffix = match key {
                "F1" => 'P',
                "F2" => 'Q',
                "F3" => 'R',
                _ => 'S',
            };
            if modifier > 1 {
                format!("\x1b[1;{modifier}{suffix}")
            } else {
                format!("\x1bO{suffix}")
            }
        }
        "F5" => tilde(15),
        "F6" => tilde(17),
        "F7" => tilde(18),
        "F8" => tilde(19),
        "F9" => tilde(20),
        "F10" => tilde(21),
        "F11" => tilde(23),
        "F12" => tilde(24),
        "Tab" if shift && !kitty => "\x1b[Z".into(),
        "Enter" | "Tab" | "Escape" | "Backspace" => {
            let code = match key {
                "Enter" => 13,
                "Tab" => 9,
                "Escape" => 27,
                _ => 127,
            };
            if kitty
                && (modifier > 1
                    || key == "Escape"
                    || mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC))
            {
                return Some(format!("\x1b[{code};{modifier}u").into_bytes());
            }
            let byte = if key == "Backspace" && ctrl {
                8
            } else {
                code as u8
            };
            let mut bytes = vec![byte];
            if alt {
                bytes.insert(0, 0x1b);
            }
            return Some(bytes);
        }
        text if text.chars().count() == 1 => {
            let ch = text.chars().next()?;
            if kitty && (ctrl || alt || mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC)) {
                return Some(
                    format!("\x1b[{};{modifier}u", ch.to_ascii_lowercase() as u32).into_bytes(),
                );
            }
            if ctrl {
                let ch = ch.to_ascii_uppercase();
                let byte = match ch {
                    '@'..='_' => ch as u8 & 0x1f,
                    ' ' | '2' => 0,
                    '6' => 0x1e,
                    '-' | '7' => 0x1f,
                    '8' | '?' => 0x7f,
                    _ => return None,
                };
                let mut bytes = vec![byte];
                if alt {
                    bytes.insert(0, 0x1b);
                }
                return Some(bytes);
            }
            text.to_owned()
        }
        _ => return None,
    };
    if alt && key.chars().count() == 1 {
        sequence.insert(0, '\x1b');
    }
    Some(sequence.into_bytes())
}

fn palette(index: usize) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [23, 30, 35],
        [239, 115, 115],
        [131, 204, 151],
        [224, 191, 112],
        [123, 170, 232],
        [183, 150, 220],
        [109, 201, 201],
        [208, 218, 222],
        [91, 107, 115],
        [255, 145, 145],
        [157, 225, 174],
        [246, 215, 140],
        [153, 197, 255],
        [207, 178, 244],
        [144, 226, 221],
        [239, 246, 247],
    ];
    match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let value = index - 16;
            let component = |x: usize| if x == 0 { 0 } else { (55 + 40 * x) as u8 };
            [
                component(value / 36),
                component((value / 6) % 6),
                component(value % 6),
            ]
        }
        232..=255 => [8 + 10 * (index as u8 - 232); 3],
        257 => DEFAULT_BG,
        259..=266 => ANSI[index - 259].map(|x| (x as f32 * 0.66) as u8),
        267 => [239, 246, 247],
        268 => DEFAULT_FG.map(|x| (x as f32 * 0.66) as u8),
        _ => DEFAULT_FG,
    }
}

fn resolve_color(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    bold: bool,
) -> [u8; 3] {
    let index = match color {
        Color::Spec(Rgb { r, g, b }) => return [r, g, b],
        Color::Indexed(index) => index as usize,
        Color::Named(named) => {
            if bold && (named as usize) < 8 {
                named as usize + 8
            } else if bold && named == NamedColor::Foreground {
                NamedColor::BrightForeground as usize
            } else {
                named as usize
            }
        }
    };
    colors[index]
        .map(|Rgb { r, g, b }| [r, g, b])
        .unwrap_or_else(|| palette(index))
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>();
    // libproc copies exactly this C struct into a correctly aligned buffer.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if written != size as i32 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let bytes: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    path.is_absolute().then_some(path)
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cwd(_: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;

    #[derive(Clone, Default)]
    struct Titles(Arc<Mutex<Vec<String>>>);

    impl EventListener for Titles {
        fn send_event(&self, event: Event) {
            if let Event::Title(title) = event {
                self.0.lock().push(title);
            }
        }
    }

    #[test]
    fn osc_limit_preserves_normal_controls_and_split_utf8() {
        let input = "a\x1b]2;中 💻\x1b\\\x1b[31m中b\x1b[0m\x1b]2;second\x07c\x1b\0]2;with-nul\x07"
            .as_bytes();
        let size = Size { cols: 80, rows: 24 };
        let expected_titles = Titles::default();
        let mut expected = Term::new(Config::default(), &size, expected_titles.clone());
        let mut direct: Processor = Processor::new();
        direct.advance(&mut expected, input);
        for chunk_size in 1..=7 {
            let titles = Titles::default();
            let mut term = Term::new(Config::default(), &size, titles.clone());
            let mut parser: Processor = Processor::new();
            let mut limiter = OscLimiter::default();
            for chunk in input.chunks(chunk_size) {
                limiter.advance(&mut parser, &mut term, chunk);
            }
            assert_eq!(*titles.0.lock(), *expected_titles.0.lock());
            for column in 0..80 {
                assert_eq!(
                    term.grid()[Line(0)][Column(column)],
                    expected.grid()[Line(0)][Column(column)]
                );
            }
            assert_eq!(limiter.state, OscState::Ground);
        }
    }

    #[test]
    fn oversized_and_unterminated_osc_stays_bounded() {
        let size = Size { cols: 80, rows: 24 };
        let titles = Titles::default();
        let mut term = Term::new(Config::default(), &size, titles.clone());
        let mut parser: Processor = Processor::new();
        let mut limiter = OscLimiter::default();
        let block = [b'x'; 8192];
        for _ in 0..4 {
            limiter.advance(&mut parser, &mut term, b"\x1b\0]2;");
            // Two MiB without a terminator must stop accumulating immediately
            // after 16 KiB, and repeating this must not retain old messages.
            for _ in 0..256 {
                limiter.advance(&mut parser, &mut term, &block);
            }
            assert_eq!(limiter.state, OscState::Discard);
            assert!(
                titles
                    .0
                    .lock()
                    .iter()
                    .all(|title| title.len() <= MAX_OSC_BYTES)
            );
            limiter.advance(&mut parser, &mut term, b"\x07");
        }
        assert_eq!(titles.0.lock().len(), 4);
        limiter.advance(&mut parser, &mut term, b"\x1b]2;");
        for _ in 0..4 {
            limiter.advance(&mut parser, &mut term, &block);
        }
        // ESC also ends an OSC and begins the following sequence. Preserve the
        // new ANSI color command and its UTF-8 character across read boundaries.
        limiter.advance(&mut parser, &mut term, b"\x1b");
        limiter.advance(&mut parser, &mut term, b"[31m\xe4\xb8");
        limiter.advance(&mut parser, &mut term, b"\xad\x1b[0m");
        let first = &term.grid()[Line(0)][Column(0)];
        assert_eq!(first.c, '中');
        assert_eq!(first.fg, Color::Named(NamedColor::Red));
        assert_eq!(limiter.state, OscState::Ground);
    }

    #[test]
    fn combining_character_flood_has_bounded_cell_storage() {
        let size = Size { cols: 80, rows: 24 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"a");
        // Two MiB of valid UTF-8 combining marks on one cell used to grow its
        // Vec forever. Exercise the parser in the same bounded blocks as PTY IO.
        let block = "\u{301}".repeat(4096);
        for _ in 0..256 {
            parser.advance(&mut term, block.as_bytes());
        }
        let cell = &term.grid()[Line(0)][Column(0)];
        assert_eq!(cell.c, 'a');
        assert_eq!(cell.zerowidth().unwrap().len(), 9);
        assert!(
            cell.zerowidth()
                .unwrap()
                .iter()
                .all(|&mark| mark == '\u{301}')
        );
        // A split UTF-8 sequence and following ANSI state still parse normally.
        parser.advance(&mut term, b"\x1b[31m\xe4\xb8");
        parser.advance(&mut term, b"\xad\x1b[0m");
        let next = &term.grid()[Line(0)][Column(1)];
        assert_eq!(next.c, '\u{4e2d}');
        assert_eq!(next.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn repeated_kitty_mode_pushes_are_bounded_and_do_not_panic() {
        let size = Size { cols: 80, rows: 24 };
        let config = Config {
            kitty_keyboard: true,
            ..Config::default()
        };
        let mut term = Term::new(config, &size, VoidListener);
        let mut parser: Processor = Processor::new();
        for _ in 0..10_000 {
            parser.advance(&mut term, b"\x1b[>1u");
        }
        assert!(term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        // The documented core stack limit is 4096; popping it must empty the
        // stack even after more than twice that many pushes.
        parser.advance(&mut term, b"\x1b[<4096u");
        assert!(!term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
    }

    fn shell(script: &str, cwd: &Path) -> TerminalSession {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        TerminalSession::spawn_command(cwd, 80, 24, command, Arc::new(|| {})).unwrap()
    }

    fn until(
        session: &TerminalSession,
        predicate: impl Fn(&TerminalSnapshot) -> bool,
    ) -> TerminalSnapshot {
        let started = Instant::now();
        loop {
            let snapshot = session.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "terminal condition timed out: {snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn real_pty_preserves_ansi_unicode_and_exit() {
        let session = shell("printf '\\033[31mhello中\\033[0m\\n'", Path::new("/tmp"));
        let snapshot = until(&session, |s| {
            session.is_exited() && s.cells.iter().any(|c| c.text == "中")
        });
        let first = snapshot.cells.iter().find(|c| c.text == "h").unwrap();
        assert_eq!(first.fg, palette(1));
        assert!(session.child_pid().is_some());
    }

    #[test]
    fn real_pty_tabbed_filenames_keep_grid_columns_and_copy_text() {
        let session = shell("printf 'left\\t\\tns-后台-用户管理\\n'", Path::new("/tmp"));
        let snapshot = until(&session, |s| {
            session.is_exited() && s.cells.iter().any(|c| c.text == "理")
        });
        let cells: Vec<_> = snapshot
            .cells
            .iter()
            .map(|cell| (cell.row, cell.col, cell.text.as_str()))
            .collect();
        assert_eq!(
            cells,
            [
                (0, 0, "l"),
                (0, 1, "e"),
                (0, 2, "f"),
                (0, 3, "t"),
                (0, 16, "n"),
                (0, 17, "s"),
                (0, 18, "-"),
                (0, 19, "后"),
                (0, 21, "台"),
                (0, 23, "-"),
                (0, 24, "用"),
                (0, 26, "户"),
                (0, 28, "管"),
                (0, 30, "理"),
            ]
        );
        session.select_start(0, 0);
        session.select_update(31, 0);
        assert_eq!(
            session.selected_text().as_deref(),
            Some("left\t\tns-后台-用户管理")
        );
    }

    #[test]
    fn real_pty_styled_tabs_keep_decorations_without_shaping_controls() {
        // A tab retains the existing blank cell's attributes, so first paint
        // a styled space and then move back to put a tab in that same cell.
        let session = shell(
            "printf '\\033[44m \\033[0m\\r\\tB\\n\\033[4m \\033[0m\\r\\tU\\n\\tS\\n'",
            Path::new("/tmp"),
        );
        until(&session, |s| {
            session.is_exited() && s.cells.iter().any(|c| c.text == "S")
        });
        session.select_start(0, 2);
        session.select_update(8, 2);
        let snapshot = session.snapshot();
        for row in 0..3 {
            let tab = snapshot
                .cells
                .iter()
                .find(|cell| cell.row == row && cell.col == 0)
                .unwrap();
            assert!(tab.text.is_empty(), "tab must not reach text shaping");
            match row {
                0 => assert_eq!(tab.bg, palette(4)),
                1 => assert!(tab.underline),
                2 => assert!(tab.selected),
                _ => unreachable!(),
            }
        }
        assert_eq!(session.selected_text().as_deref(), Some("\tS"));
    }

    #[test]
    fn interactive_pty_clears_inherited_no_color_and_preserves_ansi_colors() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("TERM", "dumb");
        command.env("COLORTERM", "");
        command.env("NO_COLOR", "1");
        command.args([
            "-c",
            r#"
            printf 'TERM=%s;COLORTERM=%s;NO_COLOR=%s\n' "$TERM" "$COLORTERM" "${NO_COLOR-unset}"
            if [ -t 1 ] && [ -z "${NO_COLOR-}" ]; then
                printf '\033[31mR\033[0m\n\033[38;5;208mO\033[0m\n\033[38;2;90;180;240mB\033[0m\n'
            fi
            NO_COLOR=1 /bin/sh -c 'printf "OVERRIDE=%s\n" "$NO_COLOR"'
        "#,
        ]);
        let session =
            TerminalSession::spawn_command(Path::new("/tmp"), 100, 24, command, Arc::new(|| {}))
                .unwrap();
        let snapshot = until(&session, |s| {
            session.is_exited()
                && s.cells
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<String>()
                    .contains("OVERRIDE=1")
        });
        let text: String = snapshot.cells.iter().map(|c| c.text.as_str()).collect();
        assert!(text.contains("TERM=xterm-256color;COLORTERM=truecolor;NO_COLOR=unset"));
        for (row, symbol, expected) in [
            (1, "R", palette(1)),
            (2, "O", palette(208)),
            (3, "B", [90, 180, 240]),
        ] {
            let cell = snapshot
                .cells
                .iter()
                .find(|cell| cell.row == row && cell.text == symbol)
                .unwrap();
            assert_eq!(cell.fg, expected);
        }
    }

    #[test]
    fn title_read_preserves_pending_output_until_the_last_character_is_rendered() {
        let session = shell(
            "printf '\\033]0;LeafTerm title test\\007git statu'; exec /bin/sleep 30",
            Path::new("/tmp"),
        );
        let line = |snapshot: &TerminalSnapshot| {
            snapshot
                .cells
                .iter()
                .filter(|cell| cell.row == 0)
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        };
        until(&session, |snapshot| line(snapshot) == "gitstatu");

        // Model the exact reader -> AboutToWait -> queued Wake order without
        // relying on a race between real keyboard and event-loop timing.
        let mut parser: Processor = Processor::new();
        parser.advance(&mut *session.term.lock(), b"s");
        session.shared.changed();
        assert!(session.is_dirty());
        assert_eq!(session.title(), "LeafTerm title test");
        let _ = session.cwd();
        assert!(!session.is_exited());
        assert!(
            session.is_dirty(),
            "reading the title must not consume the pending frame"
        );

        // The queued Wake can now trigger this frame without a second input.
        assert_eq!(line(&session.snapshot()), "gitstatus");
    }

    #[test]
    fn input_resize_and_cwd_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let script = format!(
            "cd '{}'; printf 'ready\\n'; read answer; stty size; printf 'answer:%s\\n' \"$answer\"; sleep 2",
            temp.path().display()
        );
        let session = shell(&script, Path::new("/tmp"));
        until(&session, |s| {
            s.cells
                .iter()
                .filter(|c| c.row == 0)
                .map(|c| c.text.as_str())
                .collect::<String>()
                .contains("ready")
        });
        session.resize(100, 32).unwrap();
        session.input("leaf").unwrap();
        session.send_key("Enter", false, false, false).unwrap();
        let snapshot = until(&session, |s| {
            s.cells
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>()
                .contains("answer:leaf")
        });
        assert_eq!((snapshot.cols, snapshot.rows), (100, 32));
        assert!(
            snapshot
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("32100")
        );
        std::thread::sleep(Duration::from_millis(510));
        assert_eq!(session.cwd(), temp.path().canonicalize().unwrap());
    }

    #[test]
    fn zsh_cd_lists_directories_and_tab_selects_before_enter_executes() {
        let fixture = tempfile::tempdir().unwrap();
        let startup = fixture.path().join("startup");
        let working = fixture.path().join("working");
        std::fs::create_dir_all(&startup).unwrap();
        std::fs::create_dir_all(working.join("leaf_alpha")).unwrap();
        std::fs::create_dir_all(working.join("leaf_beta")).unwrap();
        std::fs::write(working.join("not_a_directory.txt"), "file").unwrap();
        std::fs::write(
            startup.join(".zshrc"),
            "PROMPT='LEAF> '\nHISTFILE=/dev/null\n",
        )
        .unwrap();
        let mut command = CommandBuilder::new("/bin/zsh");
        command.args(["-i", "-l"]);
        command.env("ZDOTDIR", &startup);
        command.env("LC_COLLATE", "C");
        let integration =
            crate::shell::configure(&mut command, std::ffi::OsStr::new("/bin/zsh")).unwrap();
        let mut session =
            TerminalSession::spawn_command(&working, 100, 30, command, Arc::new(|| {})).unwrap();
        session._shell_integration = integration;
        let screen_text =
            |s: &TerminalSnapshot| s.cells.iter().map(|c| c.text.as_str()).collect::<String>();
        until(&session, |s| screen_text(s).contains("LEAF>"));
        session.input("c").unwrap();
        session.input("d").unwrap();
        // macOS winit reports the physical space bar as NamedKey::Space,
        // unlike pasted text, which goes directly through input/paste.
        session.send_key("Space", false, false, false).unwrap();
        let listed = until(&session, |s| {
            let text = screen_text(s);
            text.contains("leaf_alpha") && text.contains("leaf_beta")
        });
        assert!(!screen_text(&listed).contains("not_a_directory.txt"));
        session.send_key("Tab", false, false, false).unwrap();
        until(&session, |s| screen_text(s).contains("cdleaf_alpha/"));
        session.send_key("Tab", false, false, false).unwrap();
        until(&session, |s| screen_text(s).contains("cdleaf_beta/"));
        session.send_key("Enter", false, false, false).unwrap();
        std::thread::sleep(Duration::from_millis(550));
        assert_eq!(session.cwd(), working.canonicalize().unwrap());
        session.send_key("Enter", false, false, false).unwrap();
        until(&session, |_| {
            session.cwd() == working.join("leaf_beta").canonicalize().unwrap()
        });
    }

    #[test]
    fn named_space_key_preserves_modifiers_and_kitty_protocol() {
        let named_space = format!("{:?}", winit::keyboard::NamedKey::Space);
        let cases: &[(bool, bool, bool, TermMode, &[u8])] = &[
            (false, false, false, TermMode::empty(), b" "),
            (false, false, true, TermMode::empty(), b" "),
            (true, false, false, TermMode::empty(), b"\0"),
            (false, true, false, TermMode::empty(), b"\x1b "),
            (true, true, false, TermMode::empty(), b"\x1b\0"),
            (
                true,
                false,
                false,
                TermMode::DISAMBIGUATE_ESC_CODES,
                b"\x1b[32;5u",
            ),
            (
                false,
                true,
                false,
                TermMode::DISAMBIGUATE_ESC_CODES,
                b"\x1b[32;3u",
            ),
            (
                false,
                false,
                false,
                TermMode::REPORT_ALL_KEYS_AS_ESC,
                b"\x1b[32;1u",
            ),
            (
                false,
                false,
                true,
                TermMode::REPORT_ALL_KEYS_AS_ESC,
                b"\x1b[32;2u",
            ),
            (
                true,
                true,
                true,
                TermMode::REPORT_ALL_KEYS_AS_ESC,
                b"\x1b[32;8u",
            ),
        ];
        for &(ctrl, alt, shift, mode, expected) in cases {
            assert_eq!(
                key_bytes(&named_space, ctrl, alt, shift, mode).as_deref(),
                Some(expected),
                "ctrl={ctrl}, alt={alt}, shift={shift}, mode={mode:?}"
            );
        }
    }

    #[test]
    fn key_encoding_respects_application_and_kitty_modes() {
        assert_eq!(
            key_bytes("ArrowUp", false, false, false, TermMode::APP_CURSOR).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            key_bytes("c", true, false, false, TermMode::empty()).unwrap(),
            b"\x03"
        );
        assert_eq!(
            key_bytes(
                "Enter",
                false,
                false,
                true,
                TermMode::DISAMBIGUATE_ESC_CODES
            )
            .unwrap(),
            b"\x1b[13;2u"
        );
        assert_eq!(
            key_bytes("Tab", false, false, true, TermMode::empty()).unwrap(),
            b"\x1b[Z"
        );
    }

    #[cfg(unix)]
    #[test]
    fn closing_session_terminates_and_reaps_its_child() {
        let session = shell("printf 'ready\\n'; exec /bin/sleep 30", Path::new("/tmp"));
        until(&session, |snapshot| {
            snapshot
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("ready")
        });
        let pid = session.child_pid().unwrap() as libc::pid_t;
        // Signal 0 checks only this test's newly spawned process.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        drop(session);
        let started = Instant::now();
        loop {
            let result = unsafe { libc::kill(pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "closed shell was not reaped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn closing_session_cancels_blocked_input_and_kills_hup_ignoring_child() {
        let session = shell(
            "trap '' HUP; stty -echo raw; printf 'ready\\n'; exec /bin/sleep 30",
            Path::new("/tmp"),
        );
        until(&session, |snapshot| {
            snapshot
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("ready")
        });
        let pid = session.child_pid().unwrap() as libc::pid_t;
        let shared = Arc::downgrade(&session.shared);
        // Sleep does not read input. Fill the PTY so that close must cancel
        // backpressure rather than leaving an input worker and master FD alive.
        session.input(&"x".repeat(MAX_INPUT_BYTES)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        drop(session);
        let started = Instant::now();
        loop {
            let result = unsafe { libc::kill(pid, 0) };
            let reaped =
                result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if reaped && shared.upgrade().is_none() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "closed child or PTY workers leaked"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn selection_and_history_search_use_terminal_grid() {
        let session = shell("printf 'alpha beta\\n'; sleep 1", Path::new("/tmp"));
        until(&session, |s| {
            s.cells
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>()
                .contains("alphabeta")
        });
        assert!(session.find("beta", true));
        assert_eq!(session.selected_text().as_deref(), Some("beta"));
        session.select_start(0, 0);
        session.select_update(4, 0);
        assert_eq!(session.selected_text().as_deref(), Some("alpha"));
    }
}
