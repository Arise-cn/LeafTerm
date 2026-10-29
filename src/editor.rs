//! Small, bounded UTF-8 document model. Disk access belongs on the workspace worker.

use std::{
    collections::VecDeque,
    fs::{self, File, Metadata, Permissions},
    io::{Read, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_BYTES: usize = 256 * 1024;
const HISTORY_BYTES: usize = 4 * 1024 * 1024;
const HISTORY_STEPS: usize = 64;
const BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Clone, Copy, Debug)]
pub enum Movement {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Start,
    Finish,
}

#[derive(Clone)]
struct EditState {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    identity: (u64, u64),
    #[cfg(unix)]
    mode: u32,
}

impl Stamp {
    fn of(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            identity: (metadata.dev(), metadata.ino()),
            #[cfg(unix)]
            mode: metadata.mode(),
        }
    }
}

#[derive(Clone)]
struct DiskVersion {
    bytes: Arc<[u8]>,
    stamp: Stamp,
    permissions: Permissions,
}

impl DiskVersion {
    fn matches(&self, other: &Self) -> bool {
        self.stamp == other.stamp && self.bytes == other.bytes
    }
}

pub struct Document {
    path: PathBuf,
    state: EditState,
    revision: u64,
    baseline: DiskVersion,
    bom: bool,
    crlf: bool,
    recovery_review: bool,
    preferred_column: Option<usize>,
    undo: VecDeque<EditState>,
    redo: VecDeque<EditState>,
}

pub struct SaveRequest {
    path: PathBuf,
    bytes: Vec<u8>,
    baseline: DiskVersion,
}

pub struct SavedFile {
    path: PathBuf,
    version: DiskVersion,
}

/// Private recovery storage is managed by the application, not the document.
#[derive(Clone, Serialize, Deserialize)]
pub struct RecoveryDraft {
    pub path: PathBuf,
    #[serde(default)]
    pub needs_review: bool,
    text: String,
    baseline: Vec<u8>,
    bom: bool,
    crlf: bool,
    cursor: usize,
}

impl Document {
    pub fn open(path: &Path) -> Result<Self> {
        let baseline = read_disk(path)?;
        ensure!(
            !baseline.bytes.contains(&0),
            "Binary files cannot be edited."
        );
        let bom = baseline.bytes.starts_with(BOM);
        let bytes = &baseline.bytes[if bom { BOM.len() } else { 0 }..];
        let text = std::str::from_utf8(bytes)
            .context("Only UTF-8 text files can be edited.")?
            .to_owned();
        let crlf_count = text.matches("\r\n").count();
        let crlf = crlf_count > 0 && crlf_count * 2 >= text.matches('\n').count();
        Ok(Self {
            path: path.to_owned(),
            state: EditState {
                text,
                cursor: 0,
                anchor: None,
            },
            revision: 0,
            baseline,
            bom,
            crlf,
            recovery_review: false,
            preferred_column: None,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
        })
    }

    pub fn text(&self) -> &str {
        &self.state.text
    }

