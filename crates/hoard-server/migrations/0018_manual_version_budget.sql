-- Presupuesto propio para los snapshots que el usuario hizo a propósito.
--
-- `max_versions` era un único cupo para todos. En una partida que autoguarda
-- cada minuto eso significa que una sesión llena el historial entero, y la
-- copia que alguien hizo a mano antes de un jefe la echa la ráfaga de copias
-- automáticas de los cinco minutos siguientes. Con dos cupos, una ráfaga
-- automática sólo puede desplazar a otras automáticas.
--
-- NULL = sin límite, y es el defecto deliberado para las manuales: son pocas y
-- son las que importan. Qué snapshot es de qué clase se lee de `snapshots.notes`
-- ('manual' / 'pre-restore'); nulo = automático, que es lo que son todas las
-- filas anteriores a esto, así que no hay que migrar datos.
ALTER TABLE users ADD COLUMN max_manual_versions INTEGER;

CREATE INDEX IF NOT EXISTS snapshots_save_notes_idx
    ON snapshots (save_id, notes)
    WHERE deleted_at IS NULL;
