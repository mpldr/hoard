# 0021 — Servicio local + clientes finos (desktop/CLI como vistas)

- **Estado:** Propuesta — 2026-07-23
- **Origen:** conversación de arquitectura. La Parte A (topología de procesos)
  la propuso el usuario; la Parte B (saneamiento interno del motor) sale del
  diagnóstico previo. Se documentan juntas porque se tocan, pero se pueden
  ejecutar por separado.

---

## Parte A — Un servicio local dueño del motor

### Problema

Hoy el motor de sync (`hoard_agent::agent::spawn`) va **embebido en dos
binarios**: `hoard-desktop` (`crates/hoard-desktop/src/commands/agent.rs:227`) y
el daemon CLI `hoard sync` (`crates/hoard-cli/src/commands/daemon.rs:151`). Cada
proceso corre su propia copia del motor.

Como dos motores a la vez causan ping-pong de backup/restore y reuse-detection
del refresh token de Supabase (401, porque ambos rotan **el mismo** token en
`cloud.toml`), existe un árbitro: `hoard_agent::instance` — un **pidfile**
(`<state_dir>/agent.pid`). El primero que arranca lo toma; el segundo ve al
dueño vivo (`live_owner()`) y **se aparta sin arrancar su motor**.

Ese árbitro es un parche con fallos de **diseño**, no de implementación:

- **Race de arranque.** Al encender el PC, el daemon CLI (si está configurado
  para arrancar en boot) y el autostart del desktop compiten. Quien llega
  primero a `AgentLock::acquire()` gana; el otro se aparta.
- **No hay reclaim.** El chequeo `live_owner()` es *one-shot*, al arrancar. Si
  ganó la CLI y luego la paras (`hoard sync stop`), el desktop que se apartó
  **no se entera de que el dueño murió** y no retoma el motor. El sync muere en
  silencio hasta reiniciar el desktop. **Éste es el bug que motiva la ADR:
  "paro la CLI y el desktop no vuelve".**
- **El ciclo de vida del motor está atado a una UI.** Cierras la app y el motor
  se va con ella (salvo que el dueño fuera la CLI). Dos frontends no pueden
  coexistir limpiamente; el sync de fondo depende de que "el proceso correcto"
  siga vivo.
- **Multi-escritor sobre credenciales.** Que dos procesos puedan rotar el
  refresh token es la causa raíz de una familia de bugs cloud (401 / realtime
  enmudecido). El pidfile lo evita por exclusión, no por diseño.

No hay ninguna IPC local hoy (sin `UnixListener` / named pipe / socket): desktop
y CLI solo se coordinan por el pidfile + `state.json` en disco + el server
remoto.

```mermaid
graph LR
  boot[Boot / autostart] --> D1["hoard-desktop<br/>(+ motor)"]
  boot --> C1["hoard-cli sync<br/>(+ motor)"]
  D1 -->|escribe/lee| P[("agent.pid<br/>pidfile lock")]
  C1 -->|escribe/lee| P
  D1 -.->|"el 2º se aparta<br/>sin reclaim"| C1
```

### Decisión

Promover el motor a un **proceso propio de vida larga: un servicio local**
(`hoardd` / `hoard-daemon`, nombre a decidir). **Exactamente un motor por
usuario**, propiedad del servicio. `hoard-desktop` y `hoard-cli` dejan de
embeber `agent::spawn` y pasan a ser **clientes finos**: se conectan al servicio
por IPC local, pintan/imprimen lo que el servicio reporta y le mandan comandos.
Ninguno corre lógica de sync.

`hoard_agent::instance` (el pidfile) **desaparece**: el árbitro pasa a ser la
*propiedad del socket* del servicio — un mutex real con liveness, no un pidfile
que se consulta una vez.

```mermaid
graph LR
  S["hoard-daemon<br/>ÚNICO motor de sync"]
  S ---|"IPC: cmd + push eventos"| D2["desktop<br/>(cliente/vista)"]
  S ---|"IPC: cmd + push eventos"| C2["cli<br/>(cliente/vista)"]
  S -->|único escritor| CT[("cloud.toml<br/>1 rotador de token")]
  S --> R["server remoto<br/>(cloud / self-host)"]
```

#### Por qué arregla los bugs de raíz

- **Race de arranque:** desaparece. Ambos clientes hacen lo mismo e idempotente
  — "conéctate al servicio; si no hay, arráncalo". Bajo carrera, el que pierde
  el arranque simplemente se conecta al que ganó (el bind del socket resuelve el
  empate).
- **No-reclaim:** desaparece. Matar un cliente nunca mata el motor. Cerrar la
  app tampoco. El servicio sigue.
- **Multi-escritor de credenciales:** desaparece. Solo el servicio toca
  `cloud.toml` y rota el token → un único rotador → mata la familia
  401/realtime.

#### Ciclo de vida

- El servicio es el **único dueño** del sync y **sobrevive** al cierre de la UI
  (ese es el punto).
- Arranca por dos vías:
  - **En boot**, como **servicio de usuario**: systemd *user* unit (Linux),
    launchd *user agent* (macOS), Startup / Task Scheduler por-usuario (Windows).
  - **On-demand**: si la app o la CLI arrancan y no hay servicio, lo levantan
    ("spawn if absent", handshake idempotente; socket-activation en Linux si se
    quiere afinar).
- **Por-usuario, nunca system-wide.** Necesita el keyring del usuario y su login
  cloud. En máquina multiusuario, un servicio por sesión de usuario.
- **Residente:** se queda vivo aunque no haya clientes (es sync de fondo).
  "Arranca si la app/CLI arranca" significa *asegurar que está arriba*, no atar
  su vida a ellos.

#### Transporte IPC ("lo que menos consuma")

Un único protocolo que compartan ambos clientes (la CLI no puede usar la IPC de
Tauri, así que reutilizar esa queda descartado):

- **Opción A — socket local** (UDS en Linux/macOS, named pipe en Windows) con
  protocolo *framed* (p. ej. JSON con length-prefix) + un canal **push** para
  eventos en vivo. Es la de menor consumo y hace trivial el streaming de estado
  (la UI no pollea).
- **Opción B — HTTP en loopback**, reutilizando los tipos y el cliente que la
  app ya usa contra `hoard-server`. Más pesado, pero reutiliza mucho y se
  depura con `curl`.
- **Recomendación: A**, con el push como sustituto directo del `events_tx`
  in-process que hoy recibe `agent::spawn` (`agent.rs:227`, `daemon.rs:151`). El
  `AgentEvent` pasa a ser el **protocolo de eventos por cable** — converge con
  el saneamiento del contrato UI↔backend (Parte B, racimo 3).
- **Seguridad:** socket con permisos solo-usuario (0600 / ACL del named pipe)
  para que otro usuario local no maneje tu sync ni tus credenciales.
- **Versionado:** ahora hay 2+ artefactos actualizables (servicio + app + CLI)
  que deben hablar el mismo protocolo; versionar el handshake.

#### Migración por fases (sin romper releases)

0. Hoy: motor embebido en desktop y CLI.
1. Extraer `hoard-daemon`: binario que hace `agent::spawn` y expone la IPC.
   Convive con el path embebido actual.
2. Desktop → cliente: quita su `agent::spawn`, habla con el servicio, lo levanta
   si falta.
3. CLI: `hoard sync` pasa a "asegura servicio + attach"; los comandos one-shot
   pasan a llamadas IPC.
4. Borrar `instance.rs` (el pidfile): ya no hace falta, el socket es el árbitro.
5. Packaging por plataforma: systemd user unit / launchd agent / Windows.
   (`deploy/systemd` ya tiene units, pero son del **server remoto** — otra cosa;
   el patrón y el tooling existen.)

#### Riesgos / decisiones abiertas

- **Quién posee tray y notificaciones.** Hoy el tray es del desktop. Si el
  desktop es "solo un cliente", ¿quién notifica con la ventana cerrada?
  Opciones: (a) el desktop se queda residente en tray como cliente-UI
  siempre-vivo y el servicio lo alimenta; (b) el servicio manda notificaciones
  nativas del SO directo (notify-send / dbus en Linux). **Decidir.**
- **Credenciales / keyring:** el servicio debe poseer el refresh del JWT y el
  storage de creds (hoy lo hace el wrapper del desktop). Por eso es por-usuario.
- **Updater:** 2+ artefactos que versionar y mantener compatibles de protocolo.
- **Activación en Windows/macOS:** sin socket-activation limpia como systemd; el
  handshake "spawn if absent" es la respuesta portable.

---

## Parte B — Saneamiento interno del motor (relacionado, paralelo, menor urgencia)

Independiente de la Parte A, pero el límite del servicio presiona a definir bien
la superficie de comandos/eventos del motor, que es justo el racimo 3.

Los bugs recurrentes de las últimas sesiones caen en tres racimos, cada uno una
costura rota **dentro** de `hoard-agent` (25k líneas; `agent.rs` solo, 6.179):

1. **Máquina de estados del save (el grande).** sticky 90s→6s, auto-restore que
   se auto-vetaba, deferred-pull que no aterrizaba, correlación fantasma que
   envenena el slot, `is_running` sin escape. Todos son la misma cosa:
   transiciones mal hechas de una máquina de estados **nunca hecha explícita**;
   el estado vive disperso en flags/timestamps del `SaveSlot` y se reconstruye a
   mano en `mid_session_reason` / `process_poll`.
   → **Núcleo puro de política:** `enum` de estado + reductor
   `decide(estado, evento, contexto) -> (estado, Vec<Accion>)`, sin
   tokio/HTTP/sysinfo; `run_agent` queda como runtime que solo hace I/O y
   ejecuta acciones. Cada bug se vuelve un test de 5 líneas. Las piezas ya puras
   (`mid_session_reason`, `accept_correlation_signals`, merge de restore) ya
   tienen tests — **extender** el modelo, no inventarlo.
2. **Transporte cloud.** token realtime que enmudece, 429 manejado solo en
   backup y no en restore, blob EOF, hot-loop de compresión → blobs huérfanos.
   Retry/token/integridad duplicados entre backup y restore.
   → **Un solo cliente cloud** que centralice refresh de token + retry/throttle
   + integridad de blob. (La Parte A ya elimina el multi-escritor del token.)
3. **Contrato UI↔backend.** notificaciones que escuchaban un evento viejo,
   commands Tauri sin cablear, login state mismatch.
   → tipos compartidos **bajados a `hoard-core`** (hoy 179 líneas, casi vacío;
   todo depende del god-crate, al revés de como debería) y contrato de eventos
   **tipado** — que en la Parte A *es* el protocolo IPC.

`detection.rs` (3.410 líneas) es el otro monstruo, pero es un concern casi
independiente y no es epicentro de bugs; al final.

### Secuenciación sugerida

La Parte A (servicio) y B.1 (núcleo de política) son ortogonales; cualquier
orden vale. Pero fijar el límite del servicio obliga a definir la superficie
comando/evento del motor, así que **A primero deja B.3 medio hecho gratis**. B.2
se beneficia de A (un solo rotador de token). Orden pragmático:
**A → B.3 → B.1 → B.2 → detection.**

---

## Parte C — Kernel sin-IO + defensas contra regresiones

Refina la Parte B tras una segunda pasada. La idea de fondo: las defensas que
de verdad frenan el goteo de bugs del vibecodeo no son "reordenar cajas", son
**hacer el bug imposible o visible al instante**. Y todas colapsan en una sola
pieza.

### C.0 La síntesis: un kernel de dominio sin-IO

El reductor de B.1, los newtypes de B.3/C.3 y los tipos de wire de B.3/C.6 **no
tienen IO**. Y la simulación (C.2) solo es barata si eso es cierto. Luego existe
un **kernel de dominio determinista** —función de `(estado, mundo_observado,
now, seed)`— y todo lo demás (daemon runtime, DB, HTTP/reqwest, Axum, Tauri) son
*shells de IO* a su alrededor. Ese kernel es, por fin, lo que justifica
`hoard-core` (hoy 179 líneas casi vacías): crate *leaf*, solo `serde`, sin
`tokio`/`axum`/`sqlx`. Contiene newtypes validados, tipos de wire/IPC, el
reductor reconciliador y el tipo `Scenario`. Es lo único que el simulador
maneja. **No son seis defensas: es un kernel + su banco de pruebas.**

### C.1 Reconciliador con la autoridad invertida

El loop actual ya es `tokio::select!` sobre `fs_rx` + un tick con `process_poll`
(`agent.rs:1027`, `agent.rs:4366`). El cambio no es un trasplante: **el tick
pasa a ser la fuente de verdad y los eventos (fs, realtime) quedan como hints
que adelantan el tick.** Matices load-bearing:

- **`spec` vs `status`.** La entrada del reconciliador no es solo el mundo
  muestreado; es el mundo **más su propia memoria durable**. Dos piezas que a
  primera vista parecían distintas son lo mismo (memoria propia, no mundo):
  - un **journal de hechos** (sesión empieza/acaba): playtime y "la sesión
    terminó" no se reconstruyen mirando la realidad actual; si los fuerzas al
    modelo puro de convergencia, los doble-cuentas o los pierdes.
  - las **operaciones en curso**: subir GB tarda minutos; sin esto, cada tick
    relanza el upload. La idempotencia sola no basta.