    /// Changes only when contents change, so cursor movement never schedules parsing.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_dirty(&self) -> bool {
        let offset = if self.bom { BOM.len() } else { 0 };
        self.recovery_review || self.text().as_bytes() != &self.baseline.bytes[offset..]
    }

    /// Exact saved contents, including a BOM, only while no edits are pending.
    pub fn saved_bytes(&self) -> Option<Vec<u8>> {
        (!self.is_dirty()).then(|| self.baseline.bytes.to_vec())
    }

    /// Zero-based line and display column. Tabs use four-column stops.
    pub fn cursor_position(&self) -> (usize, usize) {
        let prefix = &self.text()[..self.state.cursor];
        let row = prefix.bytes().filter(|&b| b == b'\n').count();
        let start = prefix.rfind('\n').map_or(0, |i| i + 1);
        (row, display_width(&self.text()[start..self.state.cursor]))
    }

    pub fn selection_range(&self) -> Range<usize> {
        let anchor = self.state.anchor.unwrap_or(self.state.cursor);
        anchor.min(self.state.cursor)..anchor.max(self.state.cursor)
    }

    pub fn selected_text(&self) -> Option<&str> {
        let range = self.selection_range();
        (!range.is_empty()).then(|| &self.text()[range])
    }

    pub fn insert(&mut self, text: &str) -> Result<()> {
        // Even after CRLF normalization, a larger input cannot fit this document.
        ensure!(
            text.len() <= MAX_BYTES * 2,
            "Editing is limited to 256 KiB per file."
        );
        ensure!(!text.contains('\0'), "Text cannot contain NUL bytes.");
        let mut text = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.crlf {
            text = text.replace('\n', "\r\n");
        }
        let range = self.selection_range();
        let bytes =
            self.text().len() - range.len() + text.len() + if self.bom { BOM.len() } else { 0 };
        ensure!(
            bytes <= MAX_BYTES,
            "Editing is limited to 256 KiB per file."
        );
        if range.is_empty() && text.is_empty() {
            return Ok(());
        }
        self.replace(range, &text);
        Ok(())
    }

    /// Apply one undoable edit using byte ranges from this document revision.
    /// Unlike typed input, the replacement keeps its original line endings.
    pub fn replace_range(&mut self, range: Range<usize>, replacement: &str) -> Result<()> {
        ensure!(
            range.start <= range.end && range.end <= self.text().len(),
            "Replacement range is outside the document."
        );
        ensure!(
            [range.start, range.end].into_iter().all(|boundary| {
                boundary == self.text().len()
                    || self
                        .text()
                        .grapheme_indices(true)
                        .any(|(i, _)| i == boundary)
            }),
            "Replacement must start and end at complete text characters."
        );
        ensure!(
            !replacement.contains('\0'),
            "Text cannot contain NUL bytes."
        );
        let bytes = (self.text().len() - range.len())
            .saturating_add(replacement.len())
            .saturating_add(if self.bom { BOM.len() } else { 0 });
        ensure!(
            bytes <= MAX_BYTES,
            "Editing is limited to 256 KiB per file."
        );
        if &self.text()[range.clone()] != replacement {
            self.replace(range, replacement);
        }
        Ok(())
    }

    pub fn backspace(&mut self) {
        let mut range = self.selection_range();
        if range.is_empty() {
            range.start = self.previous_boundary(self.state.cursor);
        }
        if !range.is_empty() {
            self.replace(range, "");
        }
    }

    pub fn delete(&mut self) {
        let mut range = self.selection_range();
        if range.is_empty() {
            range.end = self.next_boundary(self.state.cursor);
        }
        if !range.is_empty() {
            self.replace(range, "");
        }
    }

    pub fn move_cursor(&mut self, movement: Movement, extend: bool) {
        let range = self.selection_range();
        let (row, column) = self.cursor_position();
        let cursor = match movement {
            Movement::Left if !extend && !range.is_empty() => range.start,
            Movement::Right if !extend && !range.is_empty() => range.end,
            Movement::Left => self.previous_boundary(self.state.cursor),
            Movement::Right => self.next_boundary(self.state.cursor),
            Movement::Home => self.line_bounds(row).start,
            Movement::End => self.line_bounds(row).end,
            Movement::Start => 0,
            Movement::Finish => self.text().len(),
            Movement::Up | Movement::Down => {
                let preferred = *self.preferred_column.get_or_insert(column);
                self.byte_at(
                    if matches!(movement, Movement::Up) {
                        row.saturating_sub(1)
                    } else {
                        row + 1
                    },
                    preferred,
                )
            }
        };
        if !matches!(movement, Movement::Up | Movement::Down) {
            self.preferred_column = None;
        }
        self.set_position(cursor, extend);
    }

    pub fn set_cursor(&mut self, row: usize, visual_column: usize, extend: bool) {
        let cursor = self.byte_at(row, visual_column);
        self.preferred_column = None;
        self.set_position(cursor, extend);
    }

    pub fn select_all(&mut self) {
        self.state.anchor = Some(0);
        self.state.cursor = self.text().len();
        self.preferred_column = None;
    }

    pub fn undo(&mut self) {
        if let Some(previous) = self.undo.pop_back() {
            if self.state.text != previous.text {
                self.revision = self.revision.wrapping_add(1);
            }
            self.redo
                .push_back(std::mem::replace(&mut self.state, previous));
            self.preferred_column = None;
            self.trim_history();
        }
    }

    pub fn redo(&mut self) {
        if let Some(next) = self.redo.pop_back() {
            if self.state.text != next.text {
                self.revision = self.revision.wrapping_add(1);
            }
            self.undo
                .push_back(std::mem::replace(&mut self.state, next));
            self.preferred_column = None;
            self.trim_history();
        }
    }

    pub fn save_request(&self) -> SaveRequest {
        let mut bytes =
            Vec::with_capacity(self.text().len() + if self.bom { BOM.len() } else { 0 });
        if self.bom {
            bytes.extend_from_slice(BOM);
        }
        bytes.extend_from_slice(self.text().as_bytes());
        SaveRequest {
            path: self.path.clone(),
            bytes,
            baseline: self.baseline.clone(),
        }
    }

    /// A save completion changes the disk baseline, never newer edits or selection.
    pub fn accept_save(&mut self, saved: SavedFile) {
        if saved.path == self.path {
            self.baseline = saved.version;
            self.recovery_review = false;
        }
    }

    pub fn recovery_draft(&self) -> RecoveryDraft {
        RecoveryDraft {
            path: self.path.clone(),
            needs_review: self.recovery_review,
            text: self.text().to_owned(),
            baseline: self.baseline.bytes.to_vec(),
            bom: self.bom,
            crlf: self.crlf,
            cursor: self.state.cursor,
        }
    }

    pub fn recover(draft: RecoveryDraft) -> Result<Self> {
        let bom_len = if draft.bom { BOM.len() } else { 0 };
        ensure!(
            draft.text.len() + bom_len <= MAX_BYTES && draft.baseline.len() <= MAX_BYTES,
            "Recovery draft exceeds the 256 KiB editing limit."
        );
        ensure!(
            draft.baseline.starts_with(BOM) == draft.bom,
            "Recovery draft has an invalid UTF-8 BOM."
        );
        ensure!(
            !draft.text.contains('\0') && !draft.baseline.contains(&0),
            "Recovery draft contains binary data."
        );
        std::str::from_utf8(&draft.baseline).context("Recovery baseline is not UTF-8")?;
        ensure!(
            draft.cursor == draft.text.len()
                || draft
                    .text
                    .grapheme_indices(true)
                    .any(|(i, _)| i == draft.cursor),
            "Recovery draft has an invalid cursor position."
        );
        let mut baseline = read_disk(&draft.path)?;
        // Keep today's identity/permissions but yesterday's contents: this preserves
        // conflict detection when a tool changed the file while LeafTerm was closed.
        baseline.bytes = draft.baseline.into();
        Ok(Self {
            path: draft.path,
            state: EditState {
                text: draft.text,
                cursor: draft.cursor,
                anchor: None,
            },
            revision: 0,
            baseline,
            bom: draft.bom,
            crlf: draft.crlf,
            recovery_review: draft.needs_review,
            preferred_column: None,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
        })
    }

    fn replace(&mut self, range: Range<usize>, text: &str) {
        if &self.text()[range.clone()] != text {
            self.revision = self.revision.wrapping_add(1);
        }
        self.undo.push_back(self.state.clone());
        self.redo.clear();
        self.trim_history();
        self.state.text.replace_range(range.clone(), text);
        // Inserted combining marks or joiners can merge with a neighbouring cluster.
        let requested = range.start + text.len();
        self.state.cursor = self
            .text()
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= requested)
            .unwrap_or(self.text().len());
        self.state.anchor = None;
        self.preferred_column = None;
    }

    fn trim_history(&mut self) {
        while self.undo.len() + self.redo.len() > HISTORY_STEPS
            || self
                .undo
                .iter()
                .chain(&self.redo)
                .map(|s| s.text.len())
                .sum::<usize>()
                > HISTORY_BYTES
        {
            if self.undo.pop_front().is_none() {
                self.redo.pop_front();
            }
        }
    }

    fn previous_boundary(&self, at: usize) -> usize {
        self.text()[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, at: usize) -> usize {
        at + self.text()[at..].graphemes(true).next().map_or(0, str::len)
    }

    fn set_position(&mut self, cursor: usize, extend: bool) {
        if extend {
            self.state.anchor.get_or_insert(self.state.cursor);
        } else {
            self.state.anchor = None;
        }
        self.state.cursor = cursor;
    }

    fn line_bounds(&self, row: usize) -> Range<usize> {
        let mut start = 0;
        for _ in 0..row {
            let Some(end) = self.text()[start..].find('\n') else {
                break;
            };
            start += end + 1;
        }
        let mut end = self.text()[start..]
            .find('\n')
            .map_or(self.text().len(), |i| start + i);
        if end > start && self.text().as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        start..end
    }

    fn byte_at(&self, row: usize, target: usize) -> usize {
        let range = self.line_bounds(row);
        let mut column = 0;
        for (i, grapheme) in self.text()[range.clone()].grapheme_indices(true) {
            if target <= column {
                return range.start + i;
            }
            let width = grapheme_width(grapheme, column);
            if target < column + width {
                return range.start
                    + i
                    + if target - column >= width.div_ceil(2) {
                        grapheme.len()
                    } else {
                        0
                    };
            }
            column += width;
        }
        range.end
    }
}

