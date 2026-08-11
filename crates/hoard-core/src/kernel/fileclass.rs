//! Qué es cada fichero que vive dentro de una carpeta de save.
//!
//! Una carpeta de partidas casi nunca contiene sólo partidas. `walk_source` se
//! llevaba **todo fichero regular** que hubiera dentro, y eso mete en el
//! snapshot cosas que no son dato del jugador sino dato de *esta máquina*:
//! el `Player.log` de Unity, la cola de analítica con el GUID de instalación,
//! la info de shaders de esta GPU, el `steam_autocloud.vdf`, un `graphics.ini`
//! con la resolución de este monitor.
//!
//! Dos daños distintos:
//!
//! * **Ruido.** El log se reescribe en cada arranque, así que la firma barata
//!   de `compute_set_signature` se mueve, la de contenido confirma que los
//!   bytes cambiaron de verdad (cambiaron: es un log) y se corta una versión
//!   nueva en la nube **cada vez que se abre el juego**, sin que la partida se
//!   haya tocado.
//! * **Crash.** Restaurar el `graphics.ini` del PC A sobre el PC B le mete al
//!   juego una resolución, un GPU o una ruta que en esa máquina no existen.
//!
//! ## La escalera, de menos a más destructiva
//!
//! [`FileClass::Junk`] es lo único que se deja de subir, y por eso la lista es
//! corta y por nombre exacto siempre que se puede: un fichero que no se sube no
//! se puede recuperar, así que la duda nunca cae aquí.
//!
//! [`FileClass::DeviceLocal`] es donde cae la duda: **sí se sube** (si se
//! quema el disco, está), pero un restore no lo escribe salvo que el usuario lo
//! pida a mano (`--allow-ini` en la CLI, un interruptor apagado por defecto en
//! el diálogo del escritorio). Así el fallo de clasificación más caro que puede
//! ocurrir —llamar config a algo que era la partida— cuesta un clic, no la
//! partida.
//!
//! ## El blindaje del manifiesto
//!
//! El catálogo Ludusavi trae patrón de fichero en 20.499 de sus 47.404
//! plantillas (`<base>/Saves/*.sav`), y ahí sí sabe la comunidad lo que es dato
//! de partida. Ese patrón entra aquí como `shields`: lo que casa con él es
//! partida y **no lo toca ninguna regla de abajo**. Hace falta de verdad —
//! `.ini` es el patrón de save de 582 plantillas, `.cfg` de 98 y `.log` de 64,
//! así que sin blindaje las reglas por extensión se llevarían por delante
//! partidas reales.
//!
//! Al revés **no** se usa: que el manifiesto no liste un fichero no lo condena.
//! El catálogo tiene huecos enormes (Cell to Singularity es un directorio
//! pelado, sin un solo patrón), y fiarse de él para excluir sería fiarse de un
//! hueco para borrar.

/// Qué es un fichero de dentro de la carpeta de save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    /// Dato del jugador. Se sube y se restaura, como siempre.
    SaveData,
    /// Dato de **esta máquina**, no del jugador: config, ajustes, logs
    /// genéricos. Se sube (para no perderlo nunca) pero un restore no lo
    /// escribe salvo petición explícita.
    DeviceLocal,
    /// Ni dato del jugador ni config que nadie quiera de vuelta: basura del SO,
    /// temporales, volcados de crash, telemetría del motor. No se sube ni se
    /// restaura.
    Junk,
}

impl FileClass {
    /// ¿Entra en un snapshot nuevo?
    pub fn is_backed_up(self) -> bool {
        !matches!(self, FileClass::Junk)
    }

    /// ¿Se escribe en disco al restaurar? `allow_device_local` es el
    /// interruptor que el usuario enciende a mano.
    pub fn is_restored(self, allow_device_local: bool) -> bool {
        match self {
            FileClass::SaveData => true,
            FileClass::DeviceLocal => allow_device_local,
            FileClass::Junk => false,
        }
    }
}

/// Basura del SO y del gestor de ficheros, por nombre exacto.
const JUNK_NAMES: &[&str] = &[
    ".ds_store",
    "thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    ".directory",
    // Contabilidad de Steam, no del juego: qué ficheros tocaba sincronizar y
    // cuándo. Restaurarlo en otra máquina le miente al cliente de Steam.
    "steam_autocloud.vdf",
    "remotecache.vdf",
    // Logs de motor por nombre exacto. Genéricos (`*.log`) NO caen aquí sino en
    // `DeviceLocal`: `.log` es el patrón de save de 64 plantillas del catálogo
    // y no todas están blindadas.
    "player.log",
    "player-prev.log",
    "output_log.txt",
    "output_log_prev.txt",
    // Cerrojo que el juego mantiene abierto en exclusiva mientras corre. No
    // lleva dato, y en Windows ni siquiera se puede abrir para leer con el
    // juego vivo (sharing violation, os error 32), lo que llegó a abortar el
    // backup entero a mitad del recorrido — el `session.lock` de Minecraft.
    "session.lock",
];

