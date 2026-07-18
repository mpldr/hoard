//! DETECCIÓN — correlación proceso↔escritura (fase 3, ADR 0020). LA JOYA.
//!
//! La señal más fiable de que una carpeta es un save: fue reescrita mientras
//! un proceso de JUEGO (no del sistema) estaba vivo. No depende del nombre
//! ni de la extensión, así que captura saves con nombres GUID o en idiomas
//! raros que el name-signal jamás atraparía (el benchmark del manifest mide
//! ~6% de recall sólo por nombre — ver `scoring::bench`).
//!
//! MECANISMO (este módulo): un store de observaciones persistido. Cuando el
//! `SaveWatcher` emite una escritura sobre un dir, el agente llama a
//! [`CorrelationStore::record`] con la foto de procesos vivos
//! ([`sample_game_processes`]); el store atribuye la escritura al proceso de
//! juego más probable y la guarda. El scoring consulta
//! [`CorrelationStore::signal_for`] y suma [`CORRELATION_BONUS`] (+0.50).
//!
//! NOTA DE INTEGRACIÓN: el bucle observador (cablear `SaveWatcher` sobre los
//! roots de `roots.rs` + muestreo de `sysinfo` en cada evento) vive en el
//! scheduler del agente y se cablea en un paso posterior. Este módulo provee
//! el store, el muestreo y la señal, todo testeable en aislamiento. En frío
//! (juego nunca observado) la señal vale 0 — es el límite conocido del ADR:
//! la correlación necesita al menos una sesión de juego observada.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sysinfo::System;

/// Bonus de score de ADR 0020 §2 para correlación proceso↔escritura. Es la
/// señal dominante: por sí sola más cualquier evidencia débil ya cruza el
/// cutoff de auto-confirmado (0.60).
pub const CORRELATION_BONUS: f32 = 0.50;

/// Procesos que NUNCA son el juego: sistema, shells, navegadores, el propio
/// Hoard y los launchers/overlays (que corren junto al juego pero no SON el
/// juego). Match por substring case-insensitive sobre el nombre del proceso.
const NON_GAME_PROCESS: &[&str] = &[
    // Sistema / shells.
    "svchost",
    "systemd",
    "kthreadd",
    "kworker",
    "gnome-shell",
    "plasmashell",
    "kwin",
    "xorg",
    "wayland",
    "pipewire",
    "pulseaudio",
    "dbus",
    "csrss",
    "winlogon",
    "services.exe",
    "lsass",
    "dwm.exe",
    "explorer.exe",
    "windowserver",
    "loginwindow",
    // Navegadores / runtimes genéricos.
    "chrome",
    "chromium",
    "firefox",
    "msedge",
    "brave",
    "safari",
    "electron",
    // Demonios del sistema observados colándose como "juego" (atribuían
    // escrituras a dockerd, avahi, etc.). Match por substring del nombre.
    "dockerd",
    "containerd",
    "multipathd",
    "avahi-daemon",
    "systemd-",
    "gvfsd",
    "gsd-",
    "pipewire-",
    "wireplumber",
    // Launchers / overlays / clientes (no son el juego en sí).
    "steamwebhelper",
    "steam.exe",
    "steam ",
    "epicgameslauncher",
    "galaxyclient",
    "battle.net",
    "origin.exe",
    "eadesktop",
    "ubisoftconnect",
    "uplay",
    // El cliente Ubisoft real que los juegos Ubisoft-en-Steam lanzan dentro del
    // prefijo: ni "ubisoftconnect" ni "uplay" son substring de
    // `UbisoftGameLauncher.exe` / `UbisoftGameLauncher64.exe` (el `upc.exe`
    // hermano va en la lista EXACTA). Reescribe `.../Ubisoft Game
    // Launcher/savegames/...` con su propio cloud sync y sobrevive al cierre del
    // juego → correlación eterna. Ver incidente PoP 2008 jul-2026.
    "ubisoftgamelauncher",
    // Infraestructura Wine/Proton/Steam-runtime en Linux: envuelve al juego
    // pero no es el juego. Sin esto la atribución `.first()` se queda con un
    // wrapper (`reaper`, `proton`, `wineserver`) en vez del ejecutable real.
    // Match por substring, así que `wineserver`, `wine64-preloader`,
    // `winedevice.exe` caen todos bajo "wine".
    "wine",
    "proton",
    "pressure-vessel",
    "pv-bwrap",
    "srt-bwrap",
    "reaper",
    "steam-runtime",
    "gameoverlayui",
    "steamerrorreporter",
    "winedevice",
    "plugplay.exe",
    "rpcss.exe",
    "conhost.exe",
    // Overlays / utilidades de fondo observadas correlacionándose por error con
    // carpetas de save (RivaTuner, AMD, herramientas de Windows). Tocan/observan
    // carpetas (Steam Cloud reescribe el save mientras corren) pero no son el
    // juego. Match por substring del nombre.
    "ctfmon",
    "taskhost",
    "rtss",
    "encoderserver",
    "rivatuner",
    "radeonsoftware",
    "windhawk",
    // Overlays / helpers de vendor de GPU en Windows: viven en Program Files, así
    // que la regla de ruta de sistema NO los alcanza y (a diferencia de los de
    // System32) hay que nombrarlos. Idlean a ~0% CPU, o sea que el orden por CPU
    // de `sample_game_processes` ya los relega — esto es solo red de seguridad
    // para el caso raro de juego pausado. "NVIDIA Overlay.exe", "nvcontainer.exe".
    "nvidia",
    "nvcontainer",
    // Apps de chat/dev que viven junto al juego y correlacionaban por error con
    // carpetas de save (Discord vivo mientras Steam Cloud reescribía el save de
    // Offworld → atribuido a Discord.exe). Viven en AppData/Local (Windows) o
    // /usr (Linux), así que la ruta no siempre los alcanza: hay que nombrarlos.
    // Match por substring.
    "discord",
    "slack",
    "claude",
    "anthropic",
    "node",
    "opencode",
    // OneDrive (sync daemon), Xbox / GamingServices (Windows gaming
    // infrastructure, not the game itself).
    "onedrive",
    "xbox",
    "gamingservices",
    "gamebar",
    // Herramientas de escritorio observadas disparando "heavy untracked
    // game-like process" en producción (log jul-2026): launchers/managers,
    // scripting, ofimática, editores y utilidades. Viven fuera de rutas de
    // sistema, así que hay que nombrarlas.
    "playnite",
    "autohotkey",
    "mspaint",
    "topaz",
    "winrar",
    "quicklook",
    "microsoft.media.player",
    "sdxhelper",
    "devenv",
    "winword",
    "crossdeviceservice",
    "logipluginservice",
    "generate_emu_config",
    // El propio Hoard.
    "hoard",
];

