//! Bounded search input and a debounced, cancellable worker without an index.

use crate::{app::Event, windows::WindowProxy, workspace};
use parking_lot::{Condvar, Mutex};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub struct FileSearch {
    pub query: String,
    pub cursor: usize,
    pub select_all: bool,
    pub focused: bool,
    pub selected: usize,
    pub paths: Vec<PathBuf>,
    pub truncated: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub token: u64,
}

impl FileSearch {
    pub fn insert(&mut self, text: &str) -> bool {
        self.snap_cursor(false);
        let used = if self.select_all {
            0
        } else {
            self.query.chars().count()
        };
        // A UTF-8 scalar uses at most four bytes: 256 chars also caps 1024 bytes.
        let added: String = text
            .chars()
            .filter(|c| !c.is_control())
            .take(256usize.saturating_sub(used))
            .collect();
        if added.is_empty() {
            return false;
        }
        let changed = !self.select_all || self.query != added;
        if self.select_all {
            self.query.clear();
            self.cursor = 0;
        }
        self.query.insert_str(self.cursor, &added);
        self.cursor += added.len();
        self.select_all = false;
        self.snap_cursor(true);
        changed
    }

    pub fn backspace(&mut self) -> bool {
        self.erase(false)
    }
    pub fn delete(&mut self) -> bool {
        self.erase(true)
    }

    pub fn move_cursor(&mut self, right: bool) {
        self.snap_cursor(false);
        self.cursor = if self.select_all {
            if right { self.query.len() } else { 0 }
        } else {
            self.neighbor(right)
        };
        self.select_all = false;
    }

    pub fn clear(&mut self) {
        *self = Self {
            focused: self.focused,
            token: self.token,
            ..Self::default()
        };
    }

    fn erase(&mut self, right: bool) -> bool {
        self.snap_cursor(false);
        if self.select_all {
            let changed = !self.query.is_empty();
            self.query.clear();
            self.cursor = 0;
            self.select_all = false;
            return changed;
        }
        let other = self.neighbor(right);
        if other == self.cursor {
            return false;
        }
        let start = other.min(self.cursor);
        self.query.replace_range(start..other.max(self.cursor), "");
        self.cursor = start;
        self.snap_cursor(true);
        true
    }

    fn neighbor(&self, right: bool) -> usize {
        if right {
            self.cursor
                + self.query[self.cursor..]
                    .graphemes(true)
                    .next()
                    .map_or(0, str::len)
        } else {
            self.query[..self.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(i, _)| i)
        }
    }

    fn snap_cursor(&mut self, forward: bool) {
        let at = self.cursor.min(self.query.len());
        let mut points = self
            .query
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(self.query.len()));
        self.cursor = if forward {
            points.find(|&i| i >= at).unwrap_or(self.query.len())
        } else {
            points.take_while(|&i| i <= at).last().unwrap_or(0)
        };
    }
}

struct Task {
    root: PathBuf,
    query: String,
    hidden: bool,
    token: u64,
    due: Instant,
}
#[derive(Default)]
struct Slot {
    task: Option<Task>,
    stopped: bool,
}
#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
    generation: Arc<AtomicU64>,
}
pub struct SearchWorker {
    shared: Arc<Shared>,
}

impl SearchWorker {
    pub fn new(proxy: WindowProxy) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("leafterm-file-search".into())
            .stack_size(512 * 1024)
            .spawn(move || run(worker, proxy))?;
        Ok(Self { shared })
    }

    pub fn submit(&self, root: PathBuf, query: String, hidden: bool) -> u64 {
        let mut slot = self.shared.slot.lock();
        let token = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        slot.task = (!query.trim().is_empty()).then(|| Task {
            root,
            query,
            hidden,
            token,
            due: Instant::now() + Duration::from_millis(150),
        });
        self.shared.ready.notify_one();
        token
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        let mut slot = self.shared.slot.lock();
        slot.stopped = true;
        slot.task = None;
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.ready.notify_one();
    }
}

fn run(shared: Arc<Shared>, proxy: WindowProxy) {
    loop {
        let task = {
            let mut slot = shared.slot.lock();
            loop {
                if slot.stopped {
                    return;
                }
                match slot.task.as_ref().map(|task| task.due) {
                    None => shared.ready.wait(&mut slot),
                    Some(due) if due > Instant::now() => {
                        shared.ready.wait_until(&mut slot, due);
                    }
                    Some(_) => break slot.task.take().unwrap(),
                }
            }
        };
        let cancelled = || shared.generation.load(Ordering::Acquire) != task.token;
        let result = workspace::search_files(&task.root, &task.query, task.hidden, cancelled)
            .map_err(|e| e.to_string());
        let event = Event::FileSearch {
            token: task.token,
            result,
        };
        if !cancelled() && proxy.send_event(event).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_search_edits_whole_graphemes_and_replaces_selection() {
        let mut search = FileSearch::default();
        assert!(search.insert("配置e\u{301}👩‍💻.rs"));
        for _ in 0..3 {
            search.move_cursor(false);
        }
        assert!(search.backspace());
        assert_eq!(search.query, "配置e\u{301}.rs");
        assert!(search.backspace());
        assert_eq!(search.query, "配置.rs");
        search.move_cursor(false);
        assert!(search.delete());
        assert_eq!(search.query, "配.rs");
        search.select_all = true;
        assert!(search.insert("新\n文件\t.toml"));
        assert_eq!(search.query, "新文件.toml");
        assert_eq!(search.cursor, search.query.len());
        search.select_all = true;
        assert!(!search.insert("新文件.toml"));
        assert!(!search.delete());
    }

    #[test]
    fn input_limits_and_clear_keep_only_focus_and_generation() {
        let mut search = FileSearch {
            focused: true,
            token: 7,
            ..FileSearch::default()
        };
        assert!(search.insert(&"🦀".repeat(300)));
        assert_eq!(search.query.chars().count(), 256);
        assert_eq!(search.query.len(), 1024);
        assert!(!search.insert("x"));
        search.paths.push("result.rs".into());
        search.error = Some("error".into());
        search.busy = true;
        search.truncated = true;
        search.selected = 9;
        search.clear();
        assert!(search.query.is_empty() && search.paths.is_empty() && search.error.is_none());
        assert!(search.focused);
        assert_eq!((search.token, search.cursor, search.selected), (7, 0, 0));
        assert!(!search.busy && !search.truncated && !search.select_all);
        assert!(!search.backspace());
        assert!(!search.insert("\n\0\t"));
    }
}