/// Extensiones que nunca son dato de partida.
const JUNK_EXTS: &[&str] = &[
    // Volcados de crash.
    "dmp", "mdmp", "stackdump", // Escrituras a medias y temporales de editores/descargas.
    "tmp", "temp", "part", "crdownload", "swp",
];

/// Un segmento de ruta con este nombre cuelga de telemetría del motor, no de la
/// partida.
const JUNK_SEGMENTS: &[&str] = &[
    // Unity: info de shaders y GPU de *esta* máquina.
    "shadervariantanalytics",
    // Unreal.
    "crashreportclient",
];

/// Extensiones de configuración. Sin blindaje, un fichero con una de éstas se
/// sube pero no se restaura encima de una máquina viva.
const CONFIG_EXTS: &[&str] = &[
    "ini",
    "cfg",
    "conf",
    "config",
    "toml",
    "yaml",
    "yml",
    "vdf",
    "properties",
    // Log genérico: se guarda por si acaso, pero no se restaura nunca.
    "log",
];

/// El nombre (sin extensión) **acaba** en esto ⇒ es configuración, tenga la
/// extensión que tenga. Coge `GraphicsSettings.json`, `Fallout4Prefs.ini`,
/// `UserOptions.dat`.
const CONFIG_STEM_SUFFIXES: &[&str] = &[
    "settings",
    "config",
    "configuration",
    "prefs",
    "preferences",
    "options",
];

/// El nombre (sin extensión) **es exactamente** esto ⇒ configuración.
/// Deliberadamente exacto y no "contiene": `input` es config, pero
/// `input_puzzle_solved` sería partida.
const CONFIG_STEMS: &[&str] = &[
    "graphics",
    "graphic",
    "video",
    "audio",
    "sound",
    "display",
    "resolution",
    "input",
    "controls",
    "keybinds",
    "keybindings",
    "keyboard",
    "gamepad",
    "launcher",
    "hardware",
];

/// Qué deja escribir un restore en disco.
///
/// Viaja dentro de `RestoreOptions` y acompaña a la previsualización, para que
/// lo que el `--dry-run` promete y lo que el restore hace salgan de la misma
/// decisión y no de dos copias que se van separando.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreGate {
    /// Patrones del manifiesto que blindan un fichero como dato de partida.
    pub shields: Vec<String>,
    /// El usuario ha pedido **a mano** que la config del snapshot se escriba
    /// encima de la de esta máquina (`--allow-ini`, el interruptor del
    /// diálogo). Apagado por defecto, y apagado siempre en el auto-restore:
    /// escribir la config del PC A sobre el PC B es justo el crash que este
    /// módulo existe para evitar.
    pub allow_device_local: bool,
}

impl RestoreGate {
    /// Puerta abierta de par en par: lo que había antes de que esto existiera.
    /// Para tests y para los sitios donde el llamante ya filtró.
    pub fn permissive() -> Self {
        Self {
            shields: Vec::new(),
            allow_device_local: true,
        }
    }

    /// ¿Se escribe este fichero del snapshot en disco?
    pub fn allows(&self, rel_path: &str) -> bool {
        classify(rel_path, &self.shields).is_restored(self.allow_device_local)
    }
}

/// Clasifica un fichero por su ruta **relativa a la raíz del save**, con `/`
/// como separador (la forma que ya produce `walk_source`).
///
/// `shields` son patrones de nombre de fichero sacados del manifiesto
/// (`*.sav`, `save*`). Lo que case con uno es partida y sale por la puerta de
/// arriba sin pasar por ninguna otra regla.
pub fn classify(rel_path: &str, shields: &[String]) -> FileClass {
    let lower = rel_path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);

    // 1. El manifiesto manda: si dice que esto es un save, es un save.
    if shields.iter().any(|p| glob_match(p, name)) {
        return FileClass::SaveData;
    }

    // 2. Basura inequívoca. Lo único que se deja de subir.
    if JUNK_NAMES.contains(&name) {
        return FileClass::Junk;
    }
    // Los `._foo` de AppleDouble que macOS siembra en volúmenes no-HFS.
    if name.starts_with("._") {
        return FileClass::Junk;
    }
    if let Some(ext) = extension_of(name) {
        if JUNK_EXTS.contains(&ext) {
            return FileClass::Junk;
        }
    }
    let segments: Vec<&str> = lower.split('/').collect();
    // Todo lo que cuelga de un directorio de telemetría.
    if segments
        .iter()
        .take(segments.len().saturating_sub(1))
        .any(|s| JUNK_SEGMENTS.contains(s))
    {
        return FileClass::Junk;
    }
    // La cola de eventos de Unity Analytics: `Unity/<guid>/Analytics/...`. El
    // GUID identifica la *instalación*, así que restaurarlo en otra máquina le
    // clona la identidad de analítica.
    if is_under_unity_analytics(&segments) {
        return FileClass::Junk;
    }

    // 3. Config y demás dato de esta máquina. Se sube; no se restaura solo.
    if let Some(ext) = extension_of(name) {
        if CONFIG_EXTS.contains(&ext) {
            return FileClass::DeviceLocal;
        }
    }
    let stem = stem_of(name);
    if CONFIG_STEMS.contains(&stem) || CONFIG_STEM_SUFFIXES.iter().any(|s| stem.ends_with(s)) {
        return FileClass::DeviceLocal;
    }

    FileClass::SaveData
}