/// Procesos que nunca son juego pero cuyo nombre es demasiado corto para un
/// match por substring sin falsos positivos (`"code"` colisionaba con
/// `codename.exe`, `decode.exe`). Se aplica como match EXACTO (case-
/// insensitive) del basename del proceso y del exe.
///
/// `"upc"` es el cliente Ubisoft (hermano de `UbisoftGameLauncher.exe`, que sí va
/// por substring); como substring pescaría exes legítimos tipo `upcoming.exe`.
/// `"setup"` son instaladores (como substring pescaría juegos con "setup" en la
/// carpeta); `"achievements"` es el watcher de logros de GSE/Goldberg, que corre
/// JUNTO al juego emulado pero no es el juego.
const NON_GAME_PROCESS_EXACT: &[&str] = &[
    "code",
    "code.exe",
    "upc",
    "upc.exe",
    "setup",
    "setup.exe",
    "achievements",
    "achievements.exe",
];

/// Un proceso nacido dentro de esta ventana tras el arranque del SISTEMA es
/// infraestructura de autostart (Discord, trays, overlays de GPU, daemons de
/// `node`, el propio Claude), no un juego que el usuario lanzó. Se veta como
/// FUENTE de correlación — no como juego a secas: `is_game_like` no se toca, así
/// que un juego auto-lanzado al boot se sigue detectando por nombre/handle; sólo
/// no se usa para ATRIBUIR una carpeta de save (que es de donde salía el veneno:
/// Discord vivo desde el boot heredando el save de Offworld).
const BOOT_AUTOSTART_GRACE_SECS: u64 = 120;

/// Cuántos ticks seguidos debe ganar un proceso distinto del atribuido antes de
/// robarle la atribución a una correlación ya asentada. Sin esto, `record` hacía
/// last-writer-wins: un solo tick desafortunado (Discord primario mientras Steam
/// Cloud reescribe el save) machacaba al exe real del juego.
const ATTRIBUTION_SWITCH_STREAK: u32 = 3;

/// CPU mínima para que un proceso cuente como FUENTE de una correlación. El que
/// escribió la carpeta estaba ejecutando; un residente a ~0% no pudo ser — y
/// justo así se envenenaba el store: la carpeta la reescribía otro (Steam
/// Cloud, GSE) sin el juego vivo, y la atribución caía en el primer proceso
/// "con pinta de juego" del sistema aunque llevara horas dormido (el caso
/// MOUSE: un task horario heredó la carpeta y disparó GameStarted cada hora
/// durante días). Umbral bajo a propósito: el muestreo llega justo tras la
/// escritura, cuando el escritor real siempre marca algo de CPU.
const CORRELATION_SOURCE_MIN_CPU_PCT: f32 = 0.5;

/// Sesiones fantasma seguidas (arrancó/paró por señal débil sin UNA SOLA
/// escritura en la carpeta) que tumban la observación. Un juego de verdad
/// escribe su save al jugar — y cada escritura re-graba la observación y resetea
/// los strikes ([`CorrelationStore::record`]) — así que sólo una atribución
/// envenenada acumula. Ver [`CorrelationStore::strike_phantom`].
pub const PHANTOM_SESSION_STRIKES: u32 = 2;

/// Un proceso vivo candidato a juego.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GameProcess {
    pub name: String,
    pub exe: Option<PathBuf>,
}

