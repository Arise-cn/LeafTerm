//! Native windows, routed worker events, and a single session-state writer.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    window::{Theme, Window, WindowId},
};

use crate::{app, editor::RecoveryDraft, layout::Layout};

const MAX_RECOVERY_BYTES: usize = 32 * 1024 * 1024;
const RECOVERY_DELAY: Duration = Duration::from_millis(500);

pub enum Event {
    Wake(u64),
    Window(u64, Box<app::Event>),
    NewWindow { source: u64, cwd: PathBuf },
    Save,
    Quit,
}

#[derive(Clone)]
pub struct WindowProxy {
    route: u64,
    proxy: EventLoopProxy<Event>,
}

impl WindowProxy {
    pub fn send_event(&self, event: app::Event) -> Result<(), ()> {
        let event = match event {
            app::Event::Wake => Event::Wake(self.route),
            event => Event::Window(self.route, Box::new(event)),
        };
        self.proxy.send_event(event).map_err(|_| ())
    }

    pub fn save(&self) {
        let _ = self.proxy.send_event(Event::Save);
    }
    pub fn new_window(&self, cwd: PathBuf) {
        let _ = self.proxy.send_event(Event::NewWindow {
            source: self.route,
            cwd,
        });
    }
    pub fn quit(&self) {
        let _ = self.proxy.send_event(Event::Quit);
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedPane {
    pub id: u64,
    pub cwd: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedWindow {
    pub name: String,
    pub layout: Layout,
    pub active: u64,
    pub panes: Vec<SavedPane>,
    pub font: f32,
    pub sidebar: f32,
    #[serde(default = "sidebar_visible_by_default")]
    pub sidebar_visible: bool,
}

fn sidebar_visible_by_default() -> bool {
    true
}

impl SavedWindow {
    pub fn valid(&self) -> bool {
        let ids = self.layout.ids();
        let unique: HashSet<_> = ids.iter().copied().collect();
        let saved: HashSet<_> = self.panes.iter().map(|p| p.id).collect();
        !ids.is_empty()
            && ids.len() <= 32
            && unique.len() == ids.len()
            && saved.len() == self.panes.len()
            && saved == unique
            && unique.contains(&self.active)
    }

    fn fresh(cwd: PathBuf, preferences: Option<&Self>) -> Self {
        Self {
            name: String::new(),
            layout: Layout::Pane(1),
            active: 1,
            panes: vec![SavedPane { id: 1, cwd }],
            font: preferences.map_or(14.0, |p| p.font),
            sidebar: preferences.map_or(296.0, |p| p.sidebar),
            sidebar_visible: preferences.is_none_or(|p| p.sidebar_visible),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    version: u32,
    windows: Vec<SavedWindow>,
}

#[derive(Clone, Serialize, Deserialize)]
struct RecoveryEntry {
    window_index: Option<usize>,
    window: SavedWindow,
    draft: RecoveryDraft,
}

#[derive(Serialize, Deserialize)]
struct RecoveryFile {
    version: u32,
    entries: Vec<RecoveryEntry>,
}

fn same_window(left: &SavedWindow, right: &SavedWindow) -> bool {
    let panes = |state: &SavedWindow| {
        let mut panes: Vec<_> = state.panes.iter().map(|p| (p.id, p.cwd.clone())).collect();
        panes.sort();
        panes
    };
    left.name == right.name
        && panes(left) == panes(right)
        && serde_json::to_value(&left.layout).ok() == serde_json::to_value(&right.layout).ok()
}

fn read_private_json(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Recovery path is not a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot inspect recovery file"),
    }
    let file = options.open(path).context("Cannot open recovery file")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit as u64,
        "Recovery file exceeds its size limit"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "Recovery file exceeds its size limit");
    Ok(Some(bytes))
}

struct BoundedWriter<'a> {
    file: &'a mut fs::File,
    remaining: usize,
}
impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other("Draft backup exceeds 32 MiB"));
        }
        let written = self.file.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn atomic_private_json(path: &Path, value: &impl Serialize, limit: usize) -> Result<()> {
    let parent = path
        .parent()
        .context("Recovery path has no parent directory")?;
    fs::create_dir_all(parent).context("Cannot create recovery directory")?;
    ensure!(
        !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "Recovery directory must not be a symbolic link"
    );
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Recovery path is not a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Cannot inspect recovery file"),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(
        BoundedWriter {
            file: temporary.as_file_mut(),
            remaining: limit,
        },
        value,
    )
    .context("Cannot write draft backup")?;
    temporary.as_file().sync_all()?;
    // Renaming replaces the directory entry; a replacement symlink is never followed.
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Cannot replace draft backup")?;
    Ok(())
}

