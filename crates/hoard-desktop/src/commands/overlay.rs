//! El HUD sobre el juego: una segunda ventana, casi transparente, con el
//! registro en vivo del servicio de sync.
//!
//! Es **la app normal**, no Hoard-Screen. Hoard-Screen es un proceso aparte que
//! compone paneles nativos; esto es una ventana Tauri más, con el mismo bundle
//! del frontend, que se distingue por su etiqueta (`OVERLAY_LABEL`). El
//! frontend mira esa etiqueta al arrancar y monta el HUD en vez de la
//! aplicación entera (ver `main.ts`).
//!
//! Por qué se crea aquí y no desde JS: la ventana necesita nacer sin
//! decoración, transparente, siempre encima y fuera de la barra de tareas, y
//! esas propiedades son de construcción. Crearla desde el webview obligaría
//! además a ampliar las capacidades para permitir la creación arbitraria de
//! ventanas, que es justo lo que no interesa abrir.
//!
//! **Orden con Hoard-Screen**: el overlay Pro es un proceso independiente que
//! también se pone siempre encima; entre dos ventanas "always on top" manda el
//! orden de activación del compositor, así que esta ventana se muestra **sin
//! robar el foco** (`focused = false`) para no colarse por encima de él.

use tauri::utils::config::Color;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// Etiqueta de la ventana. El frontend la compara para decidir qué montar, así
/// que cambiarla aquí obliga a cambiarla en `main.ts`.
pub const OVERLAY_LABEL: &str = "overlay";

/// Crea la ventana si no existe. Nace oculta: quien la enseña es
/// [`overlay_set_visible`], para que la primera pulsación del atajo no muestre
/// un rectángulo en blanco mientras el webview arranca.
fn ensure(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(w) = app.get_webview_window(OVERLAY_LABEL) {
        return Ok(w);
    }
    WebviewWindowBuilder::new(app, OVERLAY_LABEL, WebviewUrl::default())
        .title("Hoard")
        .transparent(true)
        // Fondo de ventana totalmente transparente. Sin esto se queda con el
        // del sistema —blanco opaco en la mayoría de plataformas— y **cualquier
        // píxel que el HUD no cubra lo enseña**: típicamente una línea clara de
        // uno o dos píxeles en un borde. La ventana principal sí fija el suyo
        // en `tauri.conf.json`; ésta se creaba sin ninguno.
        .background_color(Color(0, 0, 0, 0))
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .maximized(true)
        .visible(false)
        // Sin sombra: en Windows la sombra de una ventana sin decoración se
        // dibuja igual y deja un halo gris sobre el juego.
        .shadow(false)
        .build()
        .map_err(|e| format!("no se pudo crear el overlay: {e}"))
}

/// Muestra u oculta el HUD. Devuelve el estado en que queda.
#[tauri::command]
pub async fn overlay_set_visible(app: AppHandle, visible: bool) -> Result<bool, String> {
    let w = ensure(&app)?;
    if visible {
        let _ = w.show();
        // Se pide el foco a propósito: el HUD tiene un botón de cerrar y
        // responde a Escape, así que necesita teclado. Es lo mismo que hace
        // el overlay de Steam.
        let _ = w.set_focus();
    } else {
        let _ = w.hide();
    }
    Ok(visible)
}

/// Alterna el HUD. Es lo que llama el atajo global.
#[tauri::command]
pub async fn overlay_toggle(app: AppHandle) -> Result<bool, String> {
    let w = ensure(&app)?;
    let showing = w.is_visible().unwrap_or(false);
    overlay_set_visible(app, !showing).await
}

/// ¿Está el HUD en pantalla? Lo usa la página de Ajustes para pintar su estado.
#[tauri::command]
pub async fn overlay_is_visible(app: AppHandle) -> Result<bool, String> {
    Ok(app
        .get_webview_window(OVERLAY_LABEL)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false))
}
