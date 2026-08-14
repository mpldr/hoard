//! **Cuándo se actualiza Hoard solo, y cuándo deja de ser opcional.**
//!
//! Hasta aquí actualizar era un botón: la app miraba GitHub cada media hora,
//! pintaba una insignia ámbar y esperaba. Quien no la pulsaba se quedaba en su
//! versión para siempre, y como `hoard`, `hoardd` y la app se relevan juntos o
//! no se relevan (ver [`super`]), "para siempre" significa que un fallo
//! arreglado hace tres releases sigue vivo en máquinas que llevan meses
//! encendidas.
//!
//! Este módulo es la política que convierte aquel botón en lo que hace Steam:
//! se baja sola, se aplica sola, y si no se puede aplicar sola se aplica al
//! abrir. La decisión es **pura** —[`decide`] no toca red ni disco— porque el
//! caso que importa (¿qué pasa el segundo día, con un juego abierto, en una
//! máquina donde el paquete necesita root?) hay que poder probarlo sin una
//! máquina así delante.
//!
//! ## Las dos preguntas
//!
//! Todo sale de separar dos cosas que se confundían:
//!
//! 1. **¿Se puede aplicar sin molestar a nadie?** No lo decide el usuario ni
//!    una preferencia: lo decide **por dónde llegó la app**
//!    ([`super::Delivery`]). Un AppImage y un NSIS por-usuario se escriben en
//!    el home y nadie se entera; un `.deb` necesita un diálogo de polkit y un
//!    `.dmg` necesita una mano arrastrando. La primera familia se actualiza
//!    sola de verdad; la segunda sólo puede hacerlo con alguien delante.
//! 2. **¿Se acabó el plazo?** Desde que se ve una versión nueva corre un reloj
//!    ([`GRACE`], 48 h). Antes de que suene, una actualización que necesita a
//!    alguien se **ofrece** y se puede posponer. Después, no.
//!
//! El caso silencioso no llega nunca a la ventana: cuando la vía se aplica sola
//! y la máquina está en reposo, el usuario se entera por el número de versión.
//! El plazo existe para la otra mitad.
//!
//! ## Lo que el plazo no atropella
//!
//! "Obligatorio" no es "ahora mismo pase lo que pase". Relevar el núcleo
//! reinicia `hoardd`, y hacerlo con una subida a medias deja un blob colgando.
//! Así que hay dos frenos, y el plazo sólo levanta uno:
//!
//! - **Transferencia en vuelo** — frena siempre, plazo o no. Son segundos o
//!   minutos; esperarlos no le cuesta nada a nadie.
//! - **Juego abierto** — frena la actualización silenciosa (reiniciar el motor
//!   en mitad de una partida es justo el momento en que el sync importa), pero
//!   no la obligatoria. Si no, quien deja el juego abierto una semana no
//!   actualiza en una semana, que es el problema que veníamos a resolver.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::{Component, Delivery, Manifest};

/// Cuánto se tolera una versión vieja desde que se ve la nueva. Dos días: lo
/// que el usuario pidió, y lo que deja pasar un fin de semana sin encender el
/// ordenador sin que la primera sesión del lunes sea una actualización forzosa.
pub const GRACE: Duration = Duration::from_secs(48 * 60 * 60);

/// Anula [`GRACE`] (en horas). Para probar el plazo sin esperar dos días y para
/// una máquina que quiera ser más estricta; no se documenta como opción de
/// usuario porque el plazo largo es la política, no una preferencia.
pub const GRACE_ENV: &str = "HOARD_UPDATE_GRACE_HOURS";

/// El plazo efectivo de esta máquina.
pub fn grace() -> Duration {
    match std::env::var(GRACE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(hours) => Duration::from_secs(hours * 3600),
        None => GRACE,
    }
}

// =======================================================================
// Qué toca hacer ahora mismo
// =======================================================================

