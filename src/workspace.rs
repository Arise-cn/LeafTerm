//! Bounded workspace queries and explicitly requested conflict staging.
//! Call these functions off the UI thread.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const MAX_STATUS_BYTES: usize = 1024 * 1024;
const MAX_DIFF_BYTES: usize = 256 * 1024;
const MAX_CHANGES: usize = 8192;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const GIT_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_ENTRIES: usize = 50_000;
const MAX_SEARCH_BYTES: usize = 4 * 1024 * 1024;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub root: PathBuf,
    /// `None` for an ordinary directory; unborn branches still have a name.
    pub branch: Option<String>,
    pub changes: Vec<FileChange>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the repository root, preserving non-UTF-8 names on Unix.
    pub path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub index_status: char,
    pub worktree_status: char,
}

impl FileChange {
    pub fn is_untracked(&self) -> bool {
        self.index_status == '?' && self.worktree_status == '?'
    }

    pub fn is_staged(&self) -> bool {
        !matches!(self.index_status, ' ' | '?' | '!')
    }

    pub fn is_unstaged(&self) -> bool {
        self.is_untracked() || !matches!(self.worktree_status, ' ' | '!')
    }

    pub fn is_conflicted(&self) -> bool {
        self.index_status == 'U'
            || self.worktree_status == 'U'
            || matches!(
                (self.index_status, self.worktree_status),
                ('A', 'A') | ('D', 'D')
            )
    }

