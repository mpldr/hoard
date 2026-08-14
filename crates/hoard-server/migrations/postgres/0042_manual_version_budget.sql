-- Presupuesto propio para las versiones que el usuario hizo a propósito.
--
-- `max_versions` era un único cupo para todas, y en una partida que autoguarda
-- cada minuto eso significa que una sesión de juego lo llena entero: la copia
-- que alguien hizo a mano antes de un jefe la echaba del historial la ráfaga de
-- copias automáticas de los cinco minutos siguientes. Justamente la que quería
-- conservar.
--
-- Con dos cupos, una ráfaga automática sólo puede desplazar a otras
-- automáticas. NULL = sin límite, que es el valor por defecto y deliberado
-- para las manuales: son pocas y son las que importan.
--
-- Qué versión es de qué clase se lee de `save_versions.notes`, que ya existía
-- y nadie rellenaba. Nulo (todo lo subido hasta ahora) = automática, que es lo
-- que era, así que no hay nada que migrar.
ALTER TABLE public.profiles
    ADD COLUMN IF NOT EXISTS max_manual_versions integer;

-- La retención consulta por save + clase, y sin esto cada poda recorría la
-- tabla entera de versiones del usuario para contar cuántas hay más nuevas.
CREATE INDEX IF NOT EXISTS save_versions_save_notes_idx
    ON public.save_versions (save_id, notes)
    WHERE deleted_at IS NULL;
