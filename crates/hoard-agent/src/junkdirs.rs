//! DETECCIÓN — qué carpetas NO son un save, y qué carpetas son demasiado
//! anchas para ofrecerlas.
//!
//! Tres piezas que comparten el resto del pipeline:
//!
//! * [`is_cache_dir_name`] — caché regenerable (shaders, DX12, logs). El set
//!   exacto de `scoring::NEGATIVE_NAME_VOCAB` no atrapaba `AnvilDX12Cache` ni
//!   `FortniteShaderCache` porque el juego les antepone su propio nombre; aquí
//!   la regla es por **sufijo** y normalizando separadores, así que
//!   `Shader Cache`, `shader_cache` y `ShaderCache` son el mismo nombre.
//! * [`save_dirs_under`] — buscar carpetas de save por nombre dentro de un
//!   árbol acotado, para los juegos que guardan junto al ejecutable.
//! * [`blocked_roots`] — raíces que no se ofrecen jamás: el perfil del
//!   usuario, `Documents`, y las raíces de motor compartidas (RenPy, Godot,
//!   LOVE), donde un match tiene que apuntar a la carpeta de UN juego dentro,
//!   nunca a la raíz que las contiene todas.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::manifest::Os;
use crate::pathexpand::expand_path;

/// Carpetas que son caché regenerable o estado derivado — nunca un save, y
/// sincronizarlas movería cientos de megas de basura específica de la máquina.
const CACHE_DIR_NAMES: &[&str] = &[
    // Cachés de API gráfica.
    "dx12cache",
    "dxcache",
    "d3dcache",
    "d3dscache",
    "dxil",
    "dxbc",
    "pipelinecache",
    "psocache",
    // Cachés de motor y de vendor.
    "shadercache",
    "shadercachedb",
    "shaders",
    "shadercompiler",
    "derivedatacache",
    "ddc",
    "gpucache",
    "glcache",
    "vulkancache",
    "nvidiacache",
    // Genéricas y estado regenerado.
    "cache",
    "caches",
    "cacheddata",
    "temp",
    "tmp",
    "logs",
    "log",
    "crashes",
    "crashdumps",
    "crashreports",
    "webcache",
    "mediacache",
];

/// Sufijos que delatan a la misma familia cuando el juego les antepone su
/// nombre (`FortniteShaderCache`, `AnvilDX12Cache`, `DerivedDataCache`).
/// Terminar en "cache" es señal suficiente por sí sola: esto se comprueba
/// ANTES que los nombres de save, así que una rareza como `SaveCache` cuenta
/// como caché — que es lo que es.
const CACHE_DIR_SUFFIXES: &[&str] = &["cache"];

/// Minúsculas y sin los separadores que la gente usa indistintamente, para
/// que una sola entrada cubra todas las grafías.
pub fn normalize_dir_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-' | '.'))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// `true` si el nombre de carpeta es caché regenerable y no datos de save.
pub fn is_cache_dir_name(name: &str) -> bool {
    let n = normalize_dir_name(name);
    if n.is_empty() {
        return false;
    }
    CACHE_DIR_NAMES.contains(&n.as_str()) || CACHE_DIR_SUFFIXES.iter().any(|s| n.ends_with(s))
}

/// `true` si el nombre sugiere datos de save: `Saves`, `savegames`,
/// `SaveData`, `AutoSave`, `SAVE`… Comparación insensible a caja y a
/// separadores. Más laxo que `detection::SAVE_PATTERNS` (que exige igualdad
/// exacta) a propósito: aquí ya venimos de un árbol acotado.
pub fn looks_like_save_dir_name(name: &str) -> bool {
    !is_cache_dir_name(name) && normalize_dir_name(name).contains("save")
}

/// Suffixes that give away a COPY of saves rather than the live save:
/// `SaveGamesBackup`, `SavesOld`, `NobodyT-bak`. Suffix, never prefix —
/// `BackupSaves` is a save folder with an odd prefix and must not match.
///
/// [`normalize_dir_name`] has already eaten the separators by the time we
/// compare, so `_bak`, `-bak` and `.bak` all arrive as `…bak`. `old` is the
/// one risky term (any word ending in -old matches), which is why callers
/// treat this as a weak signal — a penalty or a warning — and never as a veto
/// on its own (see `scoring::score_dir` and `detection::is_backup_mirror`).
pub const BACKUP_DIR_SUFFIXES: &[&str] = &["backup", "backups", "bak", "old"];

