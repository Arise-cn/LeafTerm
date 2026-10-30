# LeafTerm 使用指南

[← 返回项目首页](../README.zh-CN.md)

[English](guide.md) · **简体中文**

这份指南介绍当前开发预览版的完整操作、数据保护方式和资源边界。LeafTerm 提供本地 Shell、Git 侧栏与轻量编辑功能；Codex CLI、Claude Code 等工具需要自行安装和登录。

[启动与构建](#getting-started) · [快捷键](#shortcuts) · [文件与 Git](#sidebar) · [搜索](#file-search) · [编辑](#editing) · [语法高亮](#highlighting) · [解决冲突](#merge-conflicts) · [目录补全](#directory-completion) · [颜色排查](#terminal-colors) · [布局恢复](#session-restore) · [资源边界](#resource-limits) · [开发验证](#development)

<a id="getting-started"></a>

## 启动与构建

当前可运行版本的渲染后端为 **macOS Metal**。**Windows 版本正在开发中，暂不可用**；目前也没有 Linux 可运行后端。主要开发环境是 Apple Silicon Mac，其他机器的兼容性仍需验证。

### 下载并安装

1. [下载 LeafTerm v0.1.0 macOS Apple Silicon 版](https://github.com/Arise-cn/LeafTerm/releases/download/v0.1.0/LeafTerm-0.1.0-macos-arm64.zip)，解压 ZIP。
2. 将 `LeafTerm.app` 放入**应用程序**文件夹（`/Applications`）。
3. 打开应用。如果 macOS 拦截启动，请按[首次打开说明](#macos-first-launch)处理。

这是 Apple Silicon 开发预览版，尚未使用 Apple Developer ID 证书签名或经过 Apple 公证。使用应用不需要下载源码或安装 Rust；Git 侧栏需要本机有可用的 Git，AI CLI 工具需要另行安装。本次下载的详情见[版本说明](https://github.com/Arise-cn/LeafTerm/releases/tag/v0.1.0)。

### 从源码构建和启动

源码构建需要 Rust 稳定工具链、Xcode Command Line Tools 和 Git。

在仓库目录中运行，并打开指定项目：

```sh
cargo run --release -- --fresh /path/to/your/project
```

使用 LeafTerm 仓库本身：

```sh
cargo run --release -- --fresh .
```

不指定目录时，尝试恢复上次保存的布局：

```sh
cargo run --release
```

`--fresh` 跳过布局恢复；传入目录也会从该目录建立新布局。启动参数和主要快捷键可通过 `cargo run --release -- --help` 查看。如果 Rust 安装在默认位置但尚未加入 `PATH`，可以将命令中的 `cargo` 替换为 `~/.cargo/bin/cargo`。

当前版本为 **0.1.0**。运行 `cargo run --release -- --version` 或 `leafterm --version`，可查看从 Cargo 包信息读取的编译版本。

### 构建 macOS 应用包

```sh
./scripts/bundle.sh
open dist/LeafTerm.app
```

脚本运行 `cargo build --release --locked`，生成仓库内的 `dist/LeafTerm.app`。它不会安装到 `/Applications`，也不会上传或发布。已有应用包默认保留；重新打包时显式传入 `--force`：

```sh
./scripts/bundle.sh --force
```

已完成 release 编译时，可跳过构建：

```sh
./scripts/bundle.sh --no-build
```

应用包目前使用 ad-hoc 签名，尚未使用 Apple Developer ID 证书签名或经过 Apple 公证。运行中的 Shell 和 AI 进程不会因为替换应用包而自动更新；下次启动应用时才会使用新版。

<a id="macos-first-launch"></a>

### 首次打开 macOS 应用

如果首次打开时 macOS 提示“LeafTerm 已损坏”或“无法验证开发者”，请先确认应用来自本项目的可信来源，再按以下步骤操作：

1. 将 `LeafTerm.app` 放入“应用程序”文件夹（`/Applications`）。
2. 打开 macOS 自带的“终端”，执行：

   ```sh
   sudo xattr -cr "/Applications/LeafTerm.app"
   ```

3. 按提示输入 Mac 管理员密码；输入时终端不会显示字符，输入完按回车。
4. 完成后重新打开 LeafTerm。

如果应用放在其他位置，请将命令中的路径替换为实际的 `.app` 路径。该命令会递归清除指定应用包的扩展属性，包括下载隔离标记；它不会修复实际损坏的文件。如果仍无法打开，请重新下载应用。

<a id="shortcuts"></a>

## 快捷键与鼠标操作

`⌘` 是 Command，`⌥` 是 Option，`⇧` 是 Shift。

| 操作 | 快捷键／入口 |
| --- | --- |
| 新建窗口 | `⌘ N`（`⌘ T` 兼容） |
| 左右分屏 | `⌘ D` |
| 上下分屏 | `⌘ ⇧ D` |
| 切换分屏焦点 | `⌘ ⌥ ←/↑` 上一个，`⌘ ⌥ →/↓` 下一个 |
| 最大化当前分屏／恢复 | `⌘ ⇧ Enter` |
| 调整分屏比例 | 拖动分隔线 |
| 关闭文件／当前分屏 | `⌘ W`；有未保存内容或运行会话时确认 |
| 重命名窗口 | `F2` |
| 显示／隐藏侧栏 | `⌘ B`；隐藏后终端铺满可用宽度 |
| 切换 Changes／Files | `⌘ E` |
| 搜索文件名／路径 | `⌘ P`，或点击 Files 搜索框；`↑` / `↓` 选择，`Enter` 打开，`Esc` 清空并退出搜索 |
| 手动刷新 Git | `⌘ R` 或侧栏刷新按钮 |
| 复制／粘贴 | `⌘ C` / `⌘ V` |
| 编辑文件：保存／全选／剪切 | `⌘ S` / `⌘ A` / `⌘ X` |
| 编辑文件：撤销／重做 | `⌘ Z` / `⌘ ⇧ Z` |
| 搜索终端历史 | `⌘ F`，输入文字并按 `Enter` |
| 搜索上一个／下一个匹配 | `⌘ G` / `⌘ ⇧ G`，在保留的历史中循环查找 |
| 放大／缩小字体 | `⌘ +` / `⌘ -` |
| 恢复默认字号 | `⌘ 0` |
| 关闭文件预览／Diff／取消弹窗 | `Esc` |
| 滚动编辑器／只读预览／Diff | 滚轮；只读视图也可用 `↑` / `↓` |
| 横向滚动编辑器／只读预览／Diff | `Shift` + 滚轮；只读视图也可用 `←` / `→` |
| 复制文件路径 | 右键侧栏文件条目 |
| 退出 | `⌘ Q`，确认后退出 |

关闭确认支持鼠标点击 **Cancel** 和 **Close**。有未保存的编辑内容时，会先提供保存、丢弃或取消选项。

<a id="sidebar"></a>

## 文件树与 Git 侧栏

侧栏显示当前 Git 分支，支持尚未产生提交的新分支、detached HEAD 和 Git worktree。它可以跟随当前终端的工作目录，也可以固定在某个项目。按 `⌘ B` 随时隐藏或打开侧栏；切换目录不会自动打开已隐藏的侧栏。

| 视图 | 内容与操作 |
| --- | --- |
| **Files** | 按需展开文件夹，显示 Git 变更标记；可切换隐藏文件，按名称或路径搜索，点击文件编辑，右键复制路径。 |
| **Changes** | 区分未暂存、已暂存和未跟踪文件；点击普通变更查看只读 Diff，点击 **CONFLICTS** 中的文件解决冲突。 |

按 `⌘ E` 切换视图，`⌘ R` 手动刷新 Git。Git 侧栏默认只读取仓库；冲突编辑器中的 **Mark resolved** 会显式暂存当前已保存的文件。提交、继续合并／变基、重置和切换分支等操作由你在终端完成。

<a id="file-search"></a>

## 文件搜索

在 **Files** 顶部输入文件名或相对路径，或按 `⌘ P` 聚焦搜索。支持中文和不区分大小写的子串匹配，范围包含尚未展开的子目录。

结果展示文件名、所在目录和 Git 变更标记。点击或用 `↑` / `↓` 选择并按 `Enter`，即可打开文件编辑；当前文件有未保存修改时，会先提示处理。点击 `×` 清空筛选，`Esc` 清空并把键盘焦点交回工作区。

Git 项目会搜索已跟踪文件和未忽略的新文件，遵循 `.gitignore`；隐藏文件开关同样适用。搜索匹配名称和路径，不搜索文件内容，也不会遍历 `.git` 或沿目录符号链接搜索。

搜索在后台防抖 150 ms，并取消旧查询，不保留常驻项目索引。最多显示 200 项，扫描最多 5 万项、约 4 MiB 路径数据或 2 秒。达到边界时会标明部分结果，可继续输入更精确的路径缩小范围。

<a id="editing"></a>

## 轻量文件编辑

在 **Files** 点击文件后直接编辑。支持鼠标定位和拖动选择、方向键移动，以及 `Shift` 扩展选择。`⌘ S` 保存，`⌘ Z` 撤销，`⌘ ⇧ Z` 重做，`Esc` 返回终端。

有未保存修改时，切换文件、关闭编辑器／窗口或通过 `⌘ Q` 退出，会先提示保存、丢弃或取消。**Changes** 中普通变更的 Diff 保持只读。

### 文件保护与限制

- 可编辑最多 **256 KiB** 的 UTF-8 常规文件；保留 UTF-8 BOM、CRLF 换行和文件权限。
- 二进制、非 UTF-8、大文件和符号链接以只读方式展示。
- 使用同目录临时文件原子保存。保存前发现磁盘文件已被 AI 工具或其他程序修改时，会拒绝覆盖并保留当前编辑。
- 每个编辑器最多保留 **64 步、总计约 4 MiB** 的撤销／重做记录。
- 未保存内容以私有恢复草稿保存。macOS Dock 的退出和系统退出不能通过 winit 弹窗拦截，下一次启动会尝试恢复草稿。

编辑器没有引入语言服务器、插件宿主或大型 IDE 编辑器，适合修改配置和小型代码文件。

<a id="highlighting"></a>

## 语法高亮

文件编辑器和冲突对比中的双方代码、结果区，会按文件名识别语言并着色。支持 Rust、JavaScript、JSON、YAML、Python、Shell、Markdown、HTML／CSS 等内置语法，另有轻量 TOML 语法。

TypeScript 暂用 JavaScript 语法高亮，状态栏显示 `TypeScript · JS grammar`；未知格式和普通文本保持原色。高亮提供语法着色，不提供类型检查或语义诊断。

### 计算与缓存

高亮在按需启动的后台线程计算，连续输入防抖 50 ms 并取消过期结果。编辑从变化前的行状态检查点续算；移动光标、滚动和重绘不会重新分析全文。每个可见行共用一个文字排版缓冲，行内附加颜色，保持中文、组合字符和 Tab 的位置。冲突双方从各自的上下文解析，撤销／重做后会重新着色。

文件受 256 KiB 编辑上限约束。高亮最多保存 16K 个颜色区间、128 个行状态检查点；遇到超过 4 KiB 的长行、颜色区间上限或约 150 ms 的行间计算预算，剩余内容会显示为普通文本，并标记 `Partial highlighting`。

该预算不包含首次语法表加载，也不是单条正则的硬超时。关闭编辑器释放该文件的缓存和工作线程；已加载的语法表在进程内共享复用。高亮不会扫描项目或启动语言服务器。

<a id="merge-conflicts"></a>

## 解决合并冲突

在 **Changes → CONFLICTS** 点击文件，或在文件树／搜索中打开冲突文件，即可进入冲突编辑器。上方左右显示当前冲突段的 **CURRENT** 与 **INCOMING**，保留 Git 标记中的分支标签；使用箭头切换冲突段。变基时双方的含义以 Git 生成的标签为准。

| 操作 | 结果 |
| --- | --- |
| **Accept Current** | 保留左侧内容。 |
| **Accept Incoming** | 保留右侧内容。 |
| **Accept Both** | 按当前、传入的顺序保留双方；可继续编辑结果，调整顺序或去重。 |
| `⌘ Z` / `⌘ ⇧ Z` | 撤销／重做选择或手动编辑。 |

下方结果区支持选择、输入和保存。上方比较区与下方结果区分别滚动；`Shift` + 滚轮横向查看长行。

完成所有冲突段后：

1. 检查结果，按 `⌘ S` 保存。
2. 点击 **Mark resolved**，暂存当前文件。
3. 在终端中继续检查其他变更，并自行完成提交、合并或变基。

**Mark resolved** 不会自动提交或继续合并，其他文件保持原状。暂存前会再次核对磁盘内容和 Git 冲突条目。

支持普通文本冲突、add/add 及 diff3／zdiff3 标记，文件仍受 256 KiB UTF-8 编辑限制。残缺标记会提示手动修正；未解决时不允许保存结果，未保存内容保留在恢复草稿中。删除／修改、二进制及自定义短标记（少于 7 字符）的冲突需要在终端处理。

<a id="directory-completion"></a>

## `cd` 目录补全

在 zsh 中输入 `cd `（末尾有空格），命令下方会自动列出目录。继续输入路径可筛选候选。

| 按键 | 行为 |
| --- | --- |
| `Tab` | 进入候选菜单，再按 `Tab` 切换。 |
| `Enter` | 在菜单中确认候选，再按一次执行 `cd`。只有一个候选时，`Tab` 直接补齐，回车即可执行。 |
| `Ctrl G` | 取消候选菜单，继续编辑命令。 |

补全通过当前 Shell 的行编辑器工作，只列出目录，支持中文和含空格的路径。它不会接管 Codex／Claude Code 等运行中程序的按键。

LeafTerm 保留原有 zsh 启动配置，在新建 Shell 会话中加载补全，不改写 `~/.zshrc`。其他 Shell 使用自身的补全行为。可在启动 LeafTerm 前设置 `LEAFTERM_SHELL_INTEGRATION=0`，禁用 LeafTerm 的 zsh 集成。

<a id="terminal-colors"></a>

## AI 命令行的颜色

终端显示程序输出的 ANSI 16 色、256 色和 RGB 真彩色；这与文件编辑器的语法高亮独立。普通回答正文是否上色由 CLI 自身主题决定，彩色主要体现在代码、命令、Diff 和状态信息。

新建 Shell 会声明 `TERM=xterm-256color`、`COLORTERM=truecolor`，并清除从 LeafTerm 启动进程继承的 `NO_COLOR`，避免构建／自动化工具启动应用时误关彩色输出。在 Shell 启动脚本或单条命令中主动设置 `NO_COLOR` 仍然有效。

更新应用不会改变已有 Shell／AI 进程的环境。旧会话需要恢复颜色时，先正常退出 AI CLI，在 Shell 中执行：

```sh
unset NO_COLOR
codex
```

也可以仅为这次启动移除该变量：

```sh
env -u NO_COLOR codex
# 或
env -u NO_COLOR claude
```

<a id="session-restore"></a>

## 布局与草稿恢复

布局文件保存在：

```text
~/Library/Application Support/LeafTerm/session.json
```

恢复内容包括**窗口、分屏布局、字号和工作目录**。旧版本保存的每个标签会迁移为独立窗口。重新打开时会启动新的 Shell，**不会恢复之前运行的 Codex／Claude Code 进程或未结束的命令**。退出确认会明确提示运行中的程序将停止。

侧栏显示／隐藏状态会在切换时立即保存，下次启动继续沿用，包括使用 `--fresh` 或指定新目录时。同一次运行中隐藏再显示时，保留原来的 Files／Changes 视图和目录展开状态。

未保存的编辑使用同目录的 `recovery.json` 恢复草稿。隔离测试时，可以设置绝对路径 `LEAFTERM_SESSION_PATH=/path/to/test-state/session.json`，让布局和恢复草稿使用测试位置，避免与日常会话混用。

<a id="resource-limits"></a>

## 资源边界与兼容性

低内存是设计目标，尚无已验证的整体性能承诺。GPU 绘制仍然占用内存：终端解析、文字排版和文件读取等在 CPU 上进行，界面由 GPU 绘制；Apple Silicon 的 CPU 与 GPU 共用统一内存。

| 项目 | 当前边界 |
| --- | --- |
| 单窗口终端分屏 | 最多 32 个 |
| 每个终端滚动历史 | 3000 行 |
| 单次粘贴 | 约 128 KiB |
| OSC 控制消息 | 16 KiB |
| 同时展开的文件夹 | 最多 31 个 |
| 单层目录／可见树预览 | 最多 4096 项 |
| Git 状态输出 | 最多 1 MiB／8192 项变更 |
| 单个 Diff／文件预览 | 约 256 KiB |
| 文本编辑 | 最多 256 KiB 的 UTF-8 常规文件 |
| 每个编辑器撤销／重做 | 最多 64 步、总计约 4 MiB |
| 文件搜索 | 最多 200 项结果；扫描最多 5 万项、约 4 MiB 路径数据或 2 秒 |

大型结果需要通过 Shell 中的 Git 或文件工具继续查看。这些限制用于控制常见数据增长，并不构成整个进程的硬内存上限；字体、GPU 驱动、终端单元格和依赖库仍会分配内存。性能验证应分别记录 LeafTerm、Shell 与 AI CLI 进程的占用，并包含长时间输出、大仓库刷新、分屏反复打开／关闭等场景。

当前不支持完整 Kitty 键盘协议、终端图片协议或屏幕阅读器语义。Codex CLI／Claude Code 的完整交互流程、中文输入法组合、长时间压力和其他 Mac 仍需继续验证。

<a id="development"></a>

## 开发与验证

```sh
cargo test --locked
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
```

核心组成：

| 组件 | 职责 |
| --- | --- |
| `winit` | 窗口和输入 |
| `wgpu` | 通过 Metal 绘制界面 |
| `glyphon` | 文字绘制 |
| `alacritty_terminal` | 解析终端控制序列 |
| `portable-pty` | 管理本地 Shell 和子进程 |
| `syntect` | 文件语法高亮 |

终端内核固定在 Alacritty 官方提交 [`54214706`](https://github.com/alacritty/alacritty/commit/542147067dd016fc5f49120ef0ba605a98a4e3a7)，包含零宽字符存储上限和 Kitty 键盘模式栈修复。这是固定的开发版本，升级时应重新运行兼容性与资源测试。

当前已有 88 项自动化测试和 1 项手动 release 基准。自动化覆盖真实 PTY、输入／窗口尺寸、macOS 空格键、彩色输出与环境变量、目录补全、Shell 启动配置、Git 状态与 worktree、文件读取和保存、中文字体、控制消息边界、子进程回收、布局迁移、搜索取消与资源边界、编辑撤销、冲突解析与单文件暂存，以及语法高亮与 Unicode 行内颜色映射。

已进行的窗口实测涵盖分屏、多窗口、目录跟随、文件搜索、编辑保存和草稿恢复、冲突处理与撤销，以及 JSON／TOML／Rust 高亮。开发预览版仍在迭代，这些检查不代表所有终端应用或机器都已完成兼容性验证。
