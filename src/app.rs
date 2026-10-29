use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use anyhow::Result;
use unicode_segmentation::UnicodeSegmentation;
use winit::{
    dpi::{LogicalPosition, LogicalSize},
    event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{Key, ModifiersState, NamedKey},
    window::{CursorIcon, Window, WindowId},
};

use crate::{
    editor::{Document, Movement, RecoveryDraft, SaveRequest, SavedFile},
    editor_view,
    file_search::{FileSearch, SearchWorker},
    highlight::{HighlightWorker, Highlights},
    layout::{Axis, Bounds, Divider, Layout},
    merge::{self, Conflict},
    merge_view::{self, MergeAction},
    renderer::{Rect, Renderer, TextRun},
    terminal::TerminalSession,
    theme::{AMBER, BG, BORDER, FG, GREEN, MUTED, PANEL, RED, SURFACE},
    windows::{SavedPane, SavedWindow, WindowProxy},
    workspace::{self, DiffKind, DirectoryListing, FileChange, WorkspaceSnapshot},
};

const TOP: f32 = 0.0;
const FOOT: f32 = 0.0;

pub enum Event {
    Wake,
    Highlighted {
        token: u64,
        result: Highlights,
    },
    Workspace {
        cwd: PathBuf,
        hidden: bool,
        result: Result<WorkspaceSnapshot, String>,
        directories: Vec<(PathBuf, DirectoryListing)>,
    },
    Directory {
        path: PathBuf,
        hidden: bool,
        result: Result<DirectoryListing, String>,
    },
    Preview {
        token: u64,
        result: Result<LoadedPreview, String>,
    },
    FileSaved {
        token: u64,
        result: Result<SavedFile, String>,
    },
    FileSearch {
        token: u64,
        result: Result<workspace::FileSearchResult, String>,
    },
    ConflictStaged {
        token: u64,
        result: Result<(), String>,
    },
}

enum Job {
    StageResolved {
        root: PathBuf,
        relative: PathBuf,
        expected: Vec<u8>,
        token: u64,
    },
    Refresh {
        cwd: PathBuf,
        directories: Vec<PathBuf>,
        hidden: bool,
    },
    Directory {
        path: PathBuf,
        hidden: bool,
    },
    Preview {
        source: PreviewSource,
        token: u64,
    },
    SaveFile {
        request: SaveRequest,
        token: u64,
    },
    Recover {
        draft: RecoveryDraft,
        token: u64,
    },
}

pub struct LoadedPreview {
    recovered: bool,
    conflict_root: Option<PathBuf>,
    text: String,
    document: Option<Document>,
    notice: String,
}

#[derive(Clone)]
enum PreviewSource {
    File(PathBuf),
    Conflict {
        root: PathBuf,
        path: PathBuf,
    },
    Diff {
        root: PathBuf,
        change: FileChange,
        kind: DiffKind,
    },
}

impl PreviewSource {
    fn path(&self) -> &Path {
        match self {
            Self::File(path) | Self::Conflict { path, .. } => path,
            Self::Diff { change, .. } => &change.path,
        }
    }

    fn load(&self) -> Result<LoadedPreview> {
        if let Self::Conflict { root, path } = self {
            workspace::validate_text_conflict(root, path.strip_prefix(root)?)?;
        }
        match self {
            Self::File(path) | Self::Conflict { path, .. } => match Document::open(path) {
                Ok(document) => Ok(LoadedPreview {
                    recovered: false,
                    conflict_root: None,
                    text: String::new(),
                    document: Some(document),
                    notice: String::new(),
                }),
                Err(error) => {
                    let content = workspace::load_file(path)?;
                    Ok(LoadedPreview {
                        recovered: false,
                        conflict_root: None,
                        text: content.text,
                        document: None,
                        notice: format!("Read only · {error}"),
                    })
                }
            },
            Self::Diff { root, change, kind } => {
                let content = workspace::load_diff(root, change, *kind)?;
                Ok(LoadedPreview {
                    recovered: false,
                    conflict_root: None,
                    text: content.text,
                    document: None,
                    notice: String::new(),
                })
            }
        }
    }
}

struct Preview {
    source: PreviewSource,
    token: u64,
    text: String,
    offset: usize,
    column: usize,
    loading: bool,
    document: Option<Document>,
    saving: bool,
    notice: String,
    merge: Option<MergeState>,
    highlight_revision: Option<u64>,
    highlight_token: u64,
    highlights: Highlights,
}

#[derive(Default)]
struct MergeState {
    conflicts: Vec<Conflict>,
    selected: usize,
    offset: usize,
    column: usize,
    error: Option<String>,
    staging: bool,
    staged: bool,
}

impl MergeState {
    fn refresh(&mut self, text: &str) {
        match merge::parse(text) {
            Ok(conflicts) => {
                self.conflicts = conflicts;
                self.error = None;
            }
            Err(error) => {
                self.conflicts.clear();
                self.error = Some(error);
            }
        }
        self.selected = self.selected.min(self.conflicts.len().saturating_sub(1));
    }

    fn unresolved(&self) -> bool {
        self.error.is_some() || !self.conflicts.is_empty()
    }
}

impl Preview {
    fn content(&self) -> &str {
        self.document.as_ref().map_or(&self.text, Document::text)
    }

    fn finish_save(&mut self, result: Result<SavedFile, String>) -> bool {
        self.saving = false;
        match result {
            Ok(saved) => {
                if let Some(document) = &mut self.document {
                    document.accept_save(saved);
                }
                self.notice = if self.document.as_ref().is_some_and(Document::is_dirty) {
                    "Earlier changes saved · newer edits remain unsaved"
                } else {
                    "Saved"
                }
                .into();
                true
            }
            Err(error) => {
                self.notice = error;
                false
            }
        }
    }
}

#[derive(Clone)]
enum PendingAction {
    Open(PreviewSource),
    ClosePreview,
    CloseWindow,
    Split(Axis),
    Quit,
}

enum Prompt {
    ClosePane(u64),
    Quit,
    CloseWindow,
    Rename(String),
    Search(String),
    SaveChanges(PendingAction),
}

#[derive(Clone)]
enum Action {
    View(bool),
    Directory(PathBuf),
    File(PathBuf),
    Diff(FileChange, DiffKind),
    Conflict(PathBuf),
    Merge(MergeAction),
    StageResolved,
    Refresh,
    Hidden,
    Pin,
    ClosePreview,
    SaveFile,
    FocusFileSearch,
    ClearFileSearch,
    PromptChoice(u8),
    PromptConfirm(bool),
    DiffKind(DiffKind),
}

#[derive(Default)]
struct Scene {
    rects: Vec<Rect>,
    texts: Vec<TextRun>,
    hits: Vec<(Bounds, Action)>,
    panes: Vec<(u64, Bounds)>,
    dividers: Vec<Divider>,
    editor: Option<Bounds>,
    comparison: Option<Bounds>,
}

impl Scene {
    fn rect(&mut self, b: Bounds, color: [u8; 4]) {
        self.rects.push(Rect {
            x: b.x,
            y: b.y,
            width: b.w.max(0.0),
            height: b.h.max(0.0),
            color,
        });
    }
    fn text(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        size: f32,
        color: [u8; 4],
        clip: Bounds,
    ) {
        let text: String = text.into();
        let limit = ((clip.x + clip.w - x).max(0.0) / (size * 0.45)).ceil() as usize + 4;
        let text = text.chars().take(limit.min(4096)).collect();
        self.texts.push(TextRun {
            x,
            y,
            text,
            font_size: size,
            line_height: size * 1.4,
            color,
            colors: Vec::new(),
            bold: false,
            italic: false,
            clip: [clip.x, clip.y, clip.x + clip.w, clip.y + clip.h],
        });
    }
    fn button(&mut self, b: Bounds, label: impl Into<String>, action: Action, active: bool) {
        if active {
            self.rect(b, SURFACE);
        }
        self.text(
            b.x + 9.0,
            b.y + (b.h - 18.0) / 2.0,
            label,
            12.0,
            if active { GREEN } else { MUTED },
            b,
        );
        self.hits.push((b, action));
    }
}

pub struct App {
    proxy: WindowProxy,
    worker: mpsc::SyncSender<Job>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    clipboard: Option<arboard::Clipboard>,
    initial_dir: PathBuf,
    saved: Option<SavedWindow>,
    layout: Layout,
    active: u64,
    name: String,
    pub close_requested: bool,
    sessions: HashMap<u64, TerminalSession>,
    next_id: u64,
    scene: Scene,
    modifiers: ModifiersState,
    mouse: (f32, f32),
    selection: Option<u64>,
    dragging: Option<Divider>,
    mouse_capture: Option<u64>,
    last_search: String,
    font: f32,
    cell: (f32, f32),
    sidebar: f32,
    sidebar_visible: bool,
    changes_view: bool,
    show_hidden: bool,
    file_search: FileSearch,
    search_worker: Option<SearchWorker>,
    highlight_worker: Option<HighlightWorker>,
    workspace: Option<WorkspaceSnapshot>,
    directories: HashMap<PathBuf, DirectoryListing>,
    expanded: HashSet<PathBuf>,
    pinned: Option<PathBuf>,
    workspace_cwd: PathBuf,
    busy: bool,
    last_refresh: Instant,
    sidebar_scroll: f32,
    sidebar_max: f32,
    wheel_remainder: f32,
    preview: Option<Preview>,
    editor_dragging: bool,
    after_save: Option<PendingAction>,
    draft_revision: u64,
    recovery_pending: Option<RecoveryDraft>,
    next_preview: u64,
    prompt: Option<Prompt>,
    zoom: bool,
    toast: Option<(String, Instant)>,
    preedit: String,
    occluded: bool,
}

