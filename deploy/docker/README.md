# Docker deployment

This directory contains everything needed to run Hoard in a container.

## Quick start (prebuilt image)

The compose file pulls a prebuilt multi-arch image from GHCR
(`ghcr.io/rleeon/hoard`), so no local build is needed:

```sh
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml   # set public_url, retention, etc.

cd deploy/docker
docker compose up -d
docker compose logs -f
```

Updating later is just:

```sh
docker compose pull && docker compose up -d
```

Pin a version tag (`ghcr.io/rleeon/hoard:1.0`) instead of `:latest` in
`docker-compose.yml` for reproducible upgrades.

## Building from source instead

Uncomment the `build:` block in `docker-compose.yml` (it overrides the pull),
or build the image by hand:

```sh
# from repo root
docker build -t hoard:latest -f deploy/docker/Dockerfile .
```

## Creating an admin user

```sh
docker compose exec server hoard-admin \
    --config /etc/hoard/config.toml \
    user create alice --admin --password 'CHANGE_ME'
docker compose exec server hoard-admin \
    --config /etc/hoard/config.toml \
    token create alice --device 'first-laptop'
```

The token is printed to stdout — copy it now, it cannot be recovered.

## Notes

- The image listens on `12421` internally; map a different host port via
  `HOARD_PORT=9000 docker compose up -d`.
- Data lives in a named volume `hoard-data`. Back it up like any other volume.
- For production, terminate TLS at a reverse proxy (Caddy / nginx / Traefik)
  and set `public_url` accordingly.
- The healthcheck hits `/v1/health` via `wget` (installed in the image).
  Compose's healthcheck overrides the Dockerfile one with the same probe.
