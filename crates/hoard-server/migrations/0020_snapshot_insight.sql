-- Qué cuenta una versión, derivado de su propio manifiesto. Gemela de la
-- 0048 de Postgres; el razonamiento está allí.
--
-- TEXT y no JSON: SQLite no tiene el tipo, y aquí tampoco se consulta dentro
-- del valor. Se escribe entero y se devuelve entero.
--
-- NULL = sin calcular: toda fila anterior a esto y toda versión sin manifiesto
-- por fichero.
ALTER TABLE snapshots ADD COLUMN insight TEXT;