/// The subset that needs a word boundary to count. `backup`/`backups`/`bak`
/// are unambiguous enough to match even when a name runs straight into them
/// (`savegamesbackup`): checked against the whole catalog, every leaf ending
/// in those letters really is a copy. `old` is the opposite — it is the tail
/// of ordinary words, and demanding the boundary is the only thing standing
/// between the rule and `Stranglehold`.
const BOUNDED_SUFFIXES: &[&str] = &["old"];

/// `true` when the name ends in a copy suffix **at a real word boundary**.
///
/// The boundary is the whole point. Matching the bare letters against a
/// separator-stripped name is what a first cut did, and the catalog is full of
/// counter-examples it would have condemned: `Sunday Gold`, `Stranglehold`,
/// `Stikbold`, `Defold`, `Making History Gold`, `Castle of Heart_ Retold` —
/// and `wildlife-park-gold-remastered`, whose only save path is a `savegold/`
/// folder of `.sav` files that clears the rotating-content gate and would have
/// eaten the penalty for nothing. All of them merely *end in the letters*
/// "old".
///
/// So an ambiguous suffix ([`BOUNDED_SUFFIXES`]) counts only when the name IS
/// it (`old`), or when what precedes it is a separator (`Saves_Old`) or a case
/// change (`SavesOld`). Lowercase letters running straight into it are part of
/// a longer word, not a marker. The unambiguous ones match either way.
pub fn ends_with_backup_suffix(name: &str) -> bool {
    let raw: Vec<char> = name.chars().collect();
    let lower: String = name.to_lowercase();
    for suf in BACKUP_DIR_SUFFIXES {
        let Some(head) = lower.strip_suffix(suf) else {
            continue;
        };
        // The whole name is the suffix.
        if head.is_empty() {
            return true;
        }
        if !BOUNDED_SUFFIXES.contains(suf) {
            return true;
        }
        // `head` is a char-count prefix only while the name is ASCII, which
        // every one of these markers is; index defensively all the same.
        let cut = head.chars().count();
        let Some(&prev) = raw.get(cut.wrapping_sub(1)) else {
            continue;
        };
        if matches!(prev, ' ' | '_' | '-' | '.') {
            return true;
        }
        // Case change: `SaveGames|Backup`. The suffix's own first character
        // must be the uppercase one, or we are inside a word.
        if prev.is_lowercase() && raw.get(cut).is_some_and(|c| c.is_uppercase()) {
            return true;
        }
    }
    false
}

/// Profundidad máxima bajo la raíz de instalación. Llega a los layouts que
/// los juegos usan de verdad (`<install>/savegames/<id>`,
/// `<install>/Binaries/Saves`) sin pasear un árbol de assets entero.
const SAVE_SCAN_MAX_DEPTH: usize = 3;
/// Un directorio con una cantidad implausible de subcarpetas es un volcado
/// de assets, no un sitio donde vivan saves.
const SAVE_SCAN_MAX_FANOUT: usize = 120;
/// Tope de carpetas de save que puede aportar una sola instalación, para que
/// un árbol patológico no inunde los resultados.
const SAVE_SCAN_MAX_HITS: usize = 4;

/// Busca carpetas de save por NOMBRE dentro de `root`.
///
/// Para los juegos que guardan junto al ejecutable en vez de en una ubicación
/// conocida por motor o por launcher — los títulos de Ubisoft con
/// `<install>/savegames/<id numérico>`, que ninguna plantilla enumera.
///
/// Deliberadamente conservador: profundidad y abanico acotados, cachés fuera,
/// carpetas vacías ignoradas, y **no desciende tras un acierto** (así las
/// subcarpetas de una carpeta de save no se convierten cada una en una
/// entrada). Cuando la que acertó contiene una hija más específica —la forma
/// `Saved/SaveGames` de Unreal— gana la hija.
pub fn save_dirs_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if root.as_os_str().is_empty() || !root.is_dir() {
        return out;
    }
    walk(root, 1, &mut out);
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > SAVE_SCAN_MAX_DEPTH || out.len() >= SAVE_SCAN_MAX_HITS {
        return;
    }
    let subs = subdirs(dir);
    if subs.len() > SAVE_SCAN_MAX_FANOUT {
        return;
    }
    for sub in subs {
        if out.len() >= SAVE_SCAN_MAX_HITS {
            return;
        }
        let name = sub.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        if is_cache_dir_name(name) {
            continue;
        }
        if looks_like_save_dir_name(name) {
            out.extend(resolve_save_dir(&sub));
            continue; // nunca desciende tras un acierto
        }
        walk(&sub, depth + 1, out);
    }
}