impl App {
    pub fn new(proxy: WindowProxy, initial_dir: PathBuf, saved: Option<SavedWindow>) -> Self {
        let (worker, rx) = mpsc::sync_channel::<Job>(8);
        let events = proxy.clone();
        std::thread::Builder::new()
            .name("leafterm-workspace".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let event = match job {
                        Job::StageResolved {
                            root,
                            relative,
                            expected,
                            token,
                        } => Event::ConflictStaged {
                            token,
                            result: workspace::stage_resolved(&root, &relative, &expected)
                                .map_err(|error| error.to_string()),
                        },
                        Job::Refresh {
                            cwd,
                            directories,
                            hidden,
                        } => {
                            let result = workspace::load_workspace(&cwd).map_err(|e| e.to_string());
                            let root = result
                                .as_ref()
                                .map(|w| w.root.clone())
                                .unwrap_or_else(|_| cwd.clone());
                            let mut paths = directories;
                            if !paths.contains(&root) {
                                paths.push(root);
                            }
                            let directories = paths
                                .into_iter()
                                .take(32)
                                .filter_map(|p| {
                                    workspace::read_directory(&p, hidden).ok().map(|d| (p, d))
                                })
                                .collect();
                            Event::Workspace {
                                cwd,
                                hidden,
                                result,
                                directories,
                            }
                        }
                        Job::Directory { path, hidden } => {
                            let result =
                                workspace::read_directory(&path, hidden).map_err(|e| e.to_string());
                            Event::Directory {
                                path,
                                hidden,
                                result,
                            }
                        }
                        Job::Preview { source, token } => Event::Preview {
                            token,
                            result: source.load().map_err(|e| e.to_string()),
                        },
                        Job::SaveFile { request, token } => Event::FileSaved {
                            token,
                            result: request.save().map_err(|e| e.to_string()),
                        },
                        Job::Recover { draft, token } => Event::Preview {
                            token,
                            result: {
                                let conflict_root = workspace::load_workspace(&draft.path)
                                    .ok()
                                    .and_then(|workspace| {
                                        workspace
                                            .changes
                                            .iter()
                                            .any(|change| {
                                                change.is_conflicted()
                                                    && workspace.root.join(&change.path)
                                                        == draft.path
                                            })
                                            .then_some(workspace.root)
                                    });
                                Document::recover(draft)
                                    .map(|document| LoadedPreview {
                                        recovered: true,
                                        conflict_root,
                                        text: String::new(),
                                        document: Some(document),
                                        notice: "Recovered unsaved edits · ⌘S to save".into(),
                                    })
                                    .map_err(|e| e.to_string())
                            },
                        },
                    };
                    if events.send_event(event).is_err() {
                        break;
                    }
                }
            })
            .expect("create workspace worker");
        Self {
            proxy,
            worker,
            window: None,
            renderer: None,
            clipboard: None,
            initial_dir: initial_dir.clone(),
            saved,
            layout: Layout::Pane(0),
            active: 0,
            name: String::new(),
            close_requested: false,
            sessions: HashMap::new(),
            next_id: 1,
            scene: Scene::default(),
            modifiers: ModifiersState::empty(),
            mouse: (0.0, 0.0),
            selection: None,
            dragging: None,
            mouse_capture: None,
            last_search: String::new(),
            font: 14.0,
            cell: (8.4, 20.0),
            sidebar: 296.0,
            sidebar_visible: true,
            changes_view: true,
            show_hidden: false,
            file_search: FileSearch::default(),
            search_worker: None,
            highlight_worker: None,
            workspace: None,
            directories: HashMap::new(),
            expanded: HashSet::new(),
            pinned: None,
            workspace_cwd: initial_dir,
            busy: false,
            last_refresh: Instant::now() - Duration::from_secs(10),
            sidebar_scroll: 0.0,
            sidebar_max: 0.0,
            wheel_remainder: 0.0,
            preview: None,
            editor_dragging: false,
            after_save: None,
            draft_revision: 0,
            recovery_pending: None,
            next_preview: 1,
            prompt: None,
            zoom: false,
            toast: None,
            preedit: String::new(),
            occluded: false,
        }
    }

    pub(crate) fn notify(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
        self.redraw();
    }
    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
    fn active_id(&self) -> Option<u64> {
        self.sessions
            .contains_key(&self.active)
            .then_some(self.active)
    }
    fn active_session(&self) -> Option<&TerminalSession> {
        self.active_id().and_then(|id| self.sessions.get(&id))
    }
    fn active_cwd(&self) -> PathBuf {
        self.pinned.clone().unwrap_or_else(|| {
            self.active_session()
                .map(|s| s.cwd())
                .unwrap_or_else(|| self.initial_dir.clone())
        })
    }

    fn create_session(&mut self, cwd: &Path) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let events = self.proxy.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = events.send_event(Event::Wake);
        });
        let session = TerminalSession::spawn(cwd, 100, 30, wake)?;
        self.sessions.insert(id, session);
        Ok(id)
    }

    fn initial_session(&mut self) -> Result<()> {
        let id = self.create_session(&self.initial_dir.clone())?;
        self.layout = Layout::Pane(id);
        self.active = id;
        Ok(())
    }

    fn split(&mut self, axis: Axis) {
        if self.has_unsaved_file() {
            self.prompt = Some(Prompt::SaveChanges(PendingAction::Split(axis)));
            self.redraw();
            return;
        }
        if self.sessions.len() >= 32 {
            self.notify("Pane limit reached (32).");
            return;
        }
        let Some(old) = self.active_id() else {
            return;
        };
        let cwd = self.sessions[&old].cwd();
        match self.create_session(&cwd) {
            Ok(id) => {
                self.layout.split(old, id, axis);
                self.active = id;
                self.preview = None;
                self.zoom = false;
                self.save();
                self.redraw();
            }
            Err(e) => self.notify(e.to_string()),
        }
    }

    fn close_pane(&mut self, id: u64, _event_loop: &ActiveEventLoop) {
        if let Some(layout) = self.layout.clone().remove(id) {
            if self.active == id {
                self.active = layout.ids()[0];
            }
            self.layout = layout;
            if let Some(session) = self.sessions.remove(&id) {
                session.shutdown();
                std::thread::spawn(move || drop(session));
            }
            self.zoom = false;
            self.preview = None;
            self.save();
            self.refresh(true);
            self.redraw();
        } else {
            self.close_requested = true;
        }
    }

    fn request_close(&mut self, id: u64, event_loop: &ActiveEventLoop) {
        if self.sessions.get(&id).is_some_and(|s| !s.is_exited()) {
            self.prompt = Some(Prompt::ClosePane(id));
            self.redraw();
        } else {
            self.close_pane(id, event_loop);
        }
    }

    fn refresh(&mut self, force: bool) {
        if self.busy {
            return;
        }
        if !force && self.last_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        let cwd = self.active_cwd();
        let paths = if cwd == self.workspace_cwd {
            self.expanded.iter().take(31).cloned().collect()
        } else {
            Vec::new()
        };
        if self
            .worker
            .try_send(Job::Refresh {
                cwd,
                directories: paths,
                hidden: self.show_hidden,
            })
            .is_ok()
        {
            self.busy = true;
            self.last_refresh = Instant::now();
        }
    }

    fn show_diff(&mut self, change: FileChange, kind: DiffKind) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else {
            return;
        };
        if change.is_conflicted() {
            self.show_preview(PreviewSource::Conflict {
                path: root.join(&change.path),
                root,
            });
        } else {
            self.show_preview(PreviewSource::Diff { root, change, kind });
        }
    }

    fn show_preview(&mut self, mut source: PreviewSource) {
        if let PreviewSource::File(path) = &source
            && let Some(workspace) = &self.workspace
            && workspace
                .changes
                .iter()
                .any(|change| change.is_conflicted() && workspace.root.join(&change.path) == *path)
        {
            source = PreviewSource::Conflict {
                root: workspace.root.clone(),
                path: path.clone(),
            };
        }
        self.request_leave(PendingAction::Open(source));
    }

    fn open_preview(&mut self, source: PreviewSource) {
        let token = self.next_preview;
        self.next_preview += 1;
        if self
            .worker
            .try_send(Job::Preview {
                source: source.clone(),
                token,
            })
            .is_err()
        {
            self.notify("Workspace is busy. Try again in a moment.");
            return;
        }
        let merge = matches!(&source, PreviewSource::Conflict { .. }).then(MergeState::default);
        self.preview = Some(Preview {
            source,
            token,
            text: String::new(),
            offset: 0,
            column: 0,
            loading: true,
            document: None,
            saving: false,
            notice: String::new(),
            merge,
            highlight_revision: None,
            highlight_token: 0,
            highlights: Highlights::default(),
        });
        self.draft_revision += 1;
        self.preedit.clear();
        self.redraw();
    }

    fn reload_preview(&mut self) {
        if self.recovery_pending.is_some() {
            return;
        }
        let Some(preview) = &mut self.preview else {
            return;
        };
        if preview.loading || preview.document.is_some() {
            return;
        }
        let token = self.next_preview;
        if self
            .worker
            .try_send(Job::Preview {
                source: preview.source.clone(),
                token,
            })
            .is_ok()
        {
            self.next_preview += 1;
            preview.token = token;
            preview.loading = true;
        }
    }

    pub fn has_unsaved_file(&self) -> bool {
        self.recovery_pending.is_some()
            || self.preview.as_ref().is_some_and(|p| {
                p.document.as_ref().is_some_and(Document::is_dirty)
                    || p.saving
                    || p.merge.as_ref().is_some_and(|merge| merge.staging)
            })
    }

    pub fn draft_revision(&self) -> u64 {
        self.draft_revision
    }

    pub fn recovery_draft(&self) -> Option<RecoveryDraft> {
        self.preview
            .as_ref()
            .and_then(|preview| {
                preview
                    .document
                    .as_ref()
                    .filter(|document| document.is_dirty() || preview.saving)
                    .map(|document| {
                        let mut draft = document.recovery_draft();
                        draft.needs_review |= preview.saving;
                        draft
                    })
            })
            .or_else(|| self.recovery_pending.clone())
    }

    pub fn restore_draft(&mut self, draft: RecoveryDraft) {
        self.recovery_pending = Some(draft.clone());
        self.draft_revision += 1;
        let token = self.next_preview;
        self.next_preview += 1;
        let source = PreviewSource::File(draft.path.clone());
        self.preview = Some(Preview {
            source,
            token,
            text: String::new(),
            offset: 0,
            column: 0,
            loading: true,
            document: None,
            saving: false,
            notice: String::new(),
            merge: None,
            highlight_revision: None,
            highlight_token: 0,
            highlights: Highlights::default(),
        });
        if self.worker.try_send(Job::Recover { draft, token }).is_err()
            && let Some(preview) = &mut self.preview
        {
            preview.loading = false;
            preview.text = "Recovery is waiting. Your draft remains saved on disk.".into();
        }
        self.redraw();
    }

    pub fn confirm_editor_quit(&mut self) {
        if let Some(window) = &self.window {
            window.focus_window();
        }
        self.request_leave(PendingAction::Quit);
    }

    fn request_leave(&mut self, pending: PendingAction) {
        if self
            .preview
            .as_ref()
            .and_then(|preview| preview.merge.as_ref())
            .is_some_and(|merge| merge.staging)
        {
            self.notify("Wait for marking this file resolved to finish.");
            return;
        }
        if self.has_unsaved_file() {
            self.prompt = Some(Prompt::SaveChanges(pending));
        } else {
            self.perform_pending(pending);
        }
        self.redraw();
    }

    fn perform_pending(&mut self, pending: PendingAction) {
        self.editor_dragging = false;
        self.preedit.clear();
        match pending {
            PendingAction::Open(source) => self.open_preview(source),
            PendingAction::ClosePreview => {
                self.preview = None;
                self.recovery_pending = None;
                self.draft_revision += 1;
            }
            PendingAction::CloseWindow => self.prompt = Some(Prompt::CloseWindow),
            PendingAction::Split(axis) => self.split(axis),
            PendingAction::Quit => self.proxy.quit(),
        }
        self.redraw();
    }

    fn resolve_save_prompt(&mut self, choice: u8) {
        let Some(Prompt::SaveChanges(pending)) = self.prompt.take() else {
            return;
        };
        match choice {
            1 => self.save_file(Some(pending)),
            2 => {
                if self.preview.as_ref().is_some_and(|p| {
                    p.saving || p.merge.as_ref().is_some_and(|merge| merge.staging)
                }) {
                    self.prompt = Some(Prompt::SaveChanges(pending));
                    self.notify("Wait for saving or marking this file resolved to finish.");
                    return;
                }
                self.preview = None;
                self.recovery_pending = None;
                self.draft_revision += 1;
                self.perform_pending(pending);
            }
            _ => {}
        }
        self.redraw();
    }

    fn save_file(&mut self, after: Option<PendingAction>) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        if let Some(merge) = &preview.merge {
            if merge.unresolved() {
                self.notify("Resolve all conflict blocks before saving. Undo is available; your draft is retained.");
                return;
            }
            if merge.staging {
                self.notify("Wait for marking this file resolved to finish.");
                return;
            }
        }
        let Some(document) = &preview.document else {
            self.notify("This file is read only. The recovery draft, if any, is still preserved.");
            return;
        };
        if preview.saving {
            if after.is_some() {
                self.after_save = after;
            }
            return;
        }
        if !document.is_dirty() {
            if let Some(action) = after {
                self.perform_pending(action);
            }
            return;
        }
        let job = Job::SaveFile {
            request: document.save_request(),
            token: preview.token,
        };
        if self.worker.try_send(job).is_ok() {
            preview.saving = true;
            preview.notice = "Saving…".into();
            self.after_save = after;
        } else {
            self.notify("Workspace is busy. Your edits are kept; press ⌘S again shortly.");
        }
        self.redraw();
    }

    fn editor_changed(&mut self) {
        self.draft_revision += 1;
        if let Some(preview) = &mut self.preview {
            preview.notice.clear();
            if let Some(document) = &preview.document {
                if let Some(merge) = &mut preview.merge {
                    merge.refresh(document.text());
                    if document.is_dirty() {
                        merge.staged = false;
                    }
                }
                let (row, col) = document.cursor_position();
                let bounds = self.scene.editor.unwrap_or_default();
                let rows = (bounds.h / self.cell.1).floor().max(1.0) as usize;
                let columns = (bounds.w / self.cell.0).floor().max(1.0) as usize;
                if row < preview.offset {
                    preview.offset = row;
                }
                if row >= preview.offset + rows {
                    preview.offset = row + 1 - rows;
                }
                if col < preview.column {
                    preview.column = col;
                }
                if col >= preview.column + columns {
                    preview.column = col + 1 - columns;
                }
            }
        }
        self.update_highlighting();
        self.redraw();
    }

    fn update_highlighting(&mut self) {
        let Some(preview) = &mut self.preview else {
            if let Some(worker) = self.highlight_worker.take() {
                worker.clear();
            }
            return;
        };
        let Some(document) = &preview.document else {
            if let Some(worker) = self.highlight_worker.take() {
                worker.clear();
            }
            return;
        };
        if preview.highlight_revision == Some(document.revision()) {
            return;
        }
        preview.highlight_revision = Some(document.revision());
        preview.highlights = Highlights::default();
        if self.highlight_worker.is_none() {
            match HighlightWorker::new(self.proxy.clone()) {
                Ok(worker) => self.highlight_worker = Some(worker),
                Err(error) => {
                    self.notify(format!("Syntax highlighting unavailable: {error}"));
                    return;
                }
            }
        }
        preview.highlight_token = self
            .highlight_worker
            .as_ref()
            .unwrap()
            .submit(preview.source.path().to_owned(), document.text().to_owned());
    }

    fn sync_conflict_preview(&mut self) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        if let PreviewSource::File(path) = &preview.source
            && let Some(workspace) = &self.workspace
            && workspace
                .changes
                .iter()
                .any(|change| change.is_conflicted() && workspace.root.join(&change.path) == *path)
        {
            preview.source = PreviewSource::Conflict {
                root: workspace.root.clone(),
                path: path.clone(),
            };
        }
        if matches!(preview.source, PreviewSource::Conflict { .. })
            && let Some(document) = &preview.document
        {
            preview
                .merge
                .get_or_insert_with(MergeState::default)
                .refresh(document.text());
        }
    }

    fn merge_action(&mut self, action: MergeAction) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let (Some(merge), Some(document)) = (&mut preview.merge, &mut preview.document) else {
            return;
        };
        if merge.conflicts.is_empty() || merge.error.is_some() {
            return;
        }
        match action {
            MergeAction::Previous | MergeAction::Next => {
                let count = merge.conflicts.len();
                merge.selected = (merge.selected
                    + if matches!(action, MergeAction::Previous) {
                        count - 1
                    } else {
                        1
                    })
                    % count;
                let row = merge.conflicts[merge.selected].start_line;
                document.set_cursor(row, 0, false);
                preview.offset = row.saturating_sub(2);
            }
            MergeAction::Accept(choice) => {
                let conflict = &merge.conflicts[merge.selected];
                let replacement = conflict.replacement(document.text(), choice);
                let range = conflict.range.clone();
                if let Err(error) = document.replace_range(range, &replacement) {
                    self.notify(error.to_string());
                    return;
                }
            }
        }
        merge.offset = 0;
        merge.column = 0;
        preview.column = 0;
        self.editor_changed();
    }

    fn stage_resolved(&mut self) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let PreviewSource::Conflict { root, path } = &preview.source else {
            return;
        };
        let (Some(merge), Some(document)) = (&mut preview.merge, &preview.document) else {
            return;
        };
        if merge.staging || merge.staged {
            return;
        }
        if merge.unresolved() {
            self.notify("Resolve all conflict blocks before marking this file resolved.");
            return;
        }
        let Some(expected) = document.saved_bytes().filter(|_| !preview.saving) else {
            self.notify("Save the result with ⌘S before marking this file resolved.");
            return;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            return;
        };
        let job = Job::StageResolved {
            root: root.clone(),
            relative: relative.to_owned(),
            expected,
            token: preview.token,
        };
        if self.worker.try_send(job).is_ok() {
            merge.staging = true;
            preview.notice = "Marking resolved…".into();
        } else {
            self.notify("Workspace is busy. Try marking this file resolved again shortly.");
        }
        self.redraw();
    }

    fn edit_insert(&mut self, text: &str) {
        if let Some(document) = self.preview.as_mut().and_then(|p| p.document.as_mut()) {
            if let Err(error) = document.insert(text) {
                self.notify(error.to_string());
            }
            self.editor_changed();
        }
    }

    fn editor_key(&mut self, key: &Key, text: Option<&str>) -> bool {
        if self.preview.as_ref().is_none_or(|p| p.document.is_none()) {
            return false;
        }
        let command = self.modifiers.super_key();
        let shift = self.modifiers.shift_key();
        let ctrl = self.modifiers.control_key();
        if command {
            let character = match key {
                Key::Character(ch) => Some(ch.to_lowercase()),
                _ => None,
            };
            match character.as_deref() {
                Some("s") => {
                    self.save_file(None);
                    return true;
                }
                Some("v") => {
                    if let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
                        self.edit_insert(&text);
                    }
                    return true;
                }
                Some("c") | Some("x") => {
                    let selected = self
                        .preview
                        .as_ref()
                        .and_then(|p| p.document.as_ref())
                        .and_then(|d| d.selected_text())
                        .map(str::to_owned);
                    if let (Some(text), Some(clipboard)) = (selected, &mut self.clipboard)
                        && clipboard.set_text(text).is_ok()
                        && character.as_deref() == Some("x")
                    {
                        self.preview
                            .as_mut()
                            .unwrap()
                            .document
                            .as_mut()
                            .unwrap()
                            .delete();
                        self.editor_changed();
                    }
                    return true;
                }
                Some("a") => self
                    .preview
                    .as_mut()
                    .unwrap()
                    .document
                    .as_mut()
                    .unwrap()
                    .select_all(),
                Some("z") => {
                    let document = self.preview.as_mut().unwrap().document.as_mut().unwrap();
                    if shift {
                        document.redo();
                    } else {
                        document.undo();
                    }
                }
                _ => {
                    let movement = match key {
                        Key::Named(NamedKey::ArrowLeft) => Movement::Home,
                        Key::Named(NamedKey::ArrowRight) => Movement::End,
                        Key::Named(NamedKey::ArrowUp) => Movement::Start,
                        Key::Named(NamedKey::ArrowDown) => Movement::Finish,
                        _ => return false,
                    };
                    // Command-Option arrows remain available for terminal focus.
                    if self.modifiers.alt_key() {
                        return false;
                    }
                    self.preview
                        .as_mut()
                        .unwrap()
                        .document
                        .as_mut()
                        .unwrap()
                        .move_cursor(movement, shift);
                }
            }
        } else {
            if !self.preedit.is_empty() {
                return true;
            }
            let document = self.preview.as_mut().unwrap().document.as_mut().unwrap();
            match key {
                Key::Named(NamedKey::Escape) => {
                    self.request_leave(PendingAction::ClosePreview);
                    return true;
                }
                Key::Named(NamedKey::Backspace) => document.backspace(),
                Key::Named(NamedKey::Delete) => document.delete(),
                Key::Named(NamedKey::ArrowLeft) => document.move_cursor(Movement::Left, shift),
                Key::Named(NamedKey::ArrowRight) => document.move_cursor(Movement::Right, shift),
                Key::Named(NamedKey::ArrowUp) => document.move_cursor(Movement::Up, shift),
                Key::Named(NamedKey::ArrowDown) => document.move_cursor(Movement::Down, shift),
                Key::Named(NamedKey::Home) => document.move_cursor(Movement::Home, shift),
                Key::Named(NamedKey::End) => document.move_cursor(Movement::End, shift),
                Key::Named(NamedKey::Enter) => {
                    self.edit_insert("\n");
                    return true;
                }
                Key::Named(NamedKey::Tab) => {
                    self.edit_insert("\t");
                    return true;
                }
                Key::Named(NamedKey::Space) if !ctrl => {
                    self.edit_insert(" ");
                    return true;
                }
                _ if !ctrl => {
                    if let Some(text) = text {
                        self.edit_insert(text);
                    }
                    return true;
                }
                _ => return true,
            }
        }
        self.editor_changed();
        true
    }

    fn editor_click(&mut self, extend: bool) {
        if let Some(bounds) = self.scene.editor
            && let Some(preview) = &mut self.preview
            && let Some(document) = &mut preview.document
        {
            let row = preview.offset + ((self.mouse.1 - bounds.y) / self.cell.1).max(0.0) as usize;
            let column = preview.column
                + ((self.mouse.0 - bounds.x) / self.cell.0).max(0.0).round() as usize;
            document.set_cursor(row, column, extend);
            self.redraw();
        }
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        if !self.sidebar_visible {
            self.blur_file_search();
        }
        self.dragging = None;
        self.selection = None;
        if self.sidebar_visible {
            self.refresh(true);
        }
        self.save();
        self.redraw();
    }

    fn save(&self) {
        self.proxy.save();
    }

    fn blur_file_search(&mut self) {
        if self.file_search.focused {
            self.file_search.focused = false;
            self.preedit.clear();
            self.redraw();
        }
    }

    fn focus_file_search(&mut self) {
        self.sidebar_visible = true;
        self.changes_view = false;
        self.file_search.focused = true;
        self.file_search.select_all = !self.file_search.query.is_empty();
        self.file_search.cursor = self.file_search.query.len();
        self.preedit.clear();
        self.selection = None;
        self.editor_dragging = false;
        self.refresh(true);
        self.save();
        self.redraw();
    }

    fn search_files(&mut self) {
        self.file_search.paths.clear();
        self.file_search.error = None;
        self.file_search.truncated = false;
        self.file_search.selected = 0;
        self.sidebar_scroll = 0.0;
        self.file_search.busy = !self.file_search.query.trim().is_empty();
        if self.search_worker.is_none() && self.file_search.busy {
            match SearchWorker::new(self.proxy.clone()) {
                Ok(worker) => self.search_worker = Some(worker),
                Err(error) => {
                    self.file_search.error = Some(format!("Cannot start search: {error}"));
                    self.file_search.busy = false;
                }
            }
        }
        if let Some(worker) = &self.search_worker {
            let root = self
                .workspace
                .as_ref()
                .map(|w| w.root.clone())
                .unwrap_or_else(|| self.workspace_cwd.clone());
            self.file_search.token =
                worker.submit(root, self.file_search.query.clone(), self.show_hidden);
        }
        self.redraw();
    }

    fn file_search_key(&mut self, key: &Key, text: Option<&str>) -> bool {
        if !self.file_search.focused {
            return false;
        }
        let changed = if self.modifiers.super_key() {
            match key {
                Key::Character(ch) if ch.eq_ignore_ascii_case("a") => {
                    self.file_search.select_all = true;
                    false
                }
                Key::Character(ch) if ch.eq_ignore_ascii_case("v") => {
                    let text = self.clipboard.as_mut().and_then(|c| c.get_text().ok());
                    text.is_some_and(|text| self.file_search.insert(&text))
                }
                Key::Character(ch) if ch.eq_ignore_ascii_case("c") => {
                    if self.file_search.select_all
                        && let Some(clipboard) = &mut self.clipboard
                    {
                        let _ = clipboard.set_text(self.file_search.query.clone());
                    }
                    false
                }
                Key::Character(ch) if ch.eq_ignore_ascii_case("x") => {
                    if self.file_search.select_all
                        && self.clipboard.as_mut().is_some_and(|clipboard| {
                            clipboard.set_text(self.file_search.query.clone()).is_ok()
                        })
                    {
                        self.file_search.backspace()
                    } else {
                        false
                    }
                }
                Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight) => {
                    self.file_search.cursor = if *key == Key::Named(NamedKey::ArrowLeft) {
                        0
                    } else {
                        self.file_search.query.len()
                    };
                    self.file_search.select_all = false;
                    false
                }
                _ => return false,
            }
        } else if !self.preedit.is_empty() {
            return true;
        } else {
            match key {
                Key::Named(NamedKey::Escape) => {
                    self.file_search.clear();
                    self.search_files();
                    self.blur_file_search();
                    false
                }
                Key::Named(NamedKey::Enter) => {
                    if let Some(path) = self
                        .file_search
                        .paths
                        .get(self.file_search.selected)
                        .cloned()
                    {
                        let root = self
                            .workspace
                            .as_ref()
                            .map(|w| w.root.clone())
                            .unwrap_or_else(|| self.workspace_cwd.clone());
                        self.blur_file_search();
                        self.show_preview(PreviewSource::File(root.join(path)));
                    }
                    false
                }
                Key::Named(NamedKey::ArrowUp | NamedKey::ArrowDown) => {
                    if *key == Key::Named(NamedKey::ArrowUp) {
                        self.file_search.selected = self.file_search.selected.saturating_sub(1);
                    } else {
                        self.file_search.selected = (self.file_search.selected + 1)
                            .min(self.file_search.paths.len().saturating_sub(1));
                    }
                    let top = self.file_search.selected as f32 * 44.0;
                    let height = self
                        .window
                        .as_ref()
                        .map(|window| {
                            window.inner_size().height as f32 / window.scale_factor() as f32
                        })
                        .unwrap_or(840.0)
                        - 278.0;
                    self.sidebar_scroll = self
                        .sidebar_scroll
                        .min(top)
                        .max((top + 44.0 - height).max(0.0));
                    false
                }
                Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight) => {
                    self.file_search
                        .move_cursor(*key == Key::Named(NamedKey::ArrowRight));
                    false
                }
                Key::Named(NamedKey::Home | NamedKey::End) => {
                    self.file_search.cursor = if *key == Key::Named(NamedKey::Home) {
                        0
                    } else {
                        self.file_search.query.len()
                    };
                    self.file_search.select_all = false;
                    false
                }
                Key::Named(NamedKey::Backspace) => self.file_search.backspace(),
                Key::Named(NamedKey::Delete) => self.file_search.delete(),
                Key::Named(NamedKey::Space) => self.file_search.insert(" "),
                Key::Named(NamedKey::Tab) => {
                    self.blur_file_search();
                    false
                }
                _ if !self.modifiers.control_key() && !self.modifiers.alt_key() => {
                    text.is_some_and(|text| self.file_search.insert(text))
                }
                _ => false,
            }
        };
        if changed {
            self.search_files();
        }
        self.redraw();
        true
    }

    pub fn saved_window(&self) -> SavedWindow {
        SavedWindow {
            name: self.name.clone(),
            layout: self.layout.clone(),
            active: self.active,
            panes: self
                .sessions
                .iter()
                .map(|(&id, session)| SavedPane {
                    id,
                    cwd: session.cwd(),
                })
                .collect(),
            font: self.font,
            sidebar: self.sidebar,
            sidebar_visible: self.sidebar_visible,
        }
    }

    fn restore_sessions(&mut self) -> bool {
        let Some(saved) = self.saved.take() else {
            return false;
        };
        self.font = saved.font.clamp(10.0, 26.0);
        self.sidebar = saved.sidebar.clamp(220.0, 440.0);
        self.sidebar_visible = saved.sidebar_visible;
        self.name = saved.name.clone();
        if !saved.valid() {
            return false;
        }
        let mut remap = HashMap::new();
        for p in saved.panes {
            let cwd = if p.cwd.is_dir() {
                p.cwd
            } else {
                self.initial_dir.clone()
            };
            match self.create_session(&cwd) {
                Ok(new) => {
                    remap.insert(p.id, new);
                }
                Err(_) => {
                    self.sessions.clear();
                    return false;
                }
            }
        }
        fn map_layout(layout: &mut Layout, ids: &HashMap<u64, u64>) {
            match layout {
                Layout::Pane(id) => *id = ids[id],
                Layout::Split { first, second, .. } => {
                    map_layout(first, ids);
                    map_layout(second, ids);
                }
            }
        }
        self.layout = saved.layout;
        map_layout(&mut self.layout, &remap);
        self.active = remap[&saved.active];
        true
    }

    fn activate(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::View(changes) => {
                self.changes_view = changes;
                self.blur_file_search();
                self.sidebar_scroll = 0.0;
            }
            Action::Directory(path) => {
                if self.expanded.remove(&path) {
                    self.expanded.retain(|p| !p.starts_with(&path));
                    self.directories.retain(|p, _| !p.starts_with(&path));
                } else if self.expanded.len() >= 31 {
                    self.notify("Close a folder before expanding another (31-folder cache limit).");
                } else {
                    self.expanded.insert(path.clone());
                    if self
                        .worker
                        .try_send(Job::Directory {
                            path: path.clone(),
                            hidden: self.show_hidden,
                        })
                        .is_err()
                    {
                        self.notify("Workspace busy. Refresh to reload folders.");
                    }
                }
            }
            Action::File(path) => self.show_preview(PreviewSource::File(path)),
            Action::Diff(change, kind) => self.show_diff(change, kind),
            Action::Conflict(path) => self.show_preview(PreviewSource::File(path)),
            Action::Merge(action) => self.merge_action(action),
            Action::StageResolved => self.stage_resolved(),
            Action::Refresh => {
                self.refresh(true);
                self.search_files();
            }
            Action::Hidden => {
                self.show_hidden = !self.show_hidden;
                self.refresh(true);
                self.search_files();
            }
            Action::Pin => {
                self.pinned = if self.pinned.is_some() {
                    None
                } else {
                    self.workspace.as_ref().map(|w| w.root.clone())
                };
                self.refresh(true);
            }
            Action::ClosePreview => self.request_leave(PendingAction::ClosePreview),
            Action::SaveFile => self.save_file(None),
            Action::FocusFileSearch => self.focus_file_search(),
            Action::ClearFileSearch => {
                self.file_search.clear();
                self.file_search.focused = true;
                self.preedit.clear();
                self.search_files();
            }
            Action::PromptChoice(choice) => self.resolve_save_prompt(choice),
            Action::PromptConfirm(confirm) => {
                if confirm {
                    self.confirm_prompt(event_loop);
                } else {
                    self.prompt = None;
                }
            }
            Action::DiffKind(kind) => {
                if let Some(Preview {
                    source: PreviewSource::Diff { root, change, .. },
                    ..
                }) = &self.preview
                {
                    self.show_preview(PreviewSource::Diff {
                        root: root.clone(),
                        change: change.clone(),
                        kind,
                    });
                }
            }
        }
        self.redraw();
    }

    fn build_scene(&mut self, width: f32, height: f32) -> Scene {
        let mut s = Scene::default();
        let sidebar_width = if self.sidebar_visible {
            self.sidebar
        } else {
            0.0
        };
        let all = Bounds {
            x: 0.0,
            y: 0.0,
            w: width,
            h: height,
        };
        s.rect(all, BG);
        if self.sidebar_visible {
            let sidebar = Bounds {
                x: 0.0,
                y: TOP,
                w: self.sidebar,
                h: (height - TOP - FOOT).max(1.0),
            };
            s.rect(sidebar, PANEL);
            s.rect(
                Bounds {
                    x: self.sidebar,
                    y: TOP,
                    w: 1.0,
                    h: sidebar.h,
                },
                BORDER,
            );
            let root = self
                .workspace
                .as_ref()
                .map(|w| w.root.clone())
                .unwrap_or_else(|| self.workspace_cwd.clone());
            s.text(
                20.0,
                TOP + 20.0,
                name_of(&root),
                16.0,
                FG,
                Bounds {
                    w: self.sidebar - 64.0,
                    ..sidebar
                },
            );
            s.button(
                Bounds {
                    x: self.sidebar - 49.0,
                    y: TOP + 16.0,
                    w: 33.0,
                    h: 28.0,
                },
                "↻",
                Action::Refresh,
                false,
            );
            let branch = self
                .workspace
                .as_ref()
                .and_then(|w| w.branch.as_deref())
                .unwrap_or("No Git repository");
            s.text(
                20.0,
                TOP + 48.0,
                format!("⑂  {branch}"),
                12.0,
                GREEN,
                sidebar,
            );
            s.text(
                20.0,
                TOP + 70.0,
                short_path(&root),
                10.0,
                MUTED,
                Bounds {
                    x: 0.0,
                    w: self.sidebar - 14.0,
                    ..sidebar
                },
            );
            s.button(
                Bounds {
                    x: 14.0,
                    y: TOP + 100.0,
                    w: 115.0,
                    h: 31.0,
                },
                "Changes",
                Action::View(true),
                self.changes_view,
            );
            s.button(
                Bounds {
                    x: 133.0,
                    y: TOP + 100.0,
                    w: 100.0,
                    h: 31.0,
                },
                "Files",
                Action::View(false),
                !self.changes_view,
            );
            s.rect(
                Bounds {
                    x: 14.0,
                    y: TOP + if self.changes_view { 144.0 } else { 183.0 },
                    w: self.sidebar - 28.0,
                    h: 1.0,
                },
                BORDER,
            );
            if !self.changes_view {
                self.paint_file_search_field(&mut s);
            }
            let list = Bounds {
                x: 10.0,
                y: TOP + if self.changes_view { 157.0 } else { 196.0 },
                w: self.sidebar - 20.0,
                h: (height - TOP - if self.changes_view { 215.0 } else { 254.0 }).max(1.0),
            };
            self.sidebar_max = 0.0;
            if self.changes_view {
                if let Some(ws) = &self.workspace {
                    let groups = [
                        ("CONFLICTS", None),
                        ("UNSTAGED", Some(DiffKind::Unstaged)),
                        ("STAGED", Some(DiffKind::Staged)),
                    ];
                    let mut y = list.y - self.sidebar_scroll;
                    for (title, kind) in groups {
                        let changes: Vec<_> = ws
                            .changes
                            .iter()
                            .filter(|c| match kind {
                                None => c.is_conflicted(),
                                Some(DiffKind::Unstaged) => {
                                    !c.is_conflicted() && (c.is_unstaged() || c.is_untracked())
                                }
                                Some(DiffKind::Staged) => !c.is_conflicted() && c.is_staged(),
                            })
                            .collect();
                        if kind.is_none() && changes.is_empty() {
                            continue;
                        }
                        s.text(
                            20.0,
                            y,
                            format!("{title}   {}", changes.len()),
                            10.0,
                            MUTED,
                            list,
                        );
                        y += 28.0;
                        if changes.is_empty() {
                            s.text(23.0, y, "No changes", 12.0, MUTED, list);
                            y += 30.0;
                        }
                        for c in changes {
                            let b = Bounds {
                                x: 14.0,
                                y,
                                w: self.sidebar - 28.0,
                                h: 30.0,
                            };
                            if y >= list.y && y + b.h < list.y + list.h {
                                let chosen = self.preview.as_ref().is_some_and(|d| {
                                    match (&d.source, kind) {
                                        (PreviewSource::Conflict { path, .. }, None) => {
                                            *path == ws.root.join(&c.path)
                                        }
                                        (
                                            PreviewSource::Diff {
                                                change,
                                                kind: selected,
                                                ..
                                            },
                                            Some(kind),
                                        ) => change.path == c.path && *selected == kind,
                                        _ => false,
                                    }
                                });
                                if chosen {
                                    s.rect(b, SURFACE);
                                }
                                let badge = if kind.is_none() {
                                    'U'
                                } else if c.is_untracked() {
                                    '?'
                                } else if kind == Some(DiffKind::Staged) {
                                    c.index_status
                                } else {
                                    c.worktree_status
                                };
                                s.text(
                                    23.0,
                                    y + 6.0,
                                    badge.to_string(),
                                    12.0,
                                    status_color(badge),
                                    b,
                                );
                                s.text(
                                    47.0,
                                    y + 6.0,
                                    c.path.to_string_lossy(),
                                    12.0,
                                    FG,
                                    Bounds {
                                        x: 47.0,
                                        w: self.sidebar - 65.0,
                                        ..b
                                    },
                                );
                                s.hits.push((
                                    b,
                                    match kind {
                                        Some(kind) => Action::Diff(c.clone(), kind),
                                        None => Action::Conflict(ws.root.join(&c.path)),
                                    },
                                ));
                            }
                            y += 30.0;
                        }
                        y += 18.0;
                    }
                    if ws.truncated {
                        s.text(20.0, y, "List capped · use git status", 10.0, AMBER, list);
                    }
                    self.sidebar_max = (y + self.sidebar_scroll - list.y + 28.0 - list.h).max(0.0);
                } else {
                    s.text(
                        20.0,
                        list.y,
                        if self.busy {
                            "Reading repository…"
                        } else {
                            "Repository unavailable"
                        },
                        12.0,
                        MUTED,
                        list,
                    );
                }
            } else if !self.file_search.query.trim().is_empty() {
                self.paint_file_search_results(&mut s, &root, list);
            } else {
                let mut rows = Vec::new();
                self.collect_tree(&root, 0, &mut rows);
                let truncated =
                    rows.len() >= 4096 || self.directories.values().any(|d| d.truncated);
                self.sidebar_max = (rows.len() as f32 * 28.0 - list.h
                    + if truncated { 28.0 } else { 0.0 })
                .max(0.0);
                let mut y = list.y - self.sidebar_scroll;
                for (path, name, dir, depth) in rows {
                    let b = Bounds {
                        x: 14.0,
                        y,
                        w: self.sidebar - 28.0,
                        h: 28.0,
                    };
                    if y >= list.y && y + b.h <= list.y + list.h {
                        let selected = self.preview.as_ref().is_some_and(|preview| match &preview
                            .source
                        {
                            PreviewSource::File(selected)
                            | PreviewSource::Conflict { path: selected, .. } => selected == &path,
                            PreviewSource::Diff { root, change, .. } => {
                                root.join(&change.path) == path
                            }
                        });
                        if selected {
                            s.rect(b, SURFACE);
                            s.rect(Bounds { w: 2.0, ..b }, GREEN);
                        }
                        let icon = if dir {
                            if self.expanded.contains(&path) {
                                "▾"
                            } else {
                                "▸"
                            }
                        } else {
                            "·"
                        };
                        let x = 20.0 + depth as f32 * 14.0;
                        s.text(x, y + 5.0, icon, 12.0, if dir { GREEN } else { MUTED }, b);
                        s.text(
                            x + 19.0,
                            y + 5.0,
                            name,
                            12.0,
                            FG,
                            Bounds {
                                x: x + 19.0,
                                w: (self.sidebar - x - 52.0).max(1.0),
                                ..b
                            },
                        );
                        if let Some(c) = self
                            .workspace
                            .as_ref()
                            .and_then(|w| w.changes.iter().find(|c| w.root.join(&c.path) == path))
                        {
                            let badge = c.badge();
                            s.text(
                                self.sidebar - 34.0,
                                y + 5.0,
                                badge.to_string(),
                                12.0,
                                status_color(badge),
                                b,
                            );
                        }
                        s.hits.push((
                            b,
                            if dir {
                                Action::Directory(path)
                            } else {
                                Action::File(path)
                            },
                        ));
                    }
                    y += 28.0;
                }
                if truncated {
                    let warning = Bounds {
                        y: list.y + list.h - 22.0,
                        h: 22.0,
                        ..list
                    };
                    s.rect(warning, PANEL);
                    s.text(
                        warning.x + 8.0,
                        warning.y + 4.0,
                        "Preview capped · collapse folders",
                        10.0,
                        AMBER,
                        warning,
                    );
                }
            }
            self.sidebar_scroll = self.sidebar_scroll.min(self.sidebar_max);
            s.button(
                Bounds {
                    x: 14.0,
                    y: height - FOOT - 37.0,
                    w: 130.0,
                    h: 28.0,
                },
                if self.pinned.is_some() {
                    "Pinned project"
                } else {
                    "Follow terminal"
                },
                Action::Pin,
                self.pinned.is_some(),
            );
            s.button(
                Bounds {
                    x: 153.0,
                    y: height - FOOT - 37.0,
                    w: 128.0,
                    h: 28.0,
                },
                "Hidden files",
                Action::Hidden,
                self.show_hidden,
            );
        }

        let body = Bounds {
            x: sidebar_width,
            y: 0.0,
            w: (width - sidebar_width).max(1.0),
            h: height.max(1.0),
        };
        if self.preview.is_some() {
            self.paint_preview(&mut s, body);
        } else {
            let active = self.active;
            let (panes, dividers) = if self.zoom {
                (vec![(active, body)], Vec::new())
            } else {
                self.layout.arrange(body)
            };
            s.dividers = dividers;
            for (id, bounds) in panes {
                s.rect(bounds, BORDER);
                s.rect(
                    Bounds {
                        x: bounds.x + 1.0,
                        y: bounds.y + 1.0,
                        w: bounds.w - 2.0,
                        h: bounds.h - 2.0,
                    },
                    BG,
                );
                if id == active && self.sessions.len() > 1 {
                    s.rect(Bounds { w: 2.0, ..bounds }, GREEN);
                }
                let Some(session) = self.sessions.get(&id) else {
                    continue;
                };
                let terminal = Bounds {
                    x: bounds.x + 8.0,
                    y: bounds.y + 6.0,
                    w: (bounds.w - 16.0).max(1.0),
                    h: (bounds.h - 12.0).max(1.0),
                };
                let cols = (terminal.w / self.cell.0).floor().max(2.0) as u16;
                let rows = (terminal.h / self.cell.1).floor().max(1.0) as u16;
                let _ = session.resize(cols, rows);
                let snapshot = session.snapshot();
                let terminal = Bounds {
                    w: snapshot.cols as f32 * self.cell.0,
                    h: snapshot.rows as f32 * self.cell.1,
                    ..terminal
                };
                for cell in &snapshot.cells {
                    let x = terminal.x + cell.col as f32 * self.cell.0;
                    let y = terminal.y + cell.row as f32 * self.cell.1;
                    let b = Bounds {
                        x,
                        y,
                        w: self.cell.0,
                        h: self.cell.1,
                    };
                    if cell.selected {
                        s.rect(b, [43, 73, 66, 255]);
                    } else if cell.bg != [BG[0], BG[1], BG[2]] {
                        s.rect(b, rgba(cell.bg));
                    }
                    if cell.underline {
                        s.rect(
                            Bounds {
                                x,
                                y: y + self.cell.1 - 2.0,
                                w: self.cell.0,
                                h: 1.0,
                            },
                            rgba(cell.fg),
                        );
                    }
                }
                // Coalesce same-style ASCII cells into row runs. This avoids a
                // shaping buffer per character while preserving wide-cell positions.
                let mut index = 0;
                while index < snapshot.cells.len() {
                    let cell = &snapshot.cells[index];
                    if cell.text.is_empty() {
                        index += 1;
                        continue;
                    }
                    let mut text = cell.text.clone();
                    let mut end = cell.col + 1;
                    let ascii = cell.text.len() == 1 && cell.text.is_ascii();
                    index += 1;
                    if ascii {
                        while let Some(next) = snapshot.cells.get(index) {
                            if next.row != cell.row
                                || next.col < end
                                || next.col - end > 8
                                || next.fg != cell.fg
                                || next.bold != cell.bold
                                || next.italic != cell.italic
                                || next.text.len() != 1
                                || !next.text.is_ascii()
                            {
                                break;
                            }
                            text.extend(std::iter::repeat_n(' ', (next.col - end) as usize));
                            text.push_str(&next.text);
                            end = next.col + 1;
                            index += 1;
                        }
                    }
                    if !text.trim().is_empty() {
                        s.text(
                            terminal.x + cell.col as f32 * self.cell.0,
                            terminal.y + cell.row as f32 * self.cell.1,
                            text,
                            self.font,
                            rgba(cell.fg),
                            terminal,
                        );
                        if let Some(run) = s.texts.last_mut() {
                            run.bold = cell.bold;
                            run.italic = cell.italic;
                            run.line_height = self.cell.1;
                        }
                    }
                }
                if id == active
                    && let Some((col, row)) = snapshot.cursor
                {
                    let x = terminal.x + col as f32 * self.cell.0;
                    let y = terminal.y + row as f32 * self.cell.1;
                    s.rect(
                        Bounds {
                            x,
                            y,
                            w: 2.0,
                            h: self.cell.1,
                        },
                        GREEN,
                    );
                    if !self.preedit.is_empty() && !self.file_search.focused {
                        for run in &mut s.texts {
                            if (run.y - y).abs() < 0.1
                                && run.clip[0] >= terminal.x
                                && run.clip[2] > x
                            {
                                run.clip[2] = run.clip[2].min(x);
                            }
                        }
                        s.rect(
                            Bounds {
                                x,
                                y,
                                w: self.preedit.chars().count() as f32 * self.cell.0 * 2.0,
                                h: self.cell.1,
                            },
                            SURFACE,
                        );
                        s.text(x, y, &self.preedit, self.font, GREEN, terminal);
                        if let Some(run) = s.texts.last_mut() {
                            run.line_height = self.cell.1;
                        }
                    }
                    if !self.file_search.focused
                        && let Some(w) = &self.window
                    {
                        w.set_ime_cursor_area(
                            LogicalPosition::new(x, y + self.cell.1),
                            LogicalSize::new(self.cell.0, self.cell.1),
                        );
                    }
                }
                s.panes.push((id, terminal));
            }
        }
        if let Some((status, _)) = self
            .toast
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(5))
        {
            let overlay = Bounds {
                x: sidebar_width + 12.0,
                y: height - 34.0,
                w: (width - sidebar_width - 24.0).max(1.0),
                h: 26.0,
            };
            s.texts
                .retain(|text| text.y + text.line_height <= overlay.y || text.x < sidebar_width);
            s.rect(overlay, SURFACE);
            s.text(overlay.x + 8.0, overlay.y + 5.0, status, 11.0, FG, overlay);
        }
        if let Some(prompt) = &self.prompt {
            // Text is drawn after rectangles; omit underlying text for the modal.
            s.texts.clear();
            s.hits.clear();
            s.panes.clear();
            s.dividers.clear();
            s.editor = None;
            s.comparison = None;
            s.rect(all, [0, 0, 0, 160]);
            let b = Bounds {
                x: (width - 500.0) / 2.0,
                y: (height - 180.0) / 2.0,
                w: 500.0,
                h: 180.0,
            };
            s.rect(b, SURFACE);
            let (title, detail, hint) = match prompt {
                Prompt::ClosePane(_) => (
                    "Close this terminal?",
                    "Running programs in this pane will be stopped.".to_string(),
                    "Enter to close · Esc to cancel",
                ),
                Prompt::Quit => (
                    "Quit LeafTerm?",
                    "All windows and running shells will close. Layouts are saved.".to_string(),
                    "Enter to quit · Esc to cancel",
                ),
                Prompt::CloseWindow => (
                    "Close this window?",
                    "Running shells in this window will stop.".to_string(),
                    "Enter to close · Esc to cancel",
                ),
                Prompt::Rename(value) => (
                    "Rename window",
                    format!("{value}▏"),
                    "Enter to save · Esc to cancel",
                ),
                Prompt::SaveChanges(_) => (
                    "Save changes before leaving?",
                    self.preview
                        .as_ref()
                        .map(|p| name_of(p.source.path()))
                        .unwrap_or_default(),
                    "Enter / ⌘S save · D discard · Esc cancel",
                ),
                Prompt::Search(value) => (
                    "Find in terminal",
                    format!("{value}▏"),
                    "Enter to find · Esc to return",
                ),
            };
            s.text(b.x + 26.0, b.y + 25.0, title, 18.0, FG, b);
            s.text(
                b.x + 26.0,
                b.y + 70.0,
                detail,
                12.0,
                GREEN,
                Bounds {
                    x: b.x + 26.0,
                    w: b.w - 52.0,
                    ..b
                },
            );
            s.text(b.x + 26.0, b.y + 105.0, hint, 11.0, MUTED, b);
            if matches!(prompt, Prompt::SaveChanges(_)) {
                for (index, label, choice) in [(0, "Save", 1), (1, "Discard", 2), (2, "Cancel", 0)]
                {
                    s.button(
                        Bounds {
                            x: b.x + 26.0 + index as f32 * 144.0,
                            y: b.y + 137.0,
                            w: 130.0,
                            h: 30.0,
                        },
                        label,
                        Action::PromptChoice(choice),
                        choice == 1,
                    );
                }
            } else if matches!(prompt, Prompt::CloseWindow) {
                for (index, label, confirm) in [(0, "Cancel", false), (1, "Close", true)] {
                    let button = Bounds {
                        x: b.x + b.w - 238.0 + index as f32 * 112.0,
                        y: b.y + 137.0,
                        w: 100.0,
                        h: 30.0,
                    };
                    s.rect(button, if confirm { GREEN } else { BORDER });
                    s.text(
                        button.x + 24.0,
                        button.y + 6.0,
                        label,
                        12.0,
                        if confirm { BG } else { FG },
                        button,
                    );
                    s.hits.push((button, Action::PromptConfirm(confirm)));
                }
            }
        }
        s
    }

    fn paint_file_search_field(&self, scene: &mut Scene) {
        let field = Bounds {
            x: 14.0,
            y: TOP + 143.0,
            w: self.sidebar - 28.0,
            h: 30.0,
        };
        scene.rect(field, SURFACE);
        scene.hits.push((field, Action::FocusFileSearch));
        let clip = Bounds {
            x: field.x + 9.0,
            w: field.w - 43.0,
            ..field
        };
        let search = &self.file_search;
        let mut start = search.cursor;
        let mut columns = 0;
        let available = (clip.w / 7.2).max(1.0) as usize;
        for (index, grapheme) in search.query[..search.cursor].grapheme_indices(true).rev() {
            let width = editor_view::visual_width(grapheme);
            if columns + width >= available {
                break;
            }
            columns += width;
            start = index;
        }
        let preedit = if search.focused {
            self.preedit.as_str()
        } else {
            ""
        };
        let display = format!(
            "{}{}{}",
            &search.query[start..search.cursor],
            preedit,
            &search.query[search.cursor..]
        );
        if search.focused && search.select_all {
            scene.rect(
                Bounds {
                    x: clip.x,
                    y: field.y + 5.0,
                    w: (editor_view::visual_width(&display) as f32 * 7.2).min(clip.w),
                    h: 20.0,
                },
                [43, 73, 66, 255],
            );
        }
        scene.text(
            clip.x,
            field.y + 6.0,
            if display.is_empty() {
                "Search files…  ⌘P"
            } else {
                &display
            },
            12.0,
            if display.is_empty() { MUTED } else { FG },
            clip,
        );
        if search.focused {
            scene.rect(
                Bounds {
                    y: field.y + field.h - 1.0,
                    h: 1.0,
                    ..field
                },
                GREEN,
            );
            let x = (clip.x + (columns + editor_view::visual_width(preedit)) as f32 * 7.2)
                .min(clip.x + clip.w - 1.0);
            scene.rect(
                Bounds {
                    x,
                    y: field.y + 6.0,
                    w: 1.0,
                    h: 18.0,
                },
                GREEN,
            );
            if let Some(window) = &self.window {
                window.set_ime_cursor_area(
                    LogicalPosition::new(x, field.y + field.h),
                    LogicalSize::new(7.2, 18.0),
                );
            }
        }
        if !search.query.is_empty() {
            scene.button(
                Bounds {
                    x: field.x + field.w - 30.0,
                    w: 30.0,
                    ..field
                },
                "×",
                Action::ClearFileSearch,
                false,
            );
        }
    }

    fn paint_file_search_results(&mut self, scene: &mut Scene, root: &Path, list: Bounds) {
        let search = &self.file_search;
        let status = if search.busy {
            "Searching…".to_string()
        } else if let Some(error) = &search.error {
            error.clone()
        } else if search.truncated {
            format!("{} files · partial results", search.paths.len())
        } else if search.paths.is_empty() {
            "No matching files".to_string()
        } else {
            format!("{} files · ↑↓ select · Enter open", search.paths.len())
        };
        scene.text(
            20.0,
            list.y,
            status,
            10.0,
            if search.error.is_some() || search.truncated {
                AMBER
            } else {
                MUTED
            },
            list,
        );
        let results = Bounds {
            y: list.y + 24.0,
            h: (list.h - 24.0).max(1.0),
            ..list
        };
        self.sidebar_max = (search.paths.len() as f32 * 44.0 - results.h).max(0.0);
        for (index, relative) in search.paths.iter().enumerate() {
            let y = results.y + index as f32 * 44.0 - self.sidebar_scroll;
            let bounds = Bounds {
                x: 14.0,
                y,
                w: self.sidebar - 28.0,
                h: 44.0,
            };
            if y < results.y || y + bounds.h > results.y + results.h {
                continue;
            }
            let path = root.join(relative);
            let opened = self.preview.as_ref().is_some_and(
                |preview| matches!(&preview.source, PreviewSource::File(file) | PreviewSource::Conflict { path: file, .. } if file == &path),
            );
            if (search.focused && index == search.selected) || (!search.focused && opened) {
                scene.rect(bounds, SURFACE);
                scene.rect(Bounds { w: 2.0, ..bounds }, GREEN);
            }
            let text_bounds = Bounds {
                x: 23.0,
                w: self.sidebar - 65.0,
                ..bounds
            };
            scene.text(23.0, y + 4.0, name_of(relative), 12.0, FG, text_bounds);
            let parent = relative
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".into());
            scene.text(23.0, y + 23.0, parent, 10.0, MUTED, text_bounds);
            if let Some(change) = self.workspace.as_ref().and_then(|workspace| {
                workspace
                    .changes
                    .iter()
                    .find(|change| &change.path == relative)
            }) {
                let badge = change.badge();
                scene.text(
                    self.sidebar - 34.0,
                    y + 5.0,
                    badge.to_string(),
                    12.0,
                    status_color(badge),
                    bounds,
                );
            }
            scene.hits.push((bounds, Action::File(path)));
        }
    }

    fn collect_tree(
        &self,
        dir: &Path,
        depth: usize,
        out: &mut Vec<(PathBuf, String, bool, usize)>,
    ) {
        if depth > 32 || out.len() >= 4096 {
            return;
        }
        if let Some(listing) = self.directories.get(dir) {
            for entry in &listing.entries {
                out.push((
                    entry.path.clone(),
                    if entry.is_symlink {
                        format!("{} ↗", entry.name)
                    } else {
                        entry.name.clone()
                    },
                    entry.is_dir && !entry.is_symlink,
                    depth,
                ));
                if entry.is_dir && !entry.is_symlink && self.expanded.contains(&entry.path) {
                    self.collect_tree(&entry.path, depth + 1, out);
                }
                if out.len() >= 4096 {
                    break;
                }
            }
        }
    }

    fn paint_preview(&self, s: &mut Scene, b: Bounds) {
        let Some(preview) = &self.preview else {
            return;
        };
        let diff_kind = match &preview.source {
            PreviewSource::Diff { kind, .. } => Some(*kind),
            PreviewSource::File(_) | PreviewSource::Conflict { .. } => None,
        };
        let path = preview.source.path();
        let label = self
            .workspace
            .as_ref()
            .and_then(|w| path.strip_prefix(&w.root).ok())
            .unwrap_or(path);
        s.rect(b, PANEL);
        s.text(
            b.x + 18.0,
            b.y + 15.0,
            format!(
                "{}{}",
                label.display(),
                if preview.document.as_ref().is_some_and(Document::is_dirty) {
                    " •"
                } else {
                    ""
                }
            ),
            14.0,
            if preview.document.as_ref().is_some_and(Document::is_dirty) {
                AMBER
            } else {
                FG
            },
            Bounds {
                w: (b.w
                    - if preview.merge.is_some() {
                        320.0
                    } else if diff_kind.is_some() {
                        230.0
                    } else {
                        165.0
                    })
                .max(10.0),
                ..b
            },
        );
        if let Some(kind) = diff_kind {
            s.button(
                Bounds {
                    x: b.x + b.w - 229.0,
                    y: b.y + 10.0,
                    w: 105.0,
                    h: 28.0,
                },
                "Unstaged",
                Action::DiffKind(DiffKind::Unstaged),
                kind == DiffKind::Unstaged,
            );
            s.button(
                Bounds {
                    x: b.x + b.w - 120.0,
                    y: b.y + 10.0,
                    w: 80.0,
                    h: 28.0,
                },
                "Staged",
                Action::DiffKind(DiffKind::Staged),
                kind == DiffKind::Staged,
            );
        }
        if preview.document.is_some() {
            if let Some(merge) = &preview.merge {
                let bounds = Bounds {
                    x: b.x + b.w - 305.0,
                    y: b.y + 10.0,
                    w: 162.0,
                    h: 28.0,
                };
                let ready = !merge.unresolved()
                    && !merge.staged
                    && !merge.staging
                    && !preview.saving
                    && preview
                        .document
                        .as_ref()
                        .is_some_and(|document| !document.is_dirty());
                s.rect(bounds, if ready { BORDER } else { PANEL });
                s.button(
                    bounds,
                    if merge.staging {
                        "Marking resolved…"
                    } else if merge.staged {
                        "Staged"
                    } else {
                        "Mark resolved"
                    },
                    Action::StageResolved,
                    ready,
                );
            }
            s.button(
                Bounds {
                    x: b.x + b.w - 136.0,
                    y: b.y + 10.0,
                    w: 95.0,
                    h: 28.0,
                },
                if preview.saving {
                    "Saving…"
                } else {
                    "Save  ⌘S"
                },
                Action::SaveFile,
                false,
            );
        }
        s.button(
            Bounds {
                x: b.x + b.w - 38.0,
                y: b.y + 10.0,
                w: 30.0,
                h: 28.0,
            },
            "×",
            Action::ClosePreview,
            false,
        );
        let mut clip = Bounds {
            x: b.x + 12.0,
            y: b.y + 52.0,
            w: b.w - 24.0,
            h: b.h - 64.0,
        };
        if let Some(document) = &preview.document {
            if let Some(merge) = &preview.merge {
                let frame = merge_view::paint(
                    document.text(),
                    &preview.highlights.spans,
                    &merge.conflicts,
                    merge.selected,
                    merge.error.as_deref(),
                    clip,
                    self.font,
                    self.cell,
                    merge.offset,
                    merge.column,
                );
                clip = frame.editor_bounds;
                s.comparison = frame.comparison_bounds;
                s.rects.extend(frame.rects);
                s.texts.extend(frame.texts);
                s.hits.extend(
                    frame
                        .buttons
                        .into_iter()
                        .map(|(bounds, action)| (bounds, Action::Merge(action))),
                );
            }
            let frame = editor_view::paint(
                document,
                &preview.highlights.spans,
                clip,
                preview.offset,
                preview.column,
                self.font,
                self.cell,
            );
            s.editor = Some(frame.text_bounds);
            s.rects.extend(frame.rects);
            s.texts.extend(frame.texts);
            if let Some((x, y)) = frame.cursor {
                if !self.preedit.is_empty() && !self.file_search.focused {
                    for run in &mut s.texts {
                        if (run.y - y).abs() < 0.1 && run.x >= frame.text_bounds.x {
                            run.clip[2] = run.clip[2].min(x);
                        }
                    }
                    s.text(x, y, &self.preedit, self.font, GREEN, frame.text_bounds);
                }
                if !self.file_search.focused
                    && let Some(window) = &self.window
                {
                    window.set_ime_cursor_area(
                        LogicalPosition::new(x, y + self.cell.1),
                        LogicalSize::new(self.cell.0, self.cell.1),
                    );
                }
            }
            let (row, col) = document.cursor_position();
            let mut status = if preview.notice.is_empty() {
                format!(
                    "{} · Ln {}, Col {} · ⌘S save · Esc return",
                    if document.is_dirty() {
                        "Unsaved"
                    } else {
                        "Saved"
                    },
                    row + 1,
                    col + 1
                )
            } else {
                preview.notice.clone()
            };
            if preview.highlights.limited {
                status.push_str(" · Partial highlighting");
            } else if let Some(language) = &preview.highlights.language {
                status.push_str(" · ");
                status.push_str(language);
            }
            s.text(
                b.x + 18.0,
                b.y + b.h - 16.0,
                status,
                10.0,
                if preview.notice.is_empty() {
                    MUTED
                } else {
                    AMBER
                },
                b,
            );
            return;
        }
        if preview.loading && preview.text.is_empty() {
            s.text(
                clip.x + 8.0,
                clip.y + 12.0,
                "Loading preview…",
                13.0,
                MUTED,
                clip,
            );
            return;
        }
        if preview.text.is_empty() {
            s.text(
                clip.x + 8.0,
                clip.y + 12.0,
                "No changes in this section.",
                13.0,
                MUTED,
                clip,
            );
            return;
        }
        let line_h = self.font * 1.5;
        let rows = (clip.h / line_h).max(1.0) as usize;
        for (i, line) in preview
            .text
            .lines()
            .skip(preview.offset)
            .take(rows)
            .enumerate()
        {
            let y = clip.y + i as f32 * line_h;
            let (color, bg) = if diff_kind.is_none() {
                (FG, None)
            } else if line.starts_with('+') && !line.starts_with("+++") {
                (GREEN, Some([22, 46, 36, 255]))
            } else if line.starts_with('-') && !line.starts_with("---") {
                (RED, Some([48, 29, 33, 255]))
            } else if line.starts_with("@@") {
                (AMBER, Some(SURFACE))
            } else {
                (FG, None)
            };
            if let Some(bg) = bg {
                s.rect(
                    Bounds {
                        x: clip.x,
                        y,
                        w: clip.w,
                        h: line_h,
                    },
                    bg,
                );
            }
            let visible: String = line
                .chars()
                .skip(preview.column)
                .take((clip.w / ((self.font - 1.0) * 0.45)) as usize + 4)
                .collect();
            s.text(clip.x + 8.0, y + 1.0, visible, self.font - 1.0, color, clip);
        }
        s.text(
            b.x + 18.0,
            b.y + b.h - 16.0,
            format!(
                "{} · column {} · Esc closes",
                if preview.notice.is_empty() {
                    "Read only · ↑↓ scroll · ←→ horizontal"
                } else {
                    &preview.notice
                },
                preview.column + 1
            ),
            9.0,
            MUTED,
            b,
        );
    }

    fn render(&mut self) -> Result<()> {
        let Some(window) = self.window.clone() else {
            return Ok(());
        };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 || self.occluded {
            return Ok(());
        }
        let scale = window.scale_factor() as f32;
        if let Some(renderer) = &mut self.renderer {
            let (w, h) = renderer.metrics(self.font * scale);
            self.cell = (w / scale, h / scale);
        }
        let mut scene = self.build_scene(size.width as f32 / scale, size.height as f32 / scale);
        for rect in &mut scene.rects {
            rect.x *= scale;
            rect.y *= scale;
            rect.width *= scale;
            rect.height *= scale;
        }
        for text in &mut scene.texts {
            text.x *= scale;
            text.y *= scale;
            text.font_size *= scale;
            text.line_height *= scale;
            for c in &mut text.clip {
                *c *= scale;
            }
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.draw(&scene.rects, &scene.texts)?;
        }
        self.scene = scene;
        Ok(())
    }

    fn confirm_prompt(&mut self, event_loop: &ActiveEventLoop) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        match prompt {
            Prompt::ClosePane(id) => self.close_pane(id, event_loop),
            Prompt::Quit => self.proxy.quit(),
            Prompt::CloseWindow => self.close_requested = true,
            Prompt::SaveChanges(_) => {}
            Prompt::Rename(value) => {
                if !value.trim().is_empty() {
                    self.name = value.chars().take(32).collect();
                    self.save();
                }
            }
            Prompt::Search(value) => {
                let found = self
                    .active_session()
                    .is_some_and(|session| session.find(&value, true));
                self.last_search = value;
                if !found {
                    self.notify("No match in retained terminal history.");
                }
            }
        }
    }

    fn key(&mut self, key: Key, text: Option<&str>, event_loop: &ActiveEventLoop) {
        if matches!(self.prompt, Some(Prompt::SaveChanges(_))) {
            match &key {
                Key::Named(NamedKey::Escape) => self.resolve_save_prompt(0),
                Key::Named(NamedKey::Enter) => self.resolve_save_prompt(1),
                Key::Character(ch)
                    if ch.eq_ignore_ascii_case("s") && self.modifiers.super_key() =>
                {
                    self.resolve_save_prompt(1)
                }
                Key::Character(ch)
                    if ch.eq_ignore_ascii_case("d") && !self.modifiers.super_key() =>
                {
                    self.resolve_save_prompt(2)
                }
                _ => {}
            }
            self.redraw();
            return;
        }
        if self.prompt.is_some() {
            if self.modifiers.super_key() {
                if matches!(&key, Key::Character(ch) if ch.eq_ignore_ascii_case("v"))
                    && let (Some(value), Some(Prompt::Rename(v) | Prompt::Search(v))) = (
                        self.clipboard.as_mut().and_then(|c| c.get_text().ok()),
                        &mut self.prompt,
                    )
                {
                    v.extend(
                        value
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(256usize.saturating_sub(v.chars().count())),
                    );
                }
                self.redraw();
                return;
            }
            match key {
                Key::Named(NamedKey::Escape) => self.prompt = None,
                Key::Named(NamedKey::Enter) => self.confirm_prompt(event_loop),
                Key::Named(NamedKey::Backspace) => {
                    if let Some(Prompt::Rename(v) | Prompt::Search(v)) = &mut self.prompt {
                        v.pop();
                    }
                }
                _ => {
                    if let (Some(t), Some(Prompt::Rename(v) | Prompt::Search(v))) =
                        (text, &mut self.prompt)
                        && v.len() < 256
                    {
                        v.push_str(t);
                    }
                }
            }
            self.redraw();
            return;
        }
        let command = self.modifiers.super_key();
        let shift = self.modifiers.shift_key();
        let alt = self.modifiers.alt_key();
        let ctrl = self.modifiers.control_key();
        let character = match &key {
            Key::Character(s) => Some(s.to_lowercase()),
            _ => None,
        };
        if command && character.as_deref() == Some("p") {
            self.focus_file_search();
            return;
        }
        if self.file_search_key(&key, text) {
            return;
        }
        if !self.file_search.focused && self.editor_key(&key, text) {
            return;
        }
        if command {
            match character.as_deref() {
                Some("b") => self.toggle_sidebar(),
                Some("n") | Some("t") => self.proxy.new_window(
                    self.active_session()
                        .map(|s| s.cwd())
                        .unwrap_or_else(|| self.initial_dir.clone()),
                ),
                Some("d") => self.split(if shift {
                    Axis::Vertical
                } else {
                    Axis::Horizontal
                }),
                Some("w") => {
                    if self.preview.is_some() {
                        self.request_leave(PendingAction::ClosePreview);
                        return;
                    }
                    if let Some(id) = self.active_id() {
                        self.request_close(id, event_loop);
                    }
                }
                Some("q") => {
                    self.prompt = Some(Prompt::Quit);
                    self.redraw();
                }
                Some("r") => {
                    self.refresh(true);
                    self.search_files();
                }
                Some("f") => {
                    self.prompt = Some(Prompt::Search(String::new()));
                    self.redraw();
                }
                Some("g") => {
                    if let Some(session) = self.active_session()
                        && !session.find(&self.last_search, !shift)
                    {
                        self.notify("No match in retained terminal history.");
                    }
                    self.redraw();
                }
                Some("e") => {
                    self.changes_view = !self.changes_view;
                    self.blur_file_search();
                    self.sidebar_scroll = 0.0;
                    self.redraw();
                }
                Some("c") => {
                    if let Some(text) = self.active_session().and_then(|s| s.selected_text())
                        && let Some(c) = &mut self.clipboard
                    {
                        let _ = c.set_text(text);
                    }
                }
                Some("v") => {
                    if self.preview.is_some() {
                        return;
                    }
                    let text = self.clipboard.as_mut().and_then(|c| c.get_text().ok());
                    if let (Some(text), Some(session)) = (text, self.active_session())
                        && let Err(e) = session.paste(&text)
                    {
                        self.notify(e.to_string());
                    }
                }
                Some("=") | Some("+") => {
                    self.font = (self.font + 1.0).min(26.0);
                    self.redraw();
                }
                Some("-") => {
                    self.font = (self.font - 1.0).max(10.0);
                    self.redraw();
                }
                Some("0") => {
                    self.font = 14.0;
                    self.redraw();
                }
                _ => {
                    if key == Key::Named(NamedKey::Enter) && shift {
                        self.zoom = !self.zoom;
                        self.redraw();
                    } else if alt
                        && matches!(
                            key,
                            Key::Named(
                                NamedKey::ArrowLeft
                                    | NamedKey::ArrowRight
                                    | NamedKey::ArrowUp
                                    | NamedKey::ArrowDown
                            )
                        )
                    {
                        self.cycle_focus(matches!(
                            key,
                            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp)
                        ));
                    }
                }
            }
            return;
        }
        if self.preview.is_some() {
            match key {
                Key::Named(NamedKey::Escape) => self.request_leave(PendingAction::ClosePreview),
                Key::Named(NamedKey::ArrowDown) => self.scroll_preview(3),
                Key::Named(NamedKey::ArrowUp) => self.scroll_preview(-3),
                Key::Named(NamedKey::ArrowLeft) => self.scroll_preview_x(-8),
                Key::Named(NamedKey::ArrowRight) => self.scroll_preview_x(8),
                _ => {}
            }
            self.redraw();
            return;
        }
        if key == Key::Named(NamedKey::F2) {
            self.prompt = Some(Prompt::Rename(self.name.clone()));
            self.redraw();
            return;
        }
        if !self.preedit.is_empty() {
            return;
        }
        if let Some(session) = self.active_session() {
            if key == Key::Named(NamedKey::Escape) {
                session.clear_selection();
                self.redraw();
            }
            let result = match key {
                Key::Named(name) => session.send_key(&format!("{name:?}"), ctrl, alt, shift),
                Key::Character(ch) if ctrl || alt => session.send_key(&ch, ctrl, alt, shift),
                _ => text.map(|t| session.input(t)).unwrap_or(Ok(())),
            };
            if let Err(e) = result {
                self.notify(e.to_string());
            }
        }
    }

    fn cycle_focus(&mut self, reverse: bool) {
        let ids = self.layout.ids();
        let index = ids.iter().position(|id| *id == self.active).unwrap_or(0);
        self.active = ids[(index + if reverse { ids.len() - 1 } else { 1 }) % ids.len()];
        self.refresh(true);
        self.redraw();
    }
    fn scroll_preview(&mut self, lines: i32) {
        if let Some(d) = &mut self.preview {
            d.offset = d
                .offset
                .saturating_add_signed(lines as isize)
                .min(d.content().split('\n').count().saturating_sub(1));
        }
    }
    fn scroll_comparison(&mut self, steps: i32, horizontal: bool) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let (Some(merge), Some(document)) = (&mut preview.merge, &preview.document) else {
            return;
        };
        let Some(conflict) = merge.conflicts.get(merge.selected) else {
            return;
        };
        let sides = [
            &document.text()[conflict.current.clone()],
            &document.text()[conflict.incoming.clone()],
        ];
        if horizontal {
            let max = sides
                .iter()
                .flat_map(|text| text.lines())
                .map(editor_view::visual_width)
                .max()
                .unwrap_or(0)
                .saturating_sub(1);
            merge.column = merge.column.saturating_add_signed(steps as isize).min(max);
        } else {
            let max = sides
                .iter()
                .map(|text| text.lines().count())
                .max()
                .unwrap_or(0)
                .saturating_sub(1);
            merge.offset = merge.offset.saturating_add_signed(steps as isize).min(max);
        }
    }
    fn scroll_preview_x(&mut self, columns: i32) {
        if let Some(preview) = &mut self.preview {
            let maximum = preview
                .content()
                .lines()
                .map(|line| {
                    if preview.document.is_some() {
                        editor_view::visual_width(line.trim_end_matches('\r'))
                    } else {
                        line.chars().count()
                    }
                })
                .max()
                .unwrap_or(0)
                .saturating_sub(1);
            preview.column = preview
                .column
                .saturating_add_signed(columns as isize)
                .min(maximum);
        }
    }
    fn cell_at(&self, id: u64, x: f32, y: f32) -> Option<(u16, u16)> {
        let (_, b) = self.scene.panes.iter().find(|(p, _)| *p == id)?;
        Some((
            ((x - b.x) / self.cell.0).max(0.0).min(b.w / self.cell.0) as u16,
            ((y - b.y) / self.cell.1).max(0.0).min(b.h / self.cell.1) as u16,
        ))
    }
}

