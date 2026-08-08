# Agent guidelines（个人 Fork）

Rust / GPUI 编码规范继续遵循仓库根目录 [`.rules`](./.rules)。

本文件约束 **本机 Windows 二次开发** 场景，避免无意义全量编译、误删官方 CI、以及错误工具链。

## Windows 构建与运行

- 启动 / 编译统一用：`.\script\run-windows.ps1`（可加 `--release`）。
- 不要手写零散 `cargo run`，除非已确认当前 shell 满足：
  1. 已 `call` VS2022 BuildTools 的 `vcvars64.bat`
  2. `PATH` 以 `%USERPROFILE%\.cargo\bin` 为先，再是 MSVC `link.exe` 目录
  3. `ZED_STATELESS=1`（避免被已安装的官方 Zed 单实例接管）
- 验证工具链：`where rustc` / `where cargo` 第一行必须是  
  `C:\Users\Administrator\.cargo\bin\...`，版本以 [`rust-toolchain.toml`](./rust-toolchain.toml) 为准（当前 1.95.x）。
- **禁止**使用 `C:\Program Files\Rust stable MSVC 1.94\...` 下的 cargo/rustc。混用会导致看似“每次改代码都 cargo clean”的整库重编。

## 编译缓存（不要人为弄失效）

- `target/` 是增量缓存，**不要**随便 `cargo clean`、删 `target/`，除非用户明确要求。
- 日志里大量 `Compiling ...` 多半是依赖图连锁重编，不等于 clean。改底层 crate（如 `gpui` / `project` / `util`）会扇出很多编译单元，属正常。
- 不要改全局 `RUSTFLAGS` / `CARGO_TARGET_DIR` / 随意改 [`.cargo/config.toml`](./.cargo/config.toml) 的 `rustflags`，会废掉缓存。
- 日常开发用 debug（默认 `run-windows.ps1`），不要为了“试一下”就 `--release`。
- 第一次全量（~1600 crates）很慢；同工具链下小改动应明显更快。若又接近全量，先查是不是又用错了 rustc。

## 测试要轻

- 优先跑窄测试，例如：  
  `cargo test -p worktree --features test-support --test integration <test_name>`
- **不要**默认跑 `cargo test -p project_panel` 或 workspace 级测试（依赖 GPUI，编译极慢且 GPU 占用高）。
- 不为小功能硬加重型 GPUI/panel 集成测试；能用 `worktree` / 纯逻辑 crate 覆盖就够。

## GitHub Actions（本 Fork）

- 本仓库只需个人打包：只保留 [`.github/workflows/release_fork.yml`](./.github/workflows/release_fork.yml)（push `main` → 打 macOS/Windows 包）。
- **不要**从上游恢复定时任务、Issue triage、nightly、官方 release/test 等 workflow。
- 官方 workflow 在 fork 上被 skip/取消是预期（`repository_owner == zed-industries`），不是要修的 CI bug。

## 功能开发注意

- 项目树 / Git 状态不同步：优先用已有的 **Refresh Project**（`LocalWorktree::force_rescan` + 项目面板根目录右键），不要先大改文件监视器。
- 精简说明见 [`docs/src/development/fork-strip.zh.md`](./docs/src/development/fork-strip.zh.md)。

## 提交与范围

- 未明确要求时不要 commit / push。
- 不要顺手改无关文档、不要“整理”上游 CI/协作相关文件。
- 改动保持最小，只做用户要的事。