/// Lo que hay que hacer con la versión nueva **en este instante**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stance", rename_all = "snake_case")]
pub enum Stance {
    /// Nada: o no hay versión nueva, o esta instalación no es nuestra.
    Idle,
    /// Hay versión nueva y no está bajada. Bajarla y verificar su firma; no se
    /// aplica nada todavía.
    ///
    /// Bajar antes de decidir es lo que hace que "actualizar al abrir" dure lo
    /// que dura un `rename` en vez de lo que dura un bundle de 90 MB.
    Stage { version: String },
    /// Bajada, verificada, y esta máquina puede relevarse sin pedirle nada a
    /// nadie. Aplicar sin decir nada.
    ApplyQuietly { version: String },
    /// Bajada, pero aplicarla necesita a alguien delante (polkit, un `.dmg`).
    /// Se ofrece; se puede posponer.
    Ask { version: String },
    /// Se acabó el plazo. Se aplica, y si necesita a alguien delante la ventana
    /// no deja seguir hasta que se aplique.
    Force { version: String },
    /// Toca actualizar y no es el momento. El motivo va dentro porque un freno
    /// mudo es indistinguible de un updater roto — que es exactamente cómo se
    /// perdieron 36 minutos en D.12.
    Waiting { version: String, hold: Hold },
}

impl Stance {
    /// La versión a la que apunta, si apunta a alguna.
    pub fn version(&self) -> Option<&str> {
        match self {
            Stance::Idle => None,
            Stance::Stage { version }
            | Stance::ApplyQuietly { version }
            | Stance::Ask { version }
            | Stance::Force { version }
            | Stance::Waiting { version, .. } => Some(version),
        }
    }

    /// ¿Esto se aplica sin preguntar?
    pub fn is_automatic(&self) -> bool {
        matches!(
            self,
            Stance::ApplyQuietly { .. } | Stance::Force { .. } | Stance::Stage { .. }
        )
    }
}

/// Por qué se está esperando.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// Hay una copia o una restauración a medias. Frena siempre.
    TransferInFlight,
    /// Hay un juego abierto. Frena lo silencioso, no lo obligatorio.
    GameRunning,
}

/// Los hechos con los que se decide. Todos los saca quien llama (el daemon);
/// aquí no se mira nada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Situation {
    /// La versión que corre ahora mismo.
    pub current: String,
    /// La última publicada que sabemos. `None` = no se ha podido preguntar.
    pub latest: Option<String>,
    /// Lo que ya está bajado y verificado en disco.
    pub staged: Option<String>,
    /// Cuándo se vio [`Situation::latest`] por primera vez. De aquí sale el
    /// plazo.
    pub first_seen_at: Option<OffsetDateTime>,
    /// ¿Puede esta instalación relevarse entera sin privilegios ni manos?
    /// Sale de [`Manifest::applies_unattended`].
    pub unattended: bool,
    /// ¿Hay una copia o restauración en vuelo?
    pub transfer_in_flight: bool,
    /// ¿Hay un juego abierto?
    pub game_running: bool,
}

/// Qué hacer ahora. Pura: mismas entradas, misma respuesta.
///
/// El orden de las ramas **es** la política, y hay una que sorprende: el plazo
/// se comprueba *después* de bajar y *antes* que el reposo. Bajar primero
/// porque forzar lo que no está en disco es prometer una actualización
/// instantánea que va a tardar un minuto; el plazo antes que el reposo porque
/// esperar al reposo es justo lo que el plazo viene a dejar de hacer.
pub fn decide(now: OffsetDateTime, s: &Situation) -> Stance {
    let Some(latest) = s.latest.as_deref() else {
        return Stance::Idle;
    };
    if !crate::update::is_newer(latest, &s.current) {
        return Stance::Idle;
    }
    let version = latest.to_string();

    // Bajado ≠ bajado *esto*: una release publicada mientras la anterior estaba
    // en la caché deja `staged` apuntando a la vieja, y aplicarla sería
    // instalar a sabiendas algo que ya no es lo último.
    if s.staged.as_deref() != Some(latest) {
        return Stance::Stage { version };
    }

    // Una transferencia a medias frena todo, incluido lo obligatorio.
    if s.transfer_in_flight {
        return Stance::Waiting {
            version,
            hold: Hold::TransferInFlight,
        };
    }

    let overdue = s
        .first_seen_at
        .is_some_and(|seen| now - seen >= grace().try_into().unwrap_or(time::Duration::ZERO));

    if overdue {
        return Stance::Force { version };
    }

    // Un juego abierto sólo frena lo silencioso. Fuera del plazo, esperar a que
    // cierren el juego es no actualizar nunca.
    if s.game_running {
        return Stance::Waiting {
            version,
            hold: Hold::GameRunning,
        };
    }

    if s.unattended {
        Stance::ApplyQuietly { version }
    } else {
        Stance::Ask { version }
    }
}