    pub fn badge(&self) -> char {
        if self.is_untracked() {
            '?'
        } else if self.is_conflicted() {
            'U'
        } else if self.worktree_status != ' ' {
            self.worktree_status
        } else {
            self.index_status
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryListing {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Unstaged,
    Staged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffContent {
    pub text: String,
    pub truncated: bool,
}

/// Discover a repository through Git, including worktrees with a `.git` file.
/// A directory outside Git is a valid workspace with no branch or changes.
pub fn load_workspace(cwd: &Path) -> Result<WorkspaceSnapshot> {
    let cwd = fs::canonicalize(cwd).context("Cannot open working directory")?;
    let cwd = if cwd.is_dir() {
        cwd
    } else {
        cwd.parent().context("No parent directory")?.to_path_buf()
    };
    let top = git(&cwd, ["rev-parse", "--show-toplevel"], 16 * 1024)?;
    if !top.success {
        return Ok(WorkspaceSnapshot {
            root: cwd,
            branch: None,
            changes: Vec::new(),
            truncated: false,
        });
    }
    let root = bytes_path(strip_line_end(&top.bytes));
    let branch = git(
        &root,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        16 * 1024,
    )?;
    let branch = if branch.success {
        String::from_utf8_lossy(strip_line_end(&branch.bytes)).into_owned()
    } else {
        let head = git(&root, ["rev-parse", "--short", "HEAD"], 1024)?;
        if head.success {
            format!(
                "detached @ {}",
                String::from_utf8_lossy(strip_line_end(&head.bytes))
            )
        } else {
            "unborn HEAD".to_owned()
        }
    };
    let status = git(
        &root,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        MAX_STATUS_BYTES,
    )?;
    if !status.success && !status.truncated {
        bail!("Git could not read the repository status");
    }
    let (mut changes, incomplete) = parse_porcelain(&status.bytes);
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(WorkspaceSnapshot {
        root,
        branch: Some(branch),
        changes,
        truncated: status.truncated || incomplete,
    })
}

/// Read a single level. Hidden folders and `.git` are never traversed implicitly.
pub fn read_directory(path: &Path, show_hidden: bool) -> Result<DirectoryListing> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in fs::read_dir(path).context("Cannot read directory")? {
        let item = item.context("Cannot read directory entry")?;
        let name = item.file_name();
        let display_name = name.to_string_lossy();
        if name == ".git" || (!show_hidden && display_name.starts_with('.')) {
            continue;
        }
        if entries.len() == MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let file_type = item.file_type()?;
        let is_symlink = file_type.is_symlink();
        let path = item.path();
        entries.push(FileEntry {
            is_dir: file_type.is_dir() || (is_symlink && path.is_dir()),
            is_symlink,
            path,
            name: display_name.into_owned(),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(DirectoryListing { entries, truncated })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSearchResult {
    /// File paths relative to the supplied search root.
    pub paths: Vec<PathBuf>,
    /// A result, scan, byte, time, or cancellation limit stopped the search.
    pub truncated: bool,
}

/// Search on demand without retaining a project-wide index. Symbolic links may
/// be returned as entries, but directories reached through them are never read.
pub fn search_files(
    root: &Path,
    query: &str,
    show_hidden: bool,
    cancelled: impl Fn() -> bool,
) -> Result<FileSearchResult> {
    let mut result = FileSearchResult {
        paths: Vec::new(),
        truncated: false,
    };
    if cancelled() {
        result.truncated = true;
        return Ok(result);
    }
    if query.is_empty() {
        return Ok(result);
    }
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    if !fs::symlink_metadata(root)
        .context("Cannot read search root")?
        .is_dir()
    {
        bail!("Search root must be a directory, not a symbolic link");
    }
    let query = query.to_lowercase();
    let output = git_until(
        root,
        ["ls-files", "-co", "--exclude-standard", "-z"],
        MAX_SEARCH_BYTES,
        deadline,
        &cancelled,
    );
    match output {
        Ok(output) if output.success || output.truncated => {
            result.truncated = output.truncated;
            for (scanned, record) in output.bytes.split_inclusive(|byte| *byte == 0).enumerate() {
                if cancelled() || Instant::now() >= deadline || scanned >= MAX_SEARCH_ENTRIES {
                    result.truncated = true;
                    break;
                }
                if record.last() != Some(&0) {
                    result.truncated = true;
                    break;
                }
                let relative = bytes_path(&record[..record.len() - 1]);
                if !search_visible(&relative, show_hidden)
                    || !relative.to_string_lossy().to_lowercase().contains(&query)
                    || !search_file_entry(root, &relative)
                {
                    continue;
                }
                if !result.paths.contains(&relative) {
                    if result.paths.len() == MAX_SEARCH_RESULTS {
                        result.truncated = true;
                        break;
                    }
                    result.paths.push(relative);
                }
            }
        }
        output => {
            // A real repository error must not silently bypass its ignore rules.
            let repository = root
                .ancestors()
                .any(|path| fs::symlink_metadata(path.join(".git")).is_ok());
            if repository {
                output?;
                bail!("Git could not list searchable files");
            }
            let mut pending = vec![PathBuf::new()];
            let mut scanned = 0usize;
            let mut bytes = 0usize;
            'directories: while let Some(directory) = pending.pop() {
                if cancelled() || Instant::now() >= deadline {
                    result.truncated = true;
                    break;
                }
                let full = root.join(&directory);
                if !fs::symlink_metadata(&full).is_ok_and(|metadata| metadata.is_dir()) {
                    continue;
                }
                let listing = match fs::read_dir(&full) {
                    Ok(listing) => listing,
                    Err(error) if directory.as_os_str().is_empty() => {
                        return Err(error).context("Cannot search directory");
                    }
                    Err(_) => {
                        result.truncated = true;
                        continue;
                    }
                };
                for entry in listing {
                    if cancelled() || Instant::now() >= deadline || scanned >= MAX_SEARCH_ENTRIES {
                        result.truncated = true;
                        break 'directories;
                    }
                    scanned += 1;
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => {
                            result.truncated = true;
                            continue;
                        }
                    };
                    let relative = directory.join(entry.file_name());
                    bytes = bytes.saturating_add(relative.as_os_str().as_encoded_bytes().len());
                    if bytes > MAX_SEARCH_BYTES {
                        result.truncated = true;
                        break 'directories;
                    }
                    if !search_visible(&relative, show_hidden) {
                        continue;
                    }
                    let kind = match entry.file_type() {
                        Ok(kind) => kind,
                        Err(_) => {
                            result.truncated = true;
                            continue;
                        }
                    };
                    if kind.is_dir() {
                        pending.push(relative);
                    } else if (kind.is_file() || kind.is_symlink())
                        && relative.to_string_lossy().to_lowercase().contains(&query)
                    {
                        if result.paths.len() == MAX_SEARCH_RESULTS {
                            result.truncated = true;
                            break 'directories;
                        }
                        result.paths.push(relative);
                    }
                }
            }
        }
    }
    result.paths.sort();
    Ok(result)
}

fn search_visible(relative: &Path, show_hidden: bool) -> bool {
    !relative.as_os_str().is_empty()
        && relative.components().all(|component| {
            let Component::Normal(name) = component else {
                return false;
            };
            let name = name.to_string_lossy();
            !name.eq_ignore_ascii_case(".git") && (show_hidden || !name.starts_with('.'))
        })
}

fn search_file_entry(root: &Path, relative: &Path) -> bool {
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        current.push(component.as_os_str());
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            return false;
        };
        if components.peek().is_some() {
            if !metadata.is_dir() {
                return false;
            }
        } else {
            return metadata.is_file() || metadata.file_type().is_symlink();
        }
    }
    false
}

/// Read a bounded, plain-text preview without opening an external application.
/// Call this off the UI thread, like the Git workspace queries.
pub fn load_file(path: &Path) -> Result<DiffContent> {
    let metadata = fs::symlink_metadata(path).context("Cannot read file")?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).context("Cannot read symbolic link")?;
        return Ok(DiffContent {
            text: format!("Symbolic link\n→ {}\n", target.display()),
            truncated: false,
        });
    }
    if !metadata.is_file() {
        return Ok(DiffContent {
            text: "This entry is not a regular file and cannot be previewed.".into(),
            truncated: false,
        });
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A path can change after symlink_metadata: never follow a replacement
        // link or block opening a FIFO, then check the opened handle as well.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("Cannot open file for preview")?;
    let metadata = file.metadata().context("Cannot inspect file for preview")?;
    if !metadata.is_file() {
        return Ok(DiffContent {
            text: "This entry is not a regular file and cannot be previewed.".into(),
            truncated: false,
        });
    }
    let mut bytes = Vec::new();
    file.take((MAX_DIFF_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("Cannot read file for preview")?;
    let truncated = bytes.len() > MAX_DIFF_BYTES;
    bytes.truncate(MAX_DIFF_BYTES);
    let binary = || DiffContent {
        text: format!(
            "Binary or non-UTF-8 file ({} bytes). Text preview is unavailable.",
            metadata.len()
        ),
        truncated: false,
    };
    if bytes.contains(&0) {
        return Ok(binary());
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        // The byte limit may fall inside the final character of UTF-8 text.
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])?
        }
        Err(_) => return Ok(binary()),
    };
    let mut text = if text.is_empty() {
        "This file is empty.".into()
    } else {
        text.to_owned()
    };
    if truncated {
        text.push_str("\n\n[File preview truncated at 256 KiB.]\n");
    }
    Ok(DiffContent { text, truncated })
}

/// Read only the requested diff. Git external diff and textconv are disabled.
/// Untracked files are previewed as additions without adding them to the index.
pub fn load_diff(root: &Path, change: &FileChange, kind: DiffKind) -> Result<DiffContent> {
    validate_relative_path(&change.path)?;
    if let Some(path) = &change.original_path {
        validate_relative_path(path)?;
    }
    if change.is_untracked() {
        if kind == DiffKind::Staged {
            return Ok(DiffContent {
                text: "This file has not been staged.".into(),
                truncated: false,
            });
        }
        return untracked_diff(root, &change.path);
    }
    let mut args = vec![
        OsString::from("diff"),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--no-color".into(),
        "--no-renames".into(),
        "--unified=3".into(),
        "--submodule=short".into(),
    ];
    if kind == DiffKind::Staged {
        args.push("--cached".into());
    }
    args.push("--".into());
    args.push(change.path.as_os_str().to_owned());
    if let Some(original) = &change.original_path {
        args.push(original.as_os_str().to_owned());
    }
    let output = git(root, args, MAX_DIFF_BYTES)?;
    if !output.success && !output.truncated {
        bail!("Git could not read this file's diff");
    }
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    if text.is_empty() {
        text = match kind {
            DiffKind::Staged => "No staged changes for this file.",
            DiffKind::Unstaged => "No unstaged changes for this file.",
        }
        .into();
    }
    if output.truncated {
        text.push_str("\n\n[Diff preview truncated at 256 KiB.]\n");
    }
    Ok(DiffContent {
        text,
        truncated: output.truncated,
    })
}

fn untracked_diff(root: &Path, relative: &Path) -> Result<DiffContent> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).context("Cannot read untracked file")?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&path)?;
        return Ok(DiffContent {
            text: format!(
                "New symbolic link: {}\n→ {}\n",
                relative.display(),
                target.display()
            ),
            truncated: false,
        });
    }
    if !metadata.is_file() {
        return Ok(DiffContent {
            text: "This entry is not a regular file.".into(),
            truncated: false,
        });
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_DIFF_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_DIFF_BYTES;
    bytes.truncate(MAX_DIFF_BYTES);
    if bytes.contains(&0) {
        return Ok(DiffContent {
            text: format!(
                "New binary file: {} ({} bytes)\n",
                relative.display(),
                metadata.len()
            ),
            truncated,
        });
    }
    // Prefixing millions of empty lines could otherwise multiply the preview size.
    let mut text = format!("Untracked file: {}\n\n", relative.display());
    let mut display_truncated = truncated;
    for line in String::from_utf8_lossy(&bytes).split_inclusive('\n') {
        if text.len().saturating_add(line.len()).saturating_add(1) > MAX_DIFF_BYTES {
            display_truncated = true;
            break;
        }
        text.push('+');
        text.push_str(line);
    }
    if display_truncated {
        text.push_str("\n\n[File preview truncated at 256 KiB.]\n");
    }
    Ok(DiffContent {
        text,
        truncated: display_truncated,
    })
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("Expected a repository-relative file path");
    }
    Ok(())
}

