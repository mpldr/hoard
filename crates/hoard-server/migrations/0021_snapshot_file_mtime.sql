-- El mtime de origen de cada fichero de un snapshot.
--
-- Cloud lo guarda desde que existe el CAS (`save_version_files.modified_at`);
-- aquí no llegaba siquiera por el wire. Sin él, el historial no puede decir
-- *qué* partida se tocó: con 70 mundos en la carpeta, todos parecen igual de
-- recientes y la fila acaba nombrando al más grande.
--
-- NULL = el cliente no lo mandó (todos los anteriores a esto) o el sistema de
-- ficheros no lo reportó. Nunca cero: cero es una fecha, "no sé" no lo es.
ALTER TABLE snapshot_files ADD COLUMN modified_at INTEGER;
