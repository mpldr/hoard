//! Telemetría de **Hoard Screen**: ¿abre alguien el overlay, cuánto lo tiene
//! puesto y qué monta dentro?
//!
//! Es la pregunta que decide si Screen merece más trabajo, y hasta ahora no
//! había forma de contestarla: 116 personas han tenido Pro durante siete días y
//! no existe un solo dato de si llegaron a lanzarlo una vez. Pulir a ciegas una
//! función que quizá nadie descubre es el trabajo más caro que hay.
//!
//! No hay tubería nueva. Son eventos `tracing` con un `target` fijo
//! ([`SCREEN_TARGET`]), así que viajan por `logship` como todo lo demás —con su
//! redacción de rutas y su opt-in— y se consultan con un `where target = …`.
//! Van a INFO porque el filtro del proceso (`info`) tiraría un DEBUG antes de
//! que ninguna capa lo viera, y `wire::ships_at` los exime del mínimo de Cloud
//! (WARN) igual que a las desmentidas de detección.
//!
//! ## Qué NO se manda
//!
//! Ni un título de ventana, ni un nombre de aplicación, ni una miniatura. El
//! overlay espeja ventanas arbitrarias del escritorio de otra persona: lo que
//! haya ahí no es asunto nuestro. Sale el **tipo** de panel (ventana / mirilla /
//! visor) y cuántos hay, que es lo que contesta la pregunta, y nada más.
//!
//! ## Una sesión = una fila de apertura y otra de cierre
//!
//! [`Session`] acumula en memoria mientras el overlay vive y suelta el resumen
//! al cerrar. Si la app muere de golpe con el overlay puesto, esa sesión pierde
//! su cierre: por eso el panel enseña aperturas y cierres por separado en vez de
//! fiarse de que casen. Un hueco visible es mejor que una media silenciosamente
//! sesgada hacia las sesiones cortas.

use std::sync::Mutex;
use std::time::Instant;

use hoard_core::wire::SCREEN_TARGET;

/// Por qué se acabó la sesión. Distinguirlas importa: `Crashed` mezclado con
/// `User` convierte un fallo en "al usuario no le interesó".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// El usuario cerró el overlay desde la app.
    User,
    /// El proceso terminó por su cuenta con código 0 (Esc / quit interno).
    SelfQuit,
    /// El proceso murió con código distinto de 0 o por señal.
    Crashed,
}

impl EndReason {
    fn as_str(self) -> &'static str {
        match self {
            EndReason::User => "user",
            EndReason::SelfQuit => "self_quit",
            EndReason::Crashed => "crashed",
        }
    }

    /// Traduce el código de salida del sidecar. `None` (señal, o kill nuestro
    /// tras pedir el quit) cuenta como cierre limpio: cuando cerramos nosotros
    /// ya hemos emitido el evento y este camino ni se recorre.
    pub fn from_exit_code(code: Option<i32>) -> Self {
        match code {
            Some(0) | None => EndReason::SelfQuit,
            Some(_) => EndReason::Crashed,
        }
    }
}

/// Qué monta la gente dentro del overlay. Es la pregunta de producto: si todo
/// el mundo pone mirillas y nadie espeja una ventana, Screen es otra cosa de la
/// que creemos que es.
#[derive(Clone, Copy, Debug, Default)]
struct ByKind {
    window: u32,
    crosshair: u32,
    scope: u32,
    other: u32,
}

impl ByKind {
    fn bump(&mut self, kind: &str) {
        match kind {
            "window" => self.window += 1,
            "crosshair" => self.crosshair += 1,
            "scope" => self.scope += 1,
            _ => self.other += 1,
        }
    }

    fn total(self) -> u32 {
        self.window + self.crosshair + self.scope + self.other
    }
}