/// Qué ofrecer para una carpeta cuyo nombre acertó: un contenedor como
/// `Saved` que alberga una `SaveGames` más específica resuelve a la hija; si
/// no, la propia carpeta, siempre que tenga algo dentro.
fn resolve_save_dir(dir: &Path) -> Vec<PathBuf> {
    let deeper: Vec<PathBuf> = subdirs(dir)
        .into_iter()
        .filter(|p| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
            looks_like_save_dir_name(name) && dir_non_empty(p)
        })
        .collect();
    if !deeper.is_empty() {
        return deeper;
    }
    if dir_non_empty(dir) {
        vec![dir.to_path_buf()]
    } else {
        Vec::new()
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

fn dir_non_empty(p: &Path) -> bool {
    std::fs::read_dir(p).is_ok_and(|mut r| r.next().is_some())
}

/// Raíces que NUNCA deben ofrecerse como carpeta de save de un juego.
///
/// Dos familias:
///
/// * El perfil del usuario y sus carpetas de primer nivel (`Documents`,
///   `AppData/*`, `Saved Games`). Una plantilla laxa que resuelva ahí
///   propondría sincronizar el perfil entero.
/// * **Raíces de motor compartidas**: `AppData/Roaming/RenPy` tiene los saves
///   de *todos* los juegos RenPy de la máquina, igual que Godot, LOVE y
///   `LocalLow/DefaultCompany`. Un acierto tiene que apuntar a la carpeta del
///   juego dentro de ellas; la raíz mezcla juegos distintos en un save.
///
/// Se compara por igualdad exacta de ruta: la carpeta de un juego DENTRO de
/// una raíz bloqueada es perfectamente válida y no debe filtrarse.
pub fn blocked_roots(os: Os) -> HashSet<PathBuf> {
    let mut out: HashSet<PathBuf> = HashSet::new();
    let mut add = |tmpl: &str| {
        for p in expand_path(tmpl, os) {
            out.insert(p);
        }
    };
    for tmpl in [
        "<home>",
        "<home>/Documents",
        "<home>/Desktop",
        "<home>/Downloads",
        "<home>/Saved Games",
        "<home>/Documents/My Games",
        "<winAppData>",
        "<winLocalAppData>",
        "<winLocalAppDataLow>",
        "<winDocuments>",
        "<winSavedGames>",
        "<winPublic>",
        "<winPublic>/Documents",
        "<winProgramData>",
        "<winLocalAppData>/Programs",
        "<winLocalAppData>/Packages",
        "<winLocalAppData>/User Data",
        "<xdgData>",
        "<xdgConfig>",
        "<xdgState>",
        // Raíces de motor compartidas.
        "<winAppData>/RenPy",
        "<winAppData>/Godot",
        "<winAppData>/Godot/app_userdata",
        "<winAppData>/LOVE",
        "<winLocalAppDataLow>/DefaultCompany",
        "<xdgData>/renpy",
        "<xdgData>/godot",
        "<xdgData>/love",
    ] {
        add(tmpl);
    }
    out
}

/// Motivo legible si `path` apunta a una carpeta de perfil/sistema que jamás
/// puede ser la raíz de save de un juego; `None` si es aceptable.
///
/// Complementa a [`blocked_roots`], que trabaja sobre rutas ya resueltas de
/// ESTA máquina durante la detección. Esto es **estructural**: mira la forma
/// de la ruta, así que también protege lo que escribe el usuario a mano, lo
/// que llega de otra máquina y lo que quedó envenenado en `state.json` de
/// antes de existir estas guardas.
///
/// Rastrear una raíz así no es sólo desordenado: hashea y sube el perfil
/// entero, y en Windows revienta en la primera junction legacy
/// (`AppData\Local\Application Data`, que apunta a su propio padre).
pub fn dangerous_sync_root(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some("the save path is empty".into());
    }
    // Normaliza a `/` y sin barra final para comparar por segmentos.
    let p = trimmed.replace('\\', "/");
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        return Some("it is the filesystem root".into());
    }
    let lower = p.to_lowercase();
    let segs: Vec<&str> = lower.split('/').filter(|s| !s.is_empty()).collect();

    // Los prefijos de Wine/Proton se miran antes que nada y por la COLA: uno
    // puede estar bajo `~/.local/share/Steam`, bajo una biblioteca en otro
    // disco, o donde lo dejen Lutris/Bottles. Lo que lo delata es cómo acaba la
    // ruta, no dónde empieza.
    if let Some(reason) = dangerous_wine_prefix(&segs) {
        return Some(reason);
    }

    // Windows: `C:` como primer segmento.
    if let Some(first) = segs.first() {
        if first.len() == 2 && first.ends_with(':') {
            return dangerous_windows_root(&segs[1..]);
        }
    }
    dangerous_unix_root(&segs)
}

