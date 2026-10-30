<div align="center">

<img src="assets/LeafTerm.png" width="120" alt="LeafTerm — 叶子与终端提示符组成的应用图标" />

# LeafTerm

### 少开一个 IDE，把空间留给 AI。

Your agent’s terminal. Your code, in view.

<p>
  <a href="README.md"><img src="https://img.shields.io/badge/English-253136?style=for-the-badge" alt="Switch to English" /></a>
  <a href="README.zh-CN.md"><img src="https://img.shields.io/badge/%E7%AE%80%E4%BD%93%E4%B8%AD%E6%96%87-6DDEAA?style=for-the-badge&amp;logoColor=0C1114" alt="切换到简体中文" /></a>
</p>

<p>
  <img src="https://img.shields.io/badge/version-v0.1.0-6DDEAA?style=flat-square" alt="版本 0.1.0" />
  <img src="https://img.shields.io/badge/Rust-native-DEA584?style=flat-square&amp;logo=rust&amp;logoColor=white" alt="Built with Rust" />
  <img src="https://img.shields.io/badge/Metal-GPU_rendered-6DDEAA?style=flat-square" alt="Metal GPU rendering" />
  <img src="https://img.shields.io/badge/macOS-Apple_Silicon-9AAEB8?style=flat-square&amp;logo=apple&amp;logoColor=white" alt="Developed on macOS Apple Silicon" />
  <img src="https://img.shields.io/badge/Windows-in_development-7CB7DB?style=flat-square" alt="Windows 版本开发中" />
  <img src="https://img.shields.io/badge/status-preview-E7BE6F?style=flat-square" alt="Development preview" />
</p>

<p>
  <a href="https://github.com/Arise-cn/LeafTerm/releases/download/v0.1.0/LeafTerm-0.1.0-macos-arm64.zip"><img src="https://img.shields.io/badge/%E4%B8%8B%E8%BD%BD-macOS_Apple_Silicon-6DDEAA?style=for-the-badge&amp;logo=apple&amp;logoColor=0C1114" alt="下载适用于 macOS Apple Silicon 的 LeafTerm" /></a>
  <br />
  <sub>v0.1.0 预览版 · ZIP · <a href="https://github.com/Arise-cn/LeafTerm/releases/tag/v0.1.0">版本说明</a></sub>
</p>