/// Mark one saved text conflict as resolved. This only stages the named file;
/// it never commits, continues a merge, or stages any other path.
pub fn stage_resolved(root: &Path, relative: &Path, expected: &[u8]) -> Result<()> {
    validate_relative_path(relative)?;
    if expected.len() > MAX_DIFF_BYTES {
        bail!("Only text conflicts up to 256 KiB can be marked resolved here");
    }
    let root_metadata = fs::symlink_metadata(root).context("Cannot inspect repository root")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!("The repository root must be a directory, not a symbolic link");
    }
    validate_text_conflict(root, relative)?;
    let text = std::str::from_utf8(expected)
        .context("Only UTF-8 text conflicts can be marked resolved here")?;
    if expected.contains(&0) {
        bail!("Binary conflicts must be resolved with Git");
    }
    match crate::merge::parse(text.strip_prefix('\u{feff}').unwrap_or(text)) {
        Ok(conflicts) if conflicts.is_empty() => {}
        Ok(_) => bail!("Conflict markers remain. Resolve every conflict and save the file first"),
        Err(error) => bail!("Incomplete conflict markers remain: {error}"),
    }
    check_resolved_file(root, relative, expected)?;

    let unmerged_args = [
        OsStr::new("ls-files"),
        OsStr::new("--unmerged"),
        OsStr::new("-z"),
        OsStr::new("--"),
        relative.as_os_str(),
    ];
    let unmerged = git(root, unmerged_args, MAX_STATUS_BYTES)?;
    if !unmerged.success || unmerged.truncated {
        bail!("Git could not verify this file's unresolved index entries");
    }
    if unmerged.bytes.is_empty() {
        bail!("This file no longer has an unresolved conflict in the Git index. Refresh Changes");
    }
    let mut stages: [Option<String>; 3] = [None, None, None];
    for record in unmerged.bytes.split_inclusive(|byte| *byte == 0) {
        let record = record
            .strip_suffix(&[0])
            .context("Git returned an incomplete conflict entry")?;
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("Git returned an invalid conflict entry")?;
        if bytes_path(&record[tab + 1..]) != relative {
            bail!("Git returned a conflict for a different path; nothing was staged");
        }
        let header = std::str::from_utf8(&record[..tab])
            .context("Git returned an invalid conflict header")?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() != 3 || !matches!(fields[0], "100644" | "100755") {
            bail!(
                "Only regular text file conflicts can be marked resolved here; use Git for this conflict"
            );
        }
        let stage = match fields[2] {
            "1" => 0,
            "2" => 1,
            "3" => 2,
            _ => bail!("Git returned an invalid conflict stage"),
        };
        let object = fields[1];
        if !matches!(object.len(), 40 | 64)
            || !object.bytes().all(|byte| byte.is_ascii_hexdigit())
            || stages[stage].replace(object.to_owned()).is_some()
        {
            bail!("Git returned invalid or duplicate conflict entries");
        }
    }
    if stages[1].is_none() || stages[2].is_none() {
        bail!(
            "Delete/modify and rename conflicts must be resolved with Git; both text sides are required"
        );
    }
    for object in stages.iter().flatten() {
        let blob = git(root, ["cat-file", "blob", object.as_str()], MAX_DIFF_BYTES)?;
        if !blob.success
            || blob.truncated
            || blob.bytes.contains(&0)
            || std::str::from_utf8(&blob.bytes).is_err()
        {
            bail!(
                "Binary or large conflicts must be resolved with Git; each side must be UTF-8 text up to 256 KiB"
            );
        }
    }
    // Git queries may take time. Recheck the index and on-disk content before
    // staging so a change during validation is not silently added.
    let current = git(root, unmerged_args, MAX_STATUS_BYTES)?;
    if !current.success || current.truncated || current.bytes != unmerged.bytes {
        bail!("The conflict changed in the Git index. Refresh Changes before marking it resolved");
    }
    check_resolved_file(root, relative, expected)?;
    let added = git(
        root,
        [OsStr::new("add"), OsStr::new("--"), relative.as_os_str()],
        16 * 1024,
    )?;
    if !added.success || added.truncated {
        bail!(
            "Git could not mark this file resolved. Check repository permissions or index locks, then refresh Changes"
        );
    }
    Ok(())
}

/// Check Git attributes before offering the text conflict editor or staging.
/// Short custom markers are deliberately unsupported by the line parser, and
/// Git's binary driver can leave marker-free UTF-8 files unresolved as well.
pub fn validate_text_conflict(root: &Path, relative: &Path) -> Result<()> {
    validate_relative_path(relative)?;
    let attributes = git(
        root,
        [
            OsStr::new("check-attr"),
            OsStr::new("-z"),
            OsStr::new("conflict-marker-size"),
            OsStr::new("merge"),
            OsStr::new("--"),
            relative.as_os_str(),
        ],
        16 * 1024,
    )?;
    if !attributes.success || attributes.truncated {
        bail!("Git could not verify this file's conflict attributes");
    }
    let fields: Vec<_> = attributes.bytes.split(|byte| *byte == 0).collect();
    if fields.len() != 7
        || !fields[6].is_empty()
        || bytes_path(fields[0]) != relative
        || bytes_path(fields[3]) != relative
        || fields[1] != b"conflict-marker-size"
        || fields[4] != b"merge"
    {
        bail!("Git returned invalid conflict attributes; nothing was staged");
    }
    if !matches!(fields[2], b"unspecified" | b"set" | b"unset") {
        let size = std::str::from_utf8(fields[2])
            .ok()
            .and_then(|value| value.parse::<u32>().ok());
        match size {
            Some(size) if size < 7 => {
                bail!(
                    "Custom conflict markers below 7 characters are unsupported here; resolve this file with Git"
                );
            }
            Some(size) if size <= i32::MAX as u32 => {}
            _ => bail!("Unsupported conflict-marker-size attribute; resolve this file with Git"),
        }
    }
    if matches!(fields[5], b"unset" | b"binary") {
        bail!("Git attributes mark this as a binary merge conflict; resolve this file with Git");
    }
    Ok(())
}

