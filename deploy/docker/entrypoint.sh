#!/bin/sh
# Container entrypoint:
#   - Bootstrap config from the example if /etc/hoard/config.toml is missing
#     (handy when you forgot to mount one).
#   - Run pending migrations before exec'ing the server.
#   - exec the requested command (CMD).

set -eu

CFG="${HOARD_CONFIG_PATH:-/etc/hoard/config.toml}"
EXAMPLE="/etc/hoard/config.toml.example"

if [ ! -f "$CFG" ]; then
  if [ -f "$EXAMPLE" ]; then
    echo "entrypoint: no config at $CFG, copying from example (review for production!)" >&2
    cp "$EXAMPLE" "$CFG" 2>/dev/null || {
      echo "entrypoint: cannot write $CFG (read-only mount?). Mount one explicitly." >&2
      exit 1
    }
  else
    # El compose monta ./config en /etc/hoard, así que ese mount TAPA el
    # ejemplo que trae la imagen: si la carpeta va vacía no hay ni config ni
    # ejemplo. Se dice el comando exacto en vez de "no example available".
    echo "entrypoint: no config at $CFG (the ./config mount hides the image's example)." >&2
    echo "entrypoint: from the repo root, run:" >&2
    echo "entrypoint:   mkdir -p deploy/docker/config" >&2
    echo "entrypoint:   cp deploy/config.toml.example deploy/docker/config/config.toml" >&2
    echo "entrypoint: edit it if you want (the defaults work as-is), then 'docker compose up -d' again." >&2
    exit 1
  fi
fi

# Only run migrations when we're actually starting the server. This keeps
# `docker compose exec server hoard-admin ...` (and other one-off commands)
# fast and avoids a recursive admin call.
case "${1:-}" in
  hoard-server|/usr/local/bin/hoard-server)
    echo "entrypoint: running database migrations…" >&2
    hoard-admin --config "$CFG" db migrate
    ;;
esac

exec "$@"
