# LeafTerm User Guide

[← Back to the project](../README.md) · **English** | [简体中文](guide.zh-CN.md)

This guide covers the current development preview: controls, file protection, and resource limits. LeafTerm provides a local shell, a Git sidebar, and lightweight editing. Install and sign in to tools such as Codex CLI or Claude Code separately.

[Getting started](#getting-started) · [Shortcuts](#shortcuts) · [Files and Git](#sidebar) · [Search](#file-search) · [Editing](#editing) · [Syntax highlighting](#highlighting) · [Merge conflicts](#merge-conflicts) · [Directory completion](#directory-completion) · [Terminal colors](#terminal-colors) · [Session restore](#session-restore) · [Resource limits](#resource-limits) · [Development](#development)

<a id="getting-started"></a>

## Getting started

The current renderer uses **Metal on macOS**. Windows support is in development and is not yet available; there is no runnable Linux backend. Development primarily takes place on Apple Silicon Macs; compatibility with other machines still needs verification.

### Download and install

1. [Download LeafTerm v0.1.0 for macOS Apple Silicon](https://github.com/Arise-cn/LeafTerm/releases/download/v0.1.0/LeafTerm-0.1.0-macos-arm64.zip) and unzip it.
2. Move `LeafTerm.app` to **Applications** (`/Applications`).
3. Open the app. If macOS blocks it, follow the [first launch instructions](#macos-first-launch).

This is an Apple Silicon development preview without Apple Developer ID signing or Apple notarization. The app does not require a source checkout or Rust. The Git sidebar requires a working local Git installation; install AI CLI tools separately. See the [release notes](https://github.com/Arise-cn/LeafTerm/releases/tag/v0.1.0) for this download.

### Build and run from source

Source builds require a stable Rust toolchain, Xcode Command Line Tools, and Git.

From the repository directory, open a project:

```sh
cargo run --release -- --fresh /path/to/your/project
```

To open the LeafTerm repository itself:

```sh
cargo run --release -- --fresh .
```

Omit the directory to attempt to restore the last saved layout:

```sh
cargo run --release
```

`--fresh` skips layout restoration. Passing a directory also creates a new layout rooted at that directory. Run `cargo run --release -- --help` for startup options and the main shortcuts. If Rust is installed in its default location but is not on your `PATH`, replace `cargo` with `~/.cargo/bin/cargo`.

The current version is **0.1.0**. Use `cargo run --release -- --version` or `leafterm --version` to display the version compiled from Cargo package metadata.

### Build a macOS app bundle

```sh
./scripts/bundle.sh
open dist/LeafTerm.app
```

The script runs `cargo build --release --locked` and creates `dist/LeafTerm.app` inside the repository. It does not install to `/Applications`, upload, or publish the app. An existing bundle is preserved unless you explicitly pass `--force`:

```sh
./scripts/bundle.sh --force
```

If you have already built the release executable, skip compilation:

```sh
./scripts/bundle.sh --no-build
```

The bundle uses an ad-hoc signature for local development. It is not signed with Apple Developer ID or notarized by Apple. Replacing the bundle does not update running shells or AI processes; the next app launch uses the new version.

<a id="macos-first-launch"></a>

### First launch on macOS

If macOS says LeafTerm is “damaged” or that the developer cannot be verified on first launch, and you trust the app's source, move `LeafTerm.app` to `/Applications`, open the system **Terminal** app, and run:

```sh
sudo xattr -cr "/Applications/LeafTerm.app"
```

Enter your administrator password and press Return; password characters are not displayed as you type. When the command finishes, reopen LeafTerm. If the app is stored elsewhere, replace the quoted path with its actual location.

This command recursively clears the app's extended attributes, including quarantine. It does not repair actual file corruption; if the app still fails to open, download it again.

<a id="shortcuts"></a>

## Keyboard and mouse controls

`⌘` is Command, `⌥` is Option, and `⇧` is Shift.

| Action | Shortcut or control |
| --- | --- |
| New window | `⌘ N` (`⌘ T` also works) |
| Split side by side | `⌘ D` |
| Split top and bottom | `⌘ ⇧ D` |
| Move focus between panes | `⌘ ⌥ ←/↑` for previous; `⌘ ⌥ →/↓` for next |
| Maximize the active pane / restore | `⌘ ⇧ Enter` |
| Resize panes | Drag the divider |
| Close a file / the active pane | `⌘ W`; confirmation appears for unsaved edits or running sessions |
| Rename the window | `F2` |
| Show / hide the sidebar | `⌘ B`; the terminal fills the available width when hidden |
| Switch between Changes and Files | `⌘ E` |
| Search file names / paths | `⌘ P` or click the Files search field; select with `↑` / `↓`, open with `Enter`, clear and leave search with `Esc` |
| Refresh Git | `⌘ R` or the sidebar refresh button |
| Copy / paste | `⌘ C` / `⌘ V` |
| Editor: save / select all / cut | `⌘ S` / `⌘ A` / `⌘ X` |
| Editor: undo / redo | `⌘ Z` / `⌘ ⇧ Z` |
| Search terminal history | `⌘ F`, type a query, then press `Enter` |
| Previous / next search match | `⌘ G` / `⌘ ⇧ G`; cycles through retained history |
| Increase / decrease font size | `⌘ +` / `⌘ -` |
| Reset font size | `⌘ 0` |
| Close a file preview / diff / cancel a dialog | `Esc` |
| Scroll the editor / read-only preview / diff | Scroll wheel; read-only views also accept `↑` / `↓` |
| Scroll the editor / read-only preview / diff horizontally | `Shift` + scroll wheel; read-only views also accept `←` / `→` |
| Copy a file path | Right-click its sidebar entry |
| Quit | `⌘ Q`, then confirm |

Close confirmations provide clickable **Cancel** and **Close** buttons. If an editor contains unsaved changes, a save, discard, or cancel prompt appears first.

<a id="sidebar"></a>

## File tree and Git sidebar

The sidebar shows the current Git branch, including branches without a first commit, detached HEAD states, and Git worktrees. It can follow the active terminal's working directory or stay pinned to a project. Press `⌘ B` to show or hide it at any time. Changing directories does not reopen a hidden sidebar.

| View | Contents and actions |
| --- | --- |
| **Files** | Expand folders on demand, see Git change markers, toggle hidden files, search by name or path, click to edit, and right-click to copy a path. |
| **Changes** | Browse unstaged, staged, and untracked files. Click an ordinary change to view a read-only diff, or a file under **CONFLICTS** to resolve it. |

Press `⌘ E` to switch views and `⌘ R` to refresh Git. The Git sidebar reads the repository by default. **Mark resolved** in the conflict editor explicitly stages the current saved file. Commits, continuing a merge or rebase, resets, and branch changes remain terminal operations you control.

<a id="file-search"></a>

## File search

Type a file name or relative path at the top of **Files**, or press `⌘ P` to focus search. Search supports Chinese text and case-insensitive substring matching, including files inside folders you have not expanded.

Results show the file name, containing directory, and Git change marker. Click a result, or select it with `↑` / `↓` and press `Enter`, to open the editor. If the current file has unsaved changes, you will be prompted first. Click `×` to clear the filter, or press `Esc` to clear it and return keyboard focus to the workspace.

In Git projects, search includes tracked files and new files that are not ignored, respecting `.gitignore`. The hidden-file toggle also applies. Search matches names and paths, not file contents. It does not traverse `.git` or follow directory symlinks.

Search runs in the background with a 150 ms debounce and cancels outdated queries. It does not maintain a persistent project index. Results are limited to 200 entries; scanning is limited to 50,000 entries, approximately 4 MiB of path data, or 2 seconds. When a limit is reached, results are marked as partial. Enter a more specific path to narrow the search.

<a id="editing"></a>

## Lightweight file editing

Click a file in **Files** to edit it directly. Use the mouse to position the caret and drag a selection, arrow keys to move, and `Shift` to extend a selection. `⌘ S` saves, `⌘ Z` undoes, `⌘ ⇧ Z` redoes, and `Esc` returns to the terminal.

When there are unsaved changes, switching files, closing the editor or window, or quitting with `⌘ Q` prompts you to save, discard, or cancel. Diffs for ordinary changes in **Changes** remain read-only.

### File protection and limits

- Editing supports UTF-8 regular files up to **256 KiB**, preserving UTF-8 BOMs, CRLF line endings, and file permissions.
- Binary files, non-UTF-8 files, larger files, and symlinks are shown in a read-only view.
- Saving is atomic, using a temporary file in the same directory. If an AI tool or another program has changed the file on disk, saving refuses to overwrite it and preserves your current edits.
- Each editor retains up to **64 undo/redo steps, with a combined budget of approximately 4 MiB**.
- Unsaved contents are stored in private recovery drafts. Quit actions from the macOS Dock and system shutdown cannot be intercepted by a winit confirmation dialog; LeafTerm attempts to recover drafts on the next launch.

The editor does not introduce a language server, plugin host, or large IDE editor component. It is designed for configuration changes and small code files.

<a id="highlighting"></a>

## Syntax highlighting

The file editor, both sides of a conflict comparison, and the result area detect languages from file names and apply syntax colors. Built-in grammars include Rust, JavaScript, JSON, YAML, Python, Shell, Markdown, and HTML/CSS, with an additional lightweight TOML grammar.

TypeScript currently uses the JavaScript grammar; the status label reads `TypeScript · JS grammar`. Unknown formats and plain text keep their default color. Highlighting provides syntax colors, not type checking or semantic diagnostics.

### Computation and caching

Highlighting runs on a background thread started on demand. It debounces continuous input by 50 ms and cancels outdated results. Edits resume parsing from a line-state checkpoint before the change; moving the caret, scrolling, or repainting does not reparse the whole file. Each visible line shares one text layout buffer with colors applied to ranges, preserving the positions of Chinese text, combining characters, and tabs. Each side of a conflict is parsed in its own context. Undo and redo trigger updated highlighting.

Files remain subject to the 256 KiB editing limit. Highlighting stores at most 16K color spans and 128 line-state checkpoints. If a line exceeds 4 KiB, the span limit is reached, or the approximately 150 ms processing budget checked between lines is exhausted, the remaining content appears as plain text with a `Partial highlighting` label.

This budget excludes the initial grammar-table load and is not a hard timeout for a single regular expression. Closing the editor releases that file's cache and worker thread; loaded grammar tables remain shared within the process. Highlighting does not scan the project or start a language server.

<a id="merge-conflicts"></a>

## Resolve merge conflicts

Click a file under **Changes → CONFLICTS**, or open a conflicted file from the file tree or search, to enter the conflict editor. The upper comparison shows **CURRENT** and **INCOMING** side by side for the selected conflict, preserving the branch labels from Git's markers. Use the arrows to move between conflicts. During a rebase, interpret each side using Git's generated labels.

| Action | Result |
| --- | --- |
| **Accept Current** | Keep the left side. |
| **Accept Incoming** | Keep the right side. |
| **Accept Both** | Keep current content followed by incoming content; edit the result to reorder or remove duplicates. |
| `⌘ Z` / `⌘ ⇧ Z` | Undo / redo a choice or a manual edit. |

The lower result area supports selection, typing, and saving. The upper comparison and lower result scroll independently. Use `Shift` + scroll wheel to view long lines horizontally.

After resolving every conflict block:

1. Review the result and press `⌘ S` to save.
2. Click **Mark resolved** to stage the current file.
3. Continue reviewing other changes in the terminal, then complete the commit, merge, or rebase yourself.

**Mark resolved** does not commit or continue a merge automatically, and leaves other files unchanged. It checks the file contents on disk and the Git conflict entries again before staging.

Supported cases include ordinary text conflicts, add/add conflicts, and diff3/zdiff3 markers, within the 256 KiB UTF-8 editing limit. Incomplete markers prompt manual correction. Unresolved results cannot be saved; unsaved edits remain in recovery drafts. Delete/modify conflicts, binary conflicts, and custom short markers of fewer than 7 characters must be handled in the terminal.

<a id="directory-completion"></a>

## `cd` directory completion

In zsh, type `cd ` with a trailing space to show directories below the command. Continue typing a path to filter candidates.

| Key | Behavior |
| --- | --- |
| `Tab` | Enter the candidate menu; press again to cycle. |
| `Enter` | Confirm a candidate in the menu, then press again to run `cd`. With only one candidate, `Tab` completes it directly and one `Enter` runs the command. |
| `Ctrl G` | Cancel the candidate menu and continue editing the command. |

Completion uses the shell's line editor, lists only directories, and supports Chinese names and paths with spaces. It does not take over keystrokes while a program such as Codex CLI or Claude Code is running.

LeafTerm preserves your zsh startup configuration and loads completion in new shell sessions without rewriting `~/.zshrc`. Other shells retain their own completion behavior. Set `LEAFTERM_SHELL_INTEGRATION=0` before launching LeafTerm to disable its zsh integration.

<a id="terminal-colors"></a>

## AI CLI colors

The terminal displays ANSI 16-color, 256-color, and RGB truecolor output from programs. This is separate from syntax highlighting in the file editor. Whether ordinary prose is colored depends on the CLI's theme; colors are commonly used for code, commands, diffs, and status information.

New shells advertise `TERM=xterm-256color` and `COLORTERM=truecolor`. They also clear `NO_COLOR` inherited by the LeafTerm process, preventing build or automation environments from accidentally disabling color. Explicitly setting `NO_COLOR` in shell startup files or for an individual command still works.

Updating the app does not change the environment of existing shells or AI processes. To restore colors in an older session, exit the AI CLI normally, then run this in the shell:

```sh
unset NO_COLOR
codex
```

Alternatively, remove the variable for a single launch:

```sh
env -u NO_COLOR codex
# Or:
env -u NO_COLOR claude
```

<a id="session-restore"></a>

## Layout and draft recovery

The layout is stored at:

```text
~/Library/Application Support/LeafTerm/session.json
```

Restoration includes **windows, pane layouts, font sizes, and working directories**. Each tab saved by older versions is migrated to a separate window. Reopening starts new shells; it **does not restore previous Codex CLI / Claude Code processes or unfinished commands**. The quit confirmation states that running programs will stop.

Sidebar visibility is saved immediately when toggled and reused on the next launch, including when using `--fresh` or specifying a new directory. Hiding and showing it during the same run preserves the Files/Changes view and expanded folders.

Unsaved edits use recovery drafts in `recovery.json` alongside the layout file. For isolated testing, set an absolute path such as `LEAFTERM_SESSION_PATH=/path/to/test-state/session.json`; both layout and recovery drafts use that location, keeping test state separate from everyday sessions.

<a id="resource-limits"></a>

## Resource limits and compatibility

Low memory use is a design goal, not a verified overall performance guarantee. GPU rendering still uses memory: terminal parsing, text shaping, and file reading run on the CPU, while the GPU draws the interface. Apple Silicon CPUs and GPUs share unified memory.

| Resource | Current limit |
| --- | --- |
| Terminal panes per window | 32 |
| Scrollback per terminal | 3,000 lines |
| Single paste | Approximately 128 KiB |
| OSC control message | 16 KiB |
| Simultaneously expanded folders | 31 |
| Single-directory listing / visible tree preview | 4,096 entries |
| Git status output | 1 MiB / 8,192 changed entries |
| Individual diff / file preview | Approximately 256 KiB |
| Editable text | UTF-8 regular files up to 256 KiB |
| Undo / redo per editor | 64 steps, approximately 4 MiB combined |
| File search | 200 results; scan up to 50,000 entries, approximately 4 MiB of path data, or 2 seconds |

Use Git or file tools in the shell to inspect larger results. These limits control common sources of growth; they are not a hard memory cap for the whole process. Fonts, GPU drivers, terminal cells, and dependencies still allocate memory. Performance measurements should record LeafTerm, shell, and AI CLI processes separately and include sustained output, large repository refreshes, and repeatedly opening and closing panes.

The full Kitty keyboard protocol, terminal image protocols, and screen-reader semantics are not currently supported. Complete Codex CLI / Claude Code workflows, Chinese input-method composition, long-running stress tests, and other Macs still require further compatibility testing.

<a id="development"></a>

## Development and verification

```sh
cargo test --locked
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
```

Core components:

| Component | Responsibility |
| --- | --- |
| `winit` | Windows and input |
| `wgpu` | Interface rendering through Metal |
| `glyphon` | Text rendering |
| `alacritty_terminal` | Terminal control-sequence parsing |
| `portable-pty` | Local shells and child processes |
| `syntect` | File syntax highlighting |

The terminal core is pinned to official Alacritty commit [`54214706`](https://github.com/alacritty/alacritty/commit/542147067dd016fc5f49120ef0ba605a98a4e3a7), which includes limits on stored zero-width characters and fixes to the Kitty mode stack. This is a pinned development revision; rerun compatibility and resource tests when upgrading it.

The project currently has 88 automated tests and one manual release benchmark. Automated coverage includes real PTYs, input and window sizing, the macOS space key, color output and environment variables, directory completion, shell startup configuration, Git status and worktrees, file reading and saving, Chinese fonts, control-message limits, child-process cleanup, layout migration, search cancellation and resource limits, editing undo, conflict parsing and single-file staging, and syntax highlighting with Unicode color-range mapping.

Window testing has covered splits, multiple windows, directory following, file search, editing and draft recovery, conflict resolution and undo, and JSON/TOML/Rust highlighting. The development preview continues to evolve; these checks do not imply that every terminal application or machine has been validated.