/// Prefijos de ruta de SISTEMA: un ejecutable bajo ellos no es un juego
/// (demonios, librerías, runtimes de apps empaquetadas). Los juegos viven en
/// el home del usuario (Steam, Wine `~/.wine*`, Lutris, Heroic, Flatpak data)
/// o en `/opt/<juego>` — nunca en `/usr` ni `/lib`. Filtra el grueso de la
/// basura observada (hilos de Brave en `/opt/brave.com`, Electron en
/// `/usr/lib/...`, `gsd-*` en `/usr/libexec`).
/// Vale para Linux/SteamOS y macOS (`/System/` = daemons del sistema; los juegos
/// de mac viven en `/Applications/*.app`, no aquí).
const SYSTEM_EXE_PREFIXES: &[&str] = &[
    "/usr/", "/lib", "/lib64", "/bin", "/sbin", "/run/", "/System/",
];

/// Igual pero para Windows: un exe bajo el directorio de Windows (`C:\Windows\`,
/// System32, SysWOW64, WinSxS…) es del sistema, nunca un juego. Match por
/// substring `:\windows\` para cubrir cualquier letra de unidad. Esto filtra
/// `RuntimeBroker.exe`, `ssh.exe`, `conhost.exe`, etc. que en Windows envenenaban
/// la correlación porque `SYSTEM_EXE_PREFIXES` solo tenía rutas Linux.
fn is_windows_system_exe(exe_str: &str) -> bool {
    exe_str.contains(":\\windows\\")
}

/// `true` si el proceso parece un juego (no sistema/launcher/navegador).
/// Usa el nombre y, cuando está, la ruta del ejecutable: un nombre de hilo
/// genérico (`ThreadPoolForeg`) no delata al navegador, pero su exe sí.
pub fn is_game_like(name: &str, exe: Option<&Path>) -> bool {
    let lower = name.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    if NON_GAME_PROCESS.iter().any(|bad| lower.contains(bad)) {
        return false;
    }
    if NON_GAME_PROCESS_EXACT.iter().any(|bad| lower == *bad) {
        return false;
    }
    if let Some(exe) = exe {
        let exe_str = exe.to_string_lossy().to_lowercase();
        if SYSTEM_EXE_PREFIXES.iter().any(|p| exe_str.starts_with(p)) {
            return false;
        }
        if is_windows_system_exe(&exe_str) {
            return false;
        }
        // El nombre del proceso puede ser un hilo genérico; mira también el
        // basename del ejecutable contra la lista negra.
        if let Some(base) = exe.file_name().and_then(|s| s.to_str()) {
            let base = base.to_lowercase();
            if NON_GAME_PROCESS.iter().any(|bad| base.contains(bad)) {
                return false;
            }
            if NON_GAME_PROCESS_EXACT.iter().any(|bad| base == *bad) {
                return false;
            }
        }
    }
    true
}

/// Foto de los procesos vivos que parecen juegos. `sys` debe venir ya
/// refrescado por el caller — el agente ya mantiene y refresca un `System`
/// en su tick de actividad, así que la idea es reutilizarlo, no crear otro.
///
/// Dos filtros duros además de [`is_game_like`], aprendidos de basura real
/// observada en producción (hilos de kernel `cpuhp/0`, `nv_open_q`,
/// `ib_srv_wkr-2`, threads `tokio-runtime-w`… atribuidos como "juego"):
/// - se descartan los HILOS (kernel y userland): un juego es un PROCESO, no
///   un hilo de otro; `thread_kind().is_some()` los delata;
/// - se exige un EJECUTABLE en disco: un juego siempre tiene binario; los
///   hilos de kernel no (`exe()` es `None`). Esto es load-bearing: la
///   correlación alimenta el playtime, y atribuir un save a un worker de
///   kernel —que vive 24/7— acumularía horas para siempre.
pub fn sample_game_processes(sys: &System) -> Vec<GameProcess> {
    let boot = System::boot_time();
    let mut scored: Vec<(f32, GameProcess)> = Vec::new();
    for proc in sys.processes().values() {
        if proc.thread_kind().is_some() {
            continue;
        }
        let Some(exe) = proc.exe().map(|p| p.to_path_buf()) else {
            continue;
        };
        // Veto de autostart: un residente nacido junto al sistema no es la
        // fuente de una correlación de save. `start_time`/`boot_time` van en
        // segundos epoch, así que la resta es la antigüedad tras el boot.
        if proc.start_time().saturating_sub(boot) < BOOT_AUTOSTART_GRACE_SECS {
            continue;
        }
        // Veto de residente dormido: el escritor de la carpeta estaba
        // ejecutando, así que marca CPU en el intervalo del muestreo. Un
        // proceso a ~0% no escribió nada — atribuirle el save es el veneno.
        if proc.cpu_usage() < CORRELATION_SOURCE_MIN_CPU_PCT {
            continue;
        }
        let name = proc.name().to_string_lossy().to_string();
        if is_game_like(&name, Some(&exe)) {
            scored.push((
                proc.cpu_usage(),
                GameProcess {
                    name,
                    exe: Some(exe),
                },
            ));
        }
    }
    // Orden por CPU descendente: el juego real quema CPU mientras los helpers de
    // fondo (overlays, brokers, Steam Cloud) están casi a cero. `record()` atribuye
    // por `.first()`, así que este orden hace que la correlación apunte al juego
    // y no al primer proceso arbitrario del mapa. Empate ⇒ orden estable.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, p)| p).collect()
}