// =======================================================================
// El registro en disco
// =======================================================================

/// Lo que hay que recordar entre arranques: qué se vio, cuándo se vio por
/// primera vez (el reloj del plazo), y qué hay bajado.
///
/// Vive en el directorio de estado y no en las preferencias a propósito: no es
/// una cosa que el usuario elija, es el cuaderno del updater. Mezclarlo con las
/// preferencias haría que borrar preferencias reiniciara el plazo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// La última versión publicada que hemos visto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_seen: Option<String>,
    /// Cuándo la vimos por primera vez. **El reloj del plazo.**
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub first_seen_at: Option<OffsetDateTime>,
    /// Qué versión está bajada y verificada en `staging_dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub staged_at: Option<OffsetDateTime>,
    /// Última vez que se preguntó a GitHub (para no preguntar en bucle).
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_check_at: Option<OffsetDateTime>,
    /// "Ahora no": hasta cuándo se calla lo que se puede posponer. No afecta al
    /// plazo — posponer retrasa la pregunta, no la fecha límite.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub snoozed_until: Option<OffsetDateTime>,
    /// De qué versión ya se avisó al usuario. Sólo se usa en el camino que
    /// necesita a alguien delante; sin esto el aviso saldría en cada ciclo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified: Option<String>,
    /// Qué salió mal en el último intento, y cuántos van seguidos. Lo segundo
    /// es lo que frena el bucle caliente: una release cuyo asset no existe para
    /// esta arquitectura fallaría cada cinco minutos para siempre.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub failures: u32,
}

impl Ledger {
    /// `<state>/update.json`.
    pub fn path() -> anyhow::Result<std::path::PathBuf> {
        Ok(crate::config::CliConfig::state_dir()?.join("update.json"))
    }