- **Anti-relaunch robusto a crash.** Un flag local "en curso" queda *stale* tras
  un crash. No lo resuelvas con estado local: resuélvelo **contra la verdad del
  server**. El storage es content-addressed, así que relanzar un upload ya
  aterrizado es un check de existencia barato (lookup en las tablas
  `blobs`/`chunks`), no un re-upload. Es el mismo principio que ya se aprendió en
  descarga (el fix de blob-EOF con reintento por-blob SHA-verificado); el
  reconciliador lo generaliza a la subida.
- **Observación por niveles** (Deck, batería): L0 = `stat` de mtime/size cada
  tick (barato); L1 = hash **solo** si L0 cambió o un hint enfocó ese save.
  Nunca re-hashear todo cada tick.
- **Invariante base:** convergido ⇒ solo `Hold`, cero acciones. Mata el
  hot-loop de compresión (1,29M ops en R2) — esa clase entera muere aquí.

### C.2 Sans-IO = inyectar TODO el no-determinismo

La simulación solo es gratis si el kernel es sans-IO **de verdad**, y casi todos
los bugs de timing (sticky 90s, auto-veto 5min, throttle con jitter,
min-interval) dependen del reloj. El kernel recibe `now` como entrada y **jamás**
llama a `Instant::now()` — **y también recibe el `seed` del rng**: el jitter del
throttle es aleatorio, con `thread_rng` la sim y el replay dejan de ser
deterministas. Ese es el refactor de verdad; la simulación viene gratis después.

- **Payoff:** time-travel en test (salto el reloj 5 min → pruebo el auto-veto
  sin esperar). Los bugs de timing pasan a triviales.
- **Método:** corpus primero (cada bug reciente = escenario determinista fijo),
  luego `proptest` con secuencias aleatorias + shrinking.
- **Invariantes:** las propuestas (converged ⇒ 0 acciones; storage acotado por
  tick) más el sharpening de la primera: su forma testable es **"ninguna acción
  sin un delta en la entrada que la cause"** (el hot-loop emitía acciones sin
  cambio de input). Ojo: `now` cruzando un deadline **es** un delta —por eso el
  retry tras un 429 no viola la invariante—. "Storage acotado por tick" se queda
  como guarda dura adicional.

### C.3 Newtypes con la puerta en `serde`

El veneno entró por **datos persistidos** (store envenenado, el "setup"), no por
código construyendo mal. Un `GameSlug` que valida en `new()` pero deriva
`Deserialize` a pelo no habría parado la correlación fantasma. Por tanto la
única vía de construcción —incluida la deserialización— debe pasar por el parse
validante: **no derivar `Deserialize`, usar `#[serde(try_from = "String")]`** →
una sola puerta imposible de saltar. Prioridad: los que cruzan serialización
(`GameSlug`, `Username`, `SaveId`, `Sha256`, `MachineId`).

- **Hazard de upgrade:** un `try_from` estricto sobre estado ya persistido
  **brickea** el estado viejo (el veneno ya está en disco → el daemon no carga).
  El serde-gate protege lo nuevo; lo viejo se **cleansea en la migración de
  C.4** (re-derivar el slug, loggear, marcar), nunca rechazar en duro. Van
  juntos.

### C.4 Estado en SQLite, detrás del daemon

Secuenciado **tras la Parte A**: el argumento "único escritor" solo es cierto
cuando `hoardd` es el único proceso que abre la DB. Antes del daemon tendrías
desktop + CLI abriendo SQLite multi-proceso en Windows (locking de fichero +
antivirus) y habrías cambiado una clase de bugs por otra.

- **Regla que hace real el single-writer:** la DB es **privada del daemon**; los
  clientes **nunca** la tocan, ni en lectura (read-only también choca con
  locking/AV en Windows). Todo acceso a estado va por la IPC. La DB es detalle
  interno del daemon, no fichero compartido.
- **Prefs dentro, sin excepción "editable a mano"** — la pref de 2s sobrevivió
  precisamente por ser fichero suelto.
- `rusqlite` a secas en vez de arrastrar `sqlx` async (ops de estado pequeñas y
  síncronas; no colorean de async el borde del kernel). **WAL** para commit
  atómico + durabilidad a crash. Migraciones versionadas (aquí vive el cleanse
  de C.3). Mata nonce/pref/store de golpe.

### C.5 Log de decisiones = corpus de replay

Si el kernel es puro, la terna de entrada es el formato del simulador de C.2. La
clave está en **qué** se graba: **las entradas** `(observación, now, seed)`, **no
la decisión**. Grabar-y-derivar: reproducir la traza contra el kernel-buggy
reproduce el bug; contra el kernel-parcheado **prueba el fix** — sobre la traza
real del Deck. (La decisión grabada se guarda como *aserción* para detectar
divergencia, pero la verdad del replay son las entradas.)

- **`Decision::{ Act(Action), Hold(reason) }`** — el veto es decisión de primera
  clase con motivo. Sticky y auto-veto vivían en el "no actuar"; así aparecen en
  el log y la invariante "convergido ⇒ solo `Hold`" es chequeable.
- Tabla-anillo en la SQLite de C.4 (queryable; `logship` la envía tal cual).
- **Privacidad:** contiene rutas y nombres de juego → redacción u opt-in antes
  de subir a cloud.

### C.6 Wire types en `hoard-core`, no crate nuevo

Viven en el mismo kernel leaf junto a los newtypes de C.3 (`serde`-only, sin
`tokio`/`axum`). El drift `agent::api` ↔ `server::routes` se convierte en error
de compilación. Pero **la coherencia de compilación solo vale dentro de un
build**: cliente y server se despliegan por separado, así que sigue haciendo
falta compat de wire entre versiones → append-only, `#[serde(default)]`, nunca
quitar/repurpose un campo, versión en el handshake (ya prevista en la Parte A
para la IPC), y un **test golden** de round-trip del JSON de la última release
para cazar rupturas.

### Cómo encaja con B

C reemplaza el "state machine limpio" de B.1 por el reconciliador; unifica B.3 y
B.6 en el kernel leaf; y añade las defensas nuevas (C.2 simulación, C.4 SQLite,
C.5 replay). Secuencia actualizada:
**A (daemon) → C.4 (estado en SQLite, ya con daemon) → C.1+C.2 (kernel
reconciliador + sans-IO + sim) → C.3 (newtypes) → C.5 (log/replay) → B.2 (cliente
cloud) → detection.**

(Ese orden es conceptual. El orden de **entrega** real, optimizado por
riesgo/shippability, es el de la Parte D y **manda** para ejecutar.)

---

## Parte D — Alcance, orden de entrega y protocolo de delegación

La implementación se delega a sesiones Opus, **un slice por sesión**. Esta parte
es el contrato que cada agente lee antes de tocar nada.

### D.0 Cómo entregar (meta)

- **No** en un solo task gigante ("haz toda la arquitectura"): reproduce el
  vibecodeo que queremos matar — pierde coherencia, no cabe en contexto, diff
  inrevisable.
- **No** en paralelo por partes: dependencias duras (SQLite ⟵ daemon ⟵ kernel de
  fiar; la migración lo bloquea todo) y las costuras entre agentes driftan
  (racimo 3).
- **Sí:** un slice por sesión, secuencial, verificado —y opcionalmente
  desplegado— antes del siguiente. Cada agente arranca leyendo **esta ADR +
  `CLAUDE.md`**; la ADR es la memoria compartida, así que un Opus fresco por
  slice se mantiene coherente sin heredar la conversación de diseño.

### D.1 Prerrequisitos (antes del Slice 1)

1. **Aterrizar el WIP.** Commit + release de todo lo "sin commitear/sin
   desplegar". Refactorizar sobre una pila de fixes a medias = conflictos y
   fixes perdidos. Pizarra limpia primero.
2. **Decidir el plan de migración de estado** (`state.json`/`cloud.toml` →
   SQLite sin flag-day, con cleanse de C.3). Es diseño, no código. **Bloquea el
   Slice 5**, no los anteriores.

### D.2 Guardarraíles — TODO slice los cumple

- **Fuente de verdad:** esta ADR + `CLAUDE.md`. No inventar arquitectura. Si el
  código contradice la ADR, o el slice te empuja fuera de alcance: **para y
  reporta**, no improvises.
- **Alcance cerrado:** tocar solo los ficheros del slice. Nada de "ya que estoy".
- **Paridad CLI↔desktop:** la lógica va en `hoard-agent`/kernel, nunca en los
  frontends (regla dura de `CLAUDE.md`).
- **Kernel sans-IO:** dentro del núcleo, jamás `Instant::now()`, `thread_rng` ni
  IO. `now` y `seed` se inyectan como entrada.
- **Slices de refactor = sin cambio de comportamiento**, y se **demuestra** con
  los tests existentes del motor en verde.
- **Verificar antes de dar por hecho:** `cargo check --workspace` y
  `pnpm --dir crates/hoard-desktop/ui check` limpios (0 warnings) + tests nuevos
  escritos y corriendo.
- **Invariantes como tests, no como comentarios.**
- **No commitear** salvo petición explícita (cadencia del usuario). Al terminar:
  resumen de qué cambió y cuál es el siguiente slice.
- **Producción no se toca sin permiso explícito.** Estas sesiones llevan MCP con
  acceso a la Supabase de producción, incluidas `execute_sql` y
  `apply_migration`. Regla: **nunca** escrituras ni migraciones contra
  producción; lecturas sólo si el slice las pide, y siempre reportadas. (Añadido
  tras el Slice 3, donde una consulta de *sólo lectura* a producción resultó
  valiosa —calibró la puerta de los newtypes contra 576 saves reales— pero no
  estaba autorizada. El resultado fue bueno; la regla existe para el agente que
  no tenga ese criterio.)

### D.3 Slices (orden de entrega, riesgo creciente)

Los tres primeros son refactors **in-place, sin packaging**: el motor sigue
embebido, cero cambio visible, y empiezan a pagar bugs desde el día uno.

- **Slice 1 — Beachhead del kernel** (no invertir el loop todavía). Crear el
  kernel leaf en `hoard-core` (solo `serde`): tipos `State` / `Observation` /
  `Decision::{ Act(Action), Hold(reason) }` / `Action`, con `now` y `seed` como
  entrada. Mover tras ese borde las funciones puras que **ya existen**
  (`mid_session_reason`, `accept_correlation_signals`, el merge de restore) con
  sus tests. No tocar `run_agent`. *Acceptance:* `hoard-core` compila solo con
  `serde`; comportamiento del motor idéntico (tests actuales verdes).
- **Slice 2 — Invertir la autoridad + sim.** `run_agent` pasa a reconciliador
  level-triggered (tick = verdad; fs/realtime = hints). `spec` vs `status`
  (journal de hechos de sesión + ops en curso). Observación L0/L1. Anti-relaunch
  contra la verdad del server (content-addressed). `proptest` + shrinking sobre
  el kernel. *Acceptance:* invariantes en verde; corpus D.4 reproducido y
  arreglado.
- **Slice 3 — Newtypes + wire types.** `GameSlug`/`Username`/`SaveId`/`Sha256`/
  `MachineId` con puerta en `serde` (`#[serde(try_from)]`). Tipos de wire/IPC en
  `hoard-core`. Test golden de round-trip del JSON de la última release.
  Estabiliza el contrato **antes** de construir la IPC encima.
- **Slice 4 — Daemon (Parte A).** Extraer `hoardd`, IPC (socket + push,
  construida sobre los wire types del Slice 3), "spawn if absent", borrar
  `instance.rs`. Desktop y CLI → clientes finos. Primer slice con packaging (3
  SOs, servicios de usuario). *Acceptance:* matar un cliente no mata el sync; sin
  race de arranque; un único rotador de `cloud.toml`. **Restricción:** mantener
  **estable la interfaz pública de los stores TS** de la UI (`ui/src/lib/stores`)
  al cambiar `invoke` → IPC; las pantallas no deben notar el cambio de backend.
  (Hay pulido de UI en paralelo que depende de esa interfaz fija.)
- **Slice 5 — Estado en SQLite (C.4).** `rusqlite` + WAL + migraciones, DB
  **privada del daemon** (clientes solo por IPC), migración + cleanse del estado
  viejo, prefs dentro sin excepción. *Requiere el plan de D.1.2.*
- **Slice 6 — Log/replay (C.5).** Tabla-anillo en la SQLite; loggear **entradas**
  (`observación, now, seed`), no decisiones; incluir los `Hold` (vetos).
- **Slice 7 — Cliente cloud único (B.2).** Centraliza refresh de token +
  retry/throttle + integridad de blob.
- **Slice 8 — detection.** El monstruo independiente, al final.

### D.4 Corpus de bugs (escenarios deterministas fijos para Slices 1–2)

Cada uno entra como test que reproduce el bug y afirma el invariante. La lista
viva está en la memoria del proyecto; los load-bearing:

- sticky 90s→6s y restore que se auto-vetaba 5min → invariante latencia de
  veto; `Hold` con motivo correcto.
- correlación fantasma (`slug == username`, store envenenado) → un proceso
  compartido no genera horas fantasma; identidad no acepta token genérico.
- 429 en restore (throttle solo se manejaba en backup) → disposición `Throttled`
  simétrica backup/restore.
- hot-loop de compresión (1,29M ops R2) → **convergido ⇒ 0 acciones**; ninguna
  acción sin delta de entrada.
- deferred-pull que no aterrizaba → sobrevive al veto y aterriza al cerrar el
  juego.
