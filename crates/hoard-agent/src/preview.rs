//! Qué le va a pasar a la carpeta si se restaura esta versión.
//!
//! Restaurar es la operación que más miedo da de la app, y hasta ahora se
//! confirmaba a ciegas: el usuario elegía una fecha y aceptaba sin saber si eso
//! tocaba un fichero o los ochocientos. Este módulo responde a la pregunta
//! antes de escribir nada — cuántos ficheros cambian, cuáles aparecen y cuáles
//! sólo existen en el disco — **sin descargar un solo byte**.
//!
//! Sale gratis porque las dos mitades ya estaban: el servidor publica el
//! manifiesto por fichero de cada versión (ruta, sha256 y tamaño) y
//! [`crate::backup::walk_source`] es el mismo recorrido que usa el backup, ya
//! filtrado de symlinks y de los locks transitorios que deja un juego abierto.
//! Sólo hay que cruzarlos.
//!
//! El cruce se hace en dos pasadas para no leer de más: primero por ruta y
//! tamaño, que resuelve gratis todo lo que aparece, desaparece o cambia de
//! tamaño; y sólo lo que coincide en ruta **y** en tamaño se hashea, porque es
//! lo único que puede ser "el mismo fichero" o "distinto contenido, mismo
//! tamaño" (un save de tamaño fijo, que es justo el caso frecuente).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use hoard_core::kernel::fileclass::RestoreGate;
use serde::Serialize;

/// Un fichero de la versión remota, en lo mínimo que hace falta para comparar.
/// Se construye igual desde el manifiesto de Cloud que desde el listado
/// self-hosted, que llevan los mismos tres datos con distinto nombre.
#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub relative_path: String,
    pub size_bytes: u64,
    /// `None` = **no se sabe**, no "hash malo". Las versiones legacy de archivo
    /// entero no tienen digest por fichero, y ese hueco se propaga hasta la UI
    /// en vez de fingir una comparación que no se ha hecho.
    pub sha256: Option<String>,
}

/// Un fichero que ya está en la carpeta de destino.
#[derive(Debug, Clone)]
pub struct LocalFile {
    pub relative_path: String,
    pub size_bytes: u64,
}

/// Lo que la restauración le hará a la carpeta.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RestorePreview {
    /// Ficheros que la versión trae idénticos a los que ya hay: no se tocan.
    pub unchanged: usize,
    /// Están en los dos lados con contenido distinto: se sobrescriben.
    pub modified: Vec<String>,
    /// Sólo están en la versión: se crean.
    pub added: Vec<String>,
    /// Sólo están en el disco. **No se borran** — la restauración escribe
    /// encima, no sincroniza en espejo — pero el usuario merece verlos: son
    /// las partidas que hizo después de la versión que está a punto de traer.
    pub local_only: Vec<String>,
    /// Bytes que hay que escribir (lo modificado más lo añadido).
    pub bytes_to_write: u64,
    /// `false` cuando la versión no publica hashes por fichero (las legacy de
    /// archivo entero). Entonces `modified` y `unchanged` no se pueden
    /// distinguir y la UI debe decir que no puede previsualizar, en vez de
    /// enseñar un diff vacío que se leería como "no cambia nada".
    pub comparable: bool,
}

/// Cuántas rutas se enumeran como mucho en cada lista. Un save con ochocientos
/// ficheros no cabe en un diálogo, y el recuento ya va aparte en los totales.
const MAX_LISTED: usize = 200;

/// El cruce, sin tocar disco: recibe los dos lados ya leídos y un veredicto de
/// igualdad por fichero, y decide.
///
/// `same_bytes` sólo se consulta para las rutas que coinciden en los dos lados
/// **y** tienen el mismo tamaño; para el resto la respuesta ya está decidida y
/// no hay que hashear nada.
pub fn diff(
    remote: &[RemoteFile],
    local: &[LocalFile],
    mut same_bytes: impl FnMut(&str) -> bool,
) -> RestorePreview {
    let local_by_path: HashMap<&str, &LocalFile> = local
        .iter()
        .map(|f| (f.relative_path.as_str(), f))
        .collect();
    let remote_paths: HashSet<&str> = remote.iter().map(|f| f.relative_path.as_str()).collect();

    let comparable = remote.iter().all(|f| f.sha256.is_some());
    let mut out = RestorePreview {
        comparable,
        ..Default::default()
    };

    for r in remote {
        match local_by_path.get(r.relative_path.as_str()) {
            None => {
                out.bytes_to_write += r.size_bytes;
                push_capped(&mut out.added, &r.relative_path);
            }
            Some(l) if l.size_bytes != r.size_bytes => {
                out.bytes_to_write += r.size_bytes;
                push_capped(&mut out.modified, &r.relative_path);
            }
            Some(_) => {
                // Mismo tamaño: hay que mirar el contenido. Sin hash publicado
                // no se puede afirmar que sea igual, así que cuenta como que
                // se sobrescribe — el lado seguro, y `comparable` ya avisa de
                // que la cuenta es una cota superior.
                if comparable && same_bytes(&r.relative_path) {
                    out.unchanged += 1;
                } else {
                    out.bytes_to_write += r.size_bytes;
                    push_capped(&mut out.modified, &r.relative_path);
                }
            }
        }
    }

    for l in local {
        if !remote_paths.contains(l.relative_path.as_str()) {
            push_capped(&mut out.local_only, &l.relative_path);
        }
    }

    out.modified.sort();
    out.added.sort();
    out.local_only.sort();
    out
}