/// Raíces de un prefijo de Wine/Proton. Un prefijo **es un Windows entero
/// emulado**: su `drive_c` con el perfil, `ProgramData`, el registro y todo lo
/// que el juego instaló. Rastrearlo sube cientos de MB de los que la partida
/// son unos pocos KB, y el resto se rehace solo en cualquier máquina.
///
/// Reportado en ago-2026: una Steam Deck acabó monitorizando
/// `…/steamapps/compatdata/423230/pfx` — 308,6 MB— para un save que vive en
/// `pfx/drive_c/users/steamuser/AppData/LocalLow/TheGameBakers/Furi`.
fn dangerous_wine_prefix(segs: &[&str]) -> Option<String> {
    let say = |s: &str| Some(s.to_string());

    // Dentro del prefijo, `drive_c` **es** la raíz de un Windows: las reglas de
    // Windows ya saben qué es un perfil entero o un AppData entero, así que se
    // reusan tal cual en vez de escribirlas dos veces. `rposition` por si
    // alguien anida prefijos (Bottles lo hace).
    if let Some(i) = segs.iter().rposition(|s| *s == "drive_c") {
        if let Some(reason) = dangerous_windows_root(&segs[i + 1..]) {
            return Some(reason);
        }
    }

    match segs {
        [.., "pfx"] => say("it is a whole Wine/Proton prefix"),
        [.., "compatdata"] => say("it is Steam's whole compatibility-data folder"),
        // `compatdata/<appid>` — el contenedor del prefijo de UN juego.
        [.., "compatdata", _] => say("it is a game's whole Proton prefix folder"),
        _ => None,
    }
}

fn dangerous_windows_root(rest: &[&str]) -> Option<String> {
    let say = |s: &str| Some(s.to_string());
    match rest {
        [] => say("it is a whole drive"),
        ["windows", ..] => say("it is inside the Windows system folder"),
        ["users"] => say("it is the Users folder"),
        ["users", _] => say("it is a whole user profile folder"),
        ["users", _, "appdata"] => say("it is the whole AppData folder"),
        ["users", _, "appdata", tier] if matches!(*tier, "local" | "roaming" | "locallow") => {
            say("it is a whole application-data folder")
        }
        ["users", _, folder]
            if matches!(
                *folder,
                "documents"
                    | "desktop"
                    | "downloads"
                    | "pictures"
                    | "music"
                    | "videos"
                    | "saved games"
                    | "onedrive"
            ) =>
        {
            Some(format!("it is a whole {folder} folder"))
        }
        [only]
            if matches!(
                *only,
                "program files" | "program files (x86)" | "programdata"
            ) =>
        {
            Some(format!("it is the whole {only} folder"))
        }
        _ => None,
    }
}