/// Una escritura observada en un dir, atribuida a un proceso de juego.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteObservation {
    pub dir: PathBuf,
    pub process_name: String,
    pub exe: Option<PathBuf>,
    pub observed_at_ms: u64,
    /// Cuántas veces se ha re-observado (más hits ⇒ más confianza).
    pub hits: u32,
    /// Proceso candidato distinto del atribuido y su racha de ticks seguidos
    /// como primario. Evita que un tick suelto robe la atribución; sólo tras
    /// `ATTRIBUTION_SWITCH_STREAK` gana. `default` para leer stores antiguos.
    #[serde(default)]
    challenger: Option<GameProcess>,
    #[serde(default)]
    challenger_streak: u32,
    /// Sesiones fantasma acumuladas: el proceso atribuido "arrancó" y "paró"
    /// sin que la carpeta recibiera una sola escritura. A
    /// [`PHANTOM_SESSION_STRIKES`] la observación se descarta (atribución
    /// envenenada). Cualquier escritura real ([`CorrelationStore::record`])
    /// resetea el contador. `default` para leer stores antiguos.
    #[serde(default)]
    phantom_strikes: u32,
}

/// Store persistido de correlaciones, indexado por dir observado.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CorrelationStore {
    #[serde(default)]
    observations: HashMap<PathBuf, WriteObservation>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl CorrelationStore {
    /// Ruta por defecto en disco, junto a `state.json`.
    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::config::CliConfig::state_dir()?.join("correlation.json"))
    }

    /// Carga el store; un fichero ausente o corrupto produce uno vacío
    /// (las observaciones son recolectables de nuevo, no son críticas).
    pub fn load(path: &Path) -> Self {
        let mut store: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        store.prune_invalid();
        store
    }

    /// Descarta observaciones que ya no superan las reglas ACTUALES de
    /// `is_game_like` o que no tienen exe en disco. Limpia el store envenenado
    /// por versiones previas con filtros más laxos: utilidades de fondo
    /// (`ctfmon`, `RTSS`, `taskhostw`, `RadeonSoftware`…) e hilos de kernel sin
    /// exe (`System`) que se colaban como "juego" y disparaban "arrancó". Es
    /// self-heal: en cuanto una sesión real reescriba la carpeta, se aprende de
    /// nuevo la correlación correcta.
    fn prune_invalid(&mut self) {
        self.observations
            .retain(|_, o| o.exe.is_some() && is_game_like(&o.process_name, o.exe.as_deref()));
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("serializing correlation store")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Registra una escritura en `dir` ocurrida mientras `processes` estaban
    /// vivos. Si no hay ningún proceso de juego, no hay correlación y no se
    /// guarda nada. Con varios juegos vivos se atribuye al primero (la
    /// atribución fina es best-effort; para la señal "esto es un save" basta
    /// con que CUALQUIER juego estuviera vivo).
    ///
    /// La atribución NO es last-writer-wins: una vez asentada, un proceso
    /// distinto sólo la sustituye si es primario `ATTRIBUTION_SWITCH_STREAK`
    /// ticks seguidos. Así un hit aislado de un residente (Discord mientras algo
    /// reescribe el save) no machaca al exe real del juego.
    pub fn record(&mut self, dir: &Path, processes: &[GameProcess]) {
        let Some(primary) = processes.first() else {
            return;
        };
        let entry = self
            .observations
            .entry(dir.to_path_buf())
            .or_insert_with(|| WriteObservation {
                dir: dir.to_path_buf(),
                process_name: primary.name.clone(),
                exe: primary.exe.clone(),
                observed_at_ms: 0,
                hits: 0,
                challenger: None,
                challenger_streak: 0,
                phantom_strikes: 0,
            });
        entry.observed_at_ms = now_ms();
        entry.hits = entry.hits.saturating_add(1);
        // Una escritura real absuelve: la carpeta está viva, la atribución se
        // está re-ganando ahora mismo.
        entry.phantom_strikes = 0;

        if primary.name == entry.process_name {
            // Confirma la atribución vigente; refresca el exe y borra retador.
            entry.exe = primary.exe.clone();
            entry.challenger = None;
            entry.challenger_streak = 0;
            return;
        }
        // Proceso distinto: acumula racha en vez de machacar.
        if entry
            .challenger
            .as_ref()
            .is_some_and(|c| c.name == primary.name)
        {
            entry.challenger_streak = entry.challenger_streak.saturating_add(1);
        } else {
            entry.challenger = Some(primary.clone());
            entry.challenger_streak = 1;
        }
        if entry.challenger_streak >= ATTRIBUTION_SWITCH_STREAK {
            entry.process_name = primary.name.clone();
            entry.exe = primary.exe.clone();
            entry.challenger = None;
            entry.challenger_streak = 0;
        }
    }

    /// Devuelve la observación que corrobora `dir`: coincidencia exacta o
    /// cualquier dir observado que sea `dir` o un ancestro suyo (el watcher
    /// es recursivo, así que la escritura puede registrarse en un padre).
    pub fn signal_for(&self, dir: &Path) -> Option<&WriteObservation> {
        self.observed_key(dir)
            .and_then(|k| self.observations.get(&k))
    }

    /// Clave real del store que corrobora `dir` (la misma resolución por
    /// ancestros de [`signal_for`], pero devolviendo la clave para mutar).
    fn observed_key(&self, dir: &Path) -> Option<PathBuf> {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            if self.observations.contains_key(d) {
                return Some(d.to_path_buf());
            }
            cur = d.parent();
        }
        None
    }

    /// Registra una sesión fantasma sobre la observación que corrobora `dir`:
    /// su proceso atribuido nació y murió sin una sola escritura en la
    /// carpeta. Un juego de verdad escribe al jugar, así que sólo una
    /// atribución envenenada (un task horario, un residente) acumula strikes;
    /// a [`PHANTOM_SESSION_STRIKES`] la observación se descarta y la señal
    /// débil muere con ella (se re-aprende sola en la próxima sesión real).
    /// Devuelve `Some(true)` si la observación cayó, `Some(false)` si sumó
    /// strike y sobrevive, `None` si `dir` no tenía observación.
    pub fn strike_phantom(&mut self, dir: &Path) -> Option<bool> {
        let key = self.observed_key(dir)?;
        let obs = self.observations.get_mut(&key)?;
        obs.phantom_strikes = obs.phantom_strikes.saturating_add(1);
        if obs.phantom_strikes >= PHANTOM_SESSION_STRIKES {
            self.observations.remove(&key);
            return Some(true);
        }
        Some(false)
    }

    /// Borra los strikes de la observación que corrobora `dir` — la sesión
    /// que acaba de cerrar SÍ escribió la carpeta, la atribución es legítima.
    pub fn absolve(&mut self, dir: &Path) {
        if let Some(key) = self.observed_key(dir) {
            if let Some(obs) = self.observations.get_mut(&key) {
                obs.phantom_strikes = 0;
            }
        }
    }

    /// Nombre de proceso atribuido a `dir`, sin la extensión `.exe`. Útil
    /// para la atribución de fase 4 (nombrar el save en la librería).
    pub fn attributed_name(&self, dir: &Path) -> Option<String> {
        self.signal_for(dir).map(|o| {
            o.process_name
                .strip_suffix(".exe")
                .unwrap_or(&o.process_name)
                .to_string()
        })
    }

    /// Itera las observaciones crudas (dir → observación). Lo usa la traza
    /// de diagnóstico de detección para enseñar qué dirs vigilados atribuye
    /// el store a un slug (la señal de fase 4).
    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &WriteObservation)> {
        self.observations.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }

    pub fn len(&self) -> usize {
        self.observations.len()
    }
}

