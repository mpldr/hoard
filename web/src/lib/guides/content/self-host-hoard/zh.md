---
title: "如何用 Docker 自托管 Hoard"
description: "用 Docker Compose 几分钟搭建你自己的 Hoard 服务器。开源、免费、运行在你自己的硬件上——一个完全自托管的游戏存档云，无需账号、没有容量限制。"
order: 0
featured: true
updated: 2026-06-29
---

Hoard 是开源且可自托管的。你可以不使用 Hoard Cloud，而是在自己的机器上运行同一个 `hoard-server`，让每台设备都连接到它——无需账号，容量只受你分配的磁盘大小限制。本指南用 Docker 在几分钟内把服务器跑起来。

## 为什么自托管 Hoard

- **完全掌控。** 你的存档保存在你自己掌控的硬件上，而不是别人的云端。
- **没有容量限制。** 空间仅受你自己的磁盘限制。
- **同一个应用，同样的功能。** 版本历史和后台同步与 Hoard Cloud 完全一致，改变的只有后端。
- **开源。** 你可以阅读、审计并修改服务器代码。

这正是它与 [Ludusavi](/guides/ludusavi-alternative) 这类工具的关键区别：Ludusavi 在本地备份和通过 Rclone「自带云」方面很出色，但同步需要你自己搭建。Hoard 则提供一个托管式的同步服务器，启动一次后每台设备都能连接。

## 你需要准备

- 一台保持开机的机器（家庭服务器、运行 Docker 的 NAS，或一台小型 VPS）。
- 已安装 Docker 和 Docker Compose。
- 可选：一个域名和用于 HTTPS 的反向代理（超出本地局域网的场景推荐）。

## 用 Docker Compose 安装

克隆仓库，从示例创建配置，设置 `public_url`，然后启动整套服务：

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml      # set public_url at minimum

cd deploy/docker
docker compose up -d --build
docker compose logs -f                         # wait for "listening"
```

等待日志显示服务器正在监听。数据保存在一个命名的 Docker 卷（`hoard-data`）中——像备份其他卷一样备份它。容器内部监听 `8080` 端口；用 `HOARD_PORT=9000 docker compose up -d` 可映射到其他主机端口。

## 创建用户和设备令牌

服务器没有注册页面——用户通过命令行创建：

```sh
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    user create alice --admin --password 'CHANGE_ME'
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    token create alice --device 'desktop'
```

令牌只显示一次，**之后无法找回**，请立即复制。

## 连接桌面应用

在每台机器上安装 [Hoard 桌面应用](/download)。在初始引导中选择 **Autohost**，然后粘贴你的服务器 URL 和刚创建的令牌。之后它的行为与 Hoard Cloud 完全相同：检测你的游戏、自动备份存档、保留版本历史。日常用法请参见[在多台 PC 之间同步存档](/guides/sync-game-saves-across-pcs)。

## 在生产环境中运行

对于任何暴露到本地网络之外的部署，请在反向代理（Caddy、nginx 或 Traefik）上终止 TLS，并把 `public_url` 设为你真实的 HTTPS 地址。更喜欢裸机部署？仓库还提供了 `systemd` 安装脚本，以及一个 `hoard-server upgrade` 命令，它会原子地替换二进制文件而不会中断正在进行的同步。

## 自托管还是 Hoard Cloud？

如果你已经在运行服务器并希望完全掌控、没有容量限制，自托管是理想选择。如果你不想维护基础设施，[Hoard Cloud](/pricing) 提供由我们托管的同样同步功能，并有免费档可供起步。无论哪种方式，应用和你的存档都保持可迁移——以后可以随时切换。
