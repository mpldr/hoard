# Self-host

### Docker

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml    # Use nano or vim or something lol
# public_url = "http://localhost:12421"
# (or use your IP if accessing from another machine)

cd deploy/docker
docker compose up -d              # pulls the prebuilt image from GHCR
# To build from source instead, uncomment `build:` in docker-compose.yml first.
docker compose logs -f server    # wait for "listening"

# create your user + a token for the desktop app
docker compose exec server hoard-admin --config /etc/hoard/config.toml user create myuser --admin --password 'mypassword'    #change "myuser" and "mypassword", dont delete ''.

docker compose exec server hoard-admin --config /etc/hoard/config.toml token create myuser --device 'desktop'    #changue "myuser" and if you want changue "desktop"
# SAVE THE TOKEN NOW — it cannot be retrieved later
```

In the app's onboarding, pick **Autohost**, paste the server URL and token.

### Bare metal + systemd

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
sudo ./deploy/scripts/install.sh
sudo $EDITOR /etc/hoard/config.toml    # Use nano or vim or something lol
sudo -u hoard hoard-admin --config /etc/hoard/config.toml db migrate
sudo -u hoard hoard-admin --config /etc/hoard/config.toml \ user create myuser --admin --password 'mypassword'    #change "myuser" and "mypassword", dont delete ''.
sudo systemctl start hoard-server
```

Upgrade later with `sudo hoard-server upgrade`: it swaps the binary
atomically and prints the `systemctl restart` step (it won't restart the
service itself, so an in-flight sync isn't killed).

## External storage (S3-compatible)

By default the server keeps every blob on local disk under `data_dir`. You can
instead point it at any **S3-compatible** bucket — MinIO, Backblaze B2,
Cloudflare R2, Garage, Wasabi, or `rclone serve s3` fronting Mega / Dropbox /
Google Drive — by adding a `[storage.s3]` block. Nothing else changes: the
client never talks to the bucket, there are no presigned URLs, and the upgrade
is zero-config for existing installs (omit the block and you stay on disk).

Important: **the server is still required.** It owns the SQLite index, the auth
tokens and the deduplication — the bucket only holds opaque zstd blob/chunk
bytes, sharded by content hash. The bucket *on its own* is not restorable: without
the server's database there is no mapping from saves/versions to those bytes.
The SQLite DB, `tmp/` upload staging and the upgrade marker always stay on local
disk regardless of backend, so `data_dir` must still be writable.

### MinIO

```sh
# bring up MinIO and create the bucket (console at :9001, or `mc mb`)
docker run -d -p 9000:9000 -p 9001:9000 \
  -e MINIO_ROOT_USER=hoard -e MINIO_ROOT_PASSWORD=change-me-please \
  -v /srv/minio:/data minio/minio server /data --console-address ":9001"
mc alias set h http://127.0.0.1:9000 hoard change-me-please
mc mb h/hoard
```

Then in `/etc/hoard/config.toml`:

```toml
[storage]
data_dir = "/var/lib/hoard"     # still holds the DB + tmp staging
backend  = "s3"

[storage.s3]
endpoint = "http://127.0.0.1:9000"
bucket = "hoard"
access_key_id = "hoard"
secret_access_key = "change-me-please"
force_path_style = true          # MinIO / Garage / rclone require this
```

On boot the server writes and deletes a probe object; a bad endpoint, bucket or
credential fails fast with a clear message instead of erroring mid-sync.

### rclone serve s3 (Mega / Dropbox / Drive, …)

There is no native Mega/Dropbox/Drive integration — the server speaks S3 only.
`rclone serve s3` bridges the gap: it exposes an S3 endpoint backed by any of
rclone's ~70 remotes.

```sh
# assuming you've already `rclone config`d a remote called `mega:`
rclone serve s3 mega:hoard \
  --auth-key hoardkey,hoardsecret \
  --addr 127.0.0.1:9100
```

```toml
[storage.s3]
endpoint = "http://127.0.0.1:9100"
bucket = "hoard"                 # a path inside the rclone remote
access_key_id = "hoardkey"
secret_access_key = "hoardsecret"
force_path_style = true
```

Object-storage backends behind a cloud drive are slower and rate-limited; keep
them on the same host/LAN as the server and expect restores to stream at the
remote's pace.

### Migrating between storage backends

Switching an existing install between `local` and `s3` (either direction) means
copying the blob/chunk bytes first — flipping `backend` alone would leave the
server pointing at an empty store (it refuses to boot in that case, telling you
to migrate). `hoard-admin storage` does the copy.

**Stop the server first.** Writes that arrive mid-migration would be missed.

```sh
# 1. See what you have and where.
sudo -u hoard hoard-admin --config /etc/hoard/config.toml storage status

# 2. Fill in the [storage.s3] block in config.toml (endpoint/bucket/keys),
#    but leave backend on its current value for now.

# 3. Stop the server, then copy every object to the new backend.
sudo systemctl stop hoard-server
sudo -u hoard hoard-admin --config /etc/hoard/config.toml storage migrate --to s3

# 4. Flip the switch and restart.
sudo $EDITOR /etc/hoard/config.toml        # [storage] backend = "s3"
sudo systemctl start hoard-server
```

`migrate` is idempotent and resumable — if it's interrupted, just run it again
and it skips whatever already copied. It **never deletes source data** unless
you pass `--delete-source`, and even then only after every object has been
copied and hash-verified. Going the other way is symmetric: `--to local`.

Once you've confirmed the new backend works (`storage verify --all` is green and
a restore succeeds), reclaim the old copy with a second run:

```sh
sudo -u hoard hoard-admin --config /etc/hoard/config.toml \
  storage migrate --to s3 --delete-source
```

`storage verify [--all | --sample N]` re-downloads objects and checks each one
hashes to its key — handy as a periodic bit-rot / integrity check, not just
after a migration. It exits nonzero if anything is missing or corrupt.

As with any S3 setup: the bucket only holds opaque zstd blob/chunk bytes. It is
**not restorable on its own** — without the server's SQLite database there is no
mapping from saves and versions to those bytes. Back up the DB too.

## Headless CLI

```sh
hoard config init --server http://YOUR_SERVER:12421
hoard login --token hoard_v1_...
hoard save create --game stardew-valley --label main
hoard backup <SAVE_ID> --from ~/.config/StardewValley/Saves --remember
hoard snapshots list <SAVE_ID>
hoard restore <SAVE_ID>
```