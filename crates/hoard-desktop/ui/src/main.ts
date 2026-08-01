// i18n must be imported first so registrations and `init()` happen before any
// component subscribes to `$_`. The module has top-level side effects.
import { i18nReady } from "./lib/i18n";
import { initTheme } from "./lib/stores/theme";
import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";

// Apply the persisted theme before mount so the first paint already uses the
// right palette — otherwise the app flashes the default Obsidian look before
// the store reads localStorage and swaps <html data-theme>. Pure DOM side
// effect, no i18n dependency, so it's safe to run synchronously here.
initTheme();

// Wait for svelte-i18n to finish loading the active locale's dictionary
// before mounting. If we mount eagerly, the first render hits `$_(...)`
// while no messages are loaded, svelte-i18n throws, and Svelte unwinds —
// leaving the user with a blank, body-coloured window. (See v1.2.1 bug.)
//
// Es lo *único* que bloquea al mount: cargar un diccionario ya registrado. La
// preferencia de idioma guardada en disco corre en paralelo y tiene su propio
// plazo dentro de `i18nReady` — ver el módulo de i18n.
async function bootstrap() {
  await i18nReady;
  return mount(App, {
    target: document.getElementById("app")!,
  });
}

const app = bootstrap();

export default app;
