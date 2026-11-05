# Changelog / 更新记录

## v0.1.4 — 2026-11-05

### 右键菜单渲染修复

- 修复右键菜单弹出后，菜单外的文件名、图标和预览文字也被清空的问题。
- 仅裁剪菜单实际遮挡的文字区域，保留周围内容和原有文字布局。

### Context-menu rendering fix

- Fix file names, icons, and preview text disappearing outside an open context menu.
- Clip only the text covered by the menu, preserving surrounding content and the original text layout.

### 验证与下载 / Validation and download

106 项自动化测试通过；2 项手动性能基准未在本次修复中运行。格式和 Clippy 检查通过。实际窗口验证涵盖菜单两侧文字显示、关闭菜单后恢复，以及普通点击打开文件。

106 automated tests passed; the two manual performance benchmarks were not run for this fix. Formatting and Clippy checks passed. Window checks covered text on both sides of the menu, restoration after dismissal, and opening files with a normal click.

[下载 / Download: macOS Apple Silicon ZIP](https://github.com/Arise-cn/LeafTerm/releases/download/v0.1.4/LeafTerm-0.1.4-macos-arm64.zip) · [GitHub Release](https://github.com/Arise-cn/LeafTerm/releases/tag/v0.1.4)

macOS Apple Silicon 开发预览版，使用 ad-hoc 签名，未经过 Apple Developer ID 签名或 Apple 公证。Windows 版本仍在开发中。

Development preview for macOS Apple Silicon, with an ad-hoc signature; it is not signed with Apple Developer ID or notarized by Apple. Windows support remains in development.

## v0.1.3 — 2026-11-03

### 文件树右键菜单

- 文件夹和 Files 空白处支持新建文件、新建文件夹；文件和文件夹支持重命名、移到 macOS 废纸篓。
- 支持在 Finder 中显示、用默认应用打开文件、复制绝对路径和相对项目根目录的路径。
- 支持在所选目录或文件所在目录新建终端分屏，启动独立 Shell。
- Git 变更文件可直接查看未暂存／已暂存更改；冲突文件可进入现有冲突编辑器。
- 菜单支持方向键、回车和 Esc；命名弹框支持中文输入、粘贴和全选。
- 新建和重命名不会覆盖同名项；删除使用系统废纸篓，失败时不会改为永久删除。重命名或删除前检查相关未保存编辑、进行中的保存和其他窗口；成功后刷新文件树、搜索结果和相关预览。
- 重命名已打开文件或其父目录后，同步更新编辑器路径。

### File-tree context menu

- Create files and folders from directories or empty space in Files; rename items or move them to the native macOS Trash.
- Reveal items in Finder, open files with the default app, and copy absolute or project-relative paths.
- Open an independent shell in a new terminal split at the selected directory or a file's parent directory.
- Open staged and unstaged Git diffs directly, or enter the existing conflict editor for conflicted files.
- Navigate menus with arrow keys, Enter, and Escape. Name dialogs support Chinese input, paste, and select all.
- Creation and renaming preserve existing names. Failed Trash operations never fall back to permanent deletion. Renaming and deletion check affected unsaved edits, active saves, and other windows; successful operations refresh the tree, search results, and affected previews.
- Update the editor path after renaming an open file or its parent directory.

### 验证与下载 / Validation and download

103 项自动化测试通过；2 项手动性能基准已在 Release 模式单独运行并通过。格式和 Clippy 检查通过。实际窗口验证涵盖菜单、新建中文文件名、重命名已打开文件、未保存提示和新终端分屏。

103 automated tests passed; both manual performance benchmarks were run separately in release mode and passed. Formatting and Clippy checks passed. Window checks covered menus, file creation with Chinese names, renaming an open file, unsaved-edit prompts, and new terminal splits.

[下载 / Download: macOS Apple Silicon ZIP](https://github.com/Arise-cn/LeafTerm/releases/download/v0.1.3/LeafTerm-0.1.3-macos-arm64.zip) · [GitHub Release](https://github.com/Arise-cn/LeafTerm/releases/tag/v0.1.3)

macOS Apple Silicon 开发预览版，使用 ad-hoc 签名，未经过 Apple Developer ID 签名或 Apple 公证。Windows 版本仍在开发中。

Development preview for macOS Apple Silicon, with an ad-hoc signature; it is not signed with Apple Developer ID or notarized by Apple. Windows support remains in development.
