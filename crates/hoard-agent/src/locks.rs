//! ¿Está el juego escribiendo el save AHORA MISMO?
//!
//! Sonda del sistema de ficheros, independiente de la tabla de procesos: se
//! intenta abrir cada fichero del save **en solo lectura** y, si el SO
//! responde que otro proceso lo tiene en exclusiva, es que se está escribiendo.
//!
//! Vale la pena precisamente porque no depende de reconocer al juego. Toda la
//! detección de sesión de `agent::process_poll` parte de casar un proceso con
//! el save (nombre, carpeta de instalación, handles, correlación); un juego que
//! no casa con nada aparece como "parado" mientras guarda la partida, y ahí un
//! backup copia un fichero a medias y un restore lo pisa. Esto lo cubre sin
//! saber nada del juego.
//!
//! **Sólo Windows puede afirmarlo.** En POSIX un `open()` de lectura no falla
//! porque otro proceso esté escribiendo (no hay bloqueo obligatorio), así que
//! ahí la sonda devuelve `false` y mandan los guards de siempre — que en Linux
//! son fuertes: `agent.rs` ya casa por `/proc/<pid>/fd`, la vía que en Windows
//! no existe. Las dos plataformas acaban cubiertas, cada una por su lado.

use std::path::Path;

/// Cuántos ficheros se sondean como mucho. La sonda corre en cada tick del
/// poll con un juego vivo, y una carpeta de 4000 saves (el caso `swarm` del
/// banco de pruebas) no puede convertirse en 4000 `open()` cada dos segundos.
/// Con que UNO esté bloqueado ya está contestada la pregunta, y el que el
/// juego tiene abierto suele ser de los primeros.
const MAX_PROBED_FILES: usize = 64;

/// Profundidad máxima al buscar ficheros que sondear.
const MAX_DEPTH: usize = 3;

/// `true` si algún fichero bajo `path` está abierto en exclusiva por otro
/// proceso. `path` puede ser un fichero suelto o una carpeta.
///
/// Conservador ante la duda: cualquier error que no sea un bloqueo declarado
/// (no existe, sin permisos, se borró a media sonda) cuenta como NO bloqueado.
/// Tratar "sin permisos" como bloqueo es lo que dejaba el bucle de espera
/// girando para siempre en la versión original de esta idea.
pub fn any_file_locked(path: &Path) -> bool {
    let mut budget = MAX_PROBED_FILES;
    probe(path, MAX_DEPTH, &mut budget)
}

fn probe(path: &Path, depth: usize, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.is_file() {
        *budget -= 1;
        return is_file_locked(path);
    }
    if !meta.is_dir() || depth == 0 {
        return false;
    }
    let Ok(read) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in read.flatten() {
        if *budget == 0 {
            return false;
        }
        if probe(&entry.path(), depth - 1, budget) {
            return true;
        }
    }
    false
}

/// Windows: abrir en solo lectura y mirar el error.
///
/// Se abre **sólo para leer** a propósito: los saves nunca se escriben durante
/// un backup, así que pedir escritura daría falsos positivos con cualquier
/// fichero de solo lectura. Y sólo cuentan los dos errores que significan de
/// verdad "otro proceso lo tiene": un permiso denegado normal NO es un
/// bloqueo, y tratarlo como tal congelaría el save para siempre.
#[cfg(windows)]
fn is_file_locked(path: &Path) -> bool {
    /// `ERROR_SHARING_VIOLATION`
    const SHARING_VIOLATION: i32 = 32;
    /// `ERROR_LOCK_VIOLATION`
    const LOCK_VIOLATION: i32 = 33;
    match std::fs::File::open(path) {
        Ok(_) => false,
        Err(e) => matches!(
            e.raw_os_error(),
            Some(SHARING_VIOLATION) | Some(LOCK_VIOLATION)
        ),
    }
}

/// POSIX: no hay bloqueo obligatorio, así que un `open()` de lectura no puede
/// contestar la pregunta. Se devuelve `false` en vez de inventar una respuesta.
#[cfg(not(windows))]
fn is_file_locked(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_save_folder_is_not_locked() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("slot1.sav"), b"x").unwrap();
        std::fs::create_dir(tmp.path().join("autosave")).unwrap();
        std::fs::write(tmp.path().join("autosave/a.sav"), b"x").unwrap();
        assert!(!any_file_locked(tmp.path()));
    }

    #[test]
    fn a_missing_path_is_not_locked() {
        assert!(!any_file_locked(Path::new("/definitely/not/here")));
    }

    /// Un árbol grande no puede convertirse en miles de `open()` por tick.
    #[test]
    fn the_probe_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..500 {
            std::fs::write(tmp.path().join(format!("s{i}.sav")), b"x").unwrap();
        }
        let mut budget = MAX_PROBED_FILES;
        probe(tmp.path(), MAX_DEPTH, &mut budget);
        assert_eq!(budget, 0, "debería haber agotado el presupuesto, no más");
    }

    /// En POSIX la sonda no puede afirmar nada, y eso es lo correcto: nunca
    /// debe frenar un backup por un fichero que simplemente está abierto.
    #[cfg(unix)]
    #[test]
    fn posix_never_reports_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("held.sav");
        std::fs::write(&f, b"x").unwrap();
        let _held = std::fs::OpenOptions::new().write(true).open(&f).unwrap();
        assert!(!any_file_locked(tmp.path()));
    }
}