    /// Lee el registro. Un fichero ilegible o corrupto se trata como "no hay
    /// registro": el peor caso es reiniciar el plazo, y eso es infinitamente
    /// mejor que un updater que no arranca porque un JSON quedó a medias.
    pub fn load() -> Self {
        let Ok(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Anota lo que GitHub acaba de decir.
    ///
    /// El reloj del plazo se reinicia **sólo** cuando cambia la versión. Que se
    /// reiniciara en cada sondeo es el fallo que dejaría el plazo sin sonar
    /// jamás; que no se reiniciara al salir una versión más nueva forzaría la
    /// nueva con el plazo de la anterior, que es lo contrario.
    pub fn observe(&mut self, latest: &str, now: OffsetDateTime) {
        self.last_check_at = Some(now);
        if self.latest_seen.as_deref() != Some(latest) {
            self.latest_seen = Some(latest.to_string());
            self.first_seen_at = Some(now);
            // Lo bajado era de la versión anterior: ya no vale.
            if self.staged.as_deref() != Some(latest) {
                self.staged = None;
                self.staged_at = None;
            }
            self.snoozed_until = None;
            self.notified = None;
            self.failures = 0;
            self.last_error = None;
        }
    }

    /// Cierra el ciclo: esta versión ya corre. Deja el cuaderno en blanco para
    /// la siguiente.
    pub fn applied(&mut self, version: &str) {
        if self.latest_seen.as_deref() == Some(version) {
            self.first_seen_at = None;
        }
        self.staged = None;
        self.staged_at = None;
        self.snoozed_until = None;
        self.notified = None;
        self.failures = 0;
        self.last_error = None;
    }

    /// Cuándo deja de ser opcional, si es que hay algo pendiente.
    pub fn deadline(&self) -> Option<OffsetDateTime> {
        let seen = self.first_seen_at?;
        let g: time::Duration = grace().try_into().ok()?;
        Some(seen + g)
    }
}

// =======================================================================
// ¿Se aplica sola esta instalación?
// =======================================================================

impl Delivery {
    /// ¿Se puede relevar esta vía **sin diálogos y sin manos**?
    ///
    /// No es la negación de [`Delivery::needs_elevation`]: un `.dmg` no pide
    /// privilegios y aun así hace falta una persona arrastrándolo al Finder.
    /// La pregunta que importa aquí es "¿puede pasar mientras nadie mira?", y
    /// sólo dos vías la responden que sí.
    pub fn applies_unattended(self) -> bool {
        matches!(self, Delivery::AppImage | Delivery::Nsis)
    }
}

impl Manifest {
    /// ¿Puede esta máquina relevarse entera sin pedir privilegios ni manos?
    ///
    /// **Entera**: la regla de [`super`] es que las piezas van a la misma
    /// versión o no se toca ninguna, así que basta con que una necesite un
    /// diálogo para que toda la actualización lo necesite. Aplicar el núcleo en
    /// silencio y dejar la app esperando a un `pkexec` que el usuario cancela
    /// es justo el desajuste mudo que este módulo existe para no crear.
    pub fn applies_unattended(&self) -> bool {
        if let Some(d) = self.delivery {
            if !d.is_ours() {
                return false;
            }
        }
        if self.has(Component::Desktop) && !self.delivery.is_some_and(|d| d.applies_unattended()) {
            return false;
        }
        // El núcleo dentro del bundle no se releva por su cuenta: lo trae el
        // instalador de la app, así que hereda su respuesta (que ya es `true` si
        // hemos llegado hasta aquí con Desktop instalado).
        if self.core_from_bundle {
            return self.has(Component::Desktop);
        }
        self.core_dir.as_deref().is_some_and(dir_is_writable)
    }
}

/// ¿Podemos escribir aquí sin ser root? Se prueba escribiendo, no deduciendo la
/// respuesta de la ruta: `~/.local/bin` y `/usr/bin` son lo habitual, pero
/// `HOARD_INSTALL_DIR` deja el núcleo donde el usuario quiera y una lista de
/// rutas conocidas se equivocaría en silencio justo ahí.
fn dir_is_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".hoard-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(days: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(days)
    }

    fn situation() -> Situation {
        Situation {
            current: "1.0.0".into(),
            latest: Some("1.1.0".into()),
            staged: Some("1.1.0".into()),
            first_seen_at: Some(at(0)),
            unattended: true,
            transfer_in_flight: false,
            game_running: false,
        }
    }

    #[test]
    fn nothing_to_do_without_a_newer_release() {
        let mut s = situation();
        s.latest = None;
        assert_eq!(decide(at(0), &s), Stance::Idle);

        s.latest = Some("1.0.0".into());
        assert_eq!(decide(at(0), &s), Stance::Idle);

        s.latest = Some("0.9.0".into());
        assert_eq!(decide(at(0), &s), Stance::Idle);
    }

    #[test]
    fn downloads_before_it_decides_anything_else() {
        let mut s = situation();
        s.staged = None;
        assert_eq!(
            decide(at(0), &s),
            Stance::Stage {
                version: "1.1.0".into()
            }
        );
        // Incluso pasado el plazo: forzar lo que no está en disco es prometer
        // un `rename` y entregar una descarga.
        assert_eq!(
            decide(at(30), &s),
            Stance::Stage {
                version: "1.1.0".into()
            }
        );
    }

    #[test]
    fn stale_staging_is_not_good_enough() {
        let mut s = situation();
        s.staged = Some("1.0.5".into());
        assert!(matches!(decide(at(0), &s), Stance::Stage { .. }));
    }

    #[test]
    fn quiet_when_the_delivery_allows_it() {
        let s = situation();
        assert_eq!(
            decide(at(0), &s),
            Stance::ApplyQuietly {
                version: "1.1.0".into()
            }
        );
    }

    #[test]
    fn asks_when_someone_has_to_be_there() {
        let mut s = situation();
        s.unattended = false;
        assert_eq!(
            decide(at(0), &s),
            Stance::Ask {
                version: "1.1.0".into()
            }
        );
    }