/// `Unity/<algo>/Analytics/...` en cualquier profundidad: hace falta el
/// `unity` de ancestro para no confundirse con una carpeta `analytics` que
/// resulte ser del juego.
fn is_under_unity_analytics(segments: &[&str]) -> bool {
    let Some(unity_at) = segments.iter().position(|s| *s == "unity") else {
        return false;
    };
    // El fichero en sí no cuenta como directorio contenedor.
    segments
        .iter()
        .enumerate()
        .any(|(i, s)| i > unity_at && i + 1 < segments.len() && *s == "analytics")
}

/// Extensión en minúsculas, sin el punto. `None` si no tiene, o si el punto
/// abre el nombre (`.bashrc` no tiene extensión, se llama así).
fn extension_of(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(ext)
}

/// El nombre sin la extensión.
fn stem_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// ¿Este patrón del manifiesto vale como blindaje?
///
/// `*` y `*.*` casan con todo, así que blindarían la carpeta entera y dejarían
/// el filtro en nada. No dicen *qué* es un save, sólo "hay ficheros aquí": no
/// son información, y 1.519 plantillas del catálogo son exactamente eso.
pub fn is_useful_shield(pattern: &str) -> bool {
    let p = pattern.trim();
    if !p.contains('*') && !p.contains('?') {
        // Un nombre literal es informativo, pero entonces no hace de comodín:
        // vale igual como blindaje exacto.
        return !p.is_empty();
    }
    !matches!(p, "*" | "*.*" | "?" | "**")
}