[快速开始](#quick-start) · [功能一览](#features) · [常用快捷键](#shortcuts) · [完整指南](docs/guide.zh-CN.md) · [反馈问题](https://github.com/Arise-cn/LeafTerm/issues)

</div>

---

AI 已经在终端里写代码，查看分支、检查改动、调整配置、解决冲突，却常常还要再打开一个 IDE。

**LeafTerm 把这些日常操作带回终端。** 文件树、Git Diff、轻量编辑和冲突处理就在手边；需要时展开，专注时按 `⌘B` 收起。用 Rust 构建、由 GPU 绘制，以精简资源占用为设计目标，让 AI 开发工作流少一份完整 IDE 的负担。

<p align="center">
  <img src="docs/images/workflow.gif" width="1100" alt="LeafTerm 功能演示动画：AI 终端、Git 文件树、轻量编辑、冲突处理和侧栏收起" />
  <br />
  <sub>功能示意动画，非应用录屏 · 虚构项目与示例内容 · <a href="docs/images/workflow-poster.png">静态预览</a></sub>
</p>

> **macOS 开发预览版 · Windows 版本开发中**
> 当前可运行版本使用 macOS Metal，主要在 Apple Silicon 上开发和验证。Windows 暂不可用。低内存是设计目标，目前没有公开的对比基准；GPU 渲染仍会使用系统内存。

<a id="features"></a>

## 一个终端，刚好够用的项目视野

| | 你可以做什么 |
| :--- | :--- |
| **看见 AI 的改动** | 在侧栏查看当前分支、文件状态和 Diff，区分已暂存、未暂存、未跟踪和冲突文件。支持 Git worktree。 |
| **快速找到文件** | `⌘P` 搜索文件名或相对路径，覆盖未展开的目录；按需展开文件树，显示 Git 变更标记。 |
| **顺手改个配置** | 点击文本文件直接编辑，语法高亮、`⌘S` 保存、`⌘Z` 撤销；外部修改检测帮助避免覆盖 AI 刚写入的内容。 |
| **直观解决冲突** | 对比 Current / Incoming，保留一方或双方，在结果区手动调整；操作可撤销，保存后显式标记解决。 |
| **按你的方式排布** | 快捷键新建窗口、左右／上下分屏、切换焦点、最大化当前分屏；拖动分隔线调整比例。 |
| **让终端占满空间** | `⌘B` 显示／隐藏侧栏并记住状态。去掉常驻标签栏和工具栏，品牌留在应用图标与深墨、薄荷绿配色里。 |

真实本地 Shell / PTY、ANSI 16 色 / 256 色 / 真彩色、Unicode、选择复制、粘贴和滚动历史，构成这一切的基础。

<a id="quick-start"></a>

## 快速开始

1. [下载 LeafTerm v0.1.0 macOS Apple Silicon 版](https://github.com/Arise-cn/LeafTerm/releases/download/v0.1.0/LeafTerm-0.1.0-macos-arm64.zip)，解压 ZIP。
2. 将 `LeafTerm.app` 拖入**应用程序**文件夹（`/Applications`）。
3. 打开 LeafTerm。如果 macOS 拦截启动，请按下方[首次打开说明](#macos-first-launch)处理。

下载包是 **Apple Silicon 开发预览版**，尚未使用 Apple Developer ID 证书签名或经过 Apple 公证。使用应用不需要 clone 仓库或安装 Rust；Git 侧栏需要本机有可用的 Git。

进入 LeafTerm 后，照常运行已经安装的工具：

```sh
codex
# 或
claude
```

LeafTerm 使用本地 Shell，不附带 AI CLI、模型或账号登录。

<details>
<summary><strong>从源码构建（可选）</strong></summary>

源码构建需要 **macOS、Rust 稳定工具链、Xcode Command Line Tools 和 Git**。

```sh
git clone https://github.com/Arise-cn/LeafTerm.git
cd LeafTerm
cargo run --release -- --fresh .
```

打开你自己的项目：

```sh
cargo run --release -- --fresh /path/to/your/project
```

只运行 `cargo run --release` 时，会尝试恢复上次的窗口布局和工作目录。构建带 Dock 图标的 macOS 应用：

```sh
./scripts/bundle.sh
open dist/LeafTerm.app
```

脚本生成仓库内的 `dist/LeafTerm.app`。已有应用包需要重建时，使用 `./scripts/bundle.sh --force`。

这是用于本机开发的 ad-hoc 签名包，尚未使用开发者证书签名或公证。完整启动参数和打包选项见[构建与启动](docs/guide.zh-CN.md#getting-started)。

</details>

<a id="macos-first-launch"></a>

### 首次打开 macOS 应用

LeafTerm 尚未使用 Apple Developer ID 证书签名或经过 Apple 公证。首次打开时，如果 macOS 提示“已损坏”或“无法验证开发者”，请先确认应用来自本项目的可信来源，将 `LeafTerm.app` 放入“应用程序”文件夹，再打开 macOS 自带的“终端”执行：

```sh
sudo xattr -cr "/Applications/LeafTerm.app"
```

按提示输入 Mac 管理员密码（输入时不会显示字符），完成后重新打开 LeafTerm。如果应用放在其他位置，请将命令中的路径替换为实际的 `.app` 路径。

该命令会递归清除这个应用包的扩展属性，包括下载隔离标记；它不会修复实际损坏的文件。如果仍无法打开，请重新下载应用。详见[首次打开说明](docs/guide.zh-CN.md#macos-first-launch)。

## 从检查改动，到完成修改

**1. 让 AI 在终端工作。** 在熟悉的 CLI 中继续对话，侧栏跟随当前目录，也可以固定在某个项目。

**2. 在 Files 找到文件。** `⌘P` 输入文件名，回车打开；改配置、检查代码、保存后回到终端。

**3. 在 Changes 核对结果。** 查看实际 Diff；遇到文本冲突时，逐段保留 Current、Incoming 或 Both，再检查最终内容。

Git 侧栏通常只读。冲突编辑器的 **Mark resolved** 会暂存当前已保存的文件；提交、继续合并或变基由你在终端执行。

<a id="shortcuts"></a>

## 少一点点击，多一点空间

| 快捷键 | 操作 |
| :--- | :--- |
| `⌘ B` | 显示／隐藏侧栏 |
| `⌘ P` | 搜索文件 |
| `⌘ E` | 切换 Files / Changes |
| `⌘ D` / `⌘ ⇧ D` | 左右／上下分屏 |
| `⌘ ⌥ ←` / `⌘ ⌥ →` | 切换到上一个／下一个分屏 |
| `⌘ ⇧ Enter` | 最大化当前分屏／恢复 |
| `⌘ N` | 新建窗口 |
| `⌘ S` | 保存文件 |
| `⌘ Z` / `⌘ ⇧ Z` | 编辑或解决冲突时撤销／重做 |
| `⌘ F` | 搜索终端历史 |

另有 zsh 的 `cd` 目录候选、字号调整、布局恢复等操作。查看[完整快捷键](docs/guide.zh-CN.md#shortcuts)和[使用指南](docs/guide.zh-CN.md)。

## 为 AI 工作流，留出更多余地

- **把界面交给 GPU。** `winit` 管理原生窗口和输入，`wgpu` / Metal 与 `glyphon` 绘制界面和文字。
- **按需处理项目内容。** 文件树按需展开，文件搜索在后台执行并取消旧查询，不建立常驻的全项目索引。
- **让编辑保持简单。** 语法高亮后台计算、增量更新；不启动语言服务器或插件宿主。
- **为数据增长设边界。** 限制滚动历史、编辑文件大小、搜索结果和撤销记录；超出范围时交给现有命令行工具处理。

终端内核使用 `alacritty_terminal`，本地进程由 `portable-pty` 管理，语法着色使用 `syntect`。解析、排版和文件处理仍需要 CPU；具体资源边界见[详细说明](docs/guide.zh-CN.md#resource-limits)。

<details>
<summary><strong>当前支持范围与已知限制</strong></summary>

- 当前可运行版本仅支持 macOS Metal；**Windows 版本正在开发中**，尚未提供可运行版本。Linux 暂不支持，其他 Mac 的兼容性仍需验证。
- 编辑支持最多 **256 KiB 的 UTF-8 常规文件**。大文件、二进制、非 UTF-8 文件和符号链接以只读方式展示。
- TypeScript 暂用 JavaScript 语法高亮；不提供类型检查、语言服务器或语义诊断。
- 冲突 UI 支持常见文本冲突，包括 diff3 / zdiff3；删除／修改、二进制等冲突仍需在终端处理。
- 布局恢复会重新启动 Shell，**不会恢复之前运行的 AI 进程或命令**。
- 中文输入法组合、长时间压力及 AI CLI 的完整交互流程仍在持续验证；尚不支持完整 Kitty 键盘协议、终端图片协议或屏幕阅读器语义。

更多细节：[编辑](docs/guide.zh-CN.md#editing) · [语法高亮](docs/guide.zh-CN.md#highlighting) · [冲突解决](docs/guide.zh-CN.md#merge-conflicts) · [布局恢复](docs/guide.zh-CN.md#session-restore)

</details>

## 一起打磨 LeafTerm

欢迎通过 [Issues](https://github.com/Arise-cn/LeafTerm/issues) 反馈实际使用中的问题，也欢迎提交改进。报告问题时，请附上 macOS 版本、芯片、Shell / AI CLI 版本、复现步骤，以及去除敏感信息后的截图或日志。

本地开发检查：

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

[使用指南](docs/guide.zh-CN.md) · [颜色输出排查](docs/guide.zh-CN.md#terminal-colors) · [开发与验证](docs/guide.zh-CN.md#development)

LeafTerm 使用 [MIT 许可证](LICENSE)开放源代码。第三方依赖保留各自的许可证。

---

<p align="center">
  <strong>LeafTerm</strong><br />
  <sub>Your code, in view.</sub>
</p>
