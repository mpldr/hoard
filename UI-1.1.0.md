# UI 1.1.0 — Rediseño del Dashboard

Fecha: 2026-07-26 · Alcance: **solo la ruta Dashboard** (`/dashboard`) · Sin commit.

Rediseño visual del Dashboard siguiendo el mockup aportado (grid de tarjetas
con carátula grande, lápiz de rename, stats por juego y barra resumen
inferior), dentro del lenguaje visual actual (Svelte 5 runes, Tailwind v4,
primario emerald-600, ámbar reservado a pausa/aviso/actualización).

**Lo que NO se ha tocado** (contrato congelado): `ui/src/lib/stores/*`,
`ui/src/lib/api/index.ts` (tipos que espejan el backend + puente de comandos),
nada en Rust, `App.svelte` (shell) ni otras rutas. Cero lógica de negocio
nueva en la UI: todo dato mostrado ya lo exponían los stores/API congelados.

**Invariante local/nube intacto** (ADR 0021 D.10): la pill de estado del
cuerpo de la tarjeta refleja SIEMPRE la versión que ESTE equipo tiene
(`local_version_num` + confirmaciones en vivo de `$activity`), y la cabeza de
la nube (`cloud_version_num`) va en un chip aparte sobre la carátula, con
tooltip que deletrea ambos números. No se han fusionado ni simplificado.

---

## Cambios, fichero por fichero

### `crates/hoard-desktop/ui/src/routes/Dashboard.svelte` (reescrito)

- **De lista vertical a grid de carátulas**: `grid-cols-1 → 4` responsive
  (`sm:2 · lg:3 · 2xl:4`), contenedor ensanchado a `max-w-[1600px]`. Cada
  tarjeta es el nuevo `SaveGameCard`.
- **Header**: título `dashboard.title` + línea de estado con el dot animado
  (verde pulsante si el servicio vive, zinc si no) y el contador de saves
  (claves existentes). El saludo `welcome_back` se retira del header para
  ceñirse al mockup. El botón Sign out se mantiene en el header con `mr-20`
  para no solaparse con el overlay fijo campana/ojo del shell (esquina
  superior derecha, `App.svelte`, fuera de alcance) — visualmente queda
  alineado con ellos, como en el mockup.
- **Banner de servicio caído** (nuevo): cuando `!$status.running` se muestra
  una banda ámbar (`service_offline_banner`) sobre el grid. El estado lo
  resuelven los stores; aquí solo se le da peso visual. El botón "Back up
  now" ya quedaba deshabilitado en ese caso (se conserva).
- **Toolbar** (mismo contenido, nueva disposición a la derecha, como el
  mockup): selector de orden (`recent`/`size`), grupo "max versions per
  game" **con toda su lógica intacta** (dry-run + modal de confirmación de
  poda sin cambios funcionales), y botón primario nuevo **Add Game** que
  navega a `/library` (donde vive el flujo de añadir).
- **Barra resumen inferior** (nueva): Total games (`saves.length`), Total
  versions (suma de los conteos por tarjeta), Total size (suma de
  `total_size_bytes`), Last backup (máx. `last_backup_at`, relativo +
  absoluto). Todo son agregados de datos ya expuestos.
- **Rename**: modal nuevo (patrón single-modal, `Modal.svelte` +
  `Input.svelte`) que escribe el override de nombre visible por dispositivo
  vía `stores/gameNames.ts` (`hydrateGameNames` en mount, `setGameName` al
  guardar). Campo vacío = volver al nombre automático. Es el store que ya
  existía para esto y estaba dormido; presentación pura, sin backend.
- **Pausa por tarjeta**: nueva acción (menú "…") que llama a
  `api.setSavePaused` (ya expuesta) y actualiza el array local.
- **Conteo de versiones por save** (`versionCounts`): tras cargar la lista
  se llama en segundo plano a `api.listSaveSnapshots(save_id, false)` por
  save (una lectura por tarjeta, nunca bloquea el grid). Se reconsulta
  cuando `$activity` confirma una versión nueva para ese save, y tras
  aplicar un cambio de "max versions" (la poda borra versiones).
- **QuotaBar retirado de la ruta**: el sidebar ya muestra `QuotaMini`
  (misma fuente `$auth.user`). Se MANTIENE el poll de `refreshQuota()` cada
  30 s para que la cifra del sidebar siga fresca (comentado en el código).