#[derive(Deserialize)]
struct LegacyTab {
    name: String,
    layout: Layout,
    active: u64,
}
#[derive(Deserialize)]
struct LegacySession {
    tabs: Vec<LegacyTab>,
    active_tab: usize,
    panes: Vec<SavedPane>,
    font: f32,
    sidebar: f32,
    #[serde(default = "sidebar_visible_by_default")]
    sidebar_visible: bool,
}

fn decode_session(bytes: &[u8]) -> Option<Vec<SavedWindow>> {
    if let Ok(saved) = serde_json::from_slice::<SavedSession>(bytes) {
        return (saved.version == 1
            && !saved.windows.is_empty()
            && saved.windows.len() <= 32
            && saved.windows.iter().all(SavedWindow::valid))
        .then_some(saved.windows);
    }
    let legacy: LegacySession = serde_json::from_slice(bytes).ok()?;
    if legacy.tabs.is_empty() || legacy.tabs.len() > 32 || legacy.panes.len() > 32 {
        return None;
    }
    let mut used = HashSet::new();
    let mut windows = Vec::new();
    for tab in legacy.tabs {
        let ids = tab.layout.ids();
        if ids.iter().any(|id| !used.insert(*id)) {
            return None;
        }
        let panes: Vec<_> = legacy
            .panes
            .iter()
            .filter(|p| ids.contains(&p.id))
            .cloned()
            .collect();
        let automatic_name = panes
            .iter()
            .find(|p| p.id == tab.active)
            .and_then(|p| p.cwd.file_name())
            .is_some_and(|name| name == tab.name.as_str());
        let saved = SavedWindow {
            name: if automatic_name {
                String::new()
            } else {
                tab.name
            },
            layout: tab.layout,
            active: tab.active,
            panes,
            font: legacy.font,
            sidebar: legacy.sidebar,
            sidebar_visible: legacy.sidebar_visible,
        };
        if !saved.valid() {
            return None;
        }
        windows.push(saved);
    }
    if used.len() != legacy.panes.len() {
        return None;
    }
    // Create the formerly active tab last, so it is the frontmost native window.
    let active = legacy.active_tab.min(windows.len() - 1);
    let active_window = windows.remove(active);
    windows.push(active_window);
    Some(windows)
}

pub struct Windows {
    proxy: EventLoopProxy<Event>,
    windows: HashMap<WindowId, app::App>,
    order: Vec<WindowId>,
    routes: HashMap<u64, WindowId>,
    next_route: u64,
    initial_dir: PathBuf,
    restore: bool,
    started: bool,
    draft_versions: Vec<(WindowId, u64, bool)>,
    recovery_due: Option<Instant>,
    recovery_blocked: bool,
    recovery_error: Option<String>,
    unattached_recovery: Vec<RecoveryEntry>,
}

impl Windows {
    pub fn new(proxy: EventLoopProxy<Event>, initial_dir: PathBuf, restore: bool) -> Self {
        Self {
            proxy,
            windows: HashMap::new(),
            order: Vec::new(),
            routes: HashMap::new(),
            next_route: 1,
            initial_dir,
            restore,
            started: false,
            draft_versions: Vec::new(),
            recovery_due: None,
            recovery_blocked: false,
            recovery_error: None,
            unattached_recovery: Vec::new(),
        }
    }