/// Añade a la lista hasta el tope. Pasado el tope se deja de enumerar, pero el
/// contador de la lista se sigue incrementando por fuera (ver `len` vs lo que
/// se enseña): aquí simplemente no crece más.
fn push_capped(list: &mut Vec<String>, path: &str) {
    if list.len() < MAX_LISTED {
        list.push(path.to_string());
    }
}

/// Lee la carpeta de destino y cruza con el manifiesto ya descargado.
///
/// Hashea sólo las rutas que coinciden en nombre y tamaño con la versión; el
/// resto se resuelve sin abrir el fichero. En el peor caso lee tanto como ocupa
/// el save, que es el mismo presupuesto que ya se gasta la deduplicación contra
/// disco de un restore real.
pub async fn against_disk(
    remote: &[RemoteFile],
    dest: &Path,
    gate: &RestoreGate,
) -> Result<RestorePreview> {
    // Lo que la puerta no deja pasar no se va a escribir, así que no puede
    // aparecer en la previsualización como si fuera a escribirse. La misma
    // decisión que toma el restore —incluida la excepción del save de fichero
    // suelto—, no una copia de ella que se le vaya separando.
    let names: Vec<&str> = remote.iter().map(|f| f.relative_path.as_str()).collect();
    let filtered: Vec<RemoteFile>;
    let remote = if crate::restore::is_single_file_snapshot(dest, &names) {
        remote
    } else {
        filtered = remote
            .iter()
            .filter(|f| gate.allows(&f.relative_path))
            .cloned()
            .collect();
        &filtered[..]
    };
    let local: Vec<LocalFile> = match crate::backup::walk_source(dest, &gate.shields) {
        Ok(files) => files
            .into_iter()
            .map(|f| LocalFile {
                relative_path: f.relative_path,
                size_bytes: f.size_bytes,
            })
            .collect(),
        // Carpeta que aún no existe (equipo nuevo): todo son altas. No es un
        // error, es el caso más común de un restore.
        Err(_) => Vec::new(),
    };

    // Sólo las candidatas ambiguas: misma ruta, mismo tamaño.
    let local_sizes: HashMap<&str, u64> = local
        .iter()
        .map(|f| (f.relative_path.as_str(), f.size_bytes))
        .collect();
    let mut equal: HashSet<String> = HashSet::new();
    for r in remote {
        let Some(sha) = r.sha256.as_deref() else {
            continue;
        };
        if local_sizes.get(r.relative_path.as_str()) != Some(&r.size_bytes) {
            continue;
        }
        let path = dest.join(&r.relative_path);
        // Un fichero que no se puede leer (lock de un juego abierto, permisos)
        // cuenta como distinto: la previsualización se pasa de precavida antes
        // que prometer que algo no se toca.
        if let Ok(actual) = crate::backup::hash_file(&path).await {
            if actual.eq_ignore_ascii_case(sha) {
                equal.insert(r.relative_path.clone());
            }
        }
    }

    Ok(diff(remote, &local, |p| equal.contains(p)))
}

/// El manifiesto de una versión, venga de donde venga.
///
/// Las dos mitades del producto publican los mismos tres datos por fichero con
/// nombres distintos, así que se normalizan aquí y el resto del módulo no se
/// entera de con qué servidor habla. `presign = false`: esto es una consulta,
/// no una descarga, y no debe gastar cuota de ancho de banda.
pub async fn remote_files(
    client: &crate::api::ApiClient,
    save_id: &str,
    version: i64,
) -> Result<Vec<RemoteFile>> {
    if client.is_cloud().await {
        let manifest = client
            .cloud_version_manifest(save_id, version, false)
            .await?;
        if !manifest.content_addressed {
            // Versión legacy de archivo entero: no hay listado por fichero. Se
            // devuelve vacío y `diff` lo marcará como no comparable.
            return Ok(Vec::new());
        }
        return Ok(manifest
            .files
            .into_iter()
            .map(|f| RemoteFile {
                relative_path: f.relative_path,
                size_bytes: f.size_bytes.max(0) as u64,
                sha256: (!f.sha256.is_empty()).then_some(f.sha256),
            })
            .collect());
    }
    let detail = client.snapshot_detail(save_id, version).await?;
    Ok(detail
        .files
        .into_iter()
        .map(|f| RemoteFile {
            relative_path: f.relative_path,
            size_bytes: f.size_bytes.max(0) as u64,
            sha256: f.sha256.map(|s| s.to_string()),
        })
        .collect())
}