- Estados `loading` (shimmer) y vacío conservados; el vacío gana un CTA
  "Add Game".

### `crates/hoard-desktop/ui/src/lib/components/SaveGameCard.svelte` (nuevo)

Tarjeta por juego, presentacional (acciones delegadas al padre por
callbacks; solo lee `$activity` y `$customNames`):

- **Carátula grande** `aspect-[3/4]` con `Cover.svelte` (cápsula Steam o
  cover custom del usuario; `object-cover`). Mantiene el micro-editor de
  cover custom ya existente (lápiz/restaurar al hover) — eso cubre hoy las
  carátulas verticales perfectas.
- **Chip nube** arriba-izquierda sobre la carátula (`Cloud v{N}`; ámbar si
  la nube va por delante, tooltip con ambos números) + chip ámbar "Paused".
  Nunca se pinta como versión local.
- **Menú "…"** arriba-derecha (cierra al hacer click fuera): Renombrar /
  Pausar-Reanudar / Historial.
- **Fila de nombre**: nombre visible (override `gameNames` > slug
  "embellecido" por `prettifySlug`, p. ej. `elden-ring` → `Elden Ring`;
  tooltip con el slug real), chip de label SOLO cuando hay dos saves con el
  mismo slug, lápiz de rename, **campana deshabilitada** (ver "Pendiente de
  cablear") e icono de historial.
- **Pill de estado local** (mismos estados de siempre: running / scheduled
  con cuenta atrás / uploading / ok / partial / failed / cloud-only /
  no-backup) re-estilizada como chip con dot pulsante para estados activos
  + botón "Back up now" (deshabilitado si el servicio está caído).
- **Stats por juego**: Last saved (relativo + absoluto; `last_backup_at`),
  Total size (all saves) (`total_size_bytes`, server-side, con tooltip que
  lo aclara) y Total versions (conteo almacenado; `…` mientras carga, `—`
  si no se pudo obtener).
- Microinteracciones: `tilt` 3D + elevación/sombra al hover, transiciones
  suaves; reduced-motion lo cubre la CSS global existente.

### `crates/hoard-desktop/ui/src/lib/utils/format.ts` (aditivo)

`formatBytes` intacto. Añadidos: `prettifySlug` (fallback cosmético del
nombre), `formatRelativeTime` (reutiliza las claves `history.relative_*` +
la nueva `dashboard.time_yesterday`), `formatDateTime` (absoluto
locale-aware vía `toLocaleString`).

### `crates/hoard-desktop/ui/src/lib/i18n/locales/*.json` (8 ficheros)

19 claves nuevas + 3 valores actualizados por locale, insertadas tras el
bloque `dashboard.*` existente (diff mínimo, script con validación de JSON
y paridad: los 8 locales quedan con 812 claves idénticas).

**Nuevas** (×8 locales): `dashboard.add_game`, `dashboard.rename_aria`,
`dashboard.rename_modal_title`, `dashboard.rename_modal_body`,
`dashboard.rename_success`, `dashboard.menu_open`, `dashboard.pause`,
`dashboard.resume`, `dashboard.paused_toast`, `dashboard.resumed_toast`,
`dashboard.notify_soon`, `dashboard.last_saved`, `dashboard.total_size`,
`dashboard.total_versions`, `dashboard.total_games`,
`dashboard.last_backup`, `dashboard.time_yesterday`,
`dashboard.never_saved`, `dashboard.service_offline_banner`.

**Actualizadas** (mismo key, nuevo copy, ×8): `dashboard.sort_recent`
(→ "Last saved (newest)" / "Último guardado (más reciente)"),
`dashboard.sort_size` (→ "Total size (largest)" / "Tamaño total (mayor
primero)"), `dashboard.back_up` (→ "Back up now" / "Copiar ahora").

**Borradas**: ninguna. `dashboard.welcome_back` queda sin uso (se conserva
por si se reutiliza).

## Estados nuevos (descripción)

- **Pill local** como chip coloreado: sky+dot animado (jugando), ámbar+dot
  (programado/subiendo), esmeralda (guardado en este equipo v…), ámbar
  (partial), rojo (falló/reintentando), zinc (solo nube / sin copia).
- **Chip nube** sobre la carátula: zinc (al día) / ámbar (nube por delante),
  tooltip siempre con los dos números.
- **Banner ámbar de servicio caído** bajo el header cuando
  `status.running == false`.
- **Campana por juego deshabilitada** con tooltip `notify_soon`.
- **Resumen inferior**: 4 celdas con icono + cifra grande.

## Pendiente de cablear (lo hace otro)

1. **Campana por juego (mute de notificaciones por save)**: el mockup la
   muestra, pero el motor solo expone prefs GLOBALES
   (`notify_on_success`/`notify_on_failure`); no hay flag por save. Botón
   dejado **deshabilitado** con TODO visible en `SaveGameCard.svelte` y
   tooltip localizado. Necesita pref por save en el motor + wiring.
2. ~~**Carátulas verticales reales de Steam**~~ — **HECHO (28-jul)**. El
   informe de un usuario en Discord ("make sure the covers are square or 2:3
   ratio, an option for both would be perfect") lo adelantó al lote de la
   1.1.0:
   - `covers.rs` pide primero el arte vertical de Steam
     (`library_600x900_2x.jpg`, el 600×900 real; el `library_600x900.jpg` a
     secas sirve un 300×450) por las dos rutas de CDN, cachea en
     `{app_id}_600x900.jpg` y sólo cae al `header.jpg` cuando el juego no
     tiene vertical. Un 404 deja marcador `.none` para no repreguntar; un
     error de red NO, para no fijar un juego al apaisado por un arranque sin
     conexión.
   - El marco pasa de `3/4` a **2:3 o cuadrado a elección del usuario**
     (selector en la toolbar, `stores/coverShape.svelte.ts`, por dispositivo).
   - `Cover.svelte` gana `fit="smart"`: mide la imagen y el marco y hace
     letterbox sobre una copia desenfocada de sí misma cuando no encajan
     (umbral 40%, calibrado para que 2:3-en-cuadrado rellene y
     cuadrado-en-2:3 o el header apaisado se acolchen), en vez de recortar.
   - `editor="corner"`: el lápiz deja de tapar la carátula entera y se va a
     una esquina, como pedía el hilo.
3. **`Total versions` sin N llamadas**: se calcula con
   `list_save_snapshots(save_id, false)` por tarjeta (lectura en segundo
   plano). Si se quiere evitar, un campo `version_count` en `TrackedSave`
   lo resolvería de una pasada.
4. **Chrome del mockup que es del shell, no de la ruta**: Sign out /
   campana / ojo en barra superior global y quota en sidebar. La campana +
   ojo ya existen fijos en `App.svelte` y `QuotaMini` ya está en el
   sidebar; no se tocó `App.svelte` (fuera de alcance).

## Dudas / decisiones a revisar

- **Recorte del cover apaisado en marco 3/4**: se ve ~el 30% central del
  header de Steam. Aceptable con logos centrados, pero la solución real es
  el punto 2 de "Pendiente".
- **"This device (v…)"** se conserva en la pill en lugar del "Saved (v…)"
  del mockup: es la redacción deliberada de ADR D.10 para no leer nube como
  local. Cambiar el copy es trivial si se decide lo contrario.
- **Fecha absoluta** con `toLocaleString` (formato del locale) en vez del
  ISO fijo `2026-07-21 17:47` del mockup — mejor para i18n; se cambia si se
  prefiere ISO.
- **QuotaBar fuera de la ruta** (punto de arriba): si se echa en falta,
  reintroducirlo es una línea.
- **Rename = nombre visible por dispositivo** (`gameNames`), no el label de
  sincronización: el rename de label (server-side, `renameSaveLabel`) ya
  existe en Library y no se duplicó aquí. Si el mockup quería decir label,
  se añade al mismo menú "…".
- **Nombre por defecto**: slug embellecido (`elden-ring` → `Elden Ring`).
  Slugs raros pueden quedar menos bonitos (`rdr2` → `Rdr2`); el lápiz lo
  corrige por dispositivo.
- `prettifySlug`/relativos se usan también en Library en el futuro: ya
  están en `utils/format.ts` para reutilizar.

## Verificación

```
$ pnpm --dir crates/hoard-desktop/ui check
svelte-check found 0 errors and 0 warnings
```

Sin commit, como se pidió.
