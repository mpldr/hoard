-- Veredicto de integridad por blob, para la pasada `hoard-server verify-blobs`.
--
-- Existe por la corrupción por rotación de ago-2026: el cliente hasheaba el
-- fichero y lo subía en dos lecturas distintas, y si el juego rotaba el save
-- entre medias (`save` → `save.bak` y un `save` nuevo en su sitio) el objeto
-- acababa con bytes que no son los que su sha256 promete. El cliente ya no
-- puede producirlos —hashea el propio stream del PUT y aborta antes de
-- confirmar—, pero los que se confirmaron entonces siguen ahí y nada los
-- distingue de un blob sano: sólo se nota al restaurar.
--
--   verified_at  Cuándo se leyó el objeto entero y se comprobó su hash.
--                NULL = nunca verificado.
--   integrity    Veredicto de esa lectura: 'ok', 'mismatch' (los bytes no
--                hashean a su sha256) o 'missing' (no está en el bucket).
--                NULL mientras no se haya verificado.
ALTER TABLE cloud_blobs
    ADD COLUMN IF NOT EXISTS verified_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS integrity TEXT;

-- Barrido de la pasada: los que nunca se han mirado, primero los más viejos.
-- Parcial, así que los ya verificados ni ocupan índice ni se vuelven a leer.
CREATE INDEX IF NOT EXISTS idx_cloud_blobs_unverified
    ON cloud_blobs(created_at) WHERE verified_at IS NULL;

-- Y el índice para encontrar rápido lo que hay que mirar/avisar: los rotos.
CREATE INDEX IF NOT EXISTS idx_cloud_blobs_corrupt
    ON cloud_blobs(user_id) WHERE integrity IS NOT NULL AND integrity <> 'ok';
