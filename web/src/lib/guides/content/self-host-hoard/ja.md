---
title: "DockerでHoardをセルフホストする方法"
description: "Docker Compose を使って数分で自分専用の Hoard サーバーを構築。オープンソースで無料、自分のハードウェア上に完全セルフホストのセーブデータ用クラウドを。アカウントも容量制限も不要。"
order: 0
featured: true
updated: 2026-06-29
---

Hoard はオープンソースでセルフホスト可能です。Hoard Cloud を使う代わりに、同じ `hoard-server` を自分のマシンで動かし、すべての端末をそこへ接続できます。アカウントは不要で、容量制限は与えたディスク容量だけです。このガイドでは Docker を使って数分でサーバーを立ち上げます。

## なぜ Hoard をセルフホストするのか

- **完全な所有権。** セーブデータは他人のクラウドではなく、自分が管理するハードウェアに保存されます。
- **容量制限なし。** 容量は自分のディスクだけが上限です。
- **同じアプリ、同じ機能。** バージョン履歴とバックグラウンド同期は Hoard Cloud とまったく同じように動作し、変わるのはバックエンドだけです。
- **オープンソース。** サーバーを読み、監査し、改変できます。

これが [Ludusavi](/guides/ludusavi-alternative) のようなツールとの決定的な違いです。Ludusavi はローカルバックアップや Rclone 経由の「自分のクラウドを持ち込む」方式に優れていますが、同期は自分で組む必要があります。Hoard は一度立ち上げればすべての端末が接続できる、管理された同期サーバーを提供します。

## 必要なもの

- 常時稼働するマシン（自宅サーバー、Docker が動く NAS、または小さな VPS）。
- Docker と Docker Compose がインストール済みであること。
- 任意で、HTTPS 用のドメインとリバースプロキシ（LAN を越える用途では推奨）。

## Docker Compose でインストール

リポジトリをクローンし、サンプルから設定を作成して `public_url` を設定し、スタックを起動します。

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml      # set public_url at minimum

cd deploy/docker
docker compose up -d --build
docker compose logs -f                         # wait for "listening"
```

サーバーが待ち受け状態になったとログに表示されるまで待ちます。データは名前付き Docker ボリューム（`hoard-data`）に保存されるので、他のボリュームと同様にバックアップしてください。コンテナは内部でポート `8080` を待ち受けます。別のホストポートを使うには `HOARD_PORT=9000 docker compose up -d` とします。

## ユーザーと端末トークンを作成

サーバーにサインアップ画面はありません。ユーザーはコマンドラインで作成します。

```sh
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    user create alice --admin --password 'CHANGE_ME'
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    token create alice --device 'desktop'
```

トークンは一度だけ表示され、**後から取得することはできません**。今すぐコピーしてください。

## デスクトップアプリを接続

各マシンに [Hoard デスクトップアプリ](/download) をインストールします。オンボーディングで **Autohost** を選び、サーバーの URL と作成したトークンを貼り付けます。あとは Hoard Cloud とまったく同じで、ゲームを検出し、自動でバックアップし、バージョン履歴を保持します。日常的な使い方は [複数の PC 間でセーブを同期する](/guides/sync-game-saves-across-pcs) を参照してください。

## 本番運用

ローカルネットワークを越えて公開する場合は、リバースプロキシ（Caddy、nginx、Traefik）で TLS を終端し、`public_url` を実際の HTTPS アドレスに設定します。ベアメタルがよい場合は、リポジトリに `systemd` インストールスクリプトと、進行中の同期を止めずにバイナリをアトミックに入れ替える `hoard-server upgrade` コマンドも含まれています。

## セルフホストと Hoard Cloud のどちら？

すでにサーバーを運用していて容量制限なしの完全な管理を望むなら、セルフホストが最適です。インフラの保守をしたくない場合は、[Hoard Cloud](/pricing) が同じ同期をこちらで管理して提供し、無料プランから始められます。どちらでもアプリとセーブデータは可搬性を保つので、後から切り替えられます。