    #[test]
    fn a_running_game_holds_the_quiet_path_but_not_the_deadline() {
        let mut s = situation();
        s.game_running = true;
        assert_eq!(
            decide(at(0), &s),
            Stance::Waiting {
                version: "1.1.0".into(),
                hold: Hold::GameRunning
            }
        );
        // Pasado el plazo, el juego deja de ser excusa.
        assert_eq!(
            decide(at(3), &s),
            Stance::Force {
                version: "1.1.0".into()
            }
        );
    }

    #[test]
    fn a_transfer_in_flight_holds_everything() {
        let mut s = situation();
        s.transfer_in_flight = true;
        assert_eq!(
            decide(at(30), &s),
            Stance::Waiting {
                version: "1.1.0".into(),
                hold: Hold::TransferInFlight
            }
        );
    }

    #[test]
    fn the_deadline_overrides_the_prompt() {
        let mut s = situation();
        s.unattended = false;
        assert!(matches!(decide(at(1), &s), Stance::Ask { .. }));
        assert_eq!(
            decide(at(2), &s),
            Stance::Force {
                version: "1.1.0".into()
            }
        );
    }

    #[test]
    fn the_clock_only_restarts_when_the_version_changes() {
        let mut l = Ledger::default();
        l.observe("1.1.0", at(0));
        assert_eq!(l.first_seen_at, Some(at(0)));

        // Mismo número, cuatro sondeos después: el reloj no se mueve, que es lo
        // único que hace que el plazo llegue a sonar.
        l.observe("1.1.0", at(1));
        assert_eq!(l.first_seen_at, Some(at(0)));
        assert_eq!(l.last_check_at, Some(at(1)));

        // Versión distinta: reloj nuevo, y lo bajado deja de valer.
        l.staged = Some("1.1.0".into());
        l.observe("1.2.0", at(5));
        assert_eq!(l.first_seen_at, Some(at(5)));
        assert_eq!(l.staged, None);
    }

    #[test]
    fn observing_the_version_already_staged_keeps_the_download() {
        let mut l = Ledger::default();
        l.observe("1.1.0", at(0));
        l.staged = Some("1.1.0".into());
        l.observe("1.1.0", at(1));
        assert_eq!(l.staged, Some("1.1.0".into()));
    }

    #[test]
    fn applying_clears_the_clock() {
        let mut l = Ledger::default();
        l.observe("1.1.0", at(0));
        l.staged = Some("1.1.0".into());
        l.applied("1.1.0");
        assert_eq!(l.first_seen_at, None);
        assert_eq!(l.staged, None);
        assert_eq!(l.deadline(), None);
    }

    #[test]
    fn a_managed_install_is_never_ours_to_touch() {
        let m = Manifest {
            version: "1.0.0".into(),
            components: vec![Component::Core, Component::Desktop],
            delivery: Some(Delivery::Managed),
            core_dir: None,
            desktop_path: None,
            core_from_bundle: false,
        };
        assert!(!m.applies_unattended());
    }

    #[test]
    fn a_native_package_needs_someone_there() {
        for d in [Delivery::Deb, Delivery::Rpm, Delivery::Dmg] {
            let m = Manifest {
                version: "1.0.0".into(),
                components: vec![Component::Core, Component::Desktop],
                delivery: Some(d),
                core_dir: None,
                desktop_path: None,
                core_from_bundle: true,
            };
            assert!(!m.applies_unattended(), "{d:?} should need a human");
        }
    }

    #[test]
    fn appimage_and_nsis_apply_themselves() {
        for d in [Delivery::AppImage, Delivery::Nsis] {
            assert!(d.applies_unattended(), "{d:?} should be silent");
        }
        for d in [
            Delivery::Deb,
            Delivery::Rpm,
            Delivery::Dmg,
            Delivery::Managed,
        ] {
            assert!(!d.applies_unattended(), "{d:?} should not be silent");
        }
    }

    #[test]
    fn a_headless_core_in_a_user_dir_applies_itself() {
        let tmp = std::env::temp_dir().join("hoard-auto-test-core");
        std::fs::create_dir_all(&tmp).unwrap();
        let m = Manifest {
            version: "1.0.0".into(),
            components: vec![Component::Core],
            delivery: None,
            core_dir: Some(tmp.clone()),
            desktop_path: None,
            core_from_bundle: false,
        };
        assert!(m.applies_unattended());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
