# Contributing:

You can help Hoard in many ways:

- **Report bugs** — [open an issue](https://github.com/rleeon/hoard/issues) or join the [Discord](https://discord.gg/CyFEk5T3G8)
- **Test and debug** — try the app and report what breaks
- **Suggest features** — open an issue describing what you'd like
- **Submit code** — fork, make your changes, and open a PR

Im a solo dev, no contribution is too small. Even finding a bug helps.


## Building from source

```sh
# Server / CLI / admin
cargo build --release -p hoard-server -p hoard-cli -p hoard-admin

# Desktop app (needs Node 20 + pnpm 9 + Tauri prerequisites)
pnpm --dir crates/hoard-desktop/ui install
cargo install tauri-cli --version '^2'
cargo tauri build --manifest-path crates/hoard-desktop/Cargo.toml
```

Linux build prerequisites: `libwebkit2gtk-4.1-dev`, `build-essential`,
`curl`, `wget`, `file`, `libssl-dev`, `libayatana-appindicator3-dev`,
`librsvg2-dev`, `patchelf`. See `.github/workflows/release-desktop.yml`
for the canonical list.

## Releasing

The version lives in three files (`Cargo.toml`, `tauri.conf.json`,
`ui/package.json`). Don't edit them by hand — stamp all three from one number:

```sh
node scripts/stamp-version.mjs 7.7.7   # or no arg to use the latest git tag
git commit -am "release: 7.7.7" && git tag v7.7.7 && git push --tags
```

The version shown on the website and in the app's update check is read live
from [`releases/latest`](https://github.com/rleeon/hoard/releases/latest), so
once the tagged release is published everything reports the same number with
no further edits.

## Architecture

A Rust workspace of 8 crates:

| Crate | Role |
| --- | --- |
| `hoard-core` | shared types, hashing, file-walk primitives |
| `hoard-manifest` | Ludusavi manifest parser |
| `hoard-watcher` | filesystem + process watchers (reusable lib) |
| `hoard-agent` | sync engine: backup, restore, detection; talks to the server |
| `hoard-cli` | the headless `hoard` binary |
| `hoard-admin` | server-side admin CLI (users, tokens, db) |
| `hoard-server` | Axum HTTP server; owns the DB and storage |
| `hoard-desktop` | Tauri 2 + Svelte 5 desktop app wrapping `hoard-agent` |

`hoard-server` runs in two modes from one binary. Self-hosted uses SQLite +
on-disk snapshots under `data_dir`, with bearer-token auth and the
`/v1/saves` API. Built with `--features cloud` it becomes **Hoard Cloud**:
Postgres + Cloudflare R2 object storage, Supabase JWT auth, and a
presigned-upload `/v1/cloud/*` API. The client picks the right protocol from
`/v1/health`'s `mode` field. The marketing + account site lives in `web/`
(SvelteKit, deployed to GitHub Pages).

Two more crates live in this repo under AGPL too: `hoard-screen` (the in-game
overlay behind Hoard Screen) and `hoard-wrapple` (the recap engine behind Hoard
Wrapped). Anyone can build them — but the Hoard Screen entitlement is signed
server-side, so a self-hosted build never gets it unlocked. There's nothing to
patch out locally: the gate isn't in the client, it's the server refusing to
sign.