- blob EOF → reintento por-blob SHA-verificado, no re-bajar todo.

### D.5 Fuera de alcance (no tocar)

`hoard-screen` (overlay, funciona, concern aparte), el modelo de storage/chunking
(maduro, ADR 0018/0019/0020), la web de marketing, y la lógica de billing/cloud
del server salvo lo que fuercen los Slices 3 (wire) y —si se aborda— el GC/
refcount server-side (mismo bug de convergencia, `cleanup.rs`).

### D.6 — Notas de ejecución del Slice 2 (el difícil; correr a *max* effort)

Slice 2 **sí cambia comportamiento** (arregla el corpus D.4 diferido). Refina el
bullet de D.3. El Slice 1 dejó el vocabulario sembrado en
`crates/hoard-core/src/kernel/`; construye sobre él.

**Orden interno obligatorio (dos pasos verificables):**
1. Hacer crecer el kernel y escribir el reductor puro **sin tocar `run_agent`**
   → comportamiento idéntico, tests del motor verdes. Property-tests aquí.
2. Sólo con el reductor en verde, invertir `run_agent` para consumirlo.

**Crecer los tipos sembrados:**
- `State` (spec+status): + journal de hechos de sesión (playtime, "la sesión
  terminó" — no reconstruibles del mundo actual) + operaciones en curso por save.
- `Observation`: L0 (mtime+size cada tick) + L1 (hash sólo con señal) + evidencia
  de proceso + cabeza del server / versión cloud.
- `Action`: + `Backup`/`Push`, `Restore`, `DeferPull`, backoff de throttle
  (además del `Pull` sembrado). Al ganar payload (paths/listas) **suelta el
  derive `Copy`** de `Action`/`Observation`/`Decision` sin pelear al compilador.
- `Hold`: valora `enum HoldReason` en vez de `&'static str` si algún motivo
  necesita dato dinámico (throttle "hasta {t}") — hace matcheable el "retuvo por
  X" en los tests, mejor que comparar strings.

**El reductor:** `reconcile(&State, &Observation, World) -> (State, Vec<Decision>)`,
determinista. RNG del jitter = `StdRng::seed_from_u64(world.seed)`, **nunca**
`thread_rng` (`rand` ya es dep del crate).

**Invertir `run_agent`:** tick = fuente de verdad; fs/realtime = hints que sólo
*adelantan* un tick, nunca deciden. Loop = muestrear mundo → `Observation` →
`reconcile` → ejecutar `Decision`s (`Act`→IO; `Hold`→log del motivo). `run_agent`
sin política dentro.

**spec vs status:** la entrada del reductor incluye la memoria durable propia
(`State`: journal + ops en curso), distinta del `Observation` muestreado.

**Anti-relaunch contra la verdad del server:** un upload en curso que ya aterrizó
→ check de existencia barato (content-addressed, lookup en `blobs`/`chunks`), no
re-upload. Generaliza el reintento por-blob SHA-verificado que ya existe en
descarga.

**Observación L0/L1:** stat barato cada tick; hash sólo si L0 cambió o un hint
enfocó ese save (Deck/batería).

**Invariantes (property tests, `proptest` + shrinking):**
- convergido ⇒ sólo `Hold` (cero `Act`).
- ninguna `Act` sin un delta en la entrada que la cause (`now` cruzando un
  deadline **es** delta → el retry tras un 429 no la viola).
- nunca `Act(Restore)` mid-session. **(Corregido 2026-07-24: esta línea decía
  `Act(Backup)` y era falsa — el autobackup con debounce *mientras juegas* es la
  feature, no un bug. El invariante commiteado y bueno es
  `inv_restore_never_mid_session`. Manda el código.)**
- nunca perder un local más nuevo que el remoto.
- `Act` de storage acotadas por tick.

**Acceptance:** invariantes en verde; corpus D.4 diferido reproducido y arreglado
(sticky 90s→6s, `Throttled` simétrico backup/restore, convergido⇒0 acciones,
deferred-pull aterriza); los tests que codificaban el bug se actualizan con
justificación, el resto siguen verdes.

### D.7 — Slice 2 se parte en 2a (hecho) + 2b (invertir run_agent)

El paso 1 (reductor puro + invariantes + corpus) está commiteado (`d9a153b`).
Pero **el reductor es código muerto hasta que `run_agent` lo use**: las
correcciones (hot-loop, sticky, deferred-pull, 429) sólo existen en los tests del
kernel, no en el producto. **2b no es opcional** — es donde aterriza el valor.

Para el Opus de 2b:

- **Rotar los tests inline NO es salirse de alcance.** Invertir `run_agent`
  obsoleta unit-tests inline (`fs_event_triggers_backup_without_game_running`,
  `note_deferred_pull`/`deferred_pull_ready`, `sweep`, `record_failure`…) cuya
  lógica **se muda al reductor** (ya cubierta por proptests + corpus). Migrarlos a
  tests de reductor equivalentes y borrar los inline con justificación es la
  consecuencia esperada del slice. D.2 prohíbe el trabajo de *otros* slices
  (daemon, SQLite), no la reescritura intrínseca a éste.
- **La conversión vive en el shell, no en el kernel.** `SaveSlot`↔`kernel::State`
  y `TokioInstant`↔`OffsetDateTime` las hace `run_agent` al construir
  `World`/`Observation` y al reprogramar deadlines. El kernel se queda puro (sólo
  `OffsetDateTime`); nada de tokio dentro.
- **`Pull` y `Restore`: intents distintos, ejecutor único.** No dejes que se
  vuelvan dos caminos de ejecución divergentes — el 429 fue exactamente eso
  (throttle en un camino y no en el otro). Retry/throttle/integridad unificados
  en el executor aunque el kernel los pida por separado.
- **Correr a `max`, en sesión fresca.** Es el rewrite más arriesgado del plan
  (~660 líneas del `select!` + `sweep_for_auto_restore` + ramas fs/backup/restore).
  Contexto limpio + el reductor ya commiteado. Puerta de revisión después.

### D.8 — Revisión de 2b: lo que queda (Slice 2c, **sólo kernel**)

2b es correcto de forma: tick como única autoridad, `process_poll` degradado a
muestreador, ejecutor único para `Pull`/`Restore`, conversión entera en el shell,
L1 gateado por L0, −954 líneas. Lo que la puerta devuelve va **dentro del
kernel**; el shell no se vuelve a tocar salvo para *quitarle* política.

1. **Deadlock `has_pending` + `cloud_ahead` (bug real).** La rama de restore
   retorna (`reconcile.rs:128`) antes de la de backup (`:159`), así que con la
   nube por delante nunca se emite `Backup`, y `has_pending` sólo se limpia con un
   backup. Hoy lo desatasca el *ejecutor* de `DeferPull` (`agent.rs:1191`) — eso
   es **política en el shell**, prohibido por esta ADR y además invisible al
   replay de C.5. **Causa raíz:** `deferred_notified` (`reconcile.rs:117-121`) es
   un one-shot de flanco dentro de un reductor level-triggered: guarda la
   *acción* cuando sólo debería guardar la *notificación*. **Fix:** el reductor
   emite `Backup` al diferir un pull por `has_pending`; `deferred_notified` pasa a
   de-duplicar sólo el evento de UI; el flush sale del ejecutor. **Corpus:** dos
   adelantos de nube en la misma sesión, sin cierre de juego de por medio, no se
   encallan.
2. **Bookkeeping del shell → kernel.** Los tres que 2b dejó comentados (backoff de
   fallo de *backup*; `commit` vs `no-op` en `OpResult::Ok` — el ancla del
   min-interval, regresión R.E.P.O.; limpiar el backoff de restore con versión
   nueva) no son deuda estética: **cada trozo de política fuera del kernel es un
   agujero en la fidelidad del replay de C.5**. Deben estar dentro del kernel
   **antes del Slice 6**, o el replay no reproduce esas decisiones.
3. **`upload_landed` sin cablear.** El anti-relaunch va sólo por `in_flight`, que
   no sobrevive a un crash; el check content-addressed contra la verdad del server
   (C.1) sigue sin implementar. Tolerable ahora, **obligatorio con el Slice 4**,
   donde reiniciar el daemon es rutina.

**Cambios de comportamiento aceptados, con dos a vigilar:**

- **Muere el pre-launch barrier.** La gravedad depende de la cadencia del tick: si
  es de segundos, irrelevante; si es el poll de 60 s, la ventana "la nube se
  adelanta y lanzas antes del siguiente tick" te deja jugar una sesión entera
  sobre un save viejo → conflicto al cerrar (escenario Deck↔PC). Si es lo segundo,
  el arreglo limpio y compatible con level-triggered es que **el arranque de
  proceso sea un hint que adelanta el tick**, y que el reductor permita restaurar
  con el juego vivo pero aún sin escribir (`local == synced_fingerprint`).
- **`ForceRestore` respeta el cooldown de 60 s.** Aceptable como límite de thrash;
  sólo muerde si hubo un pull hace <60 s. Si el handoff Deck↔PC se nota lento, el
  knob es distinguir *reintento* (misma versión → cooldown) de *información nueva*
  (`cloud_version` nueva → colapsa el cooldown).

**Rotación de tests: correcta.** Reapuntar
`fs_event_triggers_backup_without_game_running` de `BackupScheduled` a
`BackupStarted` en vez de borrarlo es lo que había que hacer: prueba la cadena
nueva de extremo a extremo.

### D.9 — Slice 2 cerrado (2a + 2b + 2c). Decisiones a no deshacer

D.8.1 y D.8.2 resueltos en 2c. **D.8.3 (`upload_landed` / anti-relaunch
content-addressed) sigue abierto → va con el Slice 4**, donde reiniciar el daemon
es rutina y el `in_flight` local deja de bastar.

**Dos carriles de "no actuar hasta T" — no volver a fusionarlos.**
`next_backup_at` es **sólo backoff de error** y nunca es saltable. El suelo de
min-interval **no se almacena**: se deriva de
`last_backup_at + min_backup_interval_secs`, y sólo lo salta el flush de
desatasco cross-device. Fusionarlos otra vez reintroduce (a) el R.E.P.O. — un
backup no-op anclaría el intervalo — y (b) una espera de hasta 600 s (preset
`data_saver`) antes de destrabar un pull cross-device. Derivar el suelo hace que
el no-op no lo ancle *por construcción*, en vez de por caso especial.

**Guarda pendiente sobre ese carril.** El suelo de ahorro es lo que protege de la
factura de ops de R2 (el hot-loop costó 1,29M). El carril que lo salta debe
seguir siendo **sólo** el desatasco (`has_pending ∧ cloud_ahead ∧ !in_flight`),
que es auto-limitado (si acierta, la condición se cierra; si falla, arma el
backoff no-saltable). Añadir la propiedad de seguridad que lo fija: *fuera de esa
condición, ningún backup se emite antes de `last_backup_at + min_interval`*.

**Riesgo residual del port.** 2a introdujo al menos un drift silencioso respecto
al motor original (`record_failure` anclado en `known_version` en vez de
`obs.cloud_version`, cazado en 2c). Puede haber más anclas/umbrales portados mal;
el corpus + proptests son la red. Ante un comportamiento raro del motor, sospecha
primero de un ancla portada mal, no de la arquitectura.

**Cadencia del tick, medida (2c):** `AgentConfig::poll_secs = 2` (8 s en idle vía
`IDLE_POLL_MULT`), más `reconcile_all` inmediato en cada comando, en `done_rx` y
en el nudge del debounce fs. `CLOUD_POLL_INTERVAL_SECS = 60` es otra cosa: el
airbag del push Realtime, que al llegar dispara `SetCloudVersions` + reconcile
inmediato. Por eso la pérdida del pre-launch barrier (D.8) es de **segundos** y se
decidió no compensarla.

### D.10 — Slice 3 cerrado (newtypes + wire). Decisiones a no deshacer

**Dos puertas, no una.** `hoard_core::ids` expone `parse` (estricta, la que usa
`serde` vía `#[serde(try_from = "String")]`) y `repair` (indulgente, sólo para
bytes **ya persistidos**). No fusionarlas: la primera protege el dato nuevo, la
segunda existe porque el veneno ya está en disco y un `try_from` estricto sobre
`state.json` o sobre la SQLite del server dejaría el motor sin arrancar. Los tres
desenlaces de `repair` (`Clean` / `Repaired` / `Quarantined`) nunca son un error.

**Un slug degenerado no se renombra.** `users`, `base` o el nombre de usuario del
perfil son slugs bien formados; lo que está mal es que signifiquen cualquier
cosa. Renombrarlos cambiaría la identidad `(user_id, game_slug, label)` que el
server ya conoce y crearía un save nuevo en la nube. Se **marcan**
(`CliState::is_slug_quarantined`) para que la correlación los ignore, y punto. Lo
que sí se re-deriva es el slug sintácticamente inválido (`GSE Saves`), que desde
este slice ni siquiera podría subirse.

**La lista de tokens genéricos está partida a propósito.**
`ids::GENERIC_IDENTITY_TOKENS` es estática y pura (el kernel no lee el entorno);
`agent::is_generic_identity_token` la extiende con los componentes del home real
—el username, que fue el caso que rompió—. No mover la parte dinámica al kernel.

**`GameSlug` normaliza (trim + minúsculas), no sólo valida.** Es idempotente, no
puede brickear, y mata la clase "el mismo juego con dos cajas". `SaveId` en
cambio **no** normaliza más que la caja hex: un id es una clave contra el server,
y "arreglarlo" apuntaría a otro sitio. Comprobado contra la DB de producción
(576 saves, 0 slugs no canónicos) antes de endurecer la puerta.

**`""` no es un sha malo, es "no aplica".** Las versiones legacy de archivo
entero no tienen digest por fichero. Eso se modela con `Option<Sha256>` y un
serializador que traduce `""` ↔ `None` (`wire::sha_opt`), **no** relajando la
puerta de `Sha256`. La verificación trata `None` como fallo, igual que antes.

**Alcance del wire:** el contrato self-hosted (`agent::api` ↔ `server::routes`)
más `/v1/logs`, que era drift real (`target`/`ts` obligatorios en el cliente,
`Option` en el server). Los DTO `Cloud*` (`agent::api` ↔ `server::cloud::routes`)
siguen duplicados: son el siguiente incremento, no se hicieron aquí para que el
diff fuera revisable.

**El golden es el contrato entre despliegues.** `hoard-core/tests/golden/*.json`
es el JSON byte a byte de la v1.0.4. Compilar juntos sólo garantiza coherencia
dentro de un build; cliente y server se despliegan por separado. Al añadir un
campo: `#[serde(default)]` y **no se toca el fixture**.

---

### D.11 — BLOQUEANTE de publicación: el poller de nube enmudece (shell, no kernel)

Hallado dogfoodeando el 2026-07-24 (Windows sube factorio v138, Linux nunca la
baja). **No es regresión del kernel:** `cloud_pull.rs` es shell del desktop y los
Slices 1-3 no lo tocaron. Al contrario — el `Hold{reason}` de C.5/2c es lo que
permitió cerrarlo en minutos: el reductor decía `converged`, no un veto.

**Síntoma:** `agent: cloud version cache updated from poller` aparece **solo en
los primeros ~7 s del primer lanzamiento del día** y nunca más. Tres arranques
del poller en la misma sesión (21:01:13, 21:01:55, 22:06:21) y feeds solo en
21:01:14 y 21:01:20. Cero WARN/ERROR. El poller hace como mucho **un** pull por
lanzamiento y muere en silencio.

**Cadena:** sin feed, `obs.cloud_version` queda congelado (120 mientras la nube
va por 138) → `cloud_ahead = false` → `converged` para siempre → no restaura. El
reductor decide bien sobre una entrada mentirosa: el fallo está *aguas arriba*
del kernel.

**Candidatos** (`cloud_pull.rs`): (a) el gate de single-flight no tiene guarda
RAII — `guarded_pull` pone `running = true` y si la tarea es abortada
(`start()` → `prev.abort()`) o entra en pánico, nadie lo libera y todo tick
posterior sale por `if g.running { rerun = true; return; }` **sin loguear**;
(b) `.lock().unwrap()` sobre un mutex envenenado mata la tarea del poller sin
rastro en el log. Ambas explican el silencio absoluto.

**Confirmado 2026-07-24:** el restore **manual** desde el historial funciona. El
ejecutor de restore y el kernel están sanos; lo único roto es el disparo
automático por falta de feed. Daño acotado al poller.

**Fix estructural (el importante, sí toca el kernel):** hoy el kernel **no puede
distinguir "convergido" de "ciego"** — las dos cosas se ven como
`Hold{"converged"}`, y por eso 47 min sin noticias de la nube se disfrazaron de
normalidad. `Observation` lleva `cloud_version` pero no *desde cuándo*. Añadir
`cloud_version_as_of: Option<OffsetDateTime>` y, cuando esa marca envejece más
que un umbral (p. ej. varias veces `CLOUD_POLL_INTERVAL_SECS`), emitir
`Hold{reason: "cloud state stale"}` en vez de `converged`. Mismo principio que
loguear los vetos: **un fallo invisible pasa a ser observable**. La UI puede
colgar de ahí un indicador de "sin contacto con la nube".

**Fix proximal (slice de shell, antes de publicar):** guarda RAII que libere el gate en
`Drop`; no `unwrap()` sobre locks envenenados; **loguear cada tick** y el camino
"gate ocupado" para que esto no vuelva a ser invisible; y re-alimentar versiones
cuando aparece el handle del agente (hoy, si el primer tick corre antes de que
`AppState.agent` exista, el feed se salta en silencio).

**A verificar en el mismo slice:** `~/.config/hoard/config.toml` apunta a
`http://localhost:8082` (self-hosted) mientras poller/entitlements/realtime
hablan con Cloud. Confirmar a qué servidor apunta realmente el `ApiClient` del
agente — un desajuste ahí explicaría también los `server has no snapshots yet`.

**Síntomas secundarios sin perseguir aún:** `automatic scan: done tracked=0` (la
UI no muestra "vigilando <juego>" aunque el agente tiene 4 watchers armados) y
`couldn't persist detection cache — No existe el archivo o el directorio`.

**Bug de UI hermano (peligroso, mismo día):** el panel muestra la versión de
**nube** sin distinguirla del estado **local**. Con Linux anclado en v120 y disco
del 22-jul, el panel rotulaba "guardada v138 / v142" y el usuario creyó tenerlas.
El estado interno era honesto (`last_version_num: 120`, `had_pending=false`, cero
restores en el log): mentía solo la presentación. En una herramienta de saves esto
invita a jugar encima creyendo estar al día — y con el poller muerto, esa partida
se subiría como v143 haciendo **retroceder la cabeza** de la nube. La UI debe
separar "la nube tiene vN" de "este equipo tiene vN".

#### Resuelto (2026-07-25) — los cuatro puntos

- **Kernel:** `Observation::cloud_version_as_of`; pasado
  `CLOUD_STALE_AFTER_SECS = CLOUD_POLL_INTERVAL_SECS × 5`, el reposo se rotula
  `Hold{"cloud state stale"}` en vez de `converged`. `const _: () =
  assert!(CLOUD_STALE_AFTER_POLLS >= 2)` en compilación: un solo poll perdido
  jamás declara ceguera. **La obsolescencia sólo cambia el motivo del reposo — no
  frena backups ni restores.** Decisión correcta y a no deshacer: no subir pierde
  trabajo real, mientras que subir desde una base vieja choca 409 y mergea la
  cabeza (historial content-addressed, nada se pierde). Bloquear cambiaría un
  fallo recuperable por uno que no lo es.
- **Poller:** `GateGuard` con `Drop` — causa raíz confirmada: `start()` aborta el
  poller anterior y, si el abort caía mid-pull, `running = true` quedaba clavado
  en el scheduler (que sobrevive al reinicio de la tarea) y todo tick y todo
  `kick()` posterior salía sin loguear. Además: recuperación de mutex envenenado
  en vez de `unwrap()`, una línea por tick (`trigger=timer|kick`), camino
  "gate ocupado" logueado, `warn!` si el holder pasa de 5 min, y
  `feed_agent_versions()` llamado también desde `start_agent` (el primer tick le
  ganaba la carrera al spawn del agente y tiraba el manifest en silencio).
- **Servidor: no hay desajuste.** `~/.config/hoard/config.toml` es el `CliConfig`
  de la CLI; el desktop sólo lo lee para rutas. El `ApiClient` sale de
  `library::current_client()` (prefiere self-hosted, cae a creds cloud) y el
  contexto vivo es el cloud → `api.hoard.services`.
- **UI:** `TrackedSave.last_version_num` → `cloud_version_num`, más
  `local_version_num` (cursor de `CliState`, el `known_version` del kernel). El
  pill del panel es estrictamente local; la nube tiene su propio chip, ámbar
  cuando va por delante.

**Remate pendiente (no bloquea):** `is_stale` usa `is_some_and`, así que
`cloud_version_as_of: None` — "nunca he sabido nada de la nube", la ceguera más
grave — **no** se reporta obsoleto. La distinción correcta es *contexto cloud vs
self-hosted*, no `None` vs `Some`: en contexto cloud, `None` pasado un margen de
arranque debería ser `stale`. Mitigado en la práctica porque
`feed_agent_versions()` desde `start_agent` cierra la carrera que lo producía.

---

### D.12 — El motor debe observar la nube, no esperar la papilla (dogfooding 2026-07-25)

Con el fix D.11 desplegado (7.7.14), el dogfooding dio el veredicto:

- **Lo que funcionó:** `Hold{"cloud state stale"}` salió **3015 veces** — el
  kernel ya no disfraza la ceguera de `converged`. `gate busy: 0`: el gate
  atascado está muerto. La observabilidad que añadimos es lo que permitió
  diagnosticar esto en una lectura de log.
- **Lo que no:** el poller registró **dos ticks en el primer segundo**
  (`timer` a las 03:25:48.017, `kick` a las .416) y **cero en los 36 minutos
  siguientes**. Sin `gate busy`, sin segundo `poller: started`, sin `stopped`:
  la tarea de `tokio::spawn` **desaparece**, casi seguro por un panic que no
  llega al fichero de log. Encaja con el síntoma observado: reiniciar la app →
  un único feed → el agente aprende → restaura una vez → el poller muere →
  ciego el resto de la sesión (15 subidas posteriores, cero restauraciones).

**El fallo de diseño (lo diagnosticó el usuario):** la UI sabe de la v181 porque
*ella* consulta al servidor; el agente no, porque son caminos separados. **El
motor no observa el mundo: espera a que un proceso ajeno y frágil le dé la
papilla.** Muere una tarea y el motor queda ciego para siempre, sin
autorrecuperación. Cazar este panic concreto sólo aplaza el siguiente.

**Fix estructural:** aplicar al **transporte** el principio que ya rige las
decisiones. El agente **consulta él mismo la cabeza de la nube** como parte de
observar el mundo (ya tiene `ApiClient`); el poller del desktop pasa a ser un
**hint de latencia**, no la única fuente de verdad. Un poller muerto degrada
entonces a "tardo hasta el siguiente tick" en vez de "ciego para siempre" — la
propiedad level-triggered que perseguimos desde C.1. Coste a controlar: no
convertirlo en un GET por save y tick — una sola llamada al manifiesto por
intervalo, con la observación L0/L1 gateando el resto.

**Además:** una tarea de fondo que muere en silencio es un bug por sí misma.
Supervisar (detectar salida y reiniciar con backoff) y que el panic acabe en el
log, no sólo en stderr.

**Refuerza el Slice 4.** Que UI y motor tengan caminos distintos a la verdad es
exactamente la duplicación que el daemon elimina: con `hoardd` dueño del motor,
la UI deja de ser proveedora de datos del motor y pasa a ser su cliente.

**Aparte, sin perseguir:** Planet S falla siempre en Windows con
`cloud cas init: conflict (409): non-fast-forward`. Si el 409 no está aterrizando
en el merge de cabeza que asume D.8, esa recuperación está rota. Relacionado con
la memoria `hoard-planet-s-windows-mistrack` (Windows trackea la carpeta
equivocada), así que puede ser contenido divergente y no el 409 en sí.

#### Resuelto (2026-07-25) — decisiones a no deshacer

- **El motor observa la nube.** `observe_cloud_heads` (shell del agente) pide el
  manifiesto **una vez por intervalo** y el resultado entra al reductor como
  `Observation`; el kernel sigue sin IO, con `now`/`seed` inyectados. El disparo
  vive en el tick del agente, no en una tarea de fondo de larga vida: **no hay
  nada que pueda morir en silencio**, porque cada tick vuelve a evaluar el plazo.
  El anti-relanzamiento es un `JoinHandle::is_finished()`, no un booleano — un
  pánico o una cancelación no pueden dejar el hueco "ocupado" para siempre, que
  es exactamente cómo se atascó el gate de D.11.
- **El poller del cliente es un hint, y su feed suprime la consulta propia.**
  `CLOUD_SELF_OBSERVE_AFTER_SECS = CLOUD_POLL_INTERVAL_SECS × 1,5`: un poller
  vivo rejuvenece la marca antes de que venza, así que el coste se queda en UN
  manifiesto por intervalo, no dos. `const _: () = assert!(SELF_OBSERVE <
  STALE)` en compilación — el motor **siempre** intenta refrescar antes de poder
  declararse ciego. No fusionar los dos umbrales ni fijar el de auto-observación
  por debajo de la cadencia del poll: lo primero mata la garantía, lo segundo
  duplica el GET.
- **El pánico concreto: `CloudFeed` nunca estaba en `.manage()`.**
  `app.state::<T>()` **panica** con estado no gestionado, así que `kick_all` se
  llevaba por delante la tarea que lo llamara — el bucle del poller tras su
  primer tick (dos ticks y silencio: el síntoma exacto de arriba) y el socket
  Realtime en su `Resubscribed`. Arreglado en el origen (`.manage`) y con
  `try_state` como red: un mis-wiring futuro degrada a un `warn!` y un feed
  inerte, no a un bucle muerto. Los feeds de dispositivos/campana llevaban
  muertos desde que se introdujeron.
- **Supervisión + pánico al log.** `commands::supervisor::supervise()` envuelve
  el bucle en `catch_unwind` y reinicia con backoff (5 s → 5 min, reseteado tras
  10 min sanos); un solo task, sin `spawn` anidado, para que el `abort()` de
  `start()`/`stop()` siga matándolo todo (un task anidado sobreviviría como
  huérfano: dos pollers). Lo usan **el poller y el subscriptor Realtime**: éste
  reconectaba ante errores pero un pánico lo mataba para toda la sesión, que es
  justo como murió en `kick_all`. Terminar a propósito es una *declaración* — el
  cuerpo devuelve `supervisor::Finished`—, así que un bucle que no puede
  retornar (el poller) tampoco puede pararse por accidente: el caso "returned
  unexpectedly" se cierra por construcción, no con un log. Y un `panic hook`
  global en `lib.rs` manda todo pánico al fichero de log: en una app empaquetada
  stderr es `/dev/null`, que es por lo que esto fue invisible.
- **Remate de D.11 cerrado.** `Observation::cloud_feed_expected_since`: en
  contexto cloud, "nunca supe nada de la nube" envejece desde el arranque del
  motor y se reporta `Hold{"cloud state stale"}` igual que un feed rancio. La
  distinción es *contexto* (probe cacheado de `/v1/health`, vía
  `ApiClient::probed_is_cloud` — que distingue self-hosted de "probe fallido",
  cosa que `is_cloud()` no hace), no `None` vs `Some`. El probe manda sobre el
  hint del poller: con el agente en self-hosted y una sesión cloud viva en disco,
  las cabezas que empuja el poller no son de este motor.

**Regla para tareas de fondo nuevas:** si vive más que una petición, va bajo
`supervisor::supervise`. Las tres piezas de este incidente —poller, Realtime y
los feeds de `cloud_feed`— eran `tokio::spawn` sueltos, y las tres fallaron en
silencio por el mismo pánico.

---

### D.13 — Restore no deduplica contra el disco local (rendimiento, post-release)

Con el sync ya correcto (dogfooding 2026-07-25: Windows sube al instante, Linux
baja solo y en el momento), queda una **asimetría de rendimiento**: la subida
deduplica contra los blobs del servidor, pero la bajada **no** deduplica contra
lo que ya hay en disco. El mismo almacenamiento content-addressed, aprovechado
sólo por un lado.

`restore::restore_cloud_cas` construye un job por **cada** fichero del manifiesto
y los baja todos (cero menciones de reutilización local en `restore.rs`). Caso
real: Factorio, doce zips de ~8 MB, cambia uno → se bajan ~400 MB en ~1 min
(prácticamente line-rate: no es descarga lenta, es descarga de más) cuando
bastarían ~8 MB.

**Fix:** indexar por SHA-256 los ficheros ya presentes en el destino y, cuando el
SHA coincide con el del manifiesto, **copiar desde el fichero local** en vez de
hacer el GET. Hashear el directorio cuesta un segundo o dos frente a un minuto de
red. **Propiedad de seguridad que lo hace barato:** la verificación de hash
posterior ya existe y se mantiene — una reutilización equivocada no cuadra el
SHA y salta, así que el atajo no puede corromper un restore.

**Secuencia: va DESPUÉS de publicar.** El panic de `CloudFeed` llevaba vivo desde
que se introdujeron los feeds, así que los usuarios ya desplegados probablemente
tienen el auto-restore roto igual que lo estaba aquí. Publicar la corrección pesa
más que hacerla rápida; esto es optimización, no correctness.

#### Resuelto (2026-07-25) — decisiones a no deshacer

- **El índice se construye contra una carpeta que se *pasa*, no contra `dest`.**
  `RestoreOptions::reuse_from` es un `Option<PathBuf>` aparte porque los dos
  llamantes difieren: el auto-restore extrae a un staging **vacío por
  construcción** (indexar `dest` no encontraría nada), así que apunta a la
  carpeta viva del save; el restore directo (desktop History, CLI) escribe en la
  carpeta y pasa `dest`. `None` = todo se baja, comportamiento pre-D.13 exacto.
  No colapsar el parámetro en `dest`: eso desactiva silenciosamente el dedup en
  el único camino que importa (el automático).
- **El emparejamiento es por SHA-256 y sólo por SHA-256.** `build_reuse_index`
  (IO) + `plan_byte_sources` (puro) están separados para que el join sea
  testeable sin red. Un fichero local con el mismo *nombre* pero otros bytes
  hashea distinto y se baja; uno renombrado con los mismos bytes se reutiliza —
  que es justo lo que hace que los autosaves rotatorios de Factorio dedupliquen.
  Prefiltro por tamaño antes de hashear (un fichero de otra longitud no puede ser
  el contenido buscado), así que el coste de leer disco está acotado por el
  tamaño del propio snapshot.
- **La verificación de hash de lo que aterriza se mantiene, y el fallo cae a la
  red.** `copy_local_blob` hashea mientras copia y compara contra el manifiesto;
  un fallo (índice rancio, fichero reescrito debajo) sólo hace `warn!` y baja el
  blob. Esa red de seguridad es también lo que hace inocua la carrera del restore
  directo, donde el origen de una reutilización puede ser el destino de otra
  entrada: quien pierda la carrera no cuadra el SHA y baja. **No sustituir la
  verificación por "ya sabemos el hash del índice"**: es la propiedad que
  convierte el atajo en gratis en vez de en un riesgo de corrupción.
- **No hay caché de hashes por fichero que reutilizar.** `state.json::set_hash`
  es una firma **del conjunto** (rutas+tamaños+mtimes, más un hash de contenido
  de la concatenación), no digests por fichero. Lo reutilizable eran las
  primitivas: `backup::walk_source` (mismo walk que la subida, ya filtra symlinks
  y locks transitorios) y `backup::hash_file`, ahora `pub(crate)`.
- **El progreso cuenta bytes reutilizados igual que descargados.** Si la barra
  sólo contara red se quedaría clavada al 2% en el caso Factorio y parecería
  colgada. El desglose reutilizado/bajado va en `RestoreOutcome`
  (`files_reused`/`bytes_reused`) y en el `info!` de cierre — que es la señal de
  dogfooding: ~390 MB reutilizados / ~8 MB bajados.
- **Sólo el camino cloud CAS.** El self-hosted (`download_snapshot` →
  `snapshot_download`) sirve **un `tar.zst` monolítico** por snapshot: no hay GET
  por fichero que saltarse, así que saber que un fichero ya está en disco no
  ahorra nada. Intacto, igual que la versión cloud legacy de archivo entero
  (`download_and_extract_cloud`); ambos reportan `files_reused = 0`.

---

### D.14 — Decisiones cerradas del Slice 4 (2026-07-25)

Las dos casillas que la Parte A dejaba abiertas, decididas por el usuario.

**1. Notificaciones: opción (b) — las manda el daemon.** El daemon es el dueño
del sync, así que es coherente que avise él, y es la única forma de enterarse con
la app cerrada. Se asume el coste: notificación nativa por SO (dbus/notify-send
en Linux, toast en Windows, macOS aparte). **Va al final del Slice 4**, con el
daemon ya funcionando, y **empezando por Linux** (donde se dogfoodea). El tray
sigue siendo del desktop; lo que cambia es quién origina el aviso.

**2. Entrega de eventos: journal + push, no una cosa u otra.** Resuelven
horizontes distintos y hacen falta los dos:

- **Push por el socket** → el cliente conectado *ahora*: latencia mínima, sin
  disco.
- **Journal append-only con cursor** → el cliente que **no estaba**. Sólo-push
  significa que quien arranca tarde se pierde el historial: es exactamente el bug
  de las campanas mudas (la UI sin snapshot ni backlog).

Protocolo: el cliente conecta → pide "todo lo posterior al cursor N" → luego
escucha en vivo. Misma forma que ya tienen Realtime + el airbag del poll.
**Ese journal es el log de decisiones de C.5** (tabla-anillo en la SQLite del
daemon): una sola tabla sirve al replay y al catch-up de clientes. No construir
dos.

**Coste real: no es "mandar vs guardar", es cuánto guardas.** Mandar es una
escritura a socket. Guardar eventos de verdad (backup, restore, juego
arranca/para) es trivial: son pocos. Lo que amplifica escritura es guardar **cada
decisión de cada tick** — medido en este repo el 2026-07-25: **3015
`cloud state stale` en 36 minutos** (~84/min, >100k/día) con tick de 2 s. Regla:
**guardar transiciones y acciones, no reposos repetidos**; y colapsar rachas del
mismo motivo de `Hold` en una fila con contador. Crítico en el SSD del Deck.

---

### D.15 — Slice 4a cerrado: el daemon existe (2026-07-25)

El Slice 4 se parte en sub-slices. **4a: extraer el daemon y su IPC, conviviendo
con el motor embebido.** Desktop y CLI siguen funcionando exactamente como antes
—nadie depende del servicio todavía—; 4b y 4c los convierten en clientes, 4d
borra `instance.rs`, y el empaquetado + las notificaciones nativas cierran el
Slice 4.

Crate nuevo `crates/hoardd` (lib + binario). Protocolo en `hoard_core::ipc`
(serde-only): encuadre, sobres, journal y —movidos verbatim desde
`hoard_agent::agent`— `AgentEvent`/`BackupReason`/`AgentSlotStatus`, que es la
ADR al pie de la letra: «el `AgentEvent` pasa a ser el protocolo de eventos por
cable». `hoard_agent::agent` los re-exporta, así que ni el desktop ni la CLI
notaron el movimiento, y un test golden fija la forma del JSON.

**Decisiones a no deshacer:**

- **El árbitro entre daemons es el bind; el pidfile sigue arbitrando *sólo*
  daemon↔motor-embebido.** En unix el mutex del servicio es un `flock` sobre
  `hoardd.lock` (liveness real: lo suelta el kernel al morir el proceso), y con el
  lock en la mano el socket rancio se borra **siempre** en vez de intentar
  adivinar si está vivo — ese orden (lock → unlink → bind) es lo que hace que dos
  arranques simultáneos no se pisen. En Windows lo hace
  `FILE_FLAG_FIRST_PIPE_INSTANCE`, atómico por construcción. Mientras 4b/4c no
  aterricen, el keeper del daemon respeta `instance::live_owner()` y reporta
  `EngineStatus::blocked_by_pid` en vez de arrancar un segundo motor.
- **Perder el bind es un final correcto, no un error.** El daemon que llega
  segundo sale con código 0 y el cliente que lo lanzó se conecta al que ganó.
  `Client::ensure_running` **no** comprueba "¿hay daemon?" antes de lanzar: eso
  es un TOCTOU y produciría dos motores. Lanza y reconecta.
- **El daemon sirve la IPC aunque no tenga motor, y dice por qué no lo tiene**
  (`IpcError::EngineDown { reason }`, `EngineStatus::last_error`). Sin sesión no
  se muere: el keeper reintenta con backoff. Un cliente que sólo viera "error"
  reintentaría para siempre sin poder decirle nada al usuario — misma lección que
  `Hold{reason}`.
- **El colapso de reposos es una lista explícita, no "todo lo que se repita".**
  `journal::collapse_key` devuelve `Some` sólo para eventos de reposo/veto
  (`RestoreDeferred`, `SaveAutoRestore{Failed,Stuck}`, `BackupThrottled`,
  `HeavyProcessDetected`) y la clave es el JSON completo del evento, así que sólo
  colapsan repeticiones **idénticas** y añadir un campo no puede olvidarse de la
  clave. `BackupScheduled` queda **fuera** aunque se repita: cada emisión
  significa que el debounce se reinició, y como los colapsos no se empujan en
  vivo, colapsarlo silenciaría la cuenta atrás de la UI.
- **Un colapso no se empuja; un hueco sí se confesa.** `Backlog::gap` cuando el
  anillo ya no tiene lo que el cliente pedía, `ServerFrame::Resync` cuando el
  cliente se retrasa, y `Welcome::epoch` para que un cursor de otra ejecución del
  daemon no se interprete como continuidad. Mentir por omisión aquí es el bug de
  las campanas mudas otra vez.
- **Todo lo que vive más que una petición va bajo `supervise`** (regla de D.12).
  Por eso el supervisor subió de `hoard-desktop/src/commands/supervisor.rs` a
  `hoard_agent::supervisor` (el desktop lo re-exporta desde su ruta de siempre, sin
  tocar llamantes). Supervisados: bomba de eventos, keeper del motor, bucle de
  accept, refresher del JWT y un keeper que rearma el par de `cloud_live` si
  alguna de sus dos tareas muere. Las conexiones **no** se supervisan a propósito:
  reiniciar el cuerpo de una conexión cuyo socket ya no existe no significa nada,
  y el cliente reconecta sin perder nada gracias al cursor.
- **`Running` aborta sus tareas al soltarse.** Soltar un `JoinHandle` no cancela
  la task: un motor reemplazado dejaría su poller, su latido de presencia y su
  rotador de token corriendo junto a los del nuevo — dos rotadores del mismo
  refresh token es la familia 401 que este slice existe para matar. Por lo mismo
  el keeper **suelta el motor muerto antes** de arrancar otro: dos `AgentLock`
  vivos en el mismo proceso escriben el mismo pid, y el `Drop` del viejo borraría
  el pidfile del nuevo.
- **El canal de eventos es del daemon, no del motor.** Se crea una vez y cada
  arranque del motor recibe un clon del emisor, así que un motor que rebota no
  rompe los cursores de los clientes ni obliga a recrear la bomba.
- **Config del motor: prefs del usuario, con excepción headless.** Se leen las
  mismas prefs que el desktop; si `prefs.json` **no existe**, la máquina nunca ha
  visto la GUI y se aplican los defaults de `hoard sync` (auto-restore y sync
  global ON). Un servidor casero con sólo la CLI no puede acabar en "sólo subo"
  por heredar defaults pensados para la ventana.
- **Deuda consciente:** `hoardd/src/session.rs` es un port de
  `hoard-cli/src/commands/session.rs` (resolución + máquina de fases del
  refresher) con los `println!` convertidos en `tracing`. Duplicado a propósito
  para no tocar la CLI en este sub-slice; **la copia de la CLI muere en el 4c**,
  cuando `hoard sync` pase a ser "asegura el daemon y engánchate". Si el 4c no la
  borra, esto es drift.
- **Lo de Windows está verificado a nivel de tipos, no de ejecución.** La ACL del
  named pipe (`winsec.rs`: SDDL desde el SID del token, `Everyone` fuera) y el
  `first_pipe_instance` compilan con
  `cargo check --target x86_64-pc-windows-msvc` sobre esos módulos aislados —el
  check completo no cabe en Linux porque `zstd-sys`/`libsqlite3-sys` necesitan
  MSVC—. Falta un humo real en Windows antes de fiarse.

**D.8.3 (`upload_landed` / anti-relanzamiento content-addressed) sigue abierto** y
la ADR lo declara obligatorio "con el Slice 4": no se ha hecho en 4a. Reiniciar el
daemon aún no es rutina (nadie depende de él), pero **debe entrar antes de cerrar
el Slice 4**, porque con el servicio en marcha `in_flight` deja de sobrevivir a
los reinicios.

---

### D.16 — Slice 4b cerrado: el desktop es un cliente (2026-07-25)

El desktop ya no tiene motor. `agent::spawn`, el `AgentHandle` de `AppState`, el
latido de presencia y el `AgentLock` del pidfile salen de `hoard-desktop`; en su
sitio queda `DaemonLink` (`crates/hoard-desktop/src/daemon.rs`), que habla con
`hoardd` por la IPC del 4a. Cerrar la app ya no puede parar el sync — el punto de
todo el Slice 4. **La CLI no se toca**: sigue con su motor embebido y su pidfile
hasta el 4c.

La restricción dura de D.3 se cumple: la interfaz pública de los stores TS es la
misma export por export (`activity`, `restoreStuck`, `status`, `trayState`,
`subscribeAgent`, `bootAgent`, `shutdownAgent`, `activityFeed`, `liveStatus`…) y
las pantallas no cambian ni una línea. Lo que cambió está **dentro**: el Rust
releva los eventos del servicio a los mismos canales `agent://*` de siempre.

**Decisiones a no deshacer:**

- **Dos conexiones por cliente, comandos y eventos.** `read_frame` lee cabecera y
  cuerpo en dos pasos, así que no es cancel-safe: con una sola conexión haría
  falta un `select!` entre "espera un push" y "manda una petición", y cancelar la
  lectura a medias desordena el flujo. Dos conexiones cuestan una task por
  cliente en el daemon y a cambio ninguna lectura se cancela nunca.
- **El relevo lo enciende quien escucha, no `start_agent`.** `attach_agent_events`
  lo llama el store desde `subscribeAgent()`, con los `listen()` ya puestos. A
  `start_agent` también lo llama el escaneo de Modo Automático desde Rust, y le
  gana al montaje del webview — medido en la máquina Windows: el sweep del
  scheduler conectó al servicio **antes** de que existiera la UI. Un backlog
  emitido antes que el oyente es un historial perdido en silencio, que es
  exactamente lo que el journal existe para evitar.
- **El backlog viaja con su hora.** Cada fila lleva `at` (el `last_at` del
  journal). Reproducir un `game_started` de hace dos horas tiene que pintar dos
  horas de sesión, no arrancar el contador de cero.
- **Reproducir no notifica.** El store aplica el backlog con una bandera
  `replaying` que silencia toasts y notificaciones nativas: una copia de anoche no
  puede sonar hoy. El estado en pantalla sí se reconstruye.
- **Resync es reconstruir, no parchear.** Con `gap`, epoch nuevo o `Resync` del
  push, el store vacía `activity` y `restoreStuck` y los rehace desde el journal.
  El **feed no se vacía**: sus filas sólo llegan más nuevas que el cursor (nunca
  duplican) y además tiene filas de otras fuentes que un borrado tiraría.
- **El cursor vive en memoria y no se persiste.** Una ejecución nueva de la app
  arranca con la UI vacía, así que pedir el anillo entero *es* la reconstrucción
  correcta; dentro de una ejecución el cursor evita repetir lo ya pintado al
  reconectar. Persistirlo sería estado nuevo en disco justo antes del Slice 5.
- **Un `IpcError` no se reintenta ni tira la conexión.** Sólo los fallos de
  transporte reconectan. Reintentar un error de aplicación son dos conexiones y
  dos líneas de log por cada comando mientras no hay motor, para recibir la misma
  respuesta (observado en el humo de Windows antes de arreglarlo). El `IpcError`
  implementa `Error`, así que el motivo llega legible al toast; un `{:?}` le
  enseñaría `EngineDown { reason: … }` al usuario.
- **El desktop nunca manda `Shutdown`.** Cerrar sesión o cerrar la app es
  `detach`: se sueltan las tareas de esta ventana y el servicio sigue. Parar el
  sync es una orden explícita del usuario.
- **Dos peticiones nuevas, ambas espejo de algo que el desktop hacía a mano.**
  `SetProbeCandidates` (la detección vive en el frontend hasta el Slice 8, así que
  el motor no puede adivinar las candidatas; van como `String` porque el cable es
  JSON y una ruta no-UTF-8 se descarta en el cliente, que es donde se puede decir)
  y `RestartEngine` (un cambio de cuenta invalida `ApiClient`, contexto y rotador
  a la vez, y eso sólo se arregla resolviendo la sesión de cero).
- **`RestartEngine` se *pide*, no se ejecuta en el despacho.** El único dueño del
  ciclo de vida del motor es el keeper: si el servidor IPC lo hiciera, entre
  soltar el motor viejo y terminar su apagado el keeper vería la ranura vacía y
  arrancaría otro — dos `AgentLock` vivos en el mismo proceso (el `Drop` del viejo
  borra el pidfile del nuevo) y dos rotadores del mismo refresh token durante esa
  ventana. Y el keeper duerme con un `Notify`, así que un login no espera el
  backoff de hasta cinco minutos.
- **El desktop deja de tomar el pidfile.** Es requisito, no limpieza: si lo
  siguiera tomando, el keeper del daemon vería un dueño vivo y **nunca** arrancaría
  motor. `instance.rs` sigue existiendo para la CLI y muere en el 4d.
- **El poller de nube del desktop sólo pinta.** Se le quitan el empuje de token,
  el `set_cloud_versions` y el `force_restore`: el daemon corre su propio
  `cloud_live` (Realtime + poll) y hace las tres cosas con menos latencia. Lo que
  sí se queda es el SSE self-hosted, que no tiene equivalente en el daemon y ahora
  pide el `force_restore` por IPC.
- **Un solo publicador del estado del motor.** El estado arriba/abajo no es un
  evento del journal, así que lo refresca un bucle cada 20 s (round-trip local) y
  el bombeo lo baja a "parado" en cuanto pierde el socket. La memoria de lo último
  publicado vive en `DaemonLink`, no en cada emisor: con una memoria por emisor, el
  bucle seguiría creyendo que ya publicó "arriba" y la UI se quedaría en "parado"
  hasta que el motor cambiara de estado por su cuenta.

**Windows, ya ejecutado (cierra el punto abierto de D.15).** Compilado y corrido
por SSH en la máquina del usuario (`cargo test -p hoardd` verde: 12 unitarios + 9
de IPC + 3 de "spawn if absent" con procesos de verdad). Comprobado a mano contra
el pipe real:

- se crea `\\.\pipe\hoardd-<usuario>-<hash>`;
- un segundo arranque pierde el bind, lo dice y **sale 0**;
- un cliente ajeno (PowerShell, ni una línea de nuestro Rust) completa el
  handshake y recibe el `Welcome`;
- la ACL efectiva —leída del objeto vivo, no del descriptor que le pasamos— es
  `D:P(A;;FA;;;S-1-5-21-…)(A;;FA;;;SY)`: **sin `Everyone`**, sin usuarios
  autenticados y con el DACL protegido. Ahora es un test (`the_pipe_acl_excludes_everyone`)
  y una línea de log en cada arranque, no una deducción del código;
- y el desktop de este slice, con su webview montado en la sesión interactiva,
  engancha las dos conexiones (`(commands)` y `(events)`), siembra la UI desde el
  journal y reporta el motor ausente con su motivo.

**Lo que queda del Slice 4:** 4c (la CLI, que además mata el duplicado de
`session.rs`), 4d (borrar `instance.rs`), el empaquetado —hoy `daemon_binary()`
busca el hermano del ejecutable y luego el `PATH`, así que **el bundle tiene que
llevar `hoardd`** o el desktop se queda sin servicio— y las notificaciones
nativas. Sigue abierto D.8.3 (`upload_landed`).

**Deuda consciente:** siguen existiendo dos rotadores del refresh token. El
servicio rota para el motor y el desktop rota para sus propias llamadas REST;
`cloud_auth::refresh_freshest` sana la rotación ajena releyendo disco, así que no
rompe, pero el "un único rotador" de la Parte A no está cerrado hasta que el
desktop pida el token al servicio (encaja en el Slice 7, cliente cloud único).

---

### D.17 — Slice 4c cerrado: la CLI es un cliente y el rotador es uno (2026-07-25)

`hoard sync` ya no tiene motor. `agent::spawn`, el pidfile, el rotador del token,
el poller de nube y el latido de presencia salen de `hoard-cli`; en su sitio queda
`commands/link.rs`, que habla con `hoardd` por la misma IPC del 4a. Con esto
`agent::spawn`, `presence::spawn` y `cloud_live::spawn` tienen **un único
llamante en todo el repo**: `hoardd::engine`. Y la promesa titular de la Parte A
—**un solo rotador de `cloud.toml`**— está cerrada, adelantándola del Slice 7
porque el desktop no necesita dejar de hablar con la nube para pedir el token
prestado.

**Decisiones a no deshacer:**

- **Una sesión, dos puertas, y la diferencia está en los tipos.**
  `hoard_agent::session` (la implementación compartida; el port duplicado de
  `hoardd/src/session.rs` y la copia de la CLI han muerto) expone
  `resolve_owned` —la que refresca al arrancar, sólo el servicio— y
  `resolve_borrowed`, que **no llama a GoTrue jamás**. El refresh token sólo
  viaja en `CloudEndpoint::refresh` cuando resolvió el dueño: un cliente no lo
  recibe, así que no puede rotar aunque la próxima sesión se despiste. Lo mismo
  en el desktop: `CloudCreds` perdió el campo `refresh_token`. "El cliente no
  rota" lo sostiene el compilador, no un comentario.
- **`Request::CloudToken { rejected }` y por qué el `rejected` no es opcional de
  verdad.** Un cliente puede comer un 401 con un token que **no ha caducado**
  (revocado server-side, reloj desfasado); sin decir cuál le falló, el daemon le
  devolvería el mismo y el reintento sería un bucle — que es exactamente lo que
  le pasaría al Realtime en su rama `TokenError`. Con él, el daemon rota sólo si
  el que serviría es ése; si ya lo había rotado, contesta con el nuevo sin gastar
  una rotación.
- **El margen de préstamo (5 min) tiene que ser mayor que el margen de los
  clientes** (`TOKEN_REFRESH_MARGIN` = 120 s en el realtime). Si fuera menor, el
  cliente pediría a los 120 s, recibiría el mismo token y volvería a pedir cada
  30 s hasta cruzar nuestro umbral.
- **`CloudToken` no pasa por `with_engine`.** El rotador es del **daemon**, no
  del motor: un motor caído por falta de sesión o por un bache de red no puede
  dejar al desktop sin poder hablar con la nube — y menos empujarle a rotar por su
  cuenta, que es lo que este slice mata.
- **Quién escribe el par de tokens, tras el slice:** el daemon (rotación, vía
  `cloud_auth::refresh_freshest`) y los flujos de **login/logout** de cada
  frontend, que acuñan o borran la sesión —acuñar no es rotar, y el OAuth acaba
  en el cliente—. Nada más: el desktop pasó a `save_account_snapshot`, que sólo
  reescribe el snapshot de `/v1/me`. Reescribir el par leído minutos antes era
  la forma de pisar una rotación ajena con un token viejo y disparar la
  reuse-detection.
- **Muere el chequeo `instance::live_owner()` del keeper, y es obligatorio, no
  cosmético.** Arbitraba contra el motor embebido, que ya no existe; dejarlo era
  una bomba: `is_alive` da por bueno cualquier proceso cuyo nombre contenga
  "hoard", y ahora todos los clientes lo contienen (`hoard sync run`,
  `hoard-desktop`). Un `agent.pid` rancio cuyo pid reciclara un cliente habría
  dejado al servicio **sin motor para siempre y en silencio** — la clase de fallo
  de D.11/D.12. El lock se sigue *tomando* (gratis, y hace que un frontend
  anterior a 4b/4c se aparte); el fichero entero muere en el 4d, donde también hay
  que actualizar el doc de `instance.rs`, que ya miente.
- **Sólo `hoard sync run` arranca el servicio.** Es el comando cuyo trabajo *es*
  que el sync corra. Los demás (`hoard track`, `hoard save pause`, `login`,
  `logout`, el banner, `hoard sync` a secas) **avisan si hay servicio y callan si
  no**: un `hoard whoami` no puede convertir la máquina en una máquina que
  sincroniza como efecto secundario. La contrapartida se asume y se dice: sin
  servicio nadie renueva el token Cloud, así que un one-shot usa el de disco tal
  cual y, si ya caducó, el error lleva la pista de que quien renueva es
  `hoard sync start` (`session::stale_token_hint`).
- **Enganchar es seguir, no releer.** `hoard sync run` se suscribe desde el
  cursor del `Welcome`, así que imprime lo que pasa a partir de ahí (el desktop sí
  pide el anillo entero, porque tiene estado en pantalla que reconstruir; una
  terminal no). El hueco entre el saludo y la suscripción viaja igual, así que no
  hay agujero.
- **Parar `hoard sync run` sí para el servicio**, y es la excepción consciente a
  "cerrar un cliente nunca mata el motor": ese proceso es el `ExecStart` de la
  unidad, así que para systemd/launchd/Task Scheduler **es** el servicio. Si un
  `systemctl --user stop` dejara a `hoardd` sincronizando, "parar el sync" habría
  dejado de significar nada, y `hoard sync restart` tras un `hoard upgrade` no
  relevaría el binario nuevo. `hoard sync stop` hace las dos cosas (quita el
  autostart y manda `Shutdown`), por si el servicio lo levantó otro.
- **Hueco conocido y aceptado:** un cliente **enganchado** resucita el servicio
  ~3 s después de un `Shutdown` ajeno, porque su reconexión es `ensure_running`.
  Pasa con un `hoard sync run` en primer plano o con el desktop abierto (desde el
  4b). Es defendible —quien está enganchado pidió que el sync corriera— y en el
  camino desplegado no ocurre: parar la unidad mata primero al cliente. Si algún
  día molesta, la salida limpia es que el daemon se despida
  (`ServerFrame::Goodbye`) para que el cliente distinga "lo pararon a propósito"
  de "se cayó", que es la única forma de decidirlo bien; no un fichero-marcador,
  que es el error del pidfile otra vez.
- **`--backup-only` espera al motor.** En el arranque el cliente llega antes que
  el motor (el servicio resuelve la sesión primero), así que la orden se reintenta
  hasta 2 min. Una bandera perdida en silencio aquí significa escribir en el disco
  del usuario justo cuando pidió que no.
- **`hoard sync logs` enseña dos mitades:** el log de `hoardd` (el diagnóstico
  del motor; el servicio corre desasido, así que su salida no cae ni en el journal
  de la unidad ni en ninguna terminal) y el del cliente (la crónica de eventos).
- **No hay test de extremo a extremo del préstamo, a propósito.** Prestar lee la
  sesión Cloud **real** de quien ejecuta los tests y, si le queda poca vida, la
  rota: un `cargo test` no puede tocar la sesión de nadie. Lo que se testea es la
  política pura (`needs_rotation`: token sano se presta sin rotar, uno a punto de
  morir se rota, uno rechazado nunca se devuelve, caducidad ilegible rota) y la
  forma del cable (golden de `hoard_core::ipc`).

**Lo que queda del Slice 4:** el 4d (borrar `instance.rs` y el empaquetado — el
bundle tiene que llevar `hoardd` y las units deberían apuntar a él en vez de a
`hoard sync run`) y las notificaciones nativas. Sigue abierto D.8.3
(`upload_landed`).

---

### D.18 — Slice 4d cerrado: sin pidfile, empaquetado y con despedida (2026-07-25)

Cuatro cosas, y con ellas el Slice 4 queda cerrado salvo las notificaciones
nativas: muere `instance.rs`, el bundle lleva `hoardd` y lo arranca el gestor de
servicios del usuario, se cierra D.8.3 (`upload_landed`) y se cierra el hueco del
4c (un cliente enganchado resucitaba el servicio tras un `Shutdown` ajeno).

**1. El pidfile, borrado.** `hoard_agent::instance` ya no existe: el árbitro es la
propiedad del socket (un `flock` con liveness real en unix,
`FILE_FLAG_FIRST_PIPE_INSTANCE` en Windows). Con él se van `AgentLock` del motor
del daemon y `EngineStatus::blocked_by_pid` del cable — un campo que sólo
describía la convivencia 4a–4c. Quitar un campo con `#[serde(default)]` es
compatible en las dos direcciones y hay test que lo fija. El daemon además borra
el `agent.pid` que dejaran las versiones anteriores: dejarlo en disco sólo sirve
para que dentro de un año alguien lo mire y crea que significa algo.

**2. Empaquetado.**

- **`hoardd` viaja en el bundle** como `externalBin`, igual que `hoard-screen`:
  `scripts/build-sidecar.sh` compila los dos y los deja junto a
  `tauri.conf.json`, el bundler los aplana al lado del ejecutable, y ahí es donde
  `client::daemon_binary()` mira primero. Un bundle sin `hoardd` es una app que no
  puede sincronizar, así que los tres jobs de CI que compilan el desktop crean
  también su placeholder.
- **Arranque en boot como servicio de usuario**, en `hoardd::autostart`: systemd
  *user unit* (Linux), *LaunchAgent* (macOS), tarea por-usuario del Task Scheduler
  (Windows). Nunca system-wide — el token Cloud vive en el almacén de secretos de
  *tu* sesión, que un servicio de root no puede leer.
- **La unidad ejecuta `hoardd`, no `hoard sync run`.** Desde el 4b/4c ese comando
  es un cliente, y supervisar a un espectador significa que `systemctl --user
  stop` no para el sync. Hay test de que el `ExecStart` es el daemon y de que la
  unidad y "spawn if absent" resuelven **el mismo binario**: dos vías de arranque
  que ejecutaran binarios distintos serían dos versiones del servicio según quién
  lo arrancara.
- **El traspaso es obligatorio, no cosmético.** Si ya hay daemon (lo levantó la
  app al abrirse), el que lance systemd pierde el bind y sale con 0 → unidad
  muerta, sync vivo: el peor de los dos mundos para diagnosticar. Por eso
  `install`/`restart` paran el que haya, esperan a que **suelte el socket**
  —sondeando el bind, no el handshake— y sólo entonces arrancan la unidad, y luego
  confirman que alguien escucha.
- **Quién instala.** `hoard sync start` (explícito) y el desktop, atado al
  interruptor que ya existía: "arranca al iniciar sesión" registra **dos**
  entradas (la app y el servicio) y apagarlo quita las dos. Reafirmar en cada
  arranque es lo mismo que la app ya hacía con la suya (una actualización mueve el
  binario), y es barato: no se reescribe nada si la unidad no cambió.
  *Contrapartida asumida:* tras un `hoard sync stop`, abrir la app con el
  interruptor puesto vuelve a instalar la unidad. La preferencia de la app es la
  intención persistente; el `stop` de la CLI es de ahora.
- **AppImage no puede arrancar en boot y se dice.** Su binario vive en un montaje
  efímero (`/tmp/.mount_*`) que desaparece al cerrar la app: una unidad apuntando
  ahí fallaría en el siguiente login contra una ruta inexistente. `declare()`
  aborta con el motivo y la salida (`.deb`/`.rpm`, o la CLI). El sync sigue
  corriendo con la app abierta.
- `hoard-cli/src/commands/service.rs` queda como lo que debe ser un frontend:
  traduce lo que el usuario teclea y **enseña** (estado y logs del gestor, tal
  cual él los da). La definición de la unidad y su ciclo de vida son de `hoardd`,
  que es quien la ejecuta.

**3. D.8.3 cerrado: `upload_landed` contra la verdad del server.**

El caso real es de este slice: con el daemon, **reiniciar el proceso es rutina**,
y `in_flight` vive en memoria. Un reinicio con una subida en vuelo que sí llegó a
comprometerse deja al motor creyendo que no subió nada: vuelve a subir y crea una
versión **duplicada** — mismo contenido, número nuevo, cuota gastada, ops de R2 y
un pull inútil en todos los demás equipos.

- **La pregunta se le hace al server, no a un flag local.** El almacenamiento es
  content-addressed, así que la identidad del contenido de una versión es un
  número: el digest de su manifiesto (`save_versions.sha256`, que el manifiesto de
  la nube publica como `latest_sha256`). Si el digest de lo que íbamos a subir es
  el de la cabeza, ya aterrizó.
- **No cuesta ni una petición ni una lectura.** El chequeo vive dentro del camino
  de subida cloud, justo después de hashear los ficheros —que es trabajo que ese
  camino ya hacía para el CAS— y el digest de la cabeza viene del manifiesto que
  el motor ya observa por su cuenta (D.12). Cero IO extra; lo que se ahorra es la
  subida entera.
- **El digest se calcula igual que el server o no vale para nada.** Un fallo aquí
  no da error, sólo factura: no habría coincidencia nunca y volveríamos a subir de
  más en silencio. Por eso `manifest_digest` tiene un vector fijo calculado aparte
  y tests de que el orden, el tamaño y la ruta cuentan.
- **La respuesta entra al kernel como observación** (`Observation::upload_landed`),
  que es lo que el campo llevaba esperando desde el Slice 2. El reductor la usa
  para distinguir dos no-op que hasta ahora eran el mismo: el 409 asentado a la
  cabeza **escribió** en la carpeta (y sella `last_restore_at` para no auto-vetar
  el siguiente pull) y éste no tocó nada, así que sellar un toque inexistente
  falsearía la ventana de gracia del veto. Ninguno de los dos mueve el ancla del
  min-interval: nada se subió (R.E.P.O., D.8.2).
- **Emparejar digest y versión, no dos mapas sueltos.** El empujón del cliente
  (`SetCloudVersions`) trae versiones sin digests, así que `head_for` sólo entrega
  la cabeza si el digest que tenemos es el de la versión que ahora es cabeza. Un
  digest emparejado con una versión vieja describiría contenido que ya no está, y
  creérselo sería saltarse una subida que sí hace falta.
- **Sólo el motor lo usa.** `hoard backup` (orden puntual del usuario) y la copia
  de seguridad previa al restore pasan `None`: la primera no va a pedir el
  manifiesto para adivinar si puede ahorrárselo, y la segunda **tiene** que existir
  como versión propia aunque su contenido coincida con la cabeza.
- **Se reporta como `BackupSuccess { already_landed: true }`**, campo nuevo con
  `default` (append-only, sin subir la versión de protocolo). Y no como evento
  propio: el hecho que le importa a quien mira es el mismo —"está guardado en la
  versión N"— y de ese evento cuelga la persistencia de `state.json`. Sin esa
  fila, el arranque siguiente vería la nube por delante y se bajaría su propio
  contenido. `total_bytes` es 0 porque no viajó un byte; la UI no pisa el tamaño
  de la última copia real y no manda notificación (avisar de una copia que no ha
  ocurrido es la misma mentira que sonar al reproducir el journal).

**4. El hueco del 4c cerrado: `ServerFrame::Goodbye`.**

Un cliente enganchado resucitaba el servicio ~3 s después de un `Shutdown` ajeno
porque su reconexión es "spawn if absent" y no podía distinguir "lo pararon" de
"se cayó". Se implementa la salida que el propio 4c señalaba: **que el daemon se
despida**.

- **Se despiden las dos vías deliberadas**, `Request::Shutdown` y la señal
  (`systemctl --user stop`), y **sólo** ésas: un daemon que muere de verdad
  (pánico, OOM, `kill -9`) no manda nada, así que ahí el cliente sigue
  levantándolo, que es lo correcto.
- **Y también a quien llegue tarde.** El adiós se guarda además de emitirse: el
  apagado no es instantáneo (el motor manda su último latido de presencia por
  red), y un cliente que conectara en esa ventana recibía un saludo normal, daba
  por buena la despedida anterior y relanzaba el servicio al perder el socket. El
  handshake contesta con el adiós. *Este agujero lo encontró el test*, no la
  lectura del código.
- **El testigo es de proceso y se cura solo.** Mientras esté puesto,
  `ensure_running` degrada a "conéctate" y explica el error; cualquier handshake
  con éxito lo borra, porque si hay servicio al que saludar "está parado" ya no es
  verdad. **No** es un fichero-marcador: eso es el error del pidfile otra vez.
  Consecuencia asumida y coherente con la ADR: el que no se queda apagado es un
  proceso **nuevo** (abrir la app tras pararlo lo levanta), que es exactamente la
  vía "on-demand" de la Parte A.
- **Qué hace cada cliente.** `hoard sync run` **termina** (su trabajo era seguir un
  sync que ya no corre); el desktop pinta el motor parado y espacia los reintentos
  a 30 s, sin relanzar, para engancharse solo si alguien lo arranca. `hoard sync
  run` sigue parando el servicio al recibir **su** señal: quien lo teclea pidió que
  el sync corriera, y las unidades instaladas por versiones anteriores todavía lo
  usan de `ExecStart`.
- **`ServerFrame` gana `#[serde(other)] Unknown`.** Añadir una trama dentro de la
  misma versión de protocolo sólo es compatible si el otro lado puede ignorarla;
  sin eso, la primera trama desconocida rompe el **encuadre**, y el encuadre roto
  tira la conexión — un castigo desproporcionado para "no sé qué es esto".

**Lo que queda del Slice 4:** las notificaciones nativas del SO (D.14.1),
empezando por Linux. Después, el Slice 5 (estado en SQLite).

**Sin verificar en máquina real:** el empaquetado se ha comprobado a nivel de
tipos y de tests (unidad, plist y XML de la tarea son funciones puras con tests;
el traspaso y la despedida, con procesos de verdad en Linux). Falta un humo real
de `hoard sync start` en las tres plataformas y un bundle de verdad que confirme
que `hoardd` aterriza junto al ejecutable. *(Hecho en Linux en el 4e — ver
D.19.)*

---

### D.19 — Slice 4e cerrado: avisa el servicio, y el `.deb` probado de verdad (2026-07-26)

Las notificaciones nativas las manda el daemon (decisión **b** de D.14.1), que
era lo único que le quedaba al Slice 4. **Con esto el Slice 4 está cerrado.**
Linux primero —donde se dogfoodea—, y Windows y macOS detrás de la misma
interfaz.

**Decisiones a no deshacer:**

- **El aviso sale de la bomba de eventos, no de cada ejecutor.** `engine::pump`
  es el único sitio por el que pasan todos los `AgentEvent`, así que ahí va el
  aviso. Colgarlo de la rama de backup y de la de restore por separado es
  exactamente cómo el 429 acabó manejado en un camino y no en el otro (D.7): dos
  sitios que hay que acordarse de tocar a la vez.
- **Las prefs son las que ya había, y se leen frescas.** `notify_on_success` para
  la copia guardada, `notify_on_failure` para los tres avisos de problema
  (fallo, no cabe en el plan, restore encallado). Ni una preferencia nueva: este
  slice cambia **quién** avisa, no de qué. Se releen en cada aviso porque el
  usuario toca el interruptor en Ajustes y el servicio no se reinicia por eso —
  y sólo cuando el evento es de los que pueden avisar (`notifiable`), para no
  leer `prefs.json` en cada tick. Hay test de que ese filtro barato y la puerta
  de verdad no se separan.
- **`already_landed` no suena.** No viajó un byte: el contenido ya estaba arriba
  (D.18). Avisar de una copia que no ha ocurrido es la misma mentira que sonar al
  reproducir el journal.
- **Quién avisa lo dice el cable, no una lista de plataformas en el frontend.**
  `DaemonStatus::notifications` es `notify::SUPPORTED`, una constante del build
  del daemon; el desktop lee esa bandera y se calla. Cuando aterrice el backend
  de Windows, la app se calla sola **sin tocar la UI**. Codificar "en Linux calla"
  en el frontend habría sido una regla que hay que acordarse de cambiar en otro
  sitio, que es la clase de drift del racimo 3.
- **El default de esa bandera es `false`, y es deliberado.** Un daemon anterior no
  la manda; asumir `false` significa que el frontend sigue avisando él. El peor
  caso es un aviso duplicado durante una convivencia de versiones; con el default
  al revés sería **silencio**, que es el fallo que no se ve. Hay test.
- **El texto vive en el daemon, en los ocho idiomas de la app.** No puede leer el
  i18n del frontend (es JSON del webview), y un servicio que avisa en otro idioma
  que la ventana se lee como si fuera otro programa. Son cuatro frases y no
  crecen; el idioma sale de `prefs.language` y, si el usuario no lo ha tocado,
  del entorno. Un hueco mal escrito (`{nombre}`) no da error de compilación —
  sale literalmente en la notificación—, así que hay un test que lo caza por
  idioma. Si algún día son muchas frases, la salida es compartir los `.json` en
  compilación, no dos catálogos que driftan.
- **Un fallo de entrega se queja una vez.** En una máquina sin servidor de
  notificaciones (NAS, sesión sin escritorio) fallan **todas**: la primera va a
  `warn` con el motivo, las siguientes a `debug`. Y la entrega lleva tope de 5 s,
  porque quien llama es la bomba de eventos y detrás vienen la persistencia del
  estado y el push a los clientes: un servidor de notificaciones colgado no puede
  atascar el journal.
- **Linux va por `notify-rust`, que es lo que ya usaba el desktop por debajo** (es
  la dependencia del plugin de Tauri, y ya estaba en el árbol). Así el aviso del
  servicio se ve exactamente igual que el que mandaba la app, con el mismo icono
  del tema (`hoard-desktop`).
- **El humo real del bus es un test `#[ignore]`.** Ni CI ni una sesión sin
  escritorio tienen servidor de notificaciones, así que correrlo siempre sería
  rojo por el entorno y no por el código:
  `cargo test -p hoardd -- --ignored --nocapture the_session_bus`. Verificado
  contra un servidor de notificaciones de mentira en el bus de sesión: llega
  `app="Hoard"`, `icon="hoard-desktop"` y el texto en el idioma del usuario.

**Lo que no se toca, a propósito:** un auto-restore que aterriza sigue sin
notificación nativa (era un toast de la app y sigue siéndolo). Es el candidato
más claro a añadir —con la app cerrada, unos ficheros que aparecen bajo `~` es lo
más digno de contar— pero no hay preferencia que lo gobierne y este slice no
inventa ninguna.

#### Empaquetado, verificado en máquina real (cierra el punto abierto de D.18)

`.deb` construido y **instalado** de verdad en el Linux de dogfooding. Encontró
un fallo que ningún test de tipos podía ver:

- **El sidecar viajaba sin permiso de ejecución.** `usr/bin/hoardd` iba en el
  paquete con modo `0644`, así que se instalaba sin `+x` y la app no podía
  lanzarlo: bundle correcto, instalación correcta, **app que no sincroniza**, y
  un error de `spawn` que no lleva a ninguna parte. La causa es de manual:
  `scripts/build-sidecar.sh` colocaba el binario con `cp`, y **`cp` conserva el
  modo del destino cuando el fichero ya existe**; en cuanto esa copia perdía el
  bit una vez —por ejemplo con el `touch` de placeholder que los jobs de lint
  usan para que el `build.rs` de Tauri encuentre el `externalBin`— se quedaba sin
  él en todas las builds siguientes de esa copia de trabajo. **Arreglo:**
  `install -m 0755` en vez de `cp`, más un `[ -x ]` que aborta el build. El fallo
  tiene que ser ruidoso donde se produce; un binario no ejecutable dentro de un
  `.deb` no se nota hasta que alguien lo instala.
  *Las releases de CI no estaban afectadas* (runner limpio, el destino no existe
  y `cp` hereda el modo del origen), pero nada lo garantizaba. Ahora sí.

Con eso arreglado, lo comprobado sobre el paquete instalado:

- `hoardd` **aterriza junto al ejecutable** y ejecutable: `/usr/bin/hoardd`
  (0755) al lado de `/usr/bin/hoard-desktop`, que es donde
  `client::daemon_binary()` mira primero.
- **La app levanta el servicio sola:** sin daemon y sin socket, arrancar el
  desktop instalado deja corriendo `/usr/bin/hoardd` —el hermano del ejecutable,
  no otro del `PATH`—, con el socket a `0600`, y el desktop engancha sus dos
  conexiones (`(commands)` y `(events)`). **Al cerrar la app el servicio sigue
  vivo**, que es la promesa entera del Slice 4.
- **El arranque en boot funciona:** la app reescribió la unidad
  `hoard-sync.service` —la que había apuntaba todavía a `"~/.local/bin/hoard"
  sync run`, de antes del 4d— a `ExecStart="/usr/bin/hoardd"`, `enabled`
  (`WantedBy=default.target`). `systemctl --user start` la deja `active
  (running)` sirviendo el socket, y un **cliente ajeno** (un script de Python,
  ni una línea de nuestro Rust) completa el handshake y recibe un `Status` con
  `"notifications": true` — el paquete anuncia por el cable que avisa él.

**Lo que este humo no pudo verificar, y por qué.** La sesión de dogfooding es
remota y el llavero de login está **bloqueado** (`Locked = true` en
`org.freedesktop.secrets`), así que la resolución de la sesión Cloud
(`cloud_auth`, lectura de `keyring`) se queda esperando un desbloqueo que nadie
puede contestar. Consecuencias observadas, **ninguna de este slice** pero las dos
anotadas aquí porque son de la familia D.11/D.12:

1. El motor se queda en `starting` **para siempre y sin una línea de log**:
   `EngineStatus::last_error` sigue en `None`, así que "bloqueado en el llavero"
   es indistinguible de "arrancando". Un fallo invisible más.
2. Con el motor así, `hoardd` **no se puede parar**: la llamada al llavero es
   síncrona, `task.abort()` no puede desalojarla y el proceso no termina
   (`systemctl --user stop` deja la unidad en `deactivating` hasta el SIGKILL del
   timeout). Aislado: el mismo binario instalado, con `--no-engine`, se para
   limpio en menos de dos segundos, así que el atasco es de la resolución de
   sesión, no del apagado.

El arreglo natural para ambos —tope y `spawn_blocking` alrededor del llavero, y
un `last_error` que lo diga— es de `hoard-agent`, no de este slice.

#### El llavero, siempre acotado (arreglado; `hoard-agent`)

Las dos consecuencias de arriba están arregladas. Toda llamada al llavero pasa
por `keychain::keyring_op`: la ejecuta un hilo propio, se deja de esperar a los
5 s y devuelve `KeyringTimeout` como motivo. Primero la sesión Cloud
(`cloud_auth`), después el token self-hosted (`credentials`), que se quedó igual
en la primera pasada y podía colgarse exactamente igual.

**Decisiones a no deshacer:**

- **El hilo es propio y suelto, no `spawn_blocking`.** Es la mitad que no se ve:
  al soltarse el runtime, tokio **espera a que terminen sus hilos de bloqueo**, así
  que una llamada colgada en ese pool seguiría impidiendo que el proceso muera —
  el atasco del `systemctl --user stop`. A un hilo propio y sin `join` no lo
  espera nadie al salir.
- **Uno solo, con cola.** Un hilo por llamada bastaría para no esperar de más,
  pero la llamada colgada no se puede cancelar: con el llavero bloqueado y el
  keeper reintentando cada pocos minutos, cada intento filtraría un hilo más para
  siempre. Serializando, lo que se acumula es la cola (un `Box` por intento).
- **Un solo llavero, un solo hilo.** Cloud y self-hosted comparten `keychain` a
  propósito. No hay dos llaveros: si `org.freedesktop.secrets` está bloqueado lo
  está para las dos, así que un hilo por módulo sólo daría dos hilos colgados en
  vez de uno. Con un llavero sano las operaciones tardan milisegundos y la cola no
  se nota.
- **El motivo va tipado, no sólo en el texto.** `KeyringTimeout` existe para que
  "está bloqueado" no se pueda confundir con "no hay sesión" — confundirlos es lo
  que hacía invisible el fallo. Con eso, `pick_auth` (Cloud) y `pick_token`
  (self-hosted) pueden decidir bien: el fichero 0600 salva la sesión cuando lo
  hay, y **cuando no lo hay el error del llavero sale entero** hasta `last_error`
  y el log, en vez del `Ok(None)` que pintaba a un usuario deslogueado con sus
  tokens intactos en el llavero.
- **Pero la ausencia del fichero de sesión sigue siendo "primera ejecución", no
  un error.** `save` escribe `session.toml` siempre, incluso cuando el llavero no
  está; si no hay fichero, nadie ha entrado nunca en esta máquina y el sitio del
  usuario es el asistente. Devolver el error del llavero también ahí rompería el
  primer login precisamente en las máquinas con el llavero bloqueado, que es donde
  el fallback a fichero es la única forma de entrar.
- **`credentials` no necesita su propio `load_session_async`, y es a propósito.**
  El arranque del motor self-hosted resuelve el token desde `config.toml`
  (`session::selfhosted`), no del llavero; quienes sí lo leen son la telemetría
  (hilo propio suyo) y los comandos del desktop, donde con la espera acotada el
  peor caso es un tirón de 5 s. El `spawn_blocking` está donde importaba: en
  `resolve_owned`, la task del keeper que aborta el apagado.

**Verificado en ejecución**, no sólo en tipos: con un bus de sesión falso que
acepta la conexión y no contesta nunca, el motor publica el motivo en el log y en
`engine.last_error` (antes `None`), un cliente ajeno lo lee por IPC, y un SIGTERM
mata el proceso en ~2 s con el llavero todavía mudo. En la suite hay un test de
que la task que espera al llavero se cancela al instante, y otro de que un
llavero bloqueado no se lee como "no hay sesión" — uno por sesión, Cloud y
self-hosted.

**Lo que queda igual y anotado:** `hoard-desktop/src/commands/cloud.rs` tiene
todavía su propio par `keyring_set`/`keyring_get` sin tope, hermano del que había
en `cloud_auth`. Vive en los `#[tauri::command]`, así que lo que puede colgar es
una ventana, no el servicio; el sitio al que tiene que caer es `keychain`, no una
tercera copia.

**Lo que sigue sin verificarse en máquina real:** el mismo humo en Windows y
macOS (el 4b sí ejecutó la IPC y la ACL del pipe en Windows por SSH; lo que falta
allí es el bundle + la tarea del Task Scheduler), y sus backends de
notificaciones, que ni existen todavía.

**Siguiente:** Slice 5 — estado en SQLite (C.4), con el plan de migración de
D.1.2 como prerrequisito.
