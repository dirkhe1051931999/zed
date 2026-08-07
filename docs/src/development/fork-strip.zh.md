---
title: 本仓库精简说明（个人 Fork）
description: "记录相对上游 Zed 已剔除的低耦合模块，以及本地构建与使用注意点。"
---

# 本仓库精简说明（个人 Fork） {#fork-strip}

本仓库在上游 Zed 基础上做了第一刀精简：去掉与个人本地开发关系弱、耦合相对较低的模块。目标是减小构建面，方便二次开发；**不是**上游官方产品行为。

## 已剔除 {#removed}

### 协作 / 通话

- 协作服务端 `crates/collab`
- 通话与频道 UI：`call`、`channel`、`collab_ui`
- LiveKit：`livekit_api`、`livekit_client`
- 默认 keymap 中的 Collab / Channel 绑定
- 标题栏协作头像、通话控件

因此本仓库**没有** Collab Panel、频道、语音通话、屏幕共享相关能力。详见原文档
[Collaboration](../collaboration/overview.md)（内容仍描述上游行为，本仓库不适用）。

### 自动更新与官方反馈

- `auto_update`、`auto_update_ui`、`auto_update_helper`
- `feedback`（Bug Report / Request Feature / Email 等入口）

本仓库开发构建**不会**向 zed.dev 检查更新。需要新版本时自行 `git pull` 再编译。原
[Update](../update.md) 文档描述的是上游自动更新行为。

### 工具链与实验目标

- 性能基准 crate：`benchmarks`、`editor_benchmarks`、`fs_benchmarks`、`project_benchmarks`、`worktree_benchmarks`
- `docs_preprocessor`、`eval_cli`
- `tooling/compliance`、`tooling/xtask`（`cargo xtask` 不可用）
- `gpui_web`（本 Fork 不支持 wasm / Web 目标）
- `nix/`

> **Note:** 仍保留 `tooling/perf` 的**库**部分，供 `#[perf]` 宏编译；已去掉 perf
> 二进制与 `cargo perf-test` / `perf-compare` 别名。保留 `eval_utils`（Agent
> 依赖）。

## AI 默认入口 {#ai-entry}

本 Fork 把默认 AI 入口统一为状态栏 **Open Agent**（`agent::OpenAgentPage` / 中间 Agent 页），不再使用：

- 状态栏 **Open Threads Sidebar**（`multi_workspace::ToggleWorkspaceSidebar`）
- Dock 上的 **Agent Panel** 按钮（`agent::ToggleFocus` 等仍会重定向到 Agent 页）

快捷键、欢迎页、菜单、`zed://agent`、Agent 等待通知点击等，默认都打开 Agent 页。`AgentPanel` 仍作为对话后端保留，只是不再作为用户可见入口。

## 刻意保留 {#kept}

- 编辑器核心：`editor`、`project`、`workspace`、LSP、Git、终端、调试等
- AI / Agent 相关 crate
- SSH Remote（`remote*`）
- 扩展宿主（`extension_host` 等）
- 账号 / RPC：`client`、`rpc`、`proto`、`cloud_api_*`（后续若再精简，耦合会高很多）

## Windows 本地构建 {#windows-build}

1. 安装 VS 2022 Build Tools（含 C++、Spectre 库、Windows SDK）、CMake、rustup。
2. 工具链以仓库 [`rust-toolchain.toml`](../../../rust-toolchain.toml) 为准（当前为
   `1.95.0`）。
3. 若遇 `path too long`，开启 Git / Windows 长路径支持。
4. 构建前初始化 MSVC，并保证 MSVC 的 `link.exe` 优先于
   `C:\Program Files\coreutils\bin\link.exe`：

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set PATH=%USERPROFILE%\.cargo\bin;%VCToolsInstallDir%bin\Hostx64\x64;%PATH%
cd /d D:\code\zed
cargo run -p zed
```

5. 若本机已安装官方 Zed（例如 `D:\Zed\Zed.exe`），开发版可能被单实例接管。请设置：

```bat
set ZED_STATELESS=1
target\debug\zed.exe
```

更完整的依赖说明见 [Building Zed for Windows](./windows.md)。

## Windows 标题栏按钮 {#windows-caption-buttons}

窗口右上角的最小化 / 最大化 / 关闭由 Zed 自绘。当**右侧 Agent 侧栏打开**时，会暂时隐藏这三项，避免与侧栏重叠。关闭右侧侧栏后按钮会恢复。这是上游既有行为，与本次精简无关。

> **Fix:** 精简时曾把 `title_bar::init` 随 `collab_ui` 一起删掉，导致整条标题栏不注册。现已在
> `crates/zed/src/main.rs` 中直接调用 `title_bar::init(cx)`。若仍看不到标题栏，请重新
> `cargo run -p zed`。

## 顶部菜单 {#title-bar-menu}

Windows 默认把应用菜单收在标题栏左侧汉堡按钮中。若要始终展开 File / Edit 等菜单：

```json [settings]
{
  "title_bar": {
    "show_menus": true,
    "show_project_items": true,
    "show_branch_name": true
  }
}
```

## 后续可能再砍 {#next}

若继续瘦身，优先评估（工作量大、勿与第一刀混为一谈）：

- Zed 账号与 `client` / `cloud_api_*` 的 stub 或剥离
- 仅保留本地模型时的 AI 供应商裁剪
- 不需要 Remote 时的 `remote*` 剥离