/// La previsualización completa: trae el manifiesto y lo cruza con el disco.
///
/// Un manifiesto vacío (versión legacy, o una versión sin ficheros) sale como
/// no comparable, nunca como "no cambia nada".
pub async fn restore_preview(
    client: &crate::api::ApiClient,
    save_id: &str,
    version: i64,
    dest: &Path,
    gate: &RestoreGate,
) -> Result<RestorePreview> {
    let remote = remote_files(client, save_id, version).await?;
    if remote.is_empty() {
        return Ok(RestorePreview {
            comparable: false,
            ..Default::default()
        });
    }
    against_disk(&remote, dest, gate).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(path: &str, size: u64, sha: Option<&str>) -> RemoteFile {
        RemoteFile {
            relative_path: path.to_string(),
            size_bytes: size,
            sha256: sha.map(str::to_string),
        }
    }
    fn l(path: &str, size: u64) -> LocalFile {
        LocalFile {
            relative_path: path.to_string(),
            size_bytes: size,
        }
    }

    #[test]
    fn a_size_change_needs_no_hash_at_all() {
        let remote = vec![r("save.dat", 200, Some("aa"))];
        let local = vec![l("save.dat", 100)];
        let out = diff(&remote, &local, |_| panic!("no debería hashear"));
        assert_eq!(out.modified, vec!["save.dat"]);
        assert_eq!(out.bytes_to_write, 200);
    }

    #[test]
    fn same_size_different_bytes_is_a_modification() {
        // El caso que obliga a hashear: un save de tamaño fijo que cambia de
        // contenido sin cambiar de tamaño.
        let remote = vec![r("slot1.sav", 4096, Some("aa"))];
        let local = vec![l("slot1.sav", 4096)];

        let same = diff(&remote, &local, |_| true);
        assert_eq!(same.unchanged, 1);
        assert!(same.modified.is_empty());
        assert_eq!(same.bytes_to_write, 0);

        let differs = diff(&remote, &local, |_| false);
        assert_eq!(differs.modified, vec!["slot1.sav"]);
        assert_eq!(differs.bytes_to_write, 4096);
    }

    #[test]
    fn files_only_on_disk_are_reported_but_never_counted_as_writes() {
        let remote = vec![r("a.sav", 10, Some("aa"))];
        let local = vec![l("a.sav", 10), l("b.sav", 99)];
        let out = diff(&remote, &local, |_| true);
        assert_eq!(out.local_only, vec!["b.sav"]);
        assert_eq!(out.bytes_to_write, 0);
    }

    #[test]
    fn a_version_without_per_file_hashes_says_so_instead_of_showing_no_changes() {
        // Versión legacy de archivo entero: sin digests no se puede afirmar que
        // nada cambia, y decir "0 cambios" sería mentir en la dirección peor.
        let remote = vec![r("save.dat", 10, None)];
        let local = vec![l("save.dat", 10)];
        let out = diff(&remote, &local, |_| true);
        assert!(!out.comparable);
        assert_eq!(out.unchanged, 0);
        assert_eq!(out.modified, vec!["save.dat"]);
    }

    #[test]
    fn an_empty_destination_makes_everything_an_addition() {
        let remote = vec![r("a.sav", 10, Some("aa")), r("b.sav", 20, Some("bb"))];
        let out = diff(&remote, &[], |_| true);
        assert_eq!(out.added.len(), 2);
        assert_eq!(out.bytes_to_write, 30);
        assert!(out.local_only.is_empty());
    }

    #[tokio::test]
    async fn against_disk_reads_the_folder_and_hashes_only_the_ambiguous_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("igual.sav"), b"hola").unwrap();
        std::fs::write(dir.path().join("distinto.sav"), b"adio").unwrap();
        std::fs::write(dir.path().join("sobra.sav"), b"xxxx").unwrap();

        // sha256("hola")
        let sha_hola = "b221d9dbb083a7f33428d7c2a3c3198ae925614d70210e28716ccaa7cd4ddb79";
        let remote = vec![
            r("igual.sav", 4, Some(sha_hola)),
            r("distinto.sav", 4, Some(sha_hola)),
            r("nuevo.sav", 7, Some("cc")),
        ];

        let out = against_disk(&remote, dir.path(), &RestoreGate::permissive()).await.unwrap();
        assert_eq!(out.unchanged, 1);
        assert_eq!(out.modified, vec!["distinto.sav"]);
        assert_eq!(out.added, vec!["nuevo.sav"]);
        assert_eq!(out.local_only, vec!["sobra.sav"]);
        assert_eq!(out.bytes_to_write, 4 + 7);
        assert!(out.comparable);
    }
}
