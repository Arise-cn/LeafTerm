//! File-tree menu contents and logical-pixel geometry, independent of rendering.

use std::path::PathBuf;

use crate::{
    layout::Bounds,
    workspace::{DiffKind, FileChange},
};

const WIDTH: f32 = 240.0;
const ROW_HEIGHT: f32 = 28.0;
const PADDING: f32 = 6.0;
const SEPARATOR_HEIGHT: f32 = 7.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    CreateFile,
    CreateDirectory,
    Reveal,
    OpenExternal,
    OpenTerminal,
    CopyPath,
    CopyRelativePath,
    Rename,
    Trash,
    ViewChanges(DiffKind),
    Refresh,
    CollapseAll,
}

pub struct MenuItem {
    pub label: &'static str,
    pub command: MenuCommand,
    pub separator_before: bool,
    pub destructive: bool,
}

pub struct ContextMenu {
    pub target: PathBuf,
    pub root: PathBuf,
    pub is_dir: bool,
    pub is_root: bool,
    pub position: (f32, f32),
    pub entries: Vec<MenuItem>,
    pub selected: Option<usize>,
}

impl ContextMenu {
    pub fn new(
        target: PathBuf,
        root: PathBuf,
        is_dir: bool,
        is_root: bool,
        change: Option<&FileChange>,
        position: (f32, f32),
    ) -> Self {
        let is_root = is_root || target == root;
        let is_dir = is_dir || is_root;
        let mut entries = Vec::new();
        let mut add = |label, command, separator_before| {
            let destructive = command == MenuCommand::Trash;
            entries.push(MenuItem {
                label,
                command,
                separator_before,
                destructive,
            });
        };
        if is_dir {
            add("New File…", MenuCommand::CreateFile, false);
            add("New Folder…", MenuCommand::CreateDirectory, false);
        } else {
            add("Open with Default App", MenuCommand::OpenExternal, false);
        }
        add("Reveal in Finder", MenuCommand::Reveal, is_dir);
        add("Open in New Terminal", MenuCommand::OpenTerminal, false);
        if !is_dir && let Some(change) = change {
            if change.is_conflicted() {
                add(
                    "Resolve Conflict",
                    MenuCommand::ViewChanges(DiffKind::Unstaged),
                    true,
                );
            } else {
                if change.is_unstaged() {
                    add(
                        "View Changes",
                        MenuCommand::ViewChanges(DiffKind::Unstaged),
                        true,
                    );
                }
                if change.is_staged() {
                    add(
                        "View Staged Changes",
                        MenuCommand::ViewChanges(DiffKind::Staged),
                        !change.is_unstaged(),
                    );
                }
            }
        }
        add("Copy Path", MenuCommand::CopyPath, true);
        add("Copy Relative Path", MenuCommand::CopyRelativePath, false);
        if is_root {
            add("Refresh", MenuCommand::Refresh, true);
            add("Collapse All", MenuCommand::CollapseAll, false);
        } else {
            add("Rename…", MenuCommand::Rename, true);
            add("Move to Trash", MenuCommand::Trash, false);
        }
        Self {
            target,
            root,
            is_dir,
            is_root,
            position,
            entries,
            selected: None,
        }
    }

    pub fn bounds(&self, view_width: f32, view_height: f32) -> Bounds {
        let width = view_width.max(0.0);
        let height = view_height.max(0.0);
        let w = WIDTH.min(width);
        let h = (PADDING * 2.0
            + ROW_HEIGHT * self.entries.len() as f32
            + SEPARATOR_HEIGHT
                * self
                    .entries
                    .iter()
                    .filter(|item| item.separator_before)
                    .count() as f32)
            .min(height);
        Bounds {
            x: self.position.0.clamp(0.0, width - w),
            y: self.position.1.clamp(0.0, height - h),
            w,
            h,
        }
    }

    pub fn item_bounds(&self, index: usize, view_width: f32, view_height: f32) -> Bounds {
        let bounds = self.bounds(view_width, view_height);
        let y = bounds.y
            + PADDING
            + ROW_HEIGHT * index as f32
            + SEPARATOR_HEIGHT
                * self
                    .entries
                    .iter()
                    .take(index + 1)
                    .filter(|item| item.separator_before)
                    .count() as f32;
        let bottom = (bounds.y + bounds.h - PADDING).max(bounds.y);
        Bounds {
            x: bounds.x + PADDING.min(bounds.w / 2.0),
            y: y.min(bottom),
            w: (bounds.w - PADDING * 2.0).max(0.0),
            h: if index < self.entries.len() {
                ROW_HEIGHT.min((bottom - y).max(0.0))
            } else {
                0.0
            },
        }
    }

