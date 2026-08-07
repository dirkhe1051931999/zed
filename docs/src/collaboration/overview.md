---
title: Collaboration
description: "Real-time collaboration in Zed: share projects, edit code together, and communicate with voice chat."
---

# Collaboration {#collaboration}

> **本仓库说明：** 个人 Fork 已移除 Collab Panel、频道、语音通话与 LiveKit
> 等相关实现。以下内容描述上游官方行为，在本仓库中不可用。详见
> [本仓库精简说明](../development/fork-strip.zh.md)。

Zed supports real-time multiplayer editing. Multiple people can work in the same project simultaneously, seeing each other's cursors and edits as they happen.

Open the Collaboration Panel with {#kb collab_panel::ToggleFocus}. You'll need to [sign in](../authentication.md#signing-in) to access collaboration features.

## Collaboration Panel {#collaboration-panel}

The Collaboration Panel has two sections:

1. [Channels](./channels.md): Persistent project rooms for team collaboration, with shared projects and voice chat.
2. [Contacts and Private Calls](./contacts-and-private-calls.md): Your contacts list for ad-hoc private sessions.

> **Warning:** Sharing a project gives collaborators access to your local file system within that project. Only collaborate with people you trust.

See the [Data and Privacy FAQs](https://zed.dev/faq#data-and-privacy) for more details.

## Audio Settings {#audio-settings}

### Selecting Audio Devices

You can select specific input and output audio devices instead of using system defaults. To configure audio devices:

1. Open {#kb zed::OpenSettings}
2. Navigate to **Collaboration** > **Experimental**
3. Use the **Output Audio Device** and **Input Audio Device** dropdowns to select your preferred devices

Changes take effect immediately. If you select a device that becomes unavailable, Zed falls back to system defaults.

To test your audio configuration, click **Test Audio** in the same section. This opens a window where you can verify your microphone and speaker work correctly with the selected devices.

**JSON configuration:**

```json [settings]
{
  "audio": {
    "experimental.output_audio_device": "Device Name (device-id)",
    "experimental.input_audio_device": "Device Name (device-id)"
  }
}
```

Set either value to `null` to use system defaults.