/// Glob de un solo segmento: `*` = cualquier cosa (incluida vacía), `?` = un
/// carácter. Sin clases ni alternativas — el manifiesto no las usa en el
/// último segmento.
///
/// Propio y no el de `pathexpand` porque el kernel no depende de `hoard-agent`
/// (regla dura de ADR 0021: el kernel no importa shells).
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    // Última `*` vista y dónde estaba el nombre entonces, para retroceder.
    let (mut star, mut backtrack) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            backtrack = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            backtrack += 1;
            ni = backtrack;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(path: &str) -> FileClass {
        classify(path, &[])
    }

    #[test]
    fn plain_save_files_are_save_data() {
        for p in [
            "save1.sav",
            "autosave/autosave0.sav",
            "bmonster_4_6_2026_auto_1242.pss",
            "savedGames.gd",
            "level.dat",
            "world/region/r.0.0.mca",
        ] {
            assert_eq!(c(p), FileClass::SaveData, "{p}");
        }
    }

    /// La carpeta real de Cell to Singularity de un usuario, que hoy se
    /// sincroniza entera. Es el caso que motivó el módulo.
    #[test]
    fn the_unity_folder_that_started_this() {
        assert_eq!(c("Player.log"), FileClass::Junk);
        assert_eq!(c("Player-prev.log"), FileClass::Junk);
        assert_eq!(c("steam_autocloud.vdf"), FileClass::Junk);
        assert_eq!(
            c("Unity/0a8833bc-a8ad-47f7-abed-f8d04a6f02f8/Analytics/values"),
            FileClass::Junk
        );
        assert_eq!(
            c("Unity/ShaderVariantAnalytics/ShaderRuntimeInfoEvent.json"),
            FileClass::Junk
        );
        // Y las partidas de la misma carpeta salen intactas.
        assert_eq!(c("savedGames2.gd"), FileClass::SaveData);
        assert_eq!(c("savedGamesDeepBackup.gd.restore"), FileClass::SaveData);
    }

    #[test]
    fn os_and_temp_junk() {
        for p in [
            ".DS_Store",
            "Thumbs.db",
            "desktop.ini",
            "._save.sav",
            "crash_2026.dmp",
            "save.sav.tmp",
            "download.part",
        ] {
            assert_eq!(c(p), FileClass::Junk, "{p}");
        }
    }

    #[test]
    fn config_is_device_local_not_junk() {
        for p in [
            "graphics.ini",
            "settings.toml",
            "config.json",
            "GraphicsSettings.json",
            "Fallout4Prefs.ini",
            "UserOptions.dat",
            "keybinds.cfg",
            "video.yaml",
            "debug.log",
        ] {
            assert_eq!(c(p), FileClass::DeviceLocal, "{p}");
        }
    }

    /// La regla de la escalera: lo dudoso se sube igual. Sólo la basura
    /// inequívoca se queda fuera del snapshot.
    #[test]
    fn only_junk_is_dropped_from_the_backup() {
        assert!(!FileClass::Junk.is_backed_up());
        assert!(FileClass::DeviceLocal.is_backed_up());
        assert!(FileClass::SaveData.is_backed_up());
    }

    #[test]
    fn device_local_needs_an_explicit_yes_to_be_restored() {
        assert!(!FileClass::DeviceLocal.is_restored(false));
        assert!(FileClass::DeviceLocal.is_restored(true));
        // La basura no vuelve ni pidiéndolo: el interruptor es para config.
        assert!(!FileClass::Junk.is_restored(true));
        assert!(FileClass::SaveData.is_restored(false));
    }

    /// 582 plantillas del catálogo usan `*.ini` como patrón de save. Sin
    /// blindaje se las llevaría la regla por extensión.
    #[test]
    fn the_manifest_shield_beats_every_rule_below_it() {
        let shields = vec!["*.ini".to_string()];
        assert_eq!(classify("save01.ini", &shields), FileClass::SaveData);
        assert_eq!(classify("save01.ini", &[]), FileClass::DeviceLocal);

        let log_shield = vec!["*.log".to_string()];
        assert_eq!(classify("player.log", &log_shield), FileClass::SaveData);
        assert_eq!(classify("player.log", &[]), FileClass::Junk);
    }

    #[test]
    fn shields_match_on_the_basename_at_any_depth() {
        let shields = vec!["*.bksav".to_string()];
        assert_eq!(
            classify("Saves/slot3/quick.bksav", &shields),
            FileClass::SaveData
        );
    }

    #[test]
    fn degenerate_patterns_are_not_shields() {
        // Blindarían la carpeta entera y dejarían el filtro en nada.
        assert!(!is_useful_shield("*"));
        assert!(!is_useful_shield("*.*"));
        assert!(!is_useful_shield("**"));
        assert!(is_useful_shield("*.sav"));
        assert!(is_useful_shield("save*"));
        assert!(is_useful_shield("gamedata.bin"));
    }

    #[test]
    fn the_gate_is_shut_for_config_by_default() {
        let gate = RestoreGate::default();
        assert!(gate.allows("slot1.sav"));
        assert!(!gate.allows("graphics.ini"));
        assert!(!gate.allows("Player.log"));
    }

    #[test]
    fn the_gate_opens_for_config_when_asked_but_never_for_junk() {
        let gate = RestoreGate {
            shields: Vec::new(),
            allow_device_local: true,
        };
        assert!(gate.allows("graphics.ini"));
        // Basura no vuelve ni pidiéndolo.
        assert!(!gate.allows("Player.log"));
        assert!(!gate.allows(".DS_Store"));
    }

    #[test]
    fn a_shielded_config_file_still_goes_through_a_shut_gate() {
        let gate = RestoreGate {
            shields: vec!["*.ini".to_string()],
            allow_device_local: false,
        };
        assert!(gate.allows("save01.ini"));
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*.sav", "slot1.sav"));
        assert!(!glob_match("*.sav", "slot1.savx"));
        assert!(glob_match("save*", "save"));
        assert!(glob_match("profile?.sav", "profile1.sav"));
        assert!(!glob_match("profile?.sav", "profile12.sav"));
        assert!(glob_match("*save*.dat", "my_save_2.dat"));
    }

    /// Una carpeta `analytics` que sea del juego no es telemetría de Unity: el
    /// ancestro `Unity/` es lo que la condena.
    #[test]
    fn analytics_alone_is_not_enough() {
        assert_eq!(c("analytics/run1.sav"), FileClass::SaveData);
        assert_eq!(c("Unity/x/Analytics/run1.sav"), FileClass::Junk);
    }

    #[test]
    fn dotfiles_have_no_extension() {
        assert_eq!(extension_of(".bashrc"), None);
        assert_eq!(extension_of("save.sav"), Some("sav"));
        assert_eq!(stem_of("graphicssettings.json"), "graphicssettings");
    }
}
