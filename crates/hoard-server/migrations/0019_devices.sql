-- Dispositivos de un usuario y su presencia en vivo (self-hosted).
--
-- Hasta la 1.1.2 esto sólo existía en Hoard Cloud, así que un self-hoster con
-- tres máquinas no tenía forma de saber cuáles eran ni cuál estaba encendida.
-- La procedencia de cada versión sí la tenía (`snapshots.device_name`); lo que
-- faltaba era el censo y el latido.
--
-- Una fila por (usuario, huella). La huella la calcula el cliente
-- (`hoard_agent::logship::device_identity`) y es estable para una máquina, así
-- que reinstalar la app no duplica el dispositivo.
--
-- `playing` es un JSON `[{"slug": "...", "since": "RFC3339"}]` con los juegos
-- que esa máquina está corriendo. Se guarda como texto porque es opaco para el
-- server: nadie consulta dentro, sólo se escribe entero y se devuelve entero.
--
-- No hay columna `online`: se deriva al leer de `last_seen_at` + `closed_at`.
-- Guardarla obligaría a que alguien la apagara, y una máquina que se apaga de
-- golpe no apaga nada — se quedaría encendida para siempre.
CREATE TABLE IF NOT EXISTS devices (
    id            TEXT PRIMARY KEY NOT NULL,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_name   TEXT NOT NULL,
    device_kind   TEXT,
    os            TEXT,
    app_version   TEXT,
    fingerprint   TEXT NOT NULL,
    playing       TEXT,
    last_seen_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    closed_at     TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    UNIQUE(user_id, fingerprint)
);

CREATE INDEX IF NOT EXISTS idx_devices_user ON devices(user_id, last_seen_at DESC);
