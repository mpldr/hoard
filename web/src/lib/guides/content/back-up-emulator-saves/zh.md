---
title: "如何备份和同步模拟器存档（RetroArch、Dolphin、PCSX2）"
description: "用 Hoard 在多台 PC 之间自动备份和同步你的模拟器存档文件与即时存档——支持 RetroArch、Dolphin、PCSX2、DuckStation 等。"
order: 6
updated: 2026-06-28
---

模拟器存档很容易丢失：存档文件和即时存档散落在各处的文件夹里，一次重装或换一台新 PC 就可能清除多年的进度。Hoard 会自动备份它们，并在多台机器之间保持同步。

## Hoard 支持的模拟器

Hoard 可处理常见的模拟器存档文件（`.srm`、`.sav`、记忆卡）以及主流模拟器的即时存档，包括：

- **RetroArch** —— 按核心区分的存档和即时存档
- **Dolphin**（GameCube / Wii）—— 记忆卡和 GCI 文件
- **PCSX2**（PS2）—— 记忆卡
- **DuckStation**（PS1）、**PPSSPP**（PSP）、**mGBA** 等

由于 Hoard 使用与 Ludusavi 相同的社区数据库来定位存档文件夹，许多模拟器路径都会被自动检测。对于任何自定义位置，你都可以手动把 Hoard 指向某个文件夹。

## 设置模拟器存档备份

1. **安装 Hoard**（Windows、macOS 或 Linux）并登录。
2. 打开**库**并添加你的模拟器；如果你更改了默认位置，请手动添加它的存档／即时存档文件夹。
3. 保持**自动模式**开启。Hoard 会在每次会话后备份，并保留版本历史。
4. 用同一账号在你的其他 PC 上安装 Hoard，即可在任何地方同步这些存档——请见[在多台 PC 之间同步存档](/guides/sync-game-saves-across-pcs)。

## 模拟器用 Ludusavi？

Ludusavi 同样可以在本地备份模拟器存档，对此它是一个很好的免费选择。如果你还希望这些模拟器存档在多台机器之间自动同步，并在不配置 Rclone 的情况下保留云端版本历史，那就是 Hoard 能帮上忙的地方——请阅读完整的 [Ludusavi 与 Hoard 对比](/guides/ludusavi-alternative)。

## 提示

即时存档与特定的模拟器版本绑定。请在所有 PC 上保持模拟器版本一致地更新，这样同步过来的即时存档才能在各处正常加载。
