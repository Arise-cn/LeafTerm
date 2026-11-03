//! Explicit file actions. Run these on the workspace worker, never the UI thread.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};

#[derive(Clone, Debug)]
pub enum FileOperation {
    CreateFile { parent: PathBuf, name: String },
    CreateDirectory { parent: PathBuf, name: String },
    Rename { path: PathBuf, name: String },
    Trash { path: PathBuf },
    Reveal { path: PathBuf },
    Open { path: PathBuf },
}

impl FileOperation {
    /// Existing content whose path or existence changes during this operation.
    pub fn source(&self) -> Option<&Path> {
        match self {
            Self::Rename { path, .. } | Self::Trash { path } => Some(path),
            _ => None,
        }
    }

    pub fn affects(&self, path: &Path) -> bool {
        self.source().is_some_and(|source| path.starts_with(source))
    }

    pub fn mutates(&self) -> bool {
        !matches!(self, Self::Reveal { .. } | Self::Open { .. })
    }
}

/// Returns the new path after creation or renaming. No operation overwrites an
/// existing item, and a failed Trash action never falls back to permanent removal.
pub fn execute(operation: &FileOperation) -> Result<Option<PathBuf>> {
    match operation {
        FileOperation::CreateFile { parent, name } => {
            let path = child_path(parent, name)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .context("Cannot create file; check the name and folder permissions")?;
            Ok(Some(path))
        }
        FileOperation::CreateDirectory { parent, name } => {
            let path = child_path(parent, name)?;
            fs::create_dir(&path)
                .context("Cannot create folder; check the name and folder permissions")?;
            Ok(Some(path))
        }
        FileOperation::Rename { path, name } => {
            validate_existing(path)?;
            let destination =
                child_path(path.parent().context("Cannot rename a root folder")?, name)?;
            if path != &destination {
                rename_without_replacing(path, &destination)?;
            }
            Ok(Some(destination))
        }
        FileOperation::Trash { path } => {
            let metadata = validate_existing(path)?;
            move_to_trash(path, metadata.is_dir())?;
            Ok(None)
        }
        FileOperation::Reveal { path } | FileOperation::Open { path } => {
            validate_existing(path)?;
            open_system(path, matches!(operation, FileOperation::Reveal { .. }))?;
            Ok(None)
        }
    }
}

fn child_path(parent: &Path, name: &str) -> Result<PathBuf> {
    ensure!(parent.is_absolute(), "Choose an absolute folder path");
    validate_name(name)?;
    Ok(parent.join(name))
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && !name.contains('/')
            && !name.chars().any(char::is_control)
            && matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            )
            && Path::new(name).components().count() == 1,
        "Enter a file or folder name without slashes, control characters, '.' or '..'"
    );
    Ok(())
}

fn validate_existing(path: &Path) -> Result<fs::Metadata> {
    ensure!(
        path.is_absolute() && path.file_name().is_some(),
        "Choose a file or folder, not the filesystem root"
    );
    fs::symlink_metadata(path).context("The selected item is unavailable; refresh and try again")
}

#[cfg(target_os = "macos")]
fn rename_without_replacing(source: &Path, destination: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // RENAME_EXCL atomically refuses an existing destination. APFS also supports
    // a case-only rename of the same directory entry with this flag.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context(
            "Cannot rename item; the name may already exist or the folder may be read only",
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn rename_without_replacing(_source: &Path, _destination: &Path) -> Result<()> {
    bail!("Safe file renaming is not supported on this platform yet")
}

#[cfg(target_os = "macos")]
fn move_to_trash(path: &Path, is_directory: bool) -> Result<()> {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSFileManager, NSURL};

    autoreleasepool(|_| {
        // Preserve the entry itself: never canonicalize a symbolic link, and do
        // not mark a directory symlink as a directory URL (with a trailing '/').
        let url = NSURL::from_path(path, is_directory, None).context("Cannot encode file path")?;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, None)
            .map_err(|error| anyhow::anyhow!("Cannot move item to Trash: {error}"))
    })
}

#[cfg(not(target_os = "macos"))]
fn move_to_trash(_path: &Path, _is_directory: bool) -> Result<()> {
    bail!("Moving to Trash is not supported on this platform yet")
}