/// Una sesión de overlay en curso.
pub struct Session {
    started: Instant,
    /// Desde cuándo está en modo edición, si lo está.
    editor_since: Option<Instant>,
    /// Tiempo acumulado en modo edición (segundos).
    editor_secs: f64,
    /// Cuántas veces se ha entrado en modo edición.
    editor_flips: u32,
    added: ByKind,
    removed: ByKind,
    /// Máximo de paneles vivos a la vez. El final no vale: alguien que monta
    /// cuatro y los quita antes de cerrar ha usado Screen, no lo contrario.
    peak_panels: u32,
    live_panels: i64,
    /// Escenas empujadas al overlay: mide el trasteo (mover, redimensionar,
    /// recortar) sin instrumentar cada arrastre.
    edits: u32,
    /// Botones asignados a un visor, por modo (`toggle` / `hold` / `timed`).
    bindings: u32,
    monitors: u32,
}

impl Session {
    fn new(monitors: u32) -> Self {
        Self {
            started: Instant::now(),
            editor_since: None,
            editor_secs: 0.0,
            editor_flips: 0,
            added: ByKind::default(),
            removed: ByKind::default(),
            peak_panels: 0,
            live_panels: 0,
            edits: 0,
            bindings: 0,
            monitors,
        }
    }

    /// Cierra el tramo de edición abierto, si lo hay, y devuelve el total.
    fn editor_total(&mut self) -> f64 {
        if let Some(since) = self.editor_since.take() {
            self.editor_secs += since.elapsed().as_secs_f64();
        }
        self.editor_secs
    }
}

/// Estado de Tauri: la sesión viva, si la hay. Se registra en `lib.rs` con
/// `.manage(ScreenTelemetry::default())`.
#[derive(Default)]
pub struct ScreenTelemetry(pub Mutex<Option<Session>>);

impl ScreenTelemetry {
    /// El overlay acaba de arrancar.
    pub fn opened(&self, monitors: u32) {
        let mut guard = self.0.lock().unwrap();
        // Una apertura con sesión viva no debería pasar (`screen_open` es
        // idempotente), pero si pasa, la vieja se pierde sin cierre y prefiero
        // que eso se vea en el panel a inventarle una duración.
        *guard = Some(Session::new(monitors));
        tracing::info!(
            target: SCREEN_TARGET,
            event = "open",
            monitors = monitors,
            "screen: overlay opened"
        );
    }

    /// El overlay se ha ido. Suelta el resumen de la sesión.
    ///
    /// Idempotente: si ya no hay sesión (cierre del usuario seguido del
    /// `Terminated` del proceso) no emite nada, para no contar dos veces.
    pub fn closed(&self, reason: EndReason) {
        let Some(mut s) = self.0.lock().unwrap().take() else {
            return;
        };
        let secs = s.started.elapsed().as_secs_f64();
        let editor_secs = s.editor_total();
        tracing::info!(
            target: SCREEN_TARGET,
            event = "close",
            reason = reason.as_str(),
            secs = secs.round() as u64,
            editor_secs = editor_secs.round() as u64,
            editor_flips = s.editor_flips,
            monitors = s.monitors,
            peak_panels = s.peak_panels,
            added = s.added.total(),
            added_window = s.added.window,
            added_crosshair = s.added.crosshair,
            added_scope = s.added.scope,
            removed = s.removed.total(),
            edits = s.edits,
            bindings = s.bindings,
            "screen: overlay closed"
        );
    }

    /// Modo edición dentro / fuera. Llega del propio overlay por stdout, así
    /// que cubre los dos caminos: el botón de la app y el Ctrl+O global.
    pub fn editor(&self, on: bool) {
        let mut guard = self.0.lock().unwrap();
        let Some(s) = guard.as_mut() else { return };
        if on {
            if s.editor_since.is_none() {
                s.editor_since = Some(Instant::now());
                s.editor_flips += 1;
            }
        } else if let Some(since) = s.editor_since.take() {
            s.editor_secs += since.elapsed().as_secs_f64();
        }
    }