    fn session_path() -> PathBuf {
        // Used by isolated UI checks without changing HOME or the user's session.
        if let Some(path) = std::env::var_os("LEAFTERM_SESSION_PATH").map(PathBuf::from)
            && path.is_absolute()
        {
            return path;
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        home.join("Library/Application Support/LeafTerm/session.json")
    }

    fn load() -> Option<Vec<SavedWindow>> {
        let path = Self::session_path();
        if std::fs::metadata(&path).map_or(true, |m| m.len() > 256 * 1024) {
            return None;
        }
        decode_session(&std::fs::read(path).ok()?)
    }

    fn save(&self) {
        if self.windows.is_empty() {
            return;
        }
        let windows = self
            .order
            .iter()
            .filter_map(|id| self.windows.get(id).map(app::App::saved_window))
            .collect();
        let state = SavedSession {
            version: 1,
            windows,
        };
        let path = Self::session_path();
        if let (Some(parent), Ok(bytes)) = (path.parent(), serde_json::to_vec_pretty(&state))
            && std::fs::create_dir_all(parent).is_ok()
        {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }

    fn recovery_path() -> PathBuf {
        Self::session_path().with_file_name("recovery.json")
    }

    fn load_recovery() -> Result<Vec<RecoveryEntry>> {
        let Some(bytes) = read_private_json(&Self::recovery_path(), MAX_RECOVERY_BYTES)? else {
            return Ok(Vec::new());
        };
        let file: RecoveryFile =
            serde_json::from_slice(&bytes).context("Cannot decode draft backup")?;
        ensure!(
            file.version == 1 && file.entries.len() <= 32,
            "Unsupported draft backup format or count"
        );
        ensure!(
            file.entries.iter().all(|entry| entry.window.valid()),
            "Draft backup contains an invalid window layout"
        );
        Ok(file.entries)
    }

    fn observe_drafts(&mut self) {
        let versions: Vec<_> = self
            .order
            .iter()
            .filter_map(|id| {
                self.windows
                    .get(id)
                    .map(|app| (*id, app.draft_revision(), app.has_unsaved_file()))
            })
            .collect();
        if versions != self.draft_versions {
            let cleared = versions.iter().any(|(id, _, dirty)| {
                !dirty
                    && self
                        .draft_versions
                        .iter()
                        .any(|(previous, _, was_dirty)| previous == id && *was_dirty)
            });
            self.draft_versions = versions;
            if !self.recovery_blocked {
                // Explicit save/discard removes stale drafts without the typing debounce.
                self.recovery_due = Some(
                    Instant::now()
                        + if cleared {
                            Duration::ZERO
                        } else {
                            RECOVERY_DELAY
                        },
                );
            }
        }
    }

    fn report_recovery_error(&mut self, message: String) {
        if self.recovery_error.as_ref() != Some(&message) {
            eprintln!("LeafTerm draft recovery: {message}");
            if let Some(app) = self.order.last().and_then(|id| self.windows.get_mut(id)) {
                app.notify(format!("Draft backup: {message}"));
            }
            self.recovery_error = Some(message);
        }
    }

    fn flush_recovery(&mut self) {
        if self.recovery_blocked {
            return;
        }
        let mut entries = self.unattached_recovery.clone();
        for (index, id) in self.order.iter().enumerate() {
            if let Some(app) = self.windows.get(id)
                && let Some(draft) = app.recovery_draft()
            {
                entries.push(RecoveryEntry {
                    window_index: Some(index),
                    window: app.saved_window(),
                    draft,
                });
            }
        }
        let result = if entries.len() > 32 {
            Err(anyhow::anyhow!(
                "More than 32 unsaved files; the previous backup is retained"
            ))
        } else {
            atomic_private_json(
                &Self::recovery_path(),
                &RecoveryFile {
                    version: 1,
                    entries,
                },
                MAX_RECOVERY_BYTES,
            )
        };
        match result {
            Ok(()) => {
                self.recovery_due = None;
                self.recovery_error = None;
            }
            Err(error) => {
                self.report_recovery_error(format!("{error:#}"));
                self.recovery_due = Some(Instant::now() + Duration::from_secs(2));
            }
        }
    }

    fn create(&mut self, event_loop: &ActiveEventLoop, saved: SavedWindow) -> Result<WindowId> {
        ensure!(self.windows.len() < 32, "Window limit reached (32)");
        let cwd = saved
            .panes
            .iter()
            .find(|p| p.id == saved.active)
            .map(|p| p.cwd.clone())
            .filter(|cwd| cwd.is_dir())
            .unwrap_or_else(|| self.initial_dir.clone());
        let mut attrs = Window::default_attributes()
            .with_title("LeafTerm")
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(1320.0, 840.0))
            .with_min_inner_size(LogicalSize::new(720.0, 450.0));
        let shift = self.order.len().min(8) as f64 * 24.0;
        if shift > 0.0 {
            attrs = attrs.with_position(LogicalPosition::new(80.0 + shift, 80.0 + shift));
        }
        let window = Arc::new(event_loop.create_window(attrs)?);
        let id = window.id();
        let route = self.next_route;
        self.next_route += 1;
        let proxy = WindowProxy {
            route,
            proxy: self.proxy.clone(),
        };
        let mut app = app::App::new(proxy, cwd, Some(saved));
        if let Err(error) = app.start(window) {
            app.shutdown();
            return Err(error);
        }
        self.order.push(id);
        self.routes.insert(route, id);
        self.windows.insert(id, app);
        Ok(id)
    }

    fn close_requested(&mut self, event_loop: &ActiveEventLoop) {
        let closing: Vec<_> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.windows.get(id).is_some_and(|app| app.close_requested))
            .collect();
        if closing.is_empty() {
            return;
        }
        // Keep the last window's layout for the next launch.
        self.save();
        for id in closing {
            if let Some(mut app) = self.windows.remove(&id) {
                app.shutdown();
            }
            self.order.retain(|candidate| *candidate != id);
            self.routes.retain(|_, window| *window != id);
        }
        self.flush_recovery();
        if self.windows.is_empty() {
            event_loop.exit();
        } else {
            self.save();
        }
    }
}