#[cfg(target_os = "macos")]
fn open_system(path: &Path, reveal: bool) -> Result<()> {
    let mut command = std::process::Command::new("/usr/bin/open");
    if reveal {
        command.arg("-R");
    }
    // validate_existing requires an absolute path, so a filename cannot be
    // interpreted as an option. No shell interpolation is involved.
    let output = command
        .arg(path)
        .output()
        .context("Cannot open system application")?;
    if !output.status.success() {
        bail!(
            "Cannot open item: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn open_system(_path: &Path, _reveal: bool) -> Result<()> {
    bail!("System file opening is not supported on this platform yet")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_preserves_existing_items_and_rejects_path_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().to_path_buf();
        let existing = parent.join("settings.json");
        fs::write(&existing, "keep this").unwrap();
        assert!(
            execute(&FileOperation::CreateFile {
                parent: parent.clone(),
                name: "settings.json".into(),
            })
            .is_err()
        );
        assert_eq!(fs::read_to_string(&existing).unwrap(), "keep this");
        for name in [
            "",
            " ",
            ".",
            "..",
            "../escape",
            "a/b",
            "./file",
            "/file",
            "line\nbreak",
            "nul\0byte",
        ] {
            assert!(child_path(&parent, name).is_err(), "accepted {name:?}");
        }
        let folder = execute(&FileOperation::CreateDirectory {
            parent: parent.clone(),
            name: "新文件夹".into(),
        })
        .unwrap()
        .unwrap();
        assert!(folder.is_dir());
        assert!(
            execute(&FileOperation::CreateDirectory {
                parent: parent.clone(),
                name: "新文件夹".into(),
            })
            .is_err()
        );
        let file = execute(&FileOperation::CreateFile {
            parent: folder,
            name: "file with spaces.txt".into(),
        })
        .unwrap()
        .unwrap();
        assert_eq!(fs::read(file).unwrap(), b"");
    }

    #[test]
    fn affected_paths_follow_components_not_string_prefixes() {
        let operation = FileOperation::Rename {
            path: "/project/src".into(),
            name: "source".into(),
        };
        assert!(operation.affects(Path::new("/project/src")));
        assert!(operation.affects(Path::new("/project/src/nested/file.rs")));
        assert!(!operation.affects(Path::new("/project/src-backup/file.rs")));
        assert!(operation.mutates());
        let operation = FileOperation::CreateFile {
            parent: "/project/src".into(),
            name: "file.rs".into(),
        };
        assert!(!operation.affects(Path::new("/project/src/file.rs")));
        assert!(operation.mutates());
        assert!(
            !FileOperation::Open {
                path: "/project/src".into()
            }
            .mutates()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rename_refuses_existing_targets_and_supports_case_only_changes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("file.txt");
        let existing = directory.path().join("keep.txt");
        fs::write(&source, "source").unwrap();
        fs::write(&existing, "keep").unwrap();
        assert!(
            execute(&FileOperation::Rename {
                path: source.clone(),
                name: "keep.txt".into()
            })
            .is_err()
        );
        assert_eq!(fs::read_to_string(&existing).unwrap(), "keep");
        assert_eq!(fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(
            execute(&FileOperation::Rename {
                path: source.clone(),
                name: "file.txt".into()
            })
            .unwrap(),
            Some(source.clone())
        );
        let renamed = execute(&FileOperation::Rename {
            path: source,
            name: "File.txt".into(),
        })
        .unwrap()
        .unwrap();
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "source");
        assert!(
            fs::read_dir(directory.path())
                .unwrap()
                .any(|item| item.unwrap().file_name() == "File.txt")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rename_and_creation_preserve_symbolic_link_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        fs::write(&target, "target").unwrap();
        let source = directory.path().join("link");
        symlink(&target, &source).unwrap();
        assert!(
            execute(&FileOperation::CreateFile {
                parent: directory.path().into(),
                name: "link".into()
            })
            .is_err()
        );
        let renamed = execute(&FileOperation::Rename {
            path: source.clone(),
            name: "renamed-link".into(),
        })
        .unwrap()
        .unwrap();
        assert!(!source.exists());
        assert!(
            fs::symlink_metadata(&renamed)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(renamed).unwrap(), target);
        assert_eq!(fs::read_to_string(&target).unwrap(), "target");
        let dangling = directory.path().join("dangling");
        symlink(directory.path().join("missing"), &dangling).unwrap();
        assert!(
            execute(&FileOperation::Rename {
                path: target.clone(),
                name: "dangling".into()
            })
            .is_err()
        );
        assert!(
            fs::symlink_metadata(dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "target");
    }
}