    /// Un acto del usuario dentro del editor. `kind` sólo tiene sentido para
    /// `panel_add` / `panel_remove` (`window` / `crosshair` / `scope`) y para
    /// `binding` (`toggle` / `hold` / `timed`).
    ///
    /// Además de acumular en la sesión, emite su propia fila: el resumen dice
    /// "esta sesión montó dos visores", y las filas sueltas contestan la otra
    /// pregunta, la del embudo — cuánta gente ha llegado a usar cada pieza
    /// alguna vez.
    pub fn action(&self, action: &str, kind: Option<&str>) {
        {
            let mut guard = self.0.lock().unwrap();
            if let Some(s) = guard.as_mut() {
                let k = kind.unwrap_or("");
                match action {
                    "panel_add" => {
                        s.added.bump(k);
                        s.live_panels += 1;
                        s.peak_panels = s.peak_panels.max(s.live_panels.max(0) as u32);
                    }
                    "panel_remove" => {
                        s.removed.bump(k);
                        s.live_panels = (s.live_panels - 1).max(0);
                    }
                    "edit" => s.edits += 1,
                    "binding" => s.bindings += 1,
                    _ => {}
                }
            }
        }
        // `edit` se dispara en cada empujón de escena (arrastrar un panel son
        // muchos): se acumula en la sesión pero no genera fila propia, o el
        // vertedero lo montamos nosotros.
        if action == "edit" {
            return;
        }
        tracing::info!(
            target: SCREEN_TARGET,
            event = "action",
            action = action,
            kind = kind.unwrap_or("-"),
            "screen: {action}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_time_accumulates_across_flips() {
        let t = ScreenTelemetry::default();
        t.opened(1);
        t.editor(true);
        std::thread::sleep(std::time::Duration::from_millis(20));
        t.editor(false);
        let acc = {
            let mut g = t.0.lock().unwrap();
            g.as_mut().unwrap().editor_total()
        };
        assert!(acc >= 0.02, "se perdió el tramo de edición: {acc}");
        // Encender dos veces seguidas no arranca dos cronómetros ni cuenta dos
        // entradas: el overlay reemite su modo al resincronizar (`get_scene`).
        t.editor(true);
        t.editor(true);
        let g = t.0.lock().unwrap();
        assert_eq!(g.as_ref().unwrap().editor_flips, 2);
    }

    #[test]
    fn peak_panels_survives_removing_them_before_closing() {
        let t = ScreenTelemetry::default();
        t.opened(2);
        for kind in ["window", "crosshair", "scope"] {
            t.action("panel_add", Some(kind));
        }
        t.action("panel_remove", Some("window"));
        t.action("panel_remove", Some("scope"));
        let g = t.0.lock().unwrap();
        let s = g.as_ref().unwrap();
        assert_eq!(s.peak_panels, 3);
        assert_eq!(s.added.total(), 3);
        assert_eq!(s.removed.total(), 2);
        assert_eq!(s.added.scope, 1);
    }

    /// Quitar más de lo que hay (escena resincronizada, panel borrado dos
    /// veces) no debe dejar el contador vivo en negativo y falsear el pico
    /// siguiente.
    #[test]
    fn live_panels_never_goes_negative() {
        let t = ScreenTelemetry::default();
        t.opened(1);
        t.action("panel_remove", Some("window"));
        t.action("panel_remove", Some("window"));
        t.action("panel_add", Some("window"));
        let g = t.0.lock().unwrap();
        assert_eq!(g.as_ref().unwrap().peak_panels, 1);
    }

    #[test]
    fn closing_twice_only_reports_once() {
        let t = ScreenTelemetry::default();
        t.opened(1);
        t.closed(EndReason::User);
        assert!(t.0.lock().unwrap().is_none());
        // El `Terminated` del proceso llega después del cierre del usuario: no
        // debe emitir una segunda sesión ni entrar en pánico.
        t.closed(EndReason::SelfQuit);
    }

    #[test]
    fn an_exit_code_tells_a_crash_from_a_clean_quit() {
        assert_eq!(EndReason::from_exit_code(Some(0)), EndReason::SelfQuit);
        assert_eq!(EndReason::from_exit_code(None), EndReason::SelfQuit);
        assert_eq!(EndReason::from_exit_code(Some(1)), EndReason::Crashed);
    }
}