impl SaveRequest {
    pub fn save(self) -> Result<SavedFile> {
        let current =
            read_disk(&self.path).context("Cannot save; reload the file and try again.")?;
        ensure!(
            self.baseline.matches(&current),
            "File changed on disk. Reload it before saving; your edits have not been written."
        );
        ensure!(!current.permissions.readonly(), "This file is read only.");
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .context("Cannot create a temporary save file")?;
        temporary.write_all(&self.bytes)?;
        temporary
            .as_file()
            .set_permissions(current.permissions.clone())?;
        temporary.as_file().sync_all()?;
        // Recheck after writing the temporary file, immediately before atomic replacement.
        ensure!(
            self.baseline.matches(&read_disk(&self.path)?),
            "File changed while saving. Your edits have not been written."
        );
        let file = temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .context("Cannot replace file")?;
        let metadata = file.metadata()?;
        Ok(SavedFile {
            path: self.path,
            version: DiskVersion {
                bytes: self.bytes.into(),
                stamp: Stamp::of(&metadata),
                permissions: metadata.permissions(),
            },
        })
    }
}

fn grapheme_width(grapheme: &str, column: usize) -> usize {
    if grapheme == "\t" {
        4 - column % 4
    } else {
        UnicodeWidthStr::width(grapheme)
    }
}