/// Scoring estático ([`crate::scoring::score_dir`]) más el bonus de
/// correlación si el store corrobora el dir. Mantiene `scoring.rs` libre de
/// dependencias de runtime; el bonus se suma aquí.
pub fn score_with_correlation(
    path: &Path,
    name: &str,
    store: &CorrelationStore,
) -> crate::scoring::ScoreBreakdown {
    let mut b = crate::scoring::score_dir(path, name);
    if let Some(obs) = store.signal_for(path) {
        b.score = (b.score + CORRELATION_BONUS).clamp(0.0, 1.0);
        b.reasons
            .push(format!("process correlation ({})", obs.process_name));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_game_like_rejects_system_and_launchers() {
        assert!(!is_game_like("svchost.exe", None));
        assert!(!is_game_like("steamwebhelper", None));
        assert!(!is_game_like("hoard-agent", None));
        assert!(!is_game_like("chrome", None));
        assert!(!is_game_like("", None));
        // Un juego cualquiera pasa.
        assert!(is_game_like("eldenring.exe", None));
        assert!(is_game_like("Hades.exe", None));
    }

    #[test]
    fn is_game_like_rejects_background_overlays_and_tools() {
        // Utilidades de fondo observadas correlacionándose por error con
        // carpetas de save (caso real del store envenenado).
        for n in [
            "ctfmon.exe",
            "taskhostw.exe",
            "RTSS.exe",
            "EncoderServer.exe",
            "RadeonSoftware.exe",
            "windhawk.exe",
        ] {
            assert!(!is_game_like(n, None), "{n} debería filtrarse");
        }
    }

    #[test]
    fn load_prunes_poisoned_observations() {
        // Store con las entradas reales del bug: utils de fondo y sin exe.
        let json = r#"{"observations":{
            "/saves/ark":{"dir":"/saves/ark","process_name":"RTSS.exe","exe":"C:/RivaTuner/RTSS.exe","observed_at_ms":1,"hits":9},
            "/saves/repo":{"dir":"/saves/repo","process_name":"ctfmon.exe","exe":"C:/Windows/System32/ctfmon.exe","observed_at_ms":1,"hits":9},
            "/saves/ksp":{"dir":"/saves/ksp","process_name":"windhawk.exe","exe":null,"observed_at_ms":1,"hits":9},
            "/saves/eu5":{"dir":"/saves/eu5","process_name":"System","exe":null,"observed_at_ms":1,"hits":9},
            "/saves/real":{"dir":"/saves/real","process_name":"eldenring.exe","exe":"D:/Games/eldenring.exe","observed_at_ms":1,"hits":9}
        }}"#;
        let tmp = std::env::temp_dir().join(format!("hoard-corr-prune-{}.json", now_ms()));
        std::fs::write(&tmp, json).unwrap();
        let store = CorrelationStore::load(&tmp);
        let _ = std::fs::remove_file(&tmp);
        // Sólo sobrevive el juego de verdad.
        assert_eq!(store.len(), 1);
        assert!(store.signal_for(Path::new("/saves/real")).is_some());
        assert!(store.signal_for(Path::new("/saves/ark")).is_none());
        assert!(store.signal_for(Path::new("/saves/repo")).is_none());
        assert!(store.signal_for(Path::new("/saves/ksp")).is_none());
    }

    #[test]
    fn is_game_like_rejects_wine_proton_infrastructure() {
        // Los wrappers de Linux conviven con el juego pero no SON el juego;
        // sin filtrarlos, `.first()` atribuía el save a un wrapper.
        for n in [
            "wineserver",
            "wine64-preloader",
            "winedevice.exe",
            "proton",
            "reaper",
            "pv-bwrap",
            "pressure-vessel-wrap",
            "steam-runtime-launcher-service",
            "gameoverlayui",
        ] {
            assert!(!is_game_like(n, None), "{n} debería filtrarse");
        }
        // El ejecutable real del juego (mismo nombre que vería sysinfo bajo
        // Proton) sigue pasando.
        assert!(is_game_like("eu5.exe", None));
    }

    #[test]
    fn is_game_like_rejects_by_exe_path_and_basename() {
        // Hilo genérico de Brave: el nombre no delata, el exe sí.
        assert!(!is_game_like(
            "ThreadPoolForeg",
            Some(Path::new("/opt/brave.com/brave/brave"))
        ));
        // Runtime de Electron / apps en /usr.
        assert!(!is_game_like(
            "electro:disk$0",
            Some(Path::new(
                "/usr/lib/claude-desktop/node_modules/electron/dist/electron"
            ))
        ));
        assert!(!is_game_like(
            "gmain",
            Some(Path::new("/usr/libexec/gsd-sound"))
        ));
        // Un juego de Wine en el home pasa.
        assert!(is_game_like(
            "factorio.exe",
            Some(Path::new(
                "/home/u/.wine64/drive_c/Factorio/bin/factorio.exe"
            ))
        ));
    }

    #[test]
    fn is_game_like_rejects_windows_system_and_overlays() {
        // Procesos de fondo de Windows que envenenaban la correlación real:
        // ssh.exe / RuntimeBroker.exe bajo C:\Windows\ (ruta de sistema)…
        assert!(!is_game_like(
            "ssh.exe",
            Some(Path::new("C:\\Windows\\System32\\OpenSSH\\ssh.exe"))
        ));
        assert!(!is_game_like(
            "RuntimeBroker.exe",
            Some(Path::new("C:\\Windows\\System32\\RuntimeBroker.exe"))
        ));
        // …y overlay de NVIDIA en Program Files (no lo pilla la ruta, sí el nombre).
        assert!(!is_game_like(
            "NVIDIA Overlay.exe",
            Some(Path::new(
                "C:\\Program Files\\NVIDIA Corporation\\NVIDIA app\\NVIDIA Overlay.exe"
            ))
        ));
        // Otra unidad distinta de C: también cuenta como sistema.
        assert!(!is_game_like(
            "conhost.exe",
            Some(Path::new("D:\\Windows\\System32\\conhost.exe"))
        ));
        // eu5.exe instalado en la biblioteca de Steam pasa el filtro.
        assert!(is_game_like(
            "eu5.exe",
            Some(Path::new(
                "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Europa Universalis V\\eu5.exe"
            ))
        ));
    }

    #[test]
    fn is_game_like_rejects_onedrive_xbox_gamingservices_opencode() {
        // OneDrive sync daemon, Xbox infrastructure, GamingServices, opencode.
        for n in [
            "OneDrive.exe",
            "xbox.exe",
            "GamingServices.exe",
            "opencode",
            "opencode.exe",
        ] {
            assert!(!is_game_like(n, None), "{n} should be filtered");
        }
        // Real games still pass.
        assert!(is_game_like("eldenring.exe", None));
        assert!(is_game_like("Hades.exe", None));
    }

    #[test]
    fn is_game_like_rejects_code_by_exact_basename_not_substring() {
        // "code" as a substring would clobber codename.exe, decode.exe, etc.
        // The exact match catches VSCode without false positives.
        assert!(!is_game_like("code", None));
        assert!(!is_game_like("Code.exe", None));
        assert!(!is_game_like("code.exe", None));
        // Substring matches that look like "code" but aren't exact still pass.
        assert!(is_game_like("codename.exe", None));
        assert!(is_game_like("decode.exe", None));
        // Por basename del exe también, que es el caso que el basename existe
        // para tapar: nombre de hilo genérico (no delata) + exe FUERA de una
        // ruta de sistema (VSCode instalado en el home, que `SYSTEM_EXE_PREFIXES`
        // no alcanza). Ojo al escribir estos pins: con `/usr/...` manda la regla
        // de ruta y con `C:\...` manda el nombre — ninguno llega al basename, así
        // que ambos pasarían con el bloque de basename borrado.
        assert!(!is_game_like(
            "ThreadPoolForeg",
            Some(Path::new("/home/u/.local/share/apps/vscode/code"))
        ));
        // Un juego cuyo basename NO es "code" sigue pasando desde la misma ruta.
        assert!(is_game_like(
            "ThreadPoolForeg",
            Some(Path::new(
                "/home/u/.local/share/apps/eldenring/eldenring.exe"
            ))
        ));
    }

    #[test]
    fn is_game_like_rejects_ubisoft_client() {
        // El cliente que los juegos Ubisoft-en-Steam lanzan dentro del prefijo:
        // reescribe la carpeta de save con su propio cloud sync y sobrevive al
        // cierre del juego (incidente PoP 2008 jul-2026).
        assert!(!is_game_like("upc.exe", None));
        assert!(!is_game_like("UPC.exe", None));
        assert!(!is_game_like("upc", None));
        assert!(!is_game_like("UbisoftGameLauncher.exe", None));
        assert!(!is_game_like("UbisoftGameLauncher64.exe", None));
        // Por basename del exe también (el nombre puede ser un hilo genérico).
        // Ruta tal y como la ve el agente en el prefijo Proton de la Deck: el
        // basename sólo se extrae con separador nativo, así que el path Windows
        // "Z:\..." no serviría de pin aquí.
        let prefix = "/home/deck/.steam/steam/steamapps/compatdata/19900/pfx/drive_c\
            /Program Files (x86)/Ubisoft/Ubisoft Game Launcher";
        assert!(!is_game_like(
            "ThreadPoolForeg",
            Some(&Path::new(prefix).join("upc.exe"))
        ));
        assert!(!is_game_like(
            "ThreadPoolForeg",
            Some(&Path::new(prefix).join("UbisoftGameLauncher64.exe"))
        ));
        // "upc" va en la lista EXACTA justo por esto: como substring se comería
        // exes legítimos.
        assert!(is_game_like("upcoming.exe", None));
    }

    #[test]
    fn record_and_signal_with_ancestor_match() {
        let mut store = CorrelationStore::default();
        let save = PathBuf::from("/home/u/.local/share/Game/Saves");
        store.record(
            &save,
            &[GameProcess {
                name: "game.exe".into(),
                exe: None,
            }],
        );
        // Coincidencia exacta.
        assert!(store.signal_for(&save).is_some());
        // Un hijo del dir observado también corrobora (watcher recursivo).
        assert!(store.signal_for(&save.join("slot1")).is_some());
        // Un dir no relacionado, no.
        assert!(store.signal_for(Path::new("/etc")).is_none());
        assert_eq!(store.attributed_name(&save).as_deref(), Some("game"));
    }

    #[test]
    fn record_attribution_survives_single_intruder_tick() {
        // Correlación asentada con el juego real.
        let mut store = CorrelationStore::default();
        let dir = PathBuf::from("/home/u/.local/share/Game/Saves");
        let game = GameProcess {
            name: "game.exe".into(),
            exe: Some(PathBuf::from("/games/game.exe")),
        };
        let intruder = GameProcess {
            name: "discord.exe".into(),
            exe: Some(PathBuf::from("/apps/discord.exe")),
        };
        for _ in 0..5 {
            store.record(&dir, std::slice::from_ref(&game));
        }
        assert_eq!(store.attributed_name(&dir).as_deref(), Some("game"));
        // Un tick suelto de un intruso NO roba la atribución…
        store.record(&dir, std::slice::from_ref(&intruder));
        assert_eq!(store.attributed_name(&dir).as_deref(), Some("game"));
        // …y el juego real, al reaparecer, resetea la racha del intruso.
        store.record(&dir, std::slice::from_ref(&game));
        store.record(&dir, std::slice::from_ref(&intruder));
        store.record(&dir, std::slice::from_ref(&game));
        assert_eq!(store.attributed_name(&dir).as_deref(), Some("game"));
        // Sólo tras ATTRIBUTION_SWITCH_STREAK ticks seguidos gana el retador.
        for _ in 0..ATTRIBUTION_SWITCH_STREAK {
            store.record(&dir, std::slice::from_ref(&intruder));
        }
        assert_eq!(store.attributed_name(&dir).as_deref(), Some("discord"));
    }

    #[test]
    fn is_game_like_rejects_desktop_tools_and_managers() {
        // Herramientas reales del log jul-2026 que pasaban el filtro y
        // disparaban "heavy untracked game-like process" (o peor: quedaban
        // como fuente de correlación).
        for n in [
            "Playnite.DesktopApp.exe",
            "AutoHotkey64.exe",
            "mspaint.exe",
            "Topaz Photo AI.exe",
            "WinRAR.exe",
            "QuickLook.exe",
            "Microsoft.Media.Player.exe",
            "GameBar.exe",
            "GameBarFTServer.exe",
            "sdxhelper.exe",
            "devenv.exe",
            "WINWORD.EXE",
            "CrossDeviceService.exe",
            "LogiPluginService.exe",
            "generate_emu_config.exe",
            "setup.exe",
            "achievements.exe",
        ] {
            assert!(!is_game_like(n, None), "{n} debería filtrarse");
        }
        // "setup"/"achievements" van por match EXACTO: no se comen legítimos.
        assert!(is_game_like("setupgame.exe", None));
        assert!(is_game_like("myachievements2.exe", None));
        // Juegos reales siguen pasando.
        assert!(is_game_like("eldenring.exe", None));
        assert!(is_game_like("doomx64.exe", None));
    }

    #[test]
    fn phantom_strikes_drop_poisoned_observation() {
        let mut store = CorrelationStore::default();
        let dir = PathBuf::from("/home/u/AppData/LocalLow/Fumi Games/MOUSE/Save");
        let task = GameProcess {
            name: "hourlytask.exe".into(),
            exe: Some(PathBuf::from("/apps/hourlytask.exe")),
        };
        store.record(&dir, std::slice::from_ref(&task));
        // Primera sesión fantasma: strike, pero la observación sobrevive.
        assert_eq!(store.strike_phantom(&dir), Some(false));
        assert!(store.signal_for(&dir).is_some());
        // Segunda seguida: cae. La señal débil muere con ella.
        assert_eq!(store.strike_phantom(&dir), Some(true));
        assert!(store.signal_for(&dir).is_none());
        // Sin observación, el strike es un no-op.
        assert_eq!(store.strike_phantom(&dir), None);
    }

    #[test]
    fn real_write_resets_phantom_strikes() {
        let mut store = CorrelationStore::default();
        let dir = PathBuf::from("/home/u/.local/share/EU5/save games");
        let game = GameProcess {
            name: "eu5.exe".into(),
            exe: Some(PathBuf::from("/games/eu5.exe")),
        };
        store.record(&dir, std::slice::from_ref(&game));
        assert_eq!(store.strike_phantom(&dir), Some(false));
        // Una escritura real (record) absuelve; el siguiente strike vuelve a
        // ser el primero y la observación sobrevive.
        store.record(&dir, std::slice::from_ref(&game));
        assert_eq!(store.strike_phantom(&dir), Some(false));
        assert!(store.signal_for(&dir).is_some());
        // `absolve` explícito también resetea (sesión con had_pending).
        store.absolve(&dir);
        assert_eq!(store.strike_phantom(&dir), Some(false));
        assert!(store.signal_for(&dir).is_some());
        // El strike resuelve por ancestro igual que `signal_for` (watcher
        // recursivo: la observación puede vivir en un padre).
        assert_eq!(store.strike_phantom(&dir.join("slot1")), Some(true));
        assert!(store.signal_for(&dir).is_none());
    }

    #[test]
    fn no_game_process_records_nothing() {
        let mut store = CorrelationStore::default();
        store.record(Path::new("/x"), &[]);
        assert!(store.is_empty());
    }

    #[test]
    fn correlation_rescues_invisible_folder() {
        // Carpeta con nombre opaco (GUID) y vacía: estáticamente es
        // INVISIBLE — por debajo de SCORE_POSSIBLE, el walker la descarta.
        let tmp = std::env::temp_dir().join("hoard-corr-test-guid-1234");
        let _ = std::fs::create_dir_all(&tmp);
        let static_only = crate::scoring::score_dir(&tmp, "guid-1234");
        assert!(static_only.score < crate::scoring::SCORE_POSSIBLE);

        // La correlación sola (+0.50) la sube a "posible": deja de ser
        // descartada. Para AUTO-confirmar (≥0.60) el ADR pide además una
        // señal débil — eso es deliberado.
        let mut store = CorrelationStore::default();
        store.record(
            &tmp,
            &[GameProcess {
                name: "weirdgame.exe".into(),
                exe: None,
            }],
        );
        let correlated = score_with_correlation(&tmp, "guid-1234", &store);
        assert!(correlated.score >= crate::scoring::SCORE_POSSIBLE);
        assert!(correlated.score < crate::scoring::SCORE_CONFIRMED);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn correlation_plus_content_auto_confirms() {
        // Camino dorado del ADR: nombre opaco + un save reciente +
        // correlación de proceso ⇒ auto-confirmado con margen holgado.
        let tmp = std::env::temp_dir().join(format!("hoard-corr-golden-{}", now_ms()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("game.sav"), b"binary").unwrap();

        let mut store = CorrelationStore::default();
        store.record(
            &tmp,
            &[GameProcess {
                name: "weirdgame.exe".into(),
                exe: None,
            }],
        );
        let correlated = score_with_correlation(&tmp, "12345", &store);
        assert!(correlated.score >= crate::scoring::SCORE_CONFIRMED);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn store_round_trips_to_disk() {
        let tmp = std::env::temp_dir().join(format!("hoard-corr-{}.json", now_ms()));
        let mut store = CorrelationStore::default();
        store.record(
            Path::new("/a/b/Saves"),
            &[GameProcess {
                name: "g.exe".into(),
                exe: Some(PathBuf::from("/a/b/g.exe")),
            }],
        );
        store.save(&tmp).unwrap();
        let loaded = CorrelationStore::load(&tmp);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.signal_for(Path::new("/a/b/Saves")).is_some());
        let _ = std::fs::remove_file(&tmp);
    }
}
