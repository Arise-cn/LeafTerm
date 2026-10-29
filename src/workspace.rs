//! Bounded, read-only workspace queries. Call these functions off the UI thread.

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
    let deadline = Instant::now() + GIT_TIMEOUT;
    let mut output = None;
    loop {
        if output.is_none() {
            match receiver.try_recv() {
                Ok(result) => output = Some(result.context("Cannot read Git output")?),
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("Git output reader stopped unexpectedly");
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
            });
        }
        if let Some(status) = child.try_wait()? {
            let bytes = match output {
                Some(bytes) => bytes,
                None => receiver
                    .recv_timeout(Duration::from_secs(1))
                    .context("Git output did not close")?
                    .context("Cannot read Git output")?,
            };
            let truncated = bytes.len() > limit;
            let mut bytes = bytes;
            bytes.truncate(limit);
            return Ok(GitOutput {
                bytes,
                success: status.success(),
                truncated,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Git query exceeded 8 seconds; use Refresh to try again");
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
