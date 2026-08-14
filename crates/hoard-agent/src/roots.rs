//! DETECCIÓN — enumeración de roots de usuario (fase 0, ADR 0020).
//!
//! Lista los directorios raíz donde los juegos guardan saves, por SO,
//! derivada de los placeholders que `pathexpand` ya sabe expandir
//! (`<winAppData>`, `<winLocalAppDataLow>`, `<xdgData>`, …). Es la base del
//! scan automático catalog-free: el walk por señales (fase 1+) debe
//! recorrer ESTOS roots, no sólo `install_dir` + `drive_c/users/steamuser`.
//!
//! NOTA DE INTEGRACIÓN: este módulo es la cimentación de la fase 0. Todavía
//! NO está cableado en `detection::detect_all` — recorrer todo el HOME por
//! cada slug sin resolver sería I/O explosiva, así que el cableado real
//! espera a la fase 4 (atribución), que asocia candidatos sueltos a juegos.
//! Aquí sólo se provee la lista de roots, deduplicada y filtrada a los que
//! existen en el host.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::manifest::Os;
use crate::pathexpand::expand_path;

/// Templates de roots de usuario por SO (placeholders estilo Ludusavi).
fn root_templates(os: Os) -> &'static [&'static str] {
    match os {
        Os::Windows => &[
            "<winAppData>",         // Roaming
            "<winLocalAppData>",    // Local
            "<winLocalAppDataLow>", // LocalLow — Unity Application.persistentDataPath
            "<winSavedGames>",
            "<home>/Documents",
            "<home>/Documents/My Games",
        ],
        Os::Linux => &[
            "<xdgData>",   // ~/.local/share
            "<xdgConfig>", // ~/.config
            "<home>/.local/state",
            "<home>/Documents",
            // Juegos nativos (no-Proton) que escriben en el "Saved Games" estilo
            // Windows dentro del HOME (Unity/Unreal multiplataforma, varios
            // indies). Sin esto, sólo se miraba dentro de prefijos Wine.
            "<home>/Saved Games",
        ],
        Os::Mac => &["<macAppSupport>", "<macPreferences>", "<home>/Documents"],
    }
}

/// Roots de usuario nativos que existen en este host, deduplicados.
pub fn user_save_roots(os: Os) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tmpl in root_templates(os) {
        for p in expand_path(tmpl, os) {
            if seen.insert(p.clone()) && p.is_dir() {
                out.push(p);
            }
        }
    }
    out
}

/// Roots adicionales que SOLO recorre el escaneo profundo (Linux): el gaming
/// confinado en sandboxes y los emuladores, que el tick periódico no mira por
/// coste. Cubre:
///
/// - **Flatpak**: datos por-app en `~/.var/app/<id>/{config,data,.local/share,
///   .config}` — Steam Deck, Heroic/Lutris/Bottles Flatpak, emuladores
///   EmuDeck/RetroDECK.
/// - **Snap**: `~/snap/<app>/{common,current}/.local/share` y `/.config`.
/// - **EmuDeck / RetroDECK**: `~/Emulation/saves`, `~/Emulation/storage`, y
///   las copias en microSD `/run/media/<user>/<label>/Emulation/saves`.
///
/// Todos filtrados a los que existen; vacío en SO que no sean Linux.
pub fn deep_save_roots(os: Os) -> Vec<PathBuf> {
    if !matches!(os, Os::Linux) {
        return Vec::new();
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let push = |p: PathBuf, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>| {
        if seen.insert(p.clone()) && p.is_dir() {
            out.push(p);
        }
    };

    // Flatpak: one entry per installed app id under ~/.var/app.
    if let Ok(entries) = std::fs::read_dir(home.join(".var/app")) {
        for app in entries.flatten().map(|e| e.path()) {
            for sub in ["config", "data", ".local/share", ".config"] {
                push(app.join(sub), &mut out, &mut seen);
            }
        }
    }

    // Snap: per-app data lives under ~/snap/<app>/{common,current}.
    if let Ok(entries) = std::fs::read_dir(home.join("snap")) {
        for app in entries.flatten().map(|e| e.path()) {
            for rev in ["common", "current"] {
                push(app.join(rev).join(".local/share"), &mut out, &mut seen);
                push(app.join(rev).join(".config"), &mut out, &mut seen);
            }
        }
    }

    // EmuDeck / RetroDECK conventional save roots, local and on microSD.
    push(home.join("Emulation/saves"), &mut out, &mut seen);
    push(home.join("Emulation/storage"), &mut out, &mut seen);
    if let Ok(mounts) = std::fs::read_dir("/run/media") {
        for user in mounts.flatten().map(|e| e.path()) {
            if let Ok(vols) = std::fs::read_dir(&user) {
                for vol in vols.flatten().map(|e| e.path()) {
                    push(vol.join("Emulation/saves"), &mut out, &mut seen);
                    push(vol.join("Emulation/storage"), &mut out, &mut seen);
                }
            }
        }
    }

    out
}

/// Carpetas donde la gente agrupa programas descomprimidos. Se miran un nivel
/// por dentro además de la propia raíz de la unidad.
const COLLECTION_DIRS: &[&str] = &["Emulators", "Emulation", "Emus", "Games", "Juegos", "ROMs"];

/// Sitios donde buscar programas que se instalaron descomprimiendo una carpeta
/// en vez de con un instalador.
///
/// Son dos: la **raíz de cada unidad interna** (`D:\RetroArch`) y **un nivel
/// dentro** de una carpeta-colección (`D:\Emulators\RetroArch`). Devuelve los
/// directorios a listar, no los candidatos: quien pregunta decide qué nombres
/// le valen.
///
/// Acotado a propósito, y esto es la mitad del diseño: un listado por unidad
/// más uno por colección, sin recorrer nada por debajo. Un barrido de un disco
/// de juegos leería decenas de miles de directorios para encontrar un puñado
/// de aciertos, y lo pagaría el arranque de cada escaneo.
///
/// Las unidades extraíbles, ópticas y de red se saltan: un recurso compartido
/// desconectado bloquea segundos en cada llamada, y ese coste lo notaría todo
/// el escaneo.
pub fn portable_install_roots(os: Os) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if seen.insert(p.clone()) && p.is_dir() {
            out.push(p);
        }
    };

    for drive in internal_drive_roots(os) {
        for dir in COLLECTION_DIRS {
            push(drive.join(dir), &mut out);
        }
        push(drive, &mut out);
    }
    out
}

