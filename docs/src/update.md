---
title: Update Zed
description: "Zed is designed to keep itself up to date automatically. You can always update this behavior in your settings."
---

# Update Zed

> **本仓库说明：** 个人 Fork 已移除 `auto_update` /
> `auto_update_ui`。开发构建不会自动检查或安装更新；请自行拉取源码后重新编译。详见
> [本仓库精简说明](./development/fork-strip.zh.md)。

Zed is designed to keep itself up to date automatically. You can always update this behavior in your settings.

## Auto-updates

By default, Zed checks for updates and installs them automatically the next time you restart the app. You’ll always be running the latest version with no extra steps.

If an update is available, Zed will download it in the background and apply it on restart.

## How to check your current version

To check which version of Zed you're using:

Open the Command Palette (Cmd+Shift+P on macOS, Ctrl+Shift+P on Linux/Windows).

Type and select {#action zed::About}. A modal will appear with your version information.

## How to control update behavior

If you want to turn off auto-updates, open the Settings Editor (Cmd ,) and find `Auto Update` under General Settings.