impl ApplicationHandler<Event> for Windows {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.started {
            return;
        }
        self.started = true;
        let previous = Self::load().unwrap_or_default();
        let states = if self.restore && !previous.is_empty() {
            previous
        } else {
            vec![SavedWindow::fresh(
                self.initial_dir.clone(),
                previous.last(),
            )]
        };
        let mut recovery = match Self::load_recovery() {
            Ok(entries) => entries,
            Err(error) => {
                self.recovery_blocked = true;
                self.recovery_error = Some(format!("Backup retained without changes: {error:#}"));
                Vec::new()
            }
        };
        for (index, state) in states.into_iter().enumerate() {
            let matching = self
                .restore
                .then(|| {
                    recovery.iter().position(|entry| {
                        entry.window_index == Some(index) && same_window(&entry.window, &state)
                    })
                })
                .flatten();
            match self.create(event_loop, state) {
                Ok(id) => {
                    if let Some(index) = matching {
                        let entry = recovery.remove(index);
                        self.windows
                            .get_mut(&id)
                            .unwrap()
                            .restore_draft(entry.draft);
                    }
                }
                Err(error) => eprintln!("LeafTerm window failed: {error:#}"),
            }
        }
        // A changed/fresh layout must not attach a draft to an unrelated terminal.
        for mut entry in recovery {
            match self.create(event_loop, entry.window.clone()) {
                Ok(id) => self
                    .windows
                    .get_mut(&id)
                    .unwrap()
                    .restore_draft(entry.draft),
                Err(error) => {
                    eprintln!("LeafTerm draft window failed: {error:#}");
                    entry.window_index = None;
                    self.unattached_recovery.push(entry);
                }
            }
        }
        if let Some(error) = self.recovery_error.take() {
            self.report_recovery_error(error);
        }
        if !self.unattached_recovery.is_empty() {
            self.report_recovery_error(
                "Some draft windows could not open; their backups are retained.".into(),
            );
        }
        self.observe_drafts();
        if self.windows.is_empty() {
            event_loop.exit();
        } else {
            self.save();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Wake(route) => {
                if let Some(id) = self.routes.get(&route)
                    && let Some(app) = self.windows.get_mut(id)
                {
                    app.user_event(event_loop, app::Event::Wake);
                }
            }
            Event::Window(route, event) => {
                if let Some(id) = self.routes.get(&route)
                    && let Some(app) = self.windows.get_mut(id)
                {
                    app.user_event(event_loop, *event);
                }
            }
            Event::NewWindow { source, cwd } => {
                if let Some(id) = self.routes.get(&source)
                    && let Some(app) = self.windows.get(id)
                {
                    let state = SavedWindow::fresh(cwd, Some(&app.saved_window()));
                    if let Err(error) = self.create(event_loop, state) {
                        eprintln!("LeafTerm window failed: {error:#}");
                    }
                    self.save();
                }
            }
            Event::Save => {
                self.save();
                self.observe_drafts();
            }
            Event::Quit => {
                if let Some(app) = self
                    .order
                    .iter()
                    .find_map(|id| {
                        self.windows
                            .get(id)
                            .filter(|app| app.has_unsaved_file())
                            .map(|_| *id)
                    })
                    .and_then(|id| self.windows.get_mut(&id))
                {
                    app.confirm_editor_quit();
                } else {
                    self.save();
                    self.flush_recovery();
                    event_loop.exit();
                }
            }
        }
        self.close_requested(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // During application termination winit sends Destroyed before exiting.
        // Keep Apps until exiting so the final saved snapshot contains every window.
        if matches!(event, WindowEvent::Destroyed) {
            return;
        }
        if let Some(app) = self.windows.get_mut(&id) {
            app.window_event(event_loop, id, event);
        }
        self.close_requested(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        for app in self.windows.values_mut() {
            app.about_to_wait();
        }
        self.observe_drafts();
        if self.recovery_due.is_some_and(|due| due <= Instant::now()) {
            self.flush_recovery();
        }
        let deadline = self
            .recovery_due
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(2));
        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.save();
        self.flush_recovery();
        for app in self.windows.values_mut() {
            app.shutdown();
        }
        self.windows.clear();
        self.routes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tabs_become_independent_windows_without_losing_split_panes() {
        let bytes = br#"{"tabs":[{"name":"one","layout":{"Pane":1},"active":1},{"name":"two","layout":{"Split":{"axis":"Horizontal","ratio":0.4,"first":{"Pane":2},"second":{"Pane":3}}},"active":3}],"active_tab":0,"panes":[{"id":1,"cwd":"/one"},{"id":2,"cwd":"/two"},{"id":3,"cwd":"/three"}],"font":14,"sidebar":296,"sidebar_visible":false}"#;
        let windows = decode_session(bytes).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].layout.ids(), vec![2, 3]);
        assert_eq!(windows[0].active, 3);
        assert_eq!(windows[0].panes.len(), 2);
        assert_eq!(windows[1].layout.ids(), vec![1]);
        assert!(!windows[0].sidebar_visible);
        let encoded = serde_json::to_vec(&SavedSession {
            version: 1,
            windows,
        })
        .unwrap();
        let restored = decode_session(&encoded).unwrap();
        assert_eq!(restored[0].layout.ids(), vec![2, 3]);
        assert_eq!(restored[1].panes[0].cwd, PathBuf::from("/one"));
    }

    #[test]
    fn invalid_restore_ids_are_rejected_before_starting_shells() {
        let mut state = SavedWindow::fresh("/tmp".into(), None);
        state.layout.split(1, 1, crate::layout::Axis::Horizontal);
        assert!(!state.valid());
        state.layout = Layout::Pane(1);
        state.active = 7;
        assert!(!state.valid());
        state.active = 1;
        state.panes.push(state.panes[0].clone());
        assert!(!state.valid());
    }

    #[test]
    fn new_windows_keep_preferences_but_start_with_one_fresh_pane() {
        let mut previous = SavedWindow::fresh("/old".into(), None);
        previous.sidebar_visible = false;
        previous.font = 17.0;
        previous.layout.split(1, 2, crate::layout::Axis::Vertical);
        previous.name = "custom".into();
        let fresh = SavedWindow::fresh("/new".into(), Some(&previous));
        assert_eq!(fresh.layout.ids(), vec![1]);
        assert_eq!(fresh.panes[0].cwd, PathBuf::from("/new"));
        assert_eq!(fresh.font, 17.0);
        assert!(!fresh.sidebar_visible);
        assert!(fresh.name.is_empty());
    }

    #[test]
    fn recovery_writes_are_private_atomic_and_size_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery.json");
        let original = serde_json::json!({"text": "unsaved 中文"});
        atomic_private_json(&path, &original, 1024).unwrap();
        let bytes = read_private_json(&path, 1024).unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            original
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            atomic_private_json(&path, &serde_json::json!({"text": "x".repeat(100)}), 16).is_err()
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "failed writes must retain the previous draft"
        );
        assert!(read_private_json(&path, 4).is_err());
        let empty = serde_json::json!({"version": 1, "entries": []});
        atomic_private_json(&path, &empty, 1024).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
            empty
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_does_not_follow_existing_symbolic_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("other.json");
        let path = dir.path().join("recovery.json");
        fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read_private_json(&path, 1024).is_err());
        assert!(atomic_private_json(&path, &serde_json::json!({"text":"draft"}), 1024).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn recovery_matching_ignores_preferences_but_rejects_different_terminal_layouts() {
        let saved = SavedWindow::fresh("/project".into(), None);
        let mut current = saved.clone();
        current.sidebar_visible = false;
        current.font = 20.0;
        assert!(same_window(&saved, &current));
        current.panes[0].cwd = "/different".into();
        assert!(!same_window(&saved, &current));
        current.panes[0].cwd = "/project".into();
        current.layout.split(1, 2, crate::layout::Axis::Horizontal);
        assert!(!same_window(&saved, &current));
    }
}
