# Zed（个人 Fork）

基于 [Zed](https://github.com/zed-industries/zed) 的本地二次开发仓库。上游是高性能多人协作编辑器；**本仓库已做第一刀低耦合精简**，偏向个人本地开发，行为与官方发行版不完全一致。

详细说明见：[本仓库精简说明（中文）](./docs/src/development/fork-strip.zh.md)

---

### 相对上游已剔除（摘要）

| 类别 | 已移除 |
| --- | --- |
| 协作 / 通话 | `collab` 服务端、`call` / `channel` / `collab_ui`、LiveKit |
| 自动更新 / 反馈 | `auto_update*`、`feedback` |
| 工具 / 实验 | benchmarks、`docs_preprocessor`、`eval_cli`、`xtask`、`gpui_web`、`nix/` 等 |

**仍保留：** 编辑器核心、LSP / Git / 终端 / 调试、AI Agent、SSH Remote、扩展宿主，以及 `client` / `rpc` 等账号与协议层。

---

### 本地构建（Windows）

依赖与步骤见 [Building Zed for Windows](./docs/src/development/windows.md)。

一键启动（MSVC 环境 + rustup PATH + `ZED_STATELESS` + 运行）：

```powershell
.\script\run-windows.ps1
```

等价于下面 5 步：

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set PATH=%USERPROFILE%\.cargo\bin;%VCToolsInstallDir%bin\Hostx64\x64;%PATH%
cd /d D:\code\zed
set ZED_STATELESS=1
cargo run -p zed
```

Release 构建：`.\script\run-windows.ps1 --release`

若本机已安装官方 Zed，开发版可能被单实例接管；脚本已默认设置 `ZED_STATELESS=1`。

Rust 版本以仓库根目录 [`rust-toolchain.toml`](./rust-toolchain.toml) 为准。

其他平台：

- [macOS](./docs/src/development/macos.md)
- [Linux](./docs/src/development/linux.md)

---

### 官方安装包（上游）

需要官方完整功能（协作、自动更新等）时，请使用上游发行版：

- [下载](https://zed.dev/download)
- 上游仓库：https://github.com/zed-industries/zed

---

### Licensing

源码许可与上游一致：主体为 GPL-3.0-or-later，部分组件为 Apache-2.0（以文件内标注为准）。

第三方依赖许可信息需满足 `cargo-about` / CI 要求；配置见 `script/licenses/zed-licenses.toml`。

---

### 说明

本 Fork 不代表 Zed Industries 官方立场。招聘、赞助等请参阅上游 [zed.dev](https://zed.dev) 与 [zed-industries/zed](https://github.com/zed-industries/zed)。