    pub fn item_at(&self, x: f32, y: f32, view_width: f32, view_height: f32) -> Option<usize> {
        (0..self.entries.len()).find(|&index| {
            self.item_bounds(index, view_width, view_height)
                .contains(x, y)
        })
    }

    pub fn move_selection(&mut self, reverse: bool) {
        let len = self.entries.len();
        self.selected = if len == 0 {
            None
        } else {
            Some(match self.selected.filter(|&index| index < len) {
                None if reverse => len - 1,
                None => 0,
                Some(index) if reverse => (index + len - 1) % len,
                Some(index) => (index + 1) % len,
            })
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_menu(change: Option<&FileChange>) -> ContextMenu {
        ContextMenu::new(
            "/project/file.rs".into(),
            "/project".into(),
            false,
            false,
            change,
            (700.0, 440.0),
        )
    }

    #[test]
    fn project_root_cannot_be_renamed_or_trashed() {
        // The target path also protects the root if a caller forgets its flag.
        let root = ContextMenu::new(
            "/project".into(),
            "/project".into(),
            false,
            false,
            None,
            (0.0, 0.0),
        );
        assert!(root.is_root && root.is_dir);
        for command in [MenuCommand::Rename, MenuCommand::Trash] {
            assert!(!root.entries.iter().any(|item| item.command == command));
        }
        for command in [
            MenuCommand::CreateFile,
            MenuCommand::CreateDirectory,
            MenuCommand::Refresh,
        ] {
            assert!(root.entries.iter().any(|item| item.command == command));
        }
    }

    #[test]
    fn git_entries_match_index_and_worktree_states() {
        let diff_kinds = |menu: ContextMenu| {
            menu.entries
                .into_iter()
                .filter_map(|item| match item.command {
                    MenuCommand::ViewChanges(kind) => Some(kind),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(diff_kinds(file_menu(None)).is_empty());
        for (index_status, worktree_status, expected) in [
            (' ', 'M', vec![DiffKind::Unstaged]),
            ('M', ' ', vec![DiffKind::Staged]),
            ('M', 'M', vec![DiffKind::Unstaged, DiffKind::Staged]),
            ('?', '?', vec![DiffKind::Unstaged]),
            ('U', 'U', vec![DiffKind::Unstaged]),
        ] {
            let change = FileChange {
                path: "file.rs".into(),
                original_path: None,
                index_status,
                worktree_status,
            };
            assert_eq!(diff_kinds(file_menu(Some(&change))), expected);
        }
    }

    #[test]
    fn menu_and_hit_targets_stay_inside_the_window() {
        let menu = file_menu(None);
        let bounds = menu.bounds(720.0, 450.0);
        assert!(bounds.x >= 0.0 && bounds.y >= 0.0);
        assert!(bounds.x + bounds.w <= 720.0 && bounds.y + bounds.h <= 450.0);
        for index in 0..menu.entries.len() {
            let item = menu.item_bounds(index, 720.0, 450.0);
            assert!(item.h == ROW_HEIGHT);
            assert!(item.y >= bounds.y && item.y + item.h <= bounds.y + bounds.h);
            assert_eq!(
                menu.item_at(item.x + 1.0, item.y + 1.0, 720.0, 450.0),
                Some(index)
            );
            if menu.entries[index].separator_before {
                assert_eq!(menu.item_at(item.x + 1.0, item.y - 1.0, 720.0, 450.0), None);
            }
        }
        assert_eq!(menu.item_at(bounds.x - 1.0, bounds.y, 720.0, 450.0), None);
        let tiny = menu.bounds(100.0, 80.0);
        assert_eq!(
            tiny,
            Bounds {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 80.0
            }
        );
    }

    #[test]
    fn keyboard_selection_wraps_and_supports_reverse_start() {
        let mut menu = file_menu(None);
        menu.move_selection(true);
        assert_eq!(menu.selected, Some(menu.entries.len() - 1));
        menu.move_selection(false);
        assert_eq!(menu.selected, Some(0));
        menu.move_selection(true);
        assert_eq!(menu.selected, Some(menu.entries.len() - 1));
        menu.selected = None;
        menu.move_selection(false);
        assert_eq!(menu.selected, Some(0));
    }
}