fn dangerous_unix_root(segs: &[&str]) -> Option<String> {
    let say = |s: &str| Some(s.to_string());
    match segs {
        [] => say("it is the filesystem root"),
        [only]
            if matches!(
                *only,
                "home" | "root" | "etc" | "usr" | "var" | "tmp" | "opt"
            ) =>
        {
            say("it is a system folder")
        }
        ["home", _] => say("it is a whole home folder"),
        ["home", _, dir] if matches!(*dir, ".config" | ".local" | ".steam" | ".var") => {
            Some(format!("it is a whole {dir} folder"))
        }
        ["home", _, ".local", "share"] => say("it is a whole .local/share folder"),
        ["home", _, dir]
            if matches!(
                *dir,
                "documents" | "desktop" | "downloads" | "pictures" | "music" | "videos"
            ) =>
        {
            Some(format!("it is a whole {dir} folder"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_roots_are_refused_on_both_platforms() {
        for (p, hint) in [
            ("C:\\", "drive"),
            ("C:\\Windows\\System32", "windows"),
            ("C:\\Users", "users"),
            ("C:\\Users\\jacka", "profile"),
            ("C:\\Users\\jacka\\AppData", "appdata"),
            ("C:\\Users\\jacka\\AppData\\Roaming", "application-data"),
            ("C:\\Users\\jacka\\Documents", "documents"),
            ("C:\\Users\\jacka\\Saved Games", "saved games"),
            ("C:\\Program Files (x86)", "program files"),
            ("/", "filesystem root"),
            ("/home", "system folder"),
            ("/usr", "system folder"),
            ("/home/insider", "home folder"),
            ("/home/insider/.local/share", ".local/share"),
            ("/home/insider/.config", ".config"),
            ("/home/insider/Documents", "documents"),
            // Prefijos de Wine/Proton — el caso de la Steam Deck (ago-2026).
            // Se reconocen por la cola, así que valen en cualquier biblioteca.
            (
                "/home/clock/.local/share/Steam/steamapps/compatdata/423230/pfx",
                "prefix",
            ),
            ("/mnt/juegos/SteamLibrary/steamapps/compatdata/620/pfx", "prefix"),
            ("/home/clock/.local/share/Steam/steamapps/compatdata/423230", "prefix"),
            ("/home/clock/.local/share/Steam/steamapps/compatdata", "compatibility-data"),
            // Y dentro del prefijo mandan las reglas de Windows: `drive_c` es
            // una unidad entera, y su perfil, un perfil entero.
            (
                "/home/clock/.local/share/Steam/steamapps/compatdata/423230/pfx/drive_c",
                "drive",
            ),
            (
                "/home/clock/.local/share/Steam/steamapps/compatdata/423230/pfx/drive_c/users/steamuser",
                "profile",
            ),
            (
                "/home/clock/.local/share/Steam/steamapps/compatdata/423230/pfx/drive_c/users/steamuser/AppData/LocalLow",
                "application-data",
            ),
        ] {
            let reason = dangerous_sync_root(Path::new(p));
            assert!(reason.is_some(), "{p} debería rechazarse");
            assert!(
                reason.as_deref().unwrap().to_lowercase().contains(hint),
                "{p}: motivo poco claro → {reason:?}"
            );
        }
    }

    #[test]
    fn a_real_save_folder_passes() {
        for p in [
            "C:\\Users\\jacka\\AppData\\Roaming\\GSE Saves\\413150\\remote",
            "C:\\Users\\jacka\\Documents\\My Games\\Skyrim\\Saves",
            "C:\\Users\\jacka\\Saved Games\\Planet S",
            "/home/insider/.local/share/Steam/userdata/1/413150/remote",
            "/home/insider/.config/unity3d/Studio/Game",
            "/home/insider/Documents/My Games/EU5/save games",
            "/mnt/ssd/Games/Factorio/saves",
            // La carpeta buena DENTRO del prefijo: es el destino al que apunta
            // el mensaje de «pick the game's own save folder inside it», así
            // que rechazarla convertiría la guarda en un callejón sin salida.
            "/home/clock/.local/share/Steam/steamapps/compatdata/423230/pfx/drive_c/users/steamuser/AppData/LocalLow/TheGameBakers/Furi",
            "/home/clock/.local/share/Steam/steamapps/compatdata/620/pfx/drive_c/users/steamuser/Saved Games/Portal2",
        ] {
            assert!(
                dangerous_sync_root(Path::new(p)).is_none(),
                "{p} debería aceptarse: {:?}",
                dangerous_sync_root(Path::new(p))
            );
        }
    }

    #[test]
    fn a_trailing_slash_or_mixed_separators_dont_sneak_past() {
        assert!(dangerous_sync_root(Path::new("C:\\Users\\jacka\\")).is_some());
        assert!(dangerous_sync_root(Path::new("C:/Users/jacka")).is_some());
        assert!(dangerous_sync_root(Path::new("/home/insider/")).is_some());
        assert!(dangerous_sync_root(Path::new("")).is_some());
    }

    #[test]
    fn cache_matches_every_spelling_and_the_prefixed_variants() {
        for n in [
            "cache",
            "Cache",
            "shadercache",
            "Shader Cache",
            "shader_cache",
            "ShaderCache",
            "DX12Cache",
            "AnvilDX12Cache",
            "FortniteShaderCache",
            "DerivedDataCache",
            "crashdumps",
            "Logs",
            "temp",
        ] {
            assert!(is_cache_dir_name(n), "{n} debería ser caché");
        }
        for n in ["saves", "SaveGames", "profiles", "slot1", "Documents", ""] {
            assert!(!is_cache_dir_name(n), "{n} NO debería ser caché");
        }
    }

    #[test]
    fn a_cache_named_save_is_still_a_cache() {
        // El orden importa: caché se comprueba antes que save.
        assert!(is_cache_dir_name("SaveCache"));
        assert!(!looks_like_save_dir_name("SaveCache"));
    }

    #[test]
    fn save_names_ignore_case_and_separators() {
        for n in [
            "saves",
            "SAVE",
            "Save Games",
            "save_data",
            "SaveData",
            "autosave",
        ] {
            assert!(looks_like_save_dir_name(n), "{n} debería parecer save");
        }
        for n in ["config", "binaries", "shaders"] {
            assert!(!looks_like_save_dir_name(n));
        }
    }

    #[test]
    fn backup_suffix_matches_only_at_the_end() {
        // Suffix: yes, whatever the separator or case — including the bare
        // word `Backup`, which IS the suffix.
        for n in [
            "SaveGamesBackup",
            "saves_backup",
            "Saves-Backup",
            "NobodyT-bak",
            "slot.bak",
            "SavesOld",
            "backups",
            "Backup",
        ] {
            assert!(ends_with_backup_suffix(n), "{n} ends in a copy suffix");
        }
        // Prefix or unrelated word: NO. `BackupSaves` is a save folder.
        for n in ["BackupSaves", "saves", "SaveGames", "autosave"] {
            assert!(!ends_with_backup_suffix(n), "{n} is not a copy by name");
        }
    }

    /// The names below are not invented: every one is a real save-folder leaf
    /// from the Ludusavi catalog whose letters happen to end in "old". A first
    /// cut of this rule compared the separator-stripped name and condemned all
    /// of them. `savegold` is the one that proves the cost — it is
    /// `wildlife-park-gold-remastered`'s ONLY save path, a folder of `.sav`
    /// files that clears the rotating-content gate, so the penalty would have
    /// applied with nothing to back it up.
    ///
    /// A regression here is invisible to the name-recall benchmark (that one
    /// measures the positive vocabulary, which this rule never touches), so
    /// the corpus has to live as its own test.
    #[test]
    fn real_catalog_names_ending_in_old_are_not_copies() {
        for n in [
            "Sunday Gold",
            "Making History Gold",
            "Trolley_Gold",
            "Hegemony Gold",
            "savegold",
            "rescuequestgold",
            "Stranglehold",
            "Stikbold",
            "Faerie Solitaire Harvest Defold",
            "Castle of Heart_ Retold",
            "jp.konami.mac.FroggerTTGold",
            "Blake Stone - Aliens of Gold",
        ] {
            assert!(
                !ends_with_backup_suffix(n),
                "{n} is a real game's save folder, not a backup copy"
            );
        }
        // The boundary is what separates them from the genuine articles.
        for n in ["Saves_Old", "SavesOld", "saves old", "old"] {
            assert!(ends_with_backup_suffix(n), "{n} really is a copy marker");
        }
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn finds_a_save_dir_next_to_the_game_and_prefers_the_specific_child() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("Game");
        // El caso Ubisoft: <install>/savegames/<id numérico>.
        touch(&install.join("savegames/1234567/save.dat"));
        // Y el de Unreal, un nivel más abajo.
        touch(&install.join("Binaries/Saved/SaveGames/slot.sav"));
        // Ruido que no debe salir.
        touch(&install.join("ShaderCache/x.bin"));
        touch(&install.join("Content/audio/track.ogg"));

        let mut found = save_dirs_under(&install);
        found.sort();
        assert_eq!(
            found,
            vec![
                install.join("Binaries/Saved/SaveGames"),
                install.join("savegames"),
            ],
            "esperado el save de Ubisoft y el de Unreal, sin caché"
        );
    }

    #[test]
    fn an_empty_save_dir_is_not_offered() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Game/saves")).unwrap();
        assert!(save_dirs_under(&tmp.path().join("Game")).is_empty());
    }

    #[test]
    fn the_walk_is_bounded_by_depth() {
        let tmp = tempfile::tempdir().unwrap();
        // Cuatro niveles por debajo de la raíz: fuera de alcance.
        touch(&tmp.path().join("a/b/c/d/saves/x.sav"));
        assert!(save_dirs_under(tmp.path()).is_empty());
    }

    #[test]
    fn blocked_roots_cover_the_profile_but_not_a_game_inside_it() {
        let roots = blocked_roots(Os::current());
        if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()) {
            assert!(roots.contains(&home), "el home debe estar bloqueado");
            assert!(
                !roots.contains(&home.join("Documents/My Games/Skyrim")),
                "la carpeta de UN juego dentro de una raíz bloqueada es válida"
            );
        }
    }
}
