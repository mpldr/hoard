//! Ventana principal: cuándo se muestra.
//!
//! La ventana se declara `"visible": false` en `tauri.conf.json`. Tauri la
//! crearía visible en cuanto el lado Rust termina de construirla, pero el
//! webview todavía tiene que arrancar su proceso y parsear el bundle, así que
//! el usuario veía un rectángulo en blanco (el fondo por defecto del webview,
//! no nuestro `bg-zinc-950`) durante todo ese hueco. Naciendo oculta, la
//! ventana aparece ya dibujada: es el frontend quien pide mostrarla con
//! [`ui_ready`] justo después del primer paint.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager};

/// Cuánto esperamos al frontend antes de mostrar la ventana por nuestra cuenta.
///
/// Con la ventana oculta por defecto, un frontend que nunca monta (un throw en
/// el bootstrap, como el bug de i18n de la v1.2.1) ya no deja una ventana en
/// blanco: deja una app **invisible**, que desde fuera se parece demasiado a
/// "no arranca". Este plazo garantiza que siempre haya algo en pantalla, aunque
/// sea la página rota, que es lo que el usuario puede reportar.
const FALLBACK_SHOW_AFTER: Duration = Duration::from_secs(8);

/// Decide si la ventana debe mostrarse en este arranque.
///
/// Arrancar en silencio (autostart con `--silent` + `start_minimised`) es la
/// única razón legítima para quedarse oculto: ahí la app vive en la bandeja
/// hasta que el usuario la abre. Se resuelve una vez en `setup()` porque
/// depende de los argumentos del proceso, no del estado de la UI.
#[derive(Debug, Default)]
pub struct StartHidden(AtomicBool);

impl StartHidden {
    pub fn set(&self, hidden: bool) {
        self.0.store(hidden, Ordering::Relaxed);
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Muestra la ventana principal salvo que este arranque sea silencioso.
fn show_main(app: &AppHandle) {
    if app.state::<StartHidden>().get() {
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        // Sin esto la ventana aparece detrás en algunos compositores de Linux
        // cuando el arranque fue lento: el foco se lo quedó otra ventana
        // mientras nosotros seguíamos ocultos.
        let _ = w.set_focus();
    }
}

/// El frontend ya pintó su primer frame y la ventana puede mostrarse.
///
/// Idempotente: `show()` sobre una ventana ya visible no hace nada, así que no
/// importa que el fallback se nos haya adelantado.
#[tauri::command]
pub fn ui_ready(app: AppHandle) {
    show_main(&app);
}

/// Red de seguridad: si el frontend no ha llamado a [`ui_ready`] dentro de
/// [`FALLBACK_SHOW_AFTER`], mostramos la ventana igualmente.
pub fn spawn_fallback_show(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FALLBACK_SHOW_AFTER).await;
        let already_visible = app
            .get_webview_window("main")
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        if already_visible || app.state::<StartHidden>().get() {
            return;
        }
        tracing::warn!(
            "the frontend never signalled ui_ready; showing the window anyway \
             (the UI is probably broken)"
        );
        show_main(&app);
    });
}

/// Marca este arranque como silencioso: la ventana se queda oculta hasta que
/// el usuario la invoque desde la bandeja.
pub fn mark_start_hidden(app: &AppHandle, hidden: bool) {
    app.state::<StartHidden>().set(hidden);
}

#[cfg(test)]
mod tests {
    use super::StartHidden;

    #[test]
    fn start_hidden_defaults_to_showing_the_window() {
        assert!(!StartHidden::default().get());
    }

    #[test]
    fn start_hidden_round_trips() {
        let flag = StartHidden::default();
        flag.set(true);
        assert!(flag.get());
        flag.set(false);
        assert!(!flag.get());
    }
}
