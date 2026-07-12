# Self-host

In a 1.0.3 im can look to add more fuctions to self-host, like cloud in mega, drop-box or something.

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

## Headless CLI

```sh
hoard config init --server http://YOUR_SERVER:12421
hoard login --token hoard_v1_...
hoard save create --game stardew-valley --label main
hoard backup <SAVE_ID> --from ~/.config/StardewValley/Saves --remember
hoard snapshots list <SAVE_ID>
hoard restore <SAVE_ID>
```