fn check_resolved_file(root: &Path, relative: &Path, expected: &[u8]) -> Result<()> {
    let mut path = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        path.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&path).context("Cannot inspect the resolved file path")?;
        if metadata.file_type().is_symlink() {
            bail!("Symbolic links cannot be marked resolved here; use Git for this conflict");
        }
        if components.peek().is_some() {
            if !metadata.is_dir() {
                bail!("The resolved file's parent is not a directory");
            }
        } else if !metadata.is_file() {
            bail!("Only existing regular text files can be marked resolved here");
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(&path)
        .context("Cannot read the saved resolution")?;
    if !file.metadata()?.is_file() {
        bail!("The resolved path is no longer a regular file");
    }
    let mut bytes = Vec::new();
    file.take((MAX_DIFF_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("Cannot read the saved resolution")?;
    if bytes != expected {
        bail!(
            "The file changed on disk or has unsaved edits. Reload or save it before marking resolved"
        );
    }
    Ok(())
}

fn strip_line_end(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

#[cfg(unix)]
fn bytes_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn bytes_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

fn parse_porcelain(bytes: &[u8]) -> (Vec<FileChange>, bool) {
    let mut changes = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if changes.len() == MAX_CHANGES {
            return (changes, true);
        }
        let Some(end) = bytes[offset..].iter().position(|byte| *byte == 0) else {
            return (changes, true);
        };
        let record = &bytes[offset..offset + end];
        offset += end + 1;
        if record.len() < 4 || record[2] != b' ' {
            return (changes, true);
        }
        let index_status = record[0] as char;
        let worktree_status = record[1] as char;
        let original_path =
            if matches!(index_status, 'R' | 'C') || matches!(worktree_status, 'R' | 'C') {
                let Some(end) = bytes[offset..].iter().position(|byte| *byte == 0) else {
                    return (changes, true);
                };
                let original = bytes_path(&bytes[offset..offset + end]);
                offset += end + 1;
                Some(original)
            } else {
                None
            };
        changes.push(FileChange {
            path: bytes_path(&record[3..]),
            original_path,
            index_status,
            worktree_status,
        });
    }
    (changes, false)
}

struct GitOutput {
    bytes: Vec<u8>,
    success: bool,
    truncated: bool,
    interrupted: bool,
}

/// `Child` itself does not terminate a process on drop. Keep error paths from
/// leaving a Git query running after its output reader has gone away.
struct GitChild(std::process::Child);

impl std::ops::Deref for GitChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for GitChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for GitChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Read a bounded pipe on a helper thread so a slow repository cannot trap the
/// caller forever. Exceeding the byte budget or timeout terminates this Git query.
fn git<I, S>(cwd: &Path, args: I, limit: usize) -> Result<GitOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_until(cwd, args, limit, Instant::now() + GIT_TIMEOUT, &|| false)?;
    if output.interrupted {
        bail!("Git query exceeded 8 seconds; use Refresh to try again");
    }
    Ok(output)
}

fn git_until<I, S>(
    cwd: &Path,
    args: I,
    limit: usize,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<GitOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if cancelled() || Instant::now() >= deadline {
        return Ok(GitOutput {
            bytes: Vec::new(),
            success: false,
            truncated: true,
            interrupted: true,
        });
    }
    let mut child = GitChild(
        Command::new("git")
            .arg("--no-optional-locks")
            .args([
                "-c",
                "color.ui=false",
                "-c",
                "core.quotepath=false",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Cannot start Git; install the Xcode command line tools")?,
    );
    let stdout = child.stdout.take().context("Git stdout unavailable")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let mut output: Option<Vec<u8>> = None;
    let mut status = None;
    loop {
        if cancelled() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let mut bytes = output.unwrap_or_default();
            bytes.truncate(limit);
            return Ok(GitOutput {
                bytes,
                success: false,
                truncated: true,
                interrupted: true,
            });
        }
        if output.is_none() {
            match receiver.try_recv() {
                Ok(result) => output = Some(result.context("Cannot read Git output")?),
                Err(mpsc::TryRecvError::Disconnected) => {
                    bail!("Git output reader stopped unexpectedly")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if output.as_ref().is_some_and(|bytes| bytes.len() > limit) {
            let _ = child.kill();
            let _ = child.wait();
            let mut bytes = output.unwrap();
            bytes.truncate(limit);
            return Ok(GitOutput {
                bytes,
                success: false,
                truncated: true,
                interrupted: false,
            });
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if let Some(status) = status
            && let Some(bytes) = output.take()
        {
            return Ok(GitOutput {
                bytes,
                success: status.success(),
                truncated: false,
                interrupted: false,
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "leafterm-git-test-{}-{stamp}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn run(&self, args: &[&str]) {
            let status = Command::new("git")
                .args([
                    "-c",
                    "user.name=LeafTerm Tests",
                    "-c",
                    "user.email=tests@leafterm.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&self.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        fn init(&self) {
            self.run(&["init", "-q", "-b", "main"]);
        }
        fn write(&self, name: &str, text: &str) {
            fs::write(self.0.join(name), text).unwrap();
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn conflicted_repository(
        path: &str,
        base: Option<&[u8]>,
        ours: &[u8],
        theirs: &[u8],
    ) -> TestDir {
        conflicted_repository_with_attributes(path, base, ours, theirs, "")
    }

    fn conflicted_repository_with_attributes(
        path: &str,
        base: Option<&[u8]>,
        ours: &[u8],
        theirs: &[u8],
        attributes: &str,
    ) -> TestDir {
        let dir = TestDir::new();
        dir.init();
        fs::create_dir_all(dir.0.join(path).parent().unwrap()).unwrap();
        dir.write("unrelated.txt", "unchanged\n");
        dir.write("other-conflict.txt", "base\n");
        if !attributes.is_empty() {
            dir.write(".gitattributes", attributes);
        }
        if let Some(base) = base {
            fs::write(dir.0.join(path), base).unwrap();
        }
        dir.run(&["add", "."]);
        dir.run(&["commit", "-qm", "base"]);
        dir.run(&["checkout", "-qb", "incoming"]);
        fs::write(dir.0.join(path), theirs).unwrap();
        dir.write("other-conflict.txt", "theirs\n");
        dir.run(&["add", "."]);
        dir.run(&["commit", "-qm", "incoming"]);
        dir.run(&["checkout", "-q", "main"]);
        fs::write(dir.0.join(path), ours).unwrap();
        dir.write("other-conflict.txt", "ours\n");
        dir.run(&["add", "."]);
        dir.run(&["commit", "-qm", "ours"]);
        let merge = git(&dir.0, ["merge", "--no-edit", "incoming"], MAX_STATUS_BYTES).unwrap();
        assert!(!merge.success, "fixture must create a real merge conflict");
        dir
    }

    fn assert_still_unmerged(dir: &TestDir, path: &str) {
        let unmerged = git(&dir.0, ["ls-files", "-u", "--", path], MAX_STATUS_BYTES).unwrap();
        assert!(unmerged.success && !unmerged.bytes.is_empty());
    }

    #[test]
    fn stage_resolved_only_stages_requested_literal_path() {
        let path = "config[1]*.txt";
        let dir = conflicted_repository(path, Some(b"base\n"), b"ours\n", b"theirs\n");
        dir.write("unrelated.txt", "keep this unstaged\n");
        dir.write("config1-other.txt", "also keep this untracked\n");
        let resolved = "enabled = true\n# 已解决\n".as_bytes();
        fs::write(dir.0.join(path), resolved).unwrap();
        stage_resolved(&dir.0, Path::new(path), resolved).unwrap();

        let stages = git(
            &dir.0,
            ["ls-files", "--stage", "-z", "--", path],
            MAX_STATUS_BYTES,
        )
        .unwrap();
        assert!(stages.success);
        assert_eq!(stages.bytes.iter().filter(|byte| **byte == 0).count(), 1);
        assert!(String::from_utf8_lossy(&stages.bytes).contains(" 0\tconfig[1]*.txt"));
        let stored = git(&dir.0, ["show", &format!(":{path}")], MAX_DIFF_BYTES).unwrap();
        assert!(stored.success);
        assert_eq!(stored.bytes, resolved);
        let unchanged = git(&dir.0, ["show", ":unrelated.txt"], MAX_DIFF_BYTES).unwrap();
        assert_eq!(unchanged.bytes, b"unchanged\n");
        let untracked = git(
            &dir.0,
            ["ls-files", "--", "config1-other.txt"],
            MAX_STATUS_BYTES,
        )
        .unwrap();
        assert!(untracked.bytes.is_empty());
        assert_still_unmerged(&dir, "other-conflict.txt");
        assert!(dir.0.join(".git/MERGE_HEAD").exists());
        assert!(
            stage_resolved(&dir.0, Path::new(path), resolved)
                .unwrap_err()
                .to_string()
                .contains("no longer")
        );
    }

    #[test]
    fn stage_resolved_accepts_regular_add_add_text_conflicts() {
        let path = "added.txt";
        let dir = conflicted_repository(path, None, b"ours\n", b"theirs\n");
        let resolved = b"ours\ntheirs\n";
        fs::write(dir.0.join(path), resolved).unwrap();
        stage_resolved(&dir.0, Path::new(path), resolved).unwrap();
        let unmerged = git(&dir.0, ["ls-files", "-u", "--", path], MAX_STATUS_BYTES).unwrap();
        assert!(unmerged.bytes.is_empty());
        assert_still_unmerged(&dir, "other-conflict.txt");
    }

    #[test]
    fn stage_resolved_rejects_short_custom_markers_and_binary_attributes() {
        let path = "conflict.txt";
        for attribute in ["conflict-marker-size=5", "binary", "merge=binary"] {
            let dir = conflicted_repository_with_attributes(
                path,
                Some(b"base\n"),
                b"ours\n",
                b"theirs\n",
                &format!("{path} {attribute}\n"),
            );
            let unresolved = fs::read(dir.0.join(path)).unwrap();
            if attribute == "conflict-marker-size=5" {
                assert!(unresolved.starts_with(b"<<<<< HEAD\n"));
            } else {
                assert_eq!(unresolved, b"ours\n");
            }
            // Both formats are valid Git conflicts without seven-character
            // markers, so the attribute guard must run before the text parser.
            assert!(
                crate::merge::parse(std::str::from_utf8(&unresolved).unwrap())
                    .unwrap()
                    .is_empty()
            );
            let expected_message = if attribute == "conflict-marker-size=5" {
                "below 7"
            } else {
                "binary merge conflict"
            };
            assert!(
                validate_text_conflict(&dir.0, Path::new(path))
                    .unwrap_err()
                    .to_string()
                    .contains(expected_message)
            );
            assert!(
                stage_resolved(&dir.0, Path::new(path), &unresolved)
                    .unwrap_err()
                    .to_string()
                    .contains(expected_message)
            );
            assert_still_unmerged(&dir, path);
        }
    }

    #[test]
    fn stage_resolved_accepts_long_custom_text_markers_after_resolution() {
        let path = "conflict.txt";
        let dir = conflicted_repository_with_attributes(
            path,
            Some(b"base\n"),
            b"ours\n",
            b"theirs\n",
            "conflict.txt conflict-marker-size=9 merge=text\n",
        );
        validate_text_conflict(&dir.0, Path::new(path)).unwrap();
        let unresolved = fs::read_to_string(dir.0.join(path)).unwrap();
        assert!(unresolved.starts_with("<<<<<<<<< HEAD\n"));
        assert_eq!(crate::merge::parse(&unresolved).unwrap().len(), 1);
        dir.write(path, "resolved\n");
        stage_resolved(&dir.0, Path::new(path), b"resolved\n").unwrap();
        let unmerged = git(&dir.0, ["ls-files", "-u", "--", path], MAX_STATUS_BYTES).unwrap();
        assert!(unmerged.bytes.is_empty());
    }

    #[test]
    fn stage_resolved_rejects_markers_external_changes_and_invalid_paths() {
        let path = "conflict.txt";
        let dir = conflicted_repository(path, Some(b"base\n"), b"ours\n", b"theirs\n");
        for text in [
            "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> incoming\n",
            "<<<<<<< HEAD\nours\n=======\ntheirs\n",
            "=======\ntrailing fragment\n",
            "\u{feff}<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> incoming\r\n",
        ] {
            dir.write(path, text);
            assert!(
                stage_resolved(&dir.0, Path::new(path), text.as_bytes()).is_err(),
                "{text:?}"
            );
            assert_still_unmerged(&dir, path);
        }
        dir.write(path, "external edit\n");
        assert!(
            stage_resolved(&dir.0, Path::new(path), b"saved editor contents\n")
                .unwrap_err()
                .to_string()
                .contains("changed on disk")
        );
        for relative in [
            "",
            "..",
            "../conflict.txt",
            "./conflict.txt",
            "nested/../../conflict.txt",
            "/conflict.txt",
        ] {
            assert!(
                stage_resolved(&dir.0, Path::new(relative), b"external edit\n").is_err(),
                "{relative}"
            );
        }
        dir.write(path, "\0binary");
        assert!(stage_resolved(&dir.0, Path::new(path), b"\0binary").is_err());
        fs::write(dir.0.join(path), [0xff]).unwrap();
        assert!(stage_resolved(&dir.0, Path::new(path), &[0xff]).is_err());
        let large = vec![b'a'; MAX_DIFF_BYTES + 1];
        fs::write(dir.0.join(path), &large).unwrap();
        assert!(stage_resolved(&dir.0, Path::new(path), &large).is_err());
        assert_still_unmerged(&dir, path);
    }

    #[test]
    fn stage_resolved_rejects_binary_index_conflicts_even_after_text_replacement() {
        let path = "binary.dat";
        let dir = conflicted_repository(path, Some(b"base\0"), b"ours\0", b"theirs\0");
        dir.write(path, "replacement text\n");
        assert!(
            stage_resolved(&dir.0, Path::new(path), b"replacement text\n")
                .unwrap_err()
                .to_string()
                .contains("Binary or large")
        );
        assert_still_unmerged(&dir, path);
    }

    #[test]
    fn stage_resolved_rejects_delete_modify_conflicts() {
        let path = "deleted.txt";
        let dir = conflicted_repository(path, Some(b"base\n"), b"ours\n", b"theirs\n");
        dir.run(&["merge", "--abort"]);
        dir.run(&["rm", "--", path]);
        dir.run(&["commit", "-qm", "delete on current side"]);
        let merge = git(&dir.0, ["merge", "--no-edit", "incoming"], MAX_STATUS_BYTES).unwrap();
        assert!(!merge.success);
        let present_side = fs::read(dir.0.join(path)).unwrap();
        assert!(
            stage_resolved(&dir.0, Path::new(path), &present_side)
                .unwrap_err()
                .to_string()
                .contains("both text sides")
        );
        assert_still_unmerged(&dir, path);
    }

    #[cfg(unix)]
    #[test]
    fn stage_resolved_rejects_file_and_parent_symlinks() {
        use std::os::unix::fs::symlink;
        let path = "nested/conflict.txt";
        let dir = conflicted_repository(path, Some(b"base\n"), b"ours\n", b"theirs\n");
        dir.write("resolution.txt", "resolved\n");
        fs::remove_file(dir.0.join(path)).unwrap();
        symlink(dir.0.join("resolution.txt"), dir.0.join(path)).unwrap();
        assert!(
            stage_resolved(&dir.0, Path::new(path), b"resolved\n")
                .unwrap_err()
                .to_string()
                .contains("Symbolic links")
        );
        fs::remove_file(dir.0.join(path)).unwrap();
        dir.write(path, "resolved\n");
        fs::rename(dir.0.join("nested"), dir.0.join("renamed")).unwrap();
        symlink(dir.0.join("renamed"), dir.0.join("nested")).unwrap();
        assert!(
            stage_resolved(&dir.0, Path::new(path), b"resolved\n")
                .unwrap_err()
                .to_string()
                .contains("Symbolic links")
        );
        assert_still_unmerged(&dir, path);
    }

    #[test]
    fn file_search_finds_deep_paths_case_insensitively_and_chinese() {
        for repository in [false, true] {
            let dir = TestDir::new();
            if repository {
                dir.init();
            }
            fs::create_dir_all(dir.0.join("src/unopened/deep")).unwrap();
            dir.write("src/unopened/deep/CamelCase.rs", "");
            dir.write("src/unopened/deep/中文文件.rs", "");
            dir.write("unrelated.txt", "");

            let result = search_files(&dir.0, "DEEP/cAMEL", false, || false).unwrap();
            assert_eq!(
                result.paths,
                [PathBuf::from("src/unopened/deep/CamelCase.rs")]
            );
            assert!(!result.truncated);
            let chinese = search_files(&dir.0, "中文", false, || false).unwrap();
            assert_eq!(
                chinese.paths,
                [PathBuf::from("src/unopened/deep/中文文件.rs")]
            );
            assert!(!chinese.truncated);
            // A repository subdirectory remains the supplied relative root.
            let nested = search_files(&dir.0.join("src"), "camel", false, || false).unwrap();
            assert_eq!(nested.paths, [PathBuf::from("unopened/deep/CamelCase.rs")]);
        }
    }

    #[test]
    fn file_search_obeys_hidden_setting_and_gitignore() {
        for repository in [false, true] {
            let dir = TestDir::new();
            if repository {
                dir.init();
            }
            fs::create_dir_all(dir.0.join(".hidden/nested")).unwrap();
            fs::create_dir_all(dir.0.join("ignored")).unwrap();
            dir.write(".gitignore", "ignored/\n*.cache\n");
            dir.write("visible-needle.rs", "");
            dir.write("tracked-needle.rs", "");
            dir.write(".hidden/nested/needle.rs", "");
            dir.write(".needle.rs", "");
            dir.write("ignored/needle.rs", "");
            dir.write("needle.cache", "");
            if repository {
                dir.run(&["add", "tracked-needle.rs"]);
                dir.write(".git/needle", "must not be searched");
            }

            let visible = search_files(&dir.0, "needle", false, || false).unwrap();
            assert!(!visible.truncated);
            assert!(visible.paths.contains(&PathBuf::from("visible-needle.rs")));
            assert!(visible.paths.contains(&PathBuf::from("tracked-needle.rs")));
            assert!(
                !visible
                    .paths
                    .iter()
                    .any(|path| path.to_string_lossy().starts_with('.'))
            );
            assert_eq!(visible.paths.len(), if repository { 2 } else { 4 });

            let hidden = search_files(&dir.0, "needle", true, || false).unwrap();
            assert!(!hidden.truncated);
            assert!(
                hidden
                    .paths
                    .contains(&PathBuf::from(".hidden/nested/needle.rs"))
            );
            assert!(hidden.paths.contains(&PathBuf::from(".needle.rs")));
            assert!(!hidden.paths.contains(&PathBuf::from(".git/needle")));
            assert_eq!(hidden.paths.len(), if repository { 4 } else { 6 });
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_search_never_descends_directory_symlinks() {
        use std::os::unix::fs::symlink;
        let external = TestDir::new();
        external.write("outside-needle.rs", "");
        for repository in [false, true] {
            let dir = TestDir::new();
            if repository {
                dir.init();
                fs::create_dir(dir.0.join("replaced")).unwrap();
                dir.write("replaced/outside-needle.rs", "");
                dir.run(&["add", "replaced/outside-needle.rs"]);
                fs::remove_dir_all(dir.0.join("replaced")).unwrap();
                symlink(&external.0, dir.0.join("replaced")).unwrap();
            }
            dir.write("inside-needle.rs", "");
            symlink(&external.0, dir.0.join("linked")).unwrap();
            symlink(&dir.0, dir.0.join("cycle")).unwrap();
            let result = search_files(&dir.0, "needle", true, || false).unwrap();
            assert_eq!(result.paths, [PathBuf::from("inside-needle.rs")]);
            assert!(!result.truncated);
        }
    }

    #[test]
    fn file_search_caps_results_and_marks_only_incomplete_results() {
        for repository in [false, true] {
            let dir = TestDir::new();
            if repository {
                dir.init();
            }
            for index in 0..MAX_SEARCH_RESULTS {
                dir.write(&format!("needle-{index:03}.rs"), "");
            }
            let exact = search_files(&dir.0, "needle", false, || false).unwrap();
            assert_eq!(exact.paths.len(), MAX_SEARCH_RESULTS);
            assert!(!exact.truncated);
            dir.write("needle-extra.rs", "");
            let capped = search_files(&dir.0, "needle", false, || false).unwrap();
            assert_eq!(capped.paths.len(), MAX_SEARCH_RESULTS);
            assert!(capped.truncated);
        }
    }

    #[test]
    fn file_search_cancels_before_start_and_during_a_query() {
        use std::cell::Cell;
        let dir = TestDir::new();
        let cancelled = search_files(&dir.0.join("missing"), "needle", false, || true).unwrap();
        assert!(cancelled.paths.is_empty() && cancelled.truncated);
        let empty = search_files(&dir.0, "", false, || false).unwrap();
        assert!(empty.paths.is_empty() && !empty.truncated);
        for repository in [false, true] {
            if repository {
                dir.init();
            }
            for index in 0..100 {
                dir.write(&format!("needle-{index:03}.rs"), "");
            }
            let calls = Cell::new(0);
            let partial = search_files(&dir.0, "needle", false, || {
                calls.set(calls.get() + 1);
                calls.get() >= 25
            })
            .unwrap();
            assert!(partial.truncated);
            assert!(partial.paths.len() < 100);
            assert_eq!(calls.get(), 25);
        }
    }

    #[test]
    fn file_search_git_reader_stops_at_byte_and_time_budgets() {
        let dir = TestDir::new();
        dir.init();
        dir.write("one.rs", "");
        dir.write("two.rs", "");
        let args = ["ls-files", "-co", "--exclude-standard", "-z"];
        let capped =
            git_until(&dir.0, args, 3, Instant::now() + SEARCH_TIMEOUT, &|| false).unwrap();
        assert_eq!(capped.bytes.len(), 3);
        assert!(capped.truncated && !capped.interrupted);
        let timed_out = git_until(&dir.0, args, 100, Instant::now(), &|| false).unwrap();
        assert!(timed_out.bytes.is_empty());
        assert!(timed_out.truncated && timed_out.interrupted);
    }

    #[test]
    fn nul_status_handles_rename_unicode_newlines_and_truncation() {
        let (changes, truncated) = parse_porcelain(
            "MM 中文 name.txt\0R  new\nname.rs\0old name.rs\0?? file\tname\0".as_bytes(),
        );
        assert!(!truncated);
        assert_eq!(changes.len(), 3);
        assert!(changes[0].is_staged() && changes[0].is_unstaged());
        assert_eq!(changes[1].path, Path::new("new\nname.rs"));
        assert_eq!(
            changes[1].original_path.as_deref(),
            Some(Path::new("old name.rs"))
        );
        assert!(changes[2].is_untracked());
        let (partial, truncated) = parse_porcelain(b" M good\0R  dest\0incomplete");
        assert!(truncated);
        assert_eq!(partial.len(), 1);
        let (conflicts, _) = parse_porcelain(b"AA both-added\0DD both-deleted\0UU both-modified\0");
        assert!(
            conflicts
                .iter()
                .all(|change| change.is_conflicted() && change.badge() == 'U')
        );
    }

    #[test]
    fn normal_directory_and_unborn_branch_are_valid() {
        let dir = TestDir::new();
        let normal = load_workspace(&dir.0).unwrap();
        assert!(normal.branch.is_none());
        dir.init();
        let unborn = load_workspace(&dir.0).unwrap();
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        assert!(unborn.changes.is_empty());
    }

    #[test]
    fn real_git_distinguishes_staged_unstaged_rename_deleted_and_untracked() {
        let dir = TestDir::new();
        dir.init();
        dir.write("tracked.txt", "original\n");
        dir.write("rename source.txt", "rename me\n");
        dir.write("deleted.txt", "delete me\n");
        dir.run(&["add", "."]);
        dir.run(&["commit", "-qm", "initial"]);
        dir.write("tracked.txt", "staged\n");
        dir.run(&["add", "tracked.txt"]);
        dir.write("tracked.txt", "working\n");
        dir.run(&["mv", "rename source.txt", "中文 renamed.txt"]);
        fs::remove_file(dir.0.join("deleted.txt")).unwrap();
        dir.write("新 文件.txt", "hello new file\n");
        let state = load_workspace(&dir.0).unwrap();
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert!(!state.truncated);
        let tracked = state
            .changes
            .iter()
            .find(|c| c.path == Path::new("tracked.txt"))
            .unwrap();
        assert!(tracked.is_staged() && tracked.is_unstaged());
        assert!(
            load_diff(&state.root, tracked, DiffKind::Staged)
                .unwrap()
                .text
                .contains("+staged")
        );
        assert!(
            load_diff(&state.root, tracked, DiffKind::Unstaged)
                .unwrap()
                .text
                .contains("+working")
        );
        let renamed = state
            .changes
            .iter()
            .find(|c| c.path == Path::new("中文 renamed.txt"))
            .unwrap();
        assert_eq!(
            renamed.original_path.as_deref(),
            Some(Path::new("rename source.txt"))
        );
        assert_eq!(renamed.index_status, 'R');
        assert_eq!(
            state
                .changes
                .iter()
                .find(|c| c.path == Path::new("deleted.txt"))
                .unwrap()
                .badge(),
            'D'
        );
        let untracked = state
            .changes
            .iter()
            .find(|c| c.path == Path::new("新 文件.txt"))
            .unwrap();
        assert!(untracked.is_untracked());
        assert!(
            load_diff(&state.root, untracked, DiffKind::Unstaged)
                .unwrap()
                .text
                .contains("+hello new file")
        );
    }

    #[test]
    fn discovers_worktree_and_detached_head_from_nested_directory() {
        let dir = TestDir::new();
        dir.init();
        dir.write("tracked.txt", "initial\n");
        dir.run(&["add", "."]);
        dir.run(&["commit", "-qm", "initial"]);
        let worktree = dir.0.join("other-worktree");
        dir.run(&["worktree", "add", "--detach", worktree.to_str().unwrap()]);
        assert!(worktree.join(".git").is_file());
        let nested = worktree.join("nested");
        fs::create_dir(&nested).unwrap();
        let state = load_workspace(&nested).unwrap();
        assert_eq!(
            fs::canonicalize(state.root).unwrap(),
            fs::canonicalize(worktree).unwrap()
        );
        assert!(state.branch.unwrap().starts_with("detached @ "));
    }

    #[test]
    fn literal_pathspec_does_not_include_other_files() {
        let dir = TestDir::new();
        dir.init();
        dir.write("[a].txt", "first\n");
        dir.write("a.txt", "second\n");
        dir.run(&["add", "."]);
        dir.run(&["commit", "-qm", "initial"]);
        dir.write("[a].txt", "literal only\n");
        dir.write("a.txt", "must not appear\n");
        let state = load_workspace(&dir.0).unwrap();
        let literal = state
            .changes
            .iter()
            .find(|c| c.path == Path::new("[a].txt"))
            .unwrap();
        let diff = load_diff(&state.root, literal, DiffKind::Unstaged).unwrap();
        assert!(diff.text.contains("+literal only"));
        assert!(!diff.text.contains("must not appear"));
    }

    #[test]
    fn directory_listing_is_single_level_and_hidden_files_are_optional() {
        let dir = TestDir::new();
        fs::create_dir(dir.0.join("folder")).unwrap();
        fs::write(dir.0.join("folder/deep.txt"), "hello").unwrap();
        fs::create_dir(dir.0.join(".git")).unwrap();
        dir.write("a.txt", "hello");
        dir.write(".hidden", "hello");
        let listing = read_directory(&dir.0, false).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(listing.entries[0].is_dir);
        assert_eq!(listing.entries[0].name, "folder");
        assert_eq!(read_directory(&dir.0, true).unwrap().entries.len(), 3);
    }

    #[test]
    fn file_preview_preserves_text_with_unicode_paths_and_handles_empty_files() {
        let dir = TestDir::new();
        let path = dir.0.join("中文 file.txt");
        let text = "普通文本\n+this is not a diff\n-keep this prefix\n\tindent\n";
        fs::write(&path, text).unwrap();
        let preview = load_file(&path).unwrap();
        assert_eq!(preview.text, text);
        assert!(!preview.truncated);
        fs::write(&path, "").unwrap();
        let empty = load_file(&path).unwrap();
        assert_eq!(empty.text, "This file is empty.");
        assert!(!empty.truncated);
    }

    #[test]
    fn file_preview_is_bounded_and_preserves_utf8_at_the_byte_limit() {
        let dir = TestDir::new();
        let path = dir.0.join("large.txt");
        let exact = "a".repeat(MAX_DIFF_BYTES);
        fs::write(&path, &exact).unwrap();
        let preview = load_file(&path).unwrap();
        assert_eq!(preview.text, exact);
        assert!(!preview.truncated);

        let unicode = "中".repeat(MAX_DIFF_BYTES);
        fs::write(&path, unicode).unwrap();
        let preview = load_file(&path).unwrap();
        assert!(preview.truncated);
        assert!(preview.text.len() < MAX_DIFF_BYTES + 100);
        assert_eq!(preview.text.matches('中').count(), MAX_DIFF_BYTES / 3);
        assert!(!preview.text.contains('\u{fffd}'));
        assert!(preview.text.contains("truncated at 256 KiB"));
    }

    #[test]
    fn file_preview_reports_binary_and_non_utf8_contents() {
        let dir = TestDir::new();
        let path = dir.0.join("data.bin");
        for bytes in [b"private\0contents".as_slice(), b"private\xffcontents"] {
            fs::write(&path, bytes).unwrap();
            let preview = load_file(&path).unwrap();
            assert!(preview.text.contains("Binary or non-UTF-8 file"));
            assert!(!preview.text.contains("private"));
            assert!(!preview.truncated);
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_preview_shows_symlink_targets_without_reading_them() {
        let dir = TestDir::new();
        dir.write("target.txt", "target contents must not be loaded");
        let path = dir.0.join("link.txt");
        std::os::unix::fs::symlink("target.txt", &path).unwrap();
        let preview = load_file(&path).unwrap();
        assert!(preview.text.contains("→ target.txt"));
        assert!(!preview.text.contains("target contents"));
        fs::remove_file(dir.0.join("target.txt")).unwrap();
        assert_eq!(load_file(&path).unwrap(), preview);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn file_preview_rejects_directories_and_fifos_without_blocking() {
        let dir = TestDir::new();
        assert!(
            load_file(&dir.0)
                .unwrap()
                .text
                .contains("not a regular file")
        );
        let path = dir.0.join("pipe");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c_path is a live, nul-terminated path and mode is a valid bitmask.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(load_file(&path));
        });
        let preview = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("preview must not wait for a FIFO writer")
            .unwrap();
        assert!(preview.text.contains("not a regular file"));
    }

    #[test]
    fn huge_untracked_preview_is_bounded_and_binary_is_not_rendered() {
        let dir = TestDir::new();
        let change = FileChange {
            path: "large.txt".into(),
            original_path: None,
            index_status: '?',
            worktree_status: '?',
        };
        fs::write(dir.0.join("large.txt"), vec![b'\n'; MAX_DIFF_BYTES * 2]).unwrap();
        let diff = load_diff(&dir.0, &change, DiffKind::Unstaged).unwrap();
        assert!(diff.truncated);
        assert!(diff.text.len() < MAX_DIFF_BYTES + 200);
        fs::write(dir.0.join("large.txt"), b"binary\0data").unwrap();
        assert!(
            load_diff(&dir.0, &change, DiffKind::Unstaged)
                .unwrap()
                .text
                .contains("binary file")
        );
    }

    #[cfg(unix)]
    #[test]
    fn untracked_symlink_preview_does_not_read_target_contents() {
        let dir = TestDir::new();
        dir.write("target.txt", "target contents must not be loaded");
        std::os::unix::fs::symlink("target.txt", dir.0.join("link.txt")).unwrap();
        let change = FileChange {
            path: "link.txt".into(),
            original_path: None,
            index_status: '?',
            worktree_status: '?',
        };
        let diff = load_diff(&dir.0, &change, DiffKind::Unstaged).unwrap();
        assert!(diff.text.contains("target.txt"));
        assert!(!diff.text.contains("target contents"));
    }

    #[test]
    fn tracked_diff_is_capped_before_it_can_fill_memory() {
        let dir = TestDir::new();
        dir.init();
        dir.write("large.txt", "small\n");
        dir.run(&["add", "."]);
        dir.run(&["commit", "-qm", "initial"]);
        fs::write(dir.0.join("large.txt"), vec![b'x'; MAX_DIFF_BYTES * 2]).unwrap();
        let state = load_workspace(&dir.0).unwrap();
        let diff = load_diff(&state.root, &state.changes[0], DiffKind::Unstaged).unwrap();
        assert!(diff.truncated);
        assert!(diff.text.len() < MAX_DIFF_BYTES + 200);
    }
}