fn display_width(text: &str) -> usize {
    text.graphemes(true)
        .fold(0, |column, g| column + grapheme_width(g, column))
}

fn read_disk(path: &Path) -> Result<DiskVersion> {
    let metadata = fs::symlink_metadata(path).context("Cannot inspect file")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Only regular files can be edited; symbolic links and special files are read only."
    );
    ensure!(
        metadata.len() <= MAX_BYTES as u64,
        "Editing is limited to 256 KiB per file."
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file: File = options.open(path).context("Cannot open file")?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && Stamp::of(&opened) == Stamp::of(&metadata),
        "File changed while opening; try again."
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "Editing is limited to 256 KiB per file."
    );
    let final_metadata = file.metadata()?;
    let stamp = Stamp::of(&final_metadata);
    let path_metadata = fs::symlink_metadata(path)?;
    if !path_metadata.is_file() || stamp != Stamp::of(&opened) || stamp != Stamp::of(&path_metadata)
    {
        bail!("File changed while reading; try again.");
    }
    Ok(DiskVersion {
        bytes: bytes.into(),
        stamp,
        permissions: final_metadata.permissions(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &[u8]) -> (tempfile::TempDir, PathBuf, Document) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("中文 config.txt");
        fs::write(&path, text).unwrap();
        let doc = Document::open(&path).unwrap();
        (dir, path, doc)
    }

    #[test]
    fn content_revisions_ignore_selection_save_and_no_ops_but_track_undo() {
        let (_dir, _path, mut doc) = document("中文\n".as_bytes());
        assert_eq!(doc.revision(), 0);
        doc.move_cursor(Movement::End, true);
        doc.set_cursor(0, 0, false);
        doc.backspace();
        doc.insert("").unwrap();
        doc.undo();
        assert_eq!(doc.revision(), 0);
        doc.select_all();
        doc.insert("中文\n").unwrap();
        assert_eq!(doc.revision(), 0);
        doc.insert("new").unwrap();
        assert_eq!(doc.revision(), 1);
        doc.undo();
        assert_eq!(doc.revision(), 2);
        doc.redo();
        assert_eq!(doc.revision(), 3);
        doc.accept_save(doc.save_request().save().unwrap());
        assert_eq!(doc.revision(), 3);
        let mut restored = Document::recover(doc.recovery_draft()).unwrap();
        assert_eq!(restored.revision(), 0);
        restored.insert("x").unwrap();
        assert_eq!(restored.revision(), 1);
    }

    #[test]
    fn range_replacement_is_one_undo_step_preserving_bom_and_original_line_endings() {
        let original = "前言\n<<<<<<< HEAD\r\n当前\r\n=======\r\n传入\r\n>>>>>>> topic\r\n结束\n";
        let mut bytes = BOM.to_vec();
        bytes.extend_from_slice(original.as_bytes());
        let (_dir, path, mut doc) = document(&bytes);
        assert_eq!(doc.saved_bytes(), Some(bytes.clone()));
        doc.set_cursor(0, 2, false);
        doc.move_cursor(Movement::Right, true);
        let selection = doc.selection_range();
        let cursor = doc.cursor_position();
        let range = original.find("<<<<<<<").unwrap()..original.find("结束").unwrap();
        doc.replace_range(range, "当前\r\n传入\r\n").unwrap();
        assert_eq!(doc.text(), "前言\n当前\r\n传入\r\n结束\n");
        assert!(doc.saved_bytes().is_none());
        doc.undo();
        assert_eq!(doc.text(), original);
        assert_eq!(doc.selection_range(), selection);
        assert_eq!(doc.cursor_position(), cursor);
        assert_eq!(doc.saved_bytes(), Some(bytes));
        doc.redo();
        let expected = b"\xef\xbb\xbf"
            .iter()
            .copied()
            .chain(doc.text().bytes())
            .collect::<Vec<_>>();
        doc.accept_save(doc.save_request().save().unwrap());
        assert_eq!(fs::read(path).unwrap(), expected);
        assert_eq!(doc.saved_bytes(), Some(expected));
    }

    #[test]
    fn range_replacement_rejects_invalid_utf8_grapheme_and_crlf_boundaries_without_changes() {
        let original = "中e\u{301}👩‍💻\r\n末";
        let (_dir, _path, mut doc) = document(original.as_bytes());
        let emoji = original.find('👩').unwrap();
        let newline = original.find('\n').unwrap();
        for range in [
            0..1,
            1..3,
            3..4,
            emoji..emoji + '👩'.len_utf8(),
            newline..newline + 1,
            Range { start: 4, end: 3 },
            0..usize::MAX,
        ] {
            assert!(doc.replace_range(range, "x").is_err());
            assert_eq!(doc.text(), original);
        }
        assert!(doc.replace_range(0..0, "\0").is_err());
        assert!(doc.replace_range(0..0, &"x".repeat(MAX_BYTES)).is_err());
        assert!(doc.undo.is_empty());
        assert!(!doc.is_dirty());
        doc.replace_range(0..0, "").unwrap();
        doc.replace_range(0..doc.text().len(), original).unwrap();
        assert!(doc.undo.is_empty(), "no-op edits must not consume history");
    }

    #[test]
    fn range_replacement_includes_bom_in_size_limit_and_keeps_inserted_cluster_cursor_safe() {
        let (_dir, _path, mut doc) = document(b"\xef\xbb\xbfa");
        let full = "x".repeat(MAX_BYTES - BOM.len());
        doc.replace_range(0..1, &full).unwrap();
        let end = doc.text().len();
        assert!(doc.replace_range(end..end, "x").is_err());
        assert_eq!(doc.save_request().bytes.len(), MAX_BYTES);
        doc.undo();
        assert_eq!(doc.text(), "a");
        assert!(!doc.is_dirty());
        doc.replace_range(1..1, "\u{301}").unwrap();
        assert_eq!(doc.text(), "a\u{301}");
        assert_eq!(doc.state.cursor, doc.text().len());
        doc.backspace();
        assert_eq!(doc.text(), "");
    }

    #[test]
    fn grapheme_and_crlf_edits_undo_without_corrupting_text() {
        let (_dir, _path, mut doc) = document("中e\u{301}👩‍💻\r\n末行".as_bytes());
        doc.move_cursor(Movement::End, false);
        assert_eq!(doc.cursor_position(), (0, 5));
        doc.backspace();
        assert_eq!(doc.text(), "中e\u{301}\r\n末行");
        doc.backspace();
        assert_eq!(doc.text(), "中\r\n末行");
        doc.delete();
        assert_eq!(doc.text(), "中末行");
        doc.undo();
        doc.undo();
        doc.undo();
        assert!(!doc.is_dirty());
        doc.redo();
        assert_eq!(doc.text(), "中e\u{301}\r\n末行");
        doc.insert("\n新").unwrap();
        assert_eq!(doc.text(), "中e\u{301}\r\n新\r\n末行");
        doc.set_cursor(2, 2, true);
        assert_eq!(doc.selected_text(), Some("\r\n末"));
    }

    #[test]
    fn vertical_motion_remembers_column_and_selection_uses_safe_boundaries() {
        let (_dir, _path, mut doc) = document("a\t中x\n短\na\t中x".as_bytes());
        doc.set_cursor(0, 6, false);
        doc.move_cursor(Movement::Down, false);
        assert_eq!(doc.cursor_position(), (1, 2));
        doc.move_cursor(Movement::Down, false);
        assert_eq!(doc.cursor_position(), (2, 6));
        doc.move_cursor(Movement::Left, true);
        assert_eq!(doc.selected_text(), Some("中"));
        doc.insert("文").unwrap();
        assert_eq!(doc.text(), "a\t中x\n短\na\t文x");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_bom_crlf_permissions_and_newer_edits() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path, _) = document(b"\xef\xbb\xbfkey=1\r\n");
        // Reload the permission baseline after setting the test mode.
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        let mut doc = Document::open(&path).unwrap();
        doc.move_cursor(Movement::End, false);
        doc.backspace();
        doc.insert("2").unwrap();
        let request = doc.save_request();
        doc.insert("3").unwrap();
        let position = doc.cursor_position();
        doc.accept_save(request.save().unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfkey=2\r\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(doc.cursor_position(), position);
        assert_eq!(doc.text(), "key=23\r\n");
        assert!(doc.is_dirty());
        doc.accept_save(doc.save_request().save().unwrap());
        assert!(!doc.is_dirty());
    }

    #[test]
    fn external_changes_are_not_overwritten() {
        let (_dir, path, mut doc) = document(b"original");
        doc.select_all();
        doc.insert("ours").unwrap();
        let request = doc.save_request();
        fs::write(&path, b"theirs").unwrap();
        assert!(request.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"theirs");
        assert_eq!(doc.text(), "ours");
        assert!(doc.is_dirty());
    }

    #[test]
    fn recovered_drafts_keep_original_baseline_and_detect_external_changes() {
        let (_dir, path, mut doc) = document(b"\xef\xbb\xbfkey=1\r\n");
        doc.move_cursor(Movement::End, false);
        doc.backspace();
        doc.insert("2").unwrap();
        let json = serde_json::to_vec(&doc.recovery_draft()).unwrap();
        let mut recovered = Document::recover(serde_json::from_slice(&json).unwrap()).unwrap();
        assert_eq!(recovered.text(), "key=2\r\n");
        assert_eq!(recovered.cursor_position(), doc.cursor_position());
        assert!(recovered.is_dirty());
        recovered.accept_save(recovered.save_request().save().unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfkey=2\r\n");
        assert!(!recovered.is_dirty());

        fs::write(&path, b"\xef\xbb\xbfkey=external\r\n").unwrap();
        let recovered = Document::recover(serde_json::from_slice(&json).unwrap()).unwrap();
        assert_eq!(recovered.text(), "key=2\r\n");
        assert!(recovered.save_request().save().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfkey=external\r\n");

        let mut invalid = doc.recovery_draft();
        invalid.bom = false;
        assert!(Document::recover(invalid).is_err());
        fs::remove_file(&path).unwrap();
        assert!(Document::recover(doc.recovery_draft()).is_err());
    }

    #[test]
    fn unfinished_save_recovery_keeps_undone_text_pending_review() {
        let (_dir, path, mut doc) = document(b"A");
        doc.select_all();
        doc.insert("B").unwrap();
        let request = doc.save_request();
        doc.undo();
        assert_eq!(doc.text(), "A");
        assert!(!doc.is_dirty());
        let mut draft = doc.recovery_draft();
        draft.needs_review = true;
        let pending = Document::recover(draft.clone()).unwrap();
        assert!(pending.is_dirty());
        assert!(pending.recovery_draft().needs_review);

        // A save still running during shutdown may finish after the draft was made.
        request.save().unwrap();
        let recovered = Document::recover(draft).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"B");
        assert_eq!(recovered.text(), "A");
        assert!(recovered.is_dirty());
        assert!(recovered.save_request().save().is_err());
        assert_eq!(recovered.text(), "A");
        assert_eq!(fs::read(&path).unwrap(), b"B");

        // Review is cleared only after a successful save against a matching baseline.
        let mut draft = Document::open(&path).unwrap().recovery_draft();
        draft.needs_review = true;
        let mut reviewed = Document::recover(draft).unwrap();
        assert!(reviewed.is_dirty());
        reviewed.accept_save(reviewed.save_request().save().unwrap());
        assert!(!reviewed.is_dirty());
        assert!(!reviewed.recovery_draft().needs_review);
    }

    #[test]
    fn oversized_edits_are_rejected_and_history_is_bounded() {
        let (_dir, _path, mut doc) = document(b"small");
        doc.select_all();
        assert!(doc.insert(&"a".repeat(MAX_BYTES + 1)).is_err());
        assert_eq!(doc.text(), "small");
        doc.insert(&"a".repeat(MAX_BYTES / 2)).unwrap();
        for _ in 0..100 {
            doc.insert("x").unwrap();
        }
        assert!(doc.undo.len() <= HISTORY_STEPS);
        assert!(doc.undo.iter().map(|s| s.text.len()).sum::<usize>() <= HISTORY_BYTES);
    }

    #[test]
    fn binary_invalid_utf8_and_large_files_are_not_editable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid");
        for bytes in [vec![0], vec![0xff], vec![b'x'; MAX_BYTES + 1]] {
            fs::write(&path, bytes).unwrap();
            assert!(Document::open(&path).is_err());
        }
        assert!(Document::open(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_edited_or_followed_during_save() {
        let (dir, path, mut doc) = document(b"original");
        let target = dir.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        doc.insert("edit").unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(Document::open(&path).is_err());
        assert!(doc.save_request().save().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }
}
