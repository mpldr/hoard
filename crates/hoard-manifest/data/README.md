# Hoard manifest catalogue

Save-path definitions used by `hoard-manifest`. Two zstd-compressed blobs,
both embedded in the binary so detection works fully offline on a machine
that just installed the app.

## `ludusavi-catalog.json.zst` — the games we can back up

Compact JSON derived from the [Ludusavi manifest][ludusavi]
(`mtkennerly/ludusavi-manifest`, MIT-licensed manifest tooling). The
underlying data is sourced from [PCGamingWiki][pcgw] and is licensed
**CC-BY-NC-SA-3.0**.

One entry per manifest game that has at least one save path or registry
key. Beyond the paths, each entry carries what detection needs to *resolve*
them and to recognise the game while it runs:

| Field | From | Used for |
|---|---|---|
| `paths` | `files:` | the save-path templates themselves |
| `registry` | `registry:` | the Windows registry stage |
| `install_dirs` | `installDir:` | resolving `<base>` / `<game>` templates |
| `launch_exes` | `launch:` | "is the user playing this?" — process matching |
| `steam_extra_ids` | `id.steamExtra` | regional/demo appids that are the same game |
| `lutris_slug` | `id.lutris` | naming a Lutris prefix exactly |
| `cloud_steam` | `cloud.steam` | informational notice only — never affects detection |

Path templates use Ludusavi's bracket syntax (`<winAppData>`, `<xdgData>`,
`<home>`, `<base>`, …) — expanded by `hoard-agent::pathexpand`.

The hand-curated TOML catalog that lived alongside this JSON was removed
in 1.5.0 (see ADR `0009-path-detection-overhaul`). The Ludusavi catalog
is now the single source of truth for save-path templates.

## `ludusavi-titles.json.zst` — games we can only *name*

Two thirds of the manifest is games with a title, an appid and a `launch:`
block but **no save path**. They can never be tracked, so they'd only slow
the catalog down — but they answer "what game is this process / appid?",
which is what phase-4 attribution and the untracked-process notice need in
order to stop naming a save after whatever happened to be running.

Kept deliberately minimal: display name, Steam appid, executable basenames.

[ludusavi]: https://github.com/mtkennerly/ludusavi-manifest
[pcgw]: https://www.pcgamingwiki.com/

## Refreshing both

```bash
curl -fsSL \
  https://raw.githubusercontent.com/mtkennerly/ludusavi-manifest/master/data/manifest.yaml \
  -o /tmp/manifest.yaml

GEN_CATALOG=/tmp/manifest.yaml \
  cargo test -p hoard-manifest --release -- --ignored regenerate --nocapture
```

The generator is a `#[ignore]`d test rather than a script **on purpose**: it
runs `convert_yaml`, the same function the in-app "update catalog" button
calls, so what ships can never drift from what a runtime refresh produces.
The raw YAML is not committed — only the two compressed blobs are.

`convert-ludusavi.py` is the original converter, kept for reference only.
It predates every field in the table above and would emit an incomplete
catalog; do not use it.