/// Raíces de las unidades internas de este equipo.
///
/// En Windows son las letras de unidad fijas. En Linux y macOS no hay letras,
/// así que se toman los puntos de montaje habituales de discos secundarios —
/// que es donde acaba un segundo SSD o la microSD de una Deck.
#[cfg(windows)]
pub fn internal_drive_roots(_os: Os) -> Vec<PathBuf> {
    // `DRIVE_FIXED` no está junto a las dos funciones que lo consumen: vive en
    // `System::WindowsProgramming`. Compila igual en cualquiera de los dos
    // sitios, así que el error sólo aparece al construir para Windows.
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        // `GetDriveTypeW` quiere la raíz con barra final y en UTF-16 terminado
        // en nulo: "D:\\\0".
        let root: Vec<u16> = format!("{letter}:\\\0").encode_utf16().collect();
        // SAFETY: `root` es un UTF-16 válido terminado en nulo y vive durante
        // toda la llamada.
        if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED {
            out.push(PathBuf::from(format!("{letter}:\\")));
        }
    }
    out
}

/// Equivalente no-Windows: los puntos de montaje donde aparece un disco
/// secundario. `/media/<user>` y `/run/media/<user>` los usan los escritorios
/// Linux (y la Deck para la microSD); `/mnt` es el montaje a mano de toda la
/// vida; `/Volumes` es el de macOS.
#[cfg(not(windows))]
pub fn internal_drive_roots(os: Os) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if seen.insert(p.clone()) && p.is_dir() {
            out.push(p);
        }
    };

    let containers: &[&str] = match os {
        Os::Mac => &["/Volumes"],
        _ => &["/media", "/run/media", "/mnt"],
    };
    for container in containers {
        let Ok(entries) = std::fs::read_dir(container) else {
            continue;
        };
        for entry in entries.flatten().map(|e| e.path()) {
            if !entry.is_dir() {
                continue;
            }
            // `/media/<user>/<volumen>` y `/media/<volumen>` conviven según la
            // distribución, así que se aceptan los dos niveles.
            let mut had_child = false;
            if let Ok(children) = std::fs::read_dir(&entry) {
                for child in children.flatten().map(|e| e.path()) {
                    if child.is_dir() {
                        had_child = true;
                        push(child, &mut out);
                    }
                }
            }
            if !had_child {
                push(entry, &mut out);
            }
        }
    }
    out
}

/// Nombres de usuario Windows reales dentro de un prefijo Wine/Proton.
///
/// Lista los directorios bajo `drive_c/users/` que son usuarios reales —
/// Proton usa `steamuser`, los prefijos genéricos (`wine`, PlayOnLinux,
/// lanzadores `.desktop`) usan el login del host (`$USER`). Excluye `Public`
/// (no es un perfil de usuario) y entradas no-directorio. Vacío si el prefijo
/// no existe o no tiene `drive_c/users/`.
pub fn prefix_windows_users(prefix: &Path) -> Vec<String> {
    let users_dir = prefix.join("drive_c/users");
    let entries = match std::fs::read_dir(&users_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.eq_ignore_ascii_case("Public") {
            continue;
        }
        out.push(name);
    }
    out
}

/// Subdirectorios de usuario dentro de un prefijo Wine/Proton donde caen
/// los saves, para todos los usuarios reales del prefijo. Mismo naming
/// Windows que `pathexpand::expand_placeholder_in_prefix`. `prefix` apunta al
/// directorio que contiene `drive_c/` directamente.
pub fn prefix_user_roots(prefix: &Path) -> Vec<PathBuf> {
    prefix_windows_users(prefix)
        .iter()
        .flat_map(|user| prefix_user_roots_for(prefix, user))
        .collect()
}

/// Subdirectorios de save de un usuario Windows concreto dentro de un prefijo.
pub fn prefix_user_roots_for(prefix: &Path, user: &str) -> Vec<PathBuf> {
    let userhome = prefix.join("drive_c/users").join(user);
    [
        "AppData/Roaming",
        "AppData/Local",
        "AppData/LocalLow",
        "Documents",
        "Saved Games",
    ]
    .iter()
    .map(|sub| userhome.join(sub))
    .filter(|p| p.is_dir())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_non_empty_per_os() {
        for os in [Os::Windows, Os::Linux, Os::Mac] {
            assert!(!root_templates(os).is_empty());
        }
    }

    #[test]
    fn user_save_roots_runs_and_dedups() {
        // No panics; result is deduplicated (existence depends on host).
        let roots = user_save_roots(Os::current());
        let mut seen = HashSet::new();
        for r in &roots {
            assert!(seen.insert(r.clone()), "duplicate root: {r:?}");
        }
    }

    #[test]
    fn prefix_user_roots_filters_missing() {
        // A bogus prefix has none of the steamuser subdirs.
        let roots = prefix_user_roots(Path::new("/nonexistent/prefix/pfx"));
        assert!(roots.is_empty());
    }
}
