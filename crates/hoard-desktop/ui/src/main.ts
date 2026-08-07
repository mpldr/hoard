// i18n must be imported first so registrations and `init()` happen before any
// component subscribes to `$_`. The module has top-level side effects.
import { i18nReady } from "./lib/i18n";
import { initTheme } from "./lib/stores/theme";
import { initMotion } from "./lib/stores/motion";
import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";
import Overlay from "./lib/overlay/Overlay.svelte";

// Apply the persisted theme before mount so the first paint already uses the
// right palette — otherwise the app flashes the default Obsidian look before
// the store reads localStorage and swaps <html data-theme>. Pure DOM side
// effect, no i18n dependency, so it's safe to run synchronously here.
initTheme();
// Igual con la intensidad del relieve: marca `<html data-motion>` antes del
// primer pintado para que nadie vea el resplandor a tope y luego atenuarse.
initMotion();

// Wait for svelte-i18n to finish loading the active locale's dictionary
// before mounting. If we mount eagerly, the first render hits `$_(...)`
// while no messages are loaded, svelte-i18n throws, and Svelte unwinds —
// leaving the user with a blank, body-coloured window. (See v1.2.1 bug.)
//
// Es lo *único* que bloquea al mount: cargar un diccionario ya registrado. La
// preferencia de idioma guardada en disco corre en paralelo y tiene su propio
// plazo dentro de `i18nReady` — ver el módulo de i18n.
/**
 * El HUD sobre el juego es **otra ventana** con el mismo bundle, distinguida
 * por su etiqueta (la pone `commands/overlay.rs`). Se reparte aquí, antes de
 * montar, en vez de con una ruta: el router de `App.svelte` decide su destino
 * en `onMount` según la sesión y arrastraría al HUD a la pantalla de
 * bienvenida. Leer la etiqueta de la URL evita además cargar el puente de
 * ventanas de Tauri sólo para esto.
 */
function isOverlayWindow(): boolean {
  try {
    // Tauri publica la etiqueta en la query de la ventana.
    const q = new URLSearchParams(window.location.search);
    if (q.get("label") === "overlay") return true;
  } catch {
    /* sin URL utilizable — se asume ventana principal */
  }
  return (window as { __TAURI_INTERNALS__?: { metadata?: { currentWindow?: { label?: string } } } })
    .__TAURI_INTERNALS__?.metadata?.currentWindow?.label === "overlay";
}

async function bootstrap() {
  await i18nReady;
  if (isOverlayWindow()) {
    // La ventana es transparente: el fondo lo pinta el propio HUD, así que hay
    // que quitar el color de `body` o taparía el juego con un rectángulo negro.
    document.documentElement.classList.add("is-overlay");
    return mount(Overlay, { target: document.getElementById("app")! });
  }
  return mount(App, {
    target: document.getElementById("app")!,
  });
}

const app = bootstrap();

export default app;