impl App {
    pub fn start(&mut self, window: Arc<Window>) -> Result<()> {
        window.set_ime_allowed(true);
        self.renderer = Some(pollster::block_on(Renderer::new(window.clone()))?);
        eprintln!(
            "LeafTerm renderer: {}",
            self.renderer.as_ref().unwrap().adapter_name
        );
        self.window = Some(window);
        self.clipboard = arboard::Clipboard::new().ok();
        if !self.restore_sessions() {
            self.initial_session()?;
        }
        self.refresh(true);
        self.redraw();
        Ok(())
    }

    pub fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Highlighted { token, result } => {
                if let Some(preview) = &mut self.preview
                    && preview.highlight_token == token
                    && preview.document.as_ref().is_some_and(|document| {
                        preview.highlight_revision == Some(document.revision())
                    })
                {
                    preview.highlights = result;
                    self.redraw();
                }
            }
            Event::Wake => {
                if self.preview.is_none()
                    && self
                        .layout
                        .ids()
                        .iter()
                        .any(|id| self.sessions.get(id).is_some_and(|s| s.is_dirty()))
                {
                    self.redraw();
                }
                let errors: Vec<_> = self
                    .sessions
                    .values()
                    .filter_map(|s| s.take_error())
                    .collect();
                if let Some(error) = errors.first() {
                    self.notify(error.clone());
                }
            }
            Event::Workspace {
                cwd,
                hidden,
                result,
                directories,
            } => {
                self.busy = false;
                if cwd != self.active_cwd() || hidden != self.show_hidden {
                    self.refresh(true);
                    return;
                }
                let root_changed = self.workspace.as_ref().map(|w| &w.root)
                    != result.as_ref().ok().map(|w| &w.root);
                if root_changed {
                    self.directories.clear();
                    self.expanded.clear();
                    self.sidebar_scroll = 0.0;
                    if !matches!(
                        self.preview.as_ref().map(|p| &p.source),
                        Some(PreviewSource::File(_) | PreviewSource::Conflict { .. })
                    ) {
                        self.preview = None;
                    }
                }
                self.workspace_cwd = cwd;
                let mut changed = root_changed;
                match result {
                    Ok(ws) => {
                        changed |= self.workspace.as_ref() != Some(&ws);
                        self.workspace = Some(ws);
                    }
                    Err(e) => {
                        changed |= self.workspace.take().is_some();
                        self.notify(e);
                    }
                }
                for (path, listing) in directories {
                    changed |= self.directories.get(&path) != Some(&listing);
                    self.directories.insert(path, listing);
                }
                self.directories.retain(|p, _| {
                    self.expanded.contains(p)
                        || self.workspace.as_ref().is_some_and(|w| &w.root == p)
                });
                self.sync_conflict_preview();
                self.reload_preview();
                if root_changed {
                    self.search_files();
                }
                if changed {
                    self.redraw();
                }
            }
            Event::Directory {
                path,
                hidden,
                result,
            } => {
                if hidden != self.show_hidden
                    || !self.expanded.contains(&path)
                    || !self
                        .workspace
                        .as_ref()
                        .is_some_and(|w| path.starts_with(&w.root))
                {
                    return;
                }
                match result {
                    Ok(d) => {
                        self.directories.insert(path, d);
                    }
                    Err(e) => self.notify(e),
                }
                self.redraw();
            }
            Event::Preview { token, result } => {
                if let Some(preview) = &mut self.preview
                    && preview.token == token
                {
                    preview.loading = false;
                    match result {
                        Ok(loaded) => {
                            if let Some(root) = loaded.conflict_root {
                                preview.source = PreviewSource::Conflict {
                                    root,
                                    path: preview.source.path().to_owned(),
                                };
                            }
                            preview.text = loaded.text;
                            preview.document = loaded.document;
                            preview.notice = loaded.notice;
                            if loaded.recovered && preview.document.is_some() {
                                self.recovery_pending = None;
                            }
                        }
                        Err(error) => {
                            preview.text = error;
                        }
                    }
                    preview.offset = preview
                        .offset
                        .min(preview.content().split('\n').count().saturating_sub(1));
                    self.draft_revision += 1;
                    self.sync_conflict_preview();
                    self.update_highlighting();
                    self.redraw();
                }
            }
            Event::ConflictStaged { token, result } => {
                if let Some(preview) = &mut self.preview
                    && preview.token == token
                    && let Some(merge) = &mut preview.merge
                {
                    merge.staging = false;
                    match result {
                        Ok(()) => {
                            merge.staged = preview.document.as_ref().is_some_and(|d| !d.is_dirty());
                            preview.notice = if merge.staged {
                                "Resolved and staged · continue Git in the terminal"
                            } else {
                                "Saved version staged · newer edits remain unsaved"
                            }
                            .into();
                        }
                        Err(error) => preview.notice = error,
                    }
                    self.refresh(true);
                    self.redraw();
                }
            }
            Event::FileSaved { token, result } => {
                if let Some(preview) = &mut self.preview
                    && preview.token == token
                {
                    let success = preview.finish_save(result);
                    self.draft_revision += 1;
                    if success {
                        self.recovery_pending = None;
                    }
                    if let Some(pending) = self.after_save.take()
                        && success
                        && !self.has_unsaved_file()
                    {
                        self.perform_pending(pending);
                    }
                    self.refresh(true);
                    self.redraw();
                }
            }
            Event::FileSearch { token, result } => {
                if token != self.file_search.token {
                    return;
                }
                self.file_search.busy = false;
                match result {
                    Ok(result) => {
                        self.file_search.paths = result.paths;
                        self.file_search.truncated = result.truncated;
                    }
                    Err(error) => self.file_search.error = Some(error),
                }
                self.redraw();
            }
        }
    }

    pub fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let previous_active = self.active_id();
        match event {
            WindowEvent::CloseRequested => {
                self.request_leave(PendingAction::CloseWindow);
                self.redraw();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.redraw(),
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.render() {
                    eprintln!("Render error: {e:#}");
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.key(event.logical_key, event.text.as_deref(), event_loop)
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.preedit = text;
                self.redraw();
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.preedit.clear();
                if let Some(Prompt::Rename(value) | Prompt::Search(value)) = &mut self.prompt {
                    value.extend(
                        text.chars()
                            .take(256usize.saturating_sub(value.chars().count())),
                    );
                } else if self.prompt.is_none() && self.file_search.focused {
                    if self.file_search.insert(&text) {
                        self.search_files();
                    }
                } else if self.prompt.is_none()
                    && self.preview.as_ref().is_some_and(|p| p.document.is_some())
                {
                    self.edit_insert(&text);
                } else if self.prompt.is_none()
                    && self.preview.is_none()
                    && let Some(s) = self.active_session()
                    && let Err(e) = s.input(&text)
                {
                    self.notify(e.to_string());
                }
                self.redraw();
            }
            WindowEvent::Ime(Ime::Disabled) => self.preedit.clear(),
            WindowEvent::Occluded(value) => {
                self.occluded = value;
                if !value {
                    self.refresh(true);
                    self.redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.modifiers = ModifiersState::empty();
                    self.preedit.clear();
                    self.selection = None;
                    self.editor_dragging = false;
                    if self.dragging.take().is_some() {
                        self.save();
                    }
                }
                if let Some(s) = self.active_session() {
                    s.focus(focused);
                }
                if focused {
                    self.refresh(true);
                    self.redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor())
                    .unwrap_or(1.0) as f32;
                self.mouse = (position.x as f32 / scale, position.y as f32 / scale);
                if self.prompt.is_some() {
                    if let Some(window) = &self.window {
                        let over_button = self.scene.hits.iter().any(|(bounds, action)| {
                            bounds.contains(self.mouse.0, self.mouse.1)
                                && matches!(
                                    action,
                                    Action::PromptChoice(_) | Action::PromptConfirm(_)
                                )
                        });
                        window.set_cursor(if over_button {
                            CursorIcon::Pointer
                        } else {
                            CursorIcon::Default
                        });
                    }
                    return;
                }
                if let Some(d) = &self.dragging {
                    let ratio = match d.axis {
                        Axis::Horizontal => (self.mouse.0 - d.parent.x) / d.parent.w,
                        Axis::Vertical => (self.mouse.1 - d.parent.y) / d.parent.h,
                    };
                    self.layout.set_ratio(&d.path, ratio);
                    self.redraw();
                } else if let Some(id) = self.mouse_capture {
                    let pos = self.cell_at(id, self.mouse.0, self.mouse.1);
                    if let (Some(s), Some((col, row))) = (self.sessions.get_mut(&id), pos) {
                        let _ = s.mouse_move(
                            col,
                            row,
                            self.modifiers.control_key(),
                            self.modifiers.alt_key(),
                            self.modifiers.shift_key(),
                        );
                    }
                } else if self.editor_dragging {
                    self.editor_click(true);
                } else if let Some(id) = self.selection {
                    if let (Some(s), Some((col, row))) = (
                        self.sessions.get(&id),
                        self.cell_at(id, self.mouse.0, self.mouse.1),
                    ) {
                        s.select_update(col, row);
                        self.redraw();
                    }
                } else if let Some(w) = &self.window {
                    let cursor = if let Some(d) = self
                        .scene
                        .dividers
                        .iter()
                        .find(|d| d.bounds.contains(self.mouse.0, self.mouse.1))
                    {
                        match d.axis {
                            Axis::Horizontal => CursorIcon::ColResize,
                            Axis::Vertical => CursorIcon::RowResize,
                        }
                    } else if self
                        .scene
                        .editor
                        .is_some_and(|b| b.contains(self.mouse.0, self.mouse.1))
                        || self
                            .scene
                            .panes
                            .iter()
                            .any(|(_, b)| b.contains(self.mouse.0, self.mouse.1))
                    {
                        CursorIcon::Text
                    } else if self
                        .scene
                        .hits
                        .iter()
                        .any(|(b, _)| b.contains(self.mouse.0, self.mouse.1))
                    {
                        CursorIcon::Pointer
                    } else {
                        CursorIcon::Default
                    };
                    w.set_cursor(cursor);
                    if let Some((id, _)) = self
                        .scene
                        .panes
                        .iter()
                        .find(|(_, b)| b.contains(self.mouse.0, self.mouse.1))
                        .copied()
                    {
                        let pos = self.cell_at(id, self.mouse.0, self.mouse.1);
                        if let (Some(session), Some((col, row))) = (self.sessions.get_mut(&id), pos)
                        {
                            let _ = session.mouse_move(
                                col,
                                row,
                                self.modifiers.control_key(),
                                self.modifiers.alt_key(),
                                self.modifiers.shift_key(),
                            );
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if state == ElementState::Released {
                    self.editor_dragging = false;
                    if let Some(id) = self.mouse_capture.take() {
                        let pos = self.cell_at(id, self.mouse.0, self.mouse.1);
                        if let (Some(s), Some((col, row))) = (self.sessions.get_mut(&id), pos) {
                            let _ = s.mouse_button(
                                col,
                                row,
                                0,
                                false,
                                self.modifiers.control_key(),
                                self.modifiers.alt_key(),
                                self.modifiers.shift_key(),
                            );
                        }
                    }
                    self.selection = None;
                    if self.dragging.take().is_some() {
                        self.save();
                    }
                    return;
                }
                if self.prompt.is_some() {
                    if let Some(action) = self.scene.hits.iter().rev().find_map(|(b, action)| {
                        if b.contains(self.mouse.0, self.mouse.1)
                            && matches!(action, Action::PromptChoice(_) | Action::PromptConfirm(_))
                        {
                            return Some(action.clone());
                        }
                        None
                    }) {
                        self.activate(action, event_loop);
                    }
                    return;
                }
                let search_control = self.scene.hits.iter().any(|(bounds, action)| {
                    bounds.contains(self.mouse.0, self.mouse.1)
                        && matches!(action, Action::FocusFileSearch | Action::ClearFileSearch)
                });
                if !search_control {
                    self.blur_file_search();
                }
                if self
                    .scene
                    .editor
                    .is_some_and(|bounds| bounds.contains(self.mouse.0, self.mouse.1))
                {
                    self.editor_click(self.modifiers.shift_key());
                    self.editor_dragging = true;
                    return;
                }
                if let Some(d) = self
                    .scene
                    .dividers
                    .iter()
                    .find(|d| d.bounds.contains(self.mouse.0, self.mouse.1))
                    .cloned()
                {
                    self.dragging = Some(d);
                    return;
                }
                if let Some((id, _)) = self
                    .scene
                    .panes
                    .iter()
                    .find(|(_, b)| b.contains(self.mouse.0, self.mouse.1))
                    .copied()
                {
                    self.active = id;
                    let pos = self.cell_at(id, self.mouse.0, self.mouse.1);
                    if let (Some(s), Some((col, row))) = (self.sessions.get_mut(&id), pos) {
                        if s.mouse_button(
                            col,
                            row,
                            0,
                            true,
                            self.modifiers.control_key(),
                            self.modifiers.alt_key(),
                            self.modifiers.shift_key(),
                        )
                        .unwrap_or(false)
                        {
                            self.mouse_capture = Some(id);
                        } else {
                            s.select_start(col, row);
                            self.selection = Some(id);
                        }
                    }
                    self.refresh(true);
                    self.redraw();
                } else if let Some(action) = self
                    .scene
                    .hits
                    .iter()
                    .rev()
                    .find(|(b, _)| b.contains(self.mouse.0, self.mouse.1))
                    .map(|(_, a)| a.clone())
                {
                    self.activate(action, event_loop);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                if self.prompt.is_some() {
                    return;
                }
                let path = self
                    .scene
                    .hits
                    .iter()
                    .find(|(b, _)| b.contains(self.mouse.0, self.mouse.1))
                    .and_then(|(_, a)| match a {
                        Action::File(p) | Action::Directory(p) | Action::Conflict(p) => {
                            Some(p.clone())
                        }
                        Action::Diff(c, _) => self.workspace.as_ref().map(|w| w.root.join(&c.path)),
                        _ => None,
                    });
                if let (Some(p), Some(c)) = (path, &mut self.clipboard) {
                    let _ = c.set_text(p.to_string_lossy().into_owned());
                    self.notify("Path copied.");
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.prompt.is_some() {
                    return;
                }
                let (horizontal, lines) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * 3.0, -y * 3.0),
                    MouseScrollDelta::PixelDelta(p) => (-p.x as f32 / 12.0, -p.y as f32 / 12.0),
                };
                if self.sidebar_visible && self.mouse.0 < self.sidebar {
                    self.sidebar_scroll =
                        (self.sidebar_scroll + lines * 12.0).clamp(0.0, self.sidebar_max);
                } else if self
                    .scene
                    .comparison
                    .is_some_and(|b| b.contains(self.mouse.0, self.mouse.1))
                {
                    let horizontal_scroll =
                        self.modifiers.shift_key() || horizontal.abs() > lines.abs();
                    self.wheel_remainder += if horizontal_scroll && !self.modifiers.shift_key() {
                        horizontal
                    } else {
                        lines
                    };
                    let steps = self.wheel_remainder.trunc() as i32;
                    self.wheel_remainder -= steps as f32;
                    self.scroll_comparison(steps, horizontal_scroll);
                } else if self.preview.is_some()
                    && (self.modifiers.shift_key() || horizontal.abs() > lines.abs())
                {
                    self.scroll_preview_x(
                        (if self.modifiers.shift_key() {
                            lines
                        } else {
                            horizontal
                        })
                        .round() as i32,
                    );
                } else {
                    self.wheel_remainder += lines;
                    let steps = self.wheel_remainder.trunc() as i32;
                    self.wheel_remainder -= steps as f32;
                    if self.preview.is_some() {
                        self.scroll_preview(steps);
                    } else if let Some((id, _)) = self
                        .scene
                        .panes
                        .iter()
                        .find(|(_, b)| b.contains(self.mouse.0, self.mouse.1))
                        && let Some(s) = self.sessions.get(id)
                    {
                        s.scroll(-steps);
                    }
                }
                self.redraw();
            }
            WindowEvent::DroppedFile(path) => {
                if self.prompt.is_none()
                    && self.preview.is_none()
                    && !self.file_search.focused
                    && let Some(s) = self.active_session()
                {
                    let quoted = format!("'{}' ", path.to_string_lossy().replace('\'', "'\\''"));
                    let _ = s.input(&quoted);
                }
            }
            _ => {}
        }
        if previous_active != self.active_id() {
            if let Some(old) = previous_active.and_then(|id| self.sessions.get(&id)) {
                old.focus(false);
            }
            if let Some(new) = self.active_session() {
                new.focus(true);
            }
            self.preedit.clear();
            self.wheel_remainder = 0.0;
            if self.selection != self.active_id() {
                self.selection = None;
            }
        }
    }

    pub fn about_to_wait(&mut self) {
        self.update_highlighting();
        if !self.occluded {
            self.refresh(false);
        }
        if let Some(window) = &self.window {
            let title = if self.name.is_empty() {
                self.active_session()
                    .map(|session| {
                        if session.is_exited() {
                            "Session ended".into()
                        } else {
                            // Reading chrome must not consume the terminal's pending redraw.
                            let title = session.title();
                            let label = if title.is_empty() {
                                name_of(&session.cwd())
                            } else {
                                title.chars().take(80).collect()
                            };
                            if session.has_running_job() {
                                format!("{label} · running")
                            } else {
                                label
                            }
                        }
                    })
                    .unwrap_or_else(|| "LeafTerm".into())
            } else {
                self.name.clone()
            };
            window.set_title(&format!("{title} — LeafTerm"));
        }
    }

    pub fn shutdown(&mut self) {
        for session in self.sessions.values() {
            session.shutdown();
        }
        let sessions = std::mem::take(&mut self.sessions);
        // PTY child reaping must not block events for the other native windows.
        if !sessions.is_empty() {
            std::thread::spawn(move || drop(sessions));
        }
    }
}

fn rgba(c: [u8; 3]) -> [u8; 4] {
    [c[0], c[1], c[2], 255]
}
fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}
fn short_path(p: &Path) -> String {
    let path = p.to_string_lossy();
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = path.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    path.into_owned()
}
fn status_color(c: char) -> [u8; 4] {
    match c {
        'A' | '?' => GREEN,
        'D' => RED,
        _ => AMBER,
    }
}

#[cfg(test)]
mod editor_integration_tests {
    use super::*;

    #[test]
    fn choosing_conflicts_and_undo_refresh_the_result_and_save_gate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "# before\n<<<<<<< HEAD\n当前\n=======\n传入\n>>>>>>> topic\n# between\n<<<<<<< HEAD\nold\n=======\nnew\n>>>>>>> topic\n# after\n";
        std::fs::write(&path, original).unwrap();
        let mut document = Document::open(&path).unwrap();
        let mut state = MergeState::default();
        state.refresh(document.text());
        for choice in [merge::Choice::Both, merge::Choice::Incoming] {
            assert!(state.unresolved());
            let conflict = &state.conflicts[0];
            document
                .replace_range(
                    conflict.range.clone(),
                    &conflict.replacement(document.text(), choice),
                )
                .unwrap();
            state.refresh(document.text());
        }
        assert!(!state.unresolved());
        assert_eq!(
            document.text(),
            "# before\n当前\n传入\n# between\nnew\n# after\n"
        );
        assert!(document.saved_bytes().is_none());
        document.undo();
        state.refresh(document.text());
        assert!(state.unresolved());
        assert_eq!(state.conflicts.len(), 1);
        document.undo();
        state.refresh(document.text());
        assert_eq!(document.text(), original);
        assert_eq!(state.conflicts.len(), 2);
        assert!(!document.is_dirty());
        document.select_all();
        document.insert("<<<<<<< incomplete\n").unwrap();
        state.refresh(document.text());
        assert!(state.unresolved());
        assert!(state.error.is_some());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn save_completion_keeps_new_input_dirty_and_visible() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "enabled = false\n").unwrap();
        let mut document = Document::open(&path).unwrap();
        document.select_all();
        document.insert("enabled = true\n").unwrap();
        let save = document.save_request();
        document.insert("# newer edit\n").unwrap();
        let mut preview = Preview {
            source: PreviewSource::File(path.clone()),
            token: 1,
            text: String::new(),
            offset: 0,
            column: 0,
            loading: false,
            document: Some(document),
            saving: true,
            notice: String::new(),
            merge: None,
            highlight_revision: None,
            highlight_token: 0,
            highlights: Highlights::default(),
        };
        assert!(preview.finish_save(save.save().map_err(|e| e.to_string())));
        assert!(preview.document.as_ref().unwrap().is_dirty());
        assert!(preview.content().contains("newer edit"));
        assert!(preview.notice.contains("remain unsaved"));
        assert!(!preview.saving);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "enabled = true\n");
    }
}
