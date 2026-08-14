//! # Tipos de wire compartidos (ADR 0021, C.6 — Slice 3)
//!
//! Las formas que `hoard_agent::api` y `hoard_server::routes` mantenían **a
//! mano, por duplicado**. Ahora hay una sola definición y el drift entre
//! cliente y server es un error de compilación en vez de un 422 en producción.
//!
//! Ya había drift real cuando esto se escribió: el cliente enviaba
//! `LogEntry.target`/`ts` como campos obligatorios y el server los declaraba
//! `Option`, y `SaveResponse` (server) y `Save` (cliente) llevaban desde 2025
//! subconjuntos distintos de los mismos campos.
//!
//! ## Alcance
//!
//! El contrato **self-hosted**: `/v1/health`, `/v1/auth/whoami`,
//! `/v1/me/max-versions`, `/v1/games`, `/v1/saves`, `/v1/saves/*/snapshots` y
//! `/v1/logs` (que el namespace cloud reusa tal cual). Los DTO exclusivos de
//! cloud (`Cloud*` en `agent::api` ↔ `server::cloud::routes`) siguen duplicados;
//! son el siguiente incremento natural, no parte de este slice.
//!
//! ## Disciplina de compatibilidad — leer antes de tocar nada
//!
//! Que compile no basta: **cliente y server se despliegan por separado**. Un
//! usuario self-hosted corre un server de hace tres versiones contra un desktop
//! recién actualizado, y Hoard Cloud actualiza el server sin que nadie toque los
//! clientes instalados. Por eso:
//!
//! - **Append-only.** Se añaden campos; no se quitan.
//! - **Todo campo nuevo lleva `#[serde(default)]`** (y `Option` si no hay
//!   default sensato), para que el JSON de una versión vieja siga
//!   deserializando.
//! - **Nunca se repurposea un campo.** Cambiar el significado (o el tipo) de uno
//!   existente es indistinguible de corrupción para la otra punta. Si el
//!   significado cambia, es un campo nuevo.
//! - **`#[serde(skip_serializing_if)]` sólo donde el campo hoy no se emite.** El
//!   test golden (`tests/golden_wire.rs`) fija los bytes de la última release;
//!   si un cambio los mueve, el test cae y hay que justificarlo.
//!
//! ## Estado persistido ≠ dato nuevo
//!
//! Los newtypes de [`crate::ids`] llevan la puerta estricta en `serde`, así que
//! un valor envenenado **no entra por el wire**. Pero el server construye estas
//! respuestas leyendo su propia DB, que es estado persistido y puede llevar
//! veneno de años: ahí se usa la puerta indulgente
//! ([`crate::ids::GameSlug::repair`]), nunca `parse`, o una fila mala tumbaría
//! el listado entero de un usuario. Misma regla que en `state.json` (ADR C.3).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{GameSlug, MachineId, SaveId, Sha256, Username};

/// RFC3339, la misma representación que ya cruzaba el wire.
///
/// El server self-hosted guarda los timestamps en SQLite con
/// `strftime('%Y-%m-%dT%H:%M:%SZ','now')` y los devolvía como `String` sin
/// tocarlos; el cliente los parseaba a `OffsetDateTime` con este mismo
/// serializador. Unificar el tipo no mueve un byte: `time` emite `Z` para offset
/// cero, que es exactamente lo que escribe SQLite, y acepta al parsear tanto esa
/// forma como el `+00:00` que emite el namespace cloud.
use time::serde::rfc3339 as ts;

// ---------------------------------------------------------------------------
// GET /v1/health
// ---------------------------------------------------------------------------

/// Respuesta de `/v1/health`. El cliente ramifica el protocolo entero con
/// [`Health::mode`] (`"cloud"` vs self-hosted), así que este es el tipo más
/// load-bearing del contrato.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    /// `"ok"` o `"db_error"`.
    pub status: String,
    /// Versión del binario del server.
    pub version: String,
    #[serde(default)]
    pub uptime_secs: u64,
    /// Nivel mínimo de log que este server acepta en el ingest de logs de
    /// cliente. Ausente en servers pre-ingest: el cliente lo lee como "este
    /// server no recibe logs" y desactiva el envío.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_min_level: Option<String>,
    /// `"cloud"` en el despliegue SaaS, ausente self-hosted. Selecciona el
    /// namespace (`/v1/cloud/*` vs `/v1/saves`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Este server habla el protocolo direccionado por contenido
    /// (`/v1/saves/{id}/cas/*`): el cliente declara el manifiesto y sólo sube
    /// los blobs que al server le faltan, en vez de mandar la carpeta entera en
    /// un multipart.
    ///
    /// **Es una capacidad, no una preferencia.** Ausente = server anterior a
    /// la 1.1.3, que sólo entiende el multipart; el cliente no puede deducirlo
    /// de la versión porque la versión del server y la del cliente van por
    /// separado. Se omite cuando es `false` para que el golden de la release
    /// siga cuadrando byte a byte.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cas: bool,
    /// Este server lleva registro de dispositivos y presencia en vivo
    /// (`/v1/devices`, `/v1/presence/heartbeat`). Misma disciplina que
    /// [`Health::cas`]: capacidad del binario, no ajuste; ausente = server
    /// anterior a la 1.1.3, al que no hay que mandarle latidos.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub devices: bool,
}

// ---------------------------------------------------------------------------
// GET /v1/auth/whoami  ·  PUT /v1/me/max-versions
// ---------------------------------------------------------------------------

/// Identidad + cuota del usuario autenticado.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Whoami {
    pub user_id: String,
    pub username: Username,
    pub is_admin: bool,
    /// Bytes almacenados ahora mismo. Los servers pre-v0.3 lo omiten → 0.
    #[serde(default)]
    pub storage_used_bytes: i64,
    /// Cuota total. Los servers pre-v0.3 lo omiten → 0, que la UI lee como
    /// "cuota desconocida".
    #[serde(default)]
    pub storage_quota_bytes: i64,
    /// Tope de versiones almacenadas por save. `None` = ilimitado (y lo que
    /// reporta un server sin la feature). Sólo cuenta las **automáticas**.
    #[serde(default)]
    pub max_versions: Option<i64>,
    /// Tope de las copias deliberadas (las que pidió el usuario y la red de
    /// seguridad previa a un restore). `None` = ilimitado, que es el defecto:
    /// son pocas y son justo las que se quieren conservar. Un server viejo no
    /// lo manda y se lee como ilimitado, que es como se comportaba.
    ///
    /// Se omite cuando no hay tope para que el golden de la release siga
    /// cuadrando byte a byte; ausente y `null` se leen igual gracias al
    /// `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_manual_versions: Option<i64>,
}

/// Cuerpo de `PUT /v1/me/max-versions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxVersionsBody {
    /// `null` quita el tope (ilimitado).
    pub max_versions: Option<i64>,
    /// A qué cupo se refiere: `true` = el de las copias deliberadas, `false`
    /// (por defecto) = el de las automáticas. Un cliente viejo omite el campo
    /// y toca el cupo automático, que es lo que tocaba antes de existir esto.
    ///
    /// Se omite cuando es `false` — mismo criterio que [`Health::cas`]: el
    /// golden de la release sigue cuadrando byte a byte, y el server lee la
    /// ausencia como el cupo automático de siempre.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
    /// En seco: no escribe ni borra nada, sólo cuenta cuántos snapshots
    /// *tiraría* el tope. El cliente lo enseña en la confirmación.
    #[serde(default)]
    pub dry_run: bool,
}

/// Respuesta de `PUT /v1/me/max-versions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxVersionsResponse {
    pub max_versions: Option<i64>,
    /// Eco de qué cupo se ha tocado. Omitido cuando es el automático, por lo
    /// mismo que en [`MaxVersionsBody::manual`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
    /// Snapshots tirados a la papelera por pasarse del tope nuevo (o, en
    /// `dry_run`, los que se tirarían).
    #[serde(default)]
    pub pruned: u64,
}

// ---------------------------------------------------------------------------
// GET /v1/games
// ---------------------------------------------------------------------------

/// Una entrada del catálogo de juegos.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub slug: GameSlug,
    pub display_name: String,
    pub engine: Option<String>,
    #[serde(default)]
    pub save_paths_json: Option<String>,
}

// ---------------------------------------------------------------------------
// /v1/saves
// ---------------------------------------------------------------------------

/// Cuerpo de `POST /v1/saves`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSaveRequest {
    pub game_slug: GameSlug,
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_os: Option<String>,
    /// Metadatos opcionales de clientes nuevos. Cuando la tabla `games` del
    /// server no conoce el slug (server sembrado con un catálogo Ludusavi más
    /// viejo), con esto hace upsert de una fila stub en vez de devolver 422.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_app_id: Option<i64>,
}

/// Cuerpo de `PATCH /v1/saves/{id}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchSaveRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub local_path_hint: Option<String>,
    #[serde(default)]
    pub client_os: Option<String>,
}

/// Un save. Unión de lo que emitía `server::routes::saves::SaveResponse` y lo
/// que esperaba `agent::api::Save`; los campos que sólo tenía una punta van
/// `Option` + `default` para que ninguna versión desplegada se rompa.
///
/// Los campos agregados (`snapshot_count`, `total_size_bytes`) son `Option`
/// porque cloud no los calcula: `None` es "no lo sé", distinto de `Some(0)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Save {
    pub id: SaveId,
    /// Sólo lo emite cloud. Se omite al serializar cuando falta, para que el
    /// self-hosted siga emitiendo exactamente el JSON de la release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub game_slug: GameSlug,
    pub label: String,
    #[serde(default)]
    pub local_path_hint: Option<String>,
    #[serde(default)]
    pub client_os: Option<String>,
    #[serde(default)]
    pub latest_version_num: Option<i64>,
    #[serde(default)]
    pub snapshot_count: Option<i64>,
    #[serde(default)]
    pub total_size_bytes: Option<i64>,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
    #[serde(with = "ts")]
    pub updated_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// /v1/saves/{id}/snapshots
// ---------------------------------------------------------------------------

/// Quién pidió esta versión.
///
/// Importa para la retención. Una partida que autoguarda cada minuto —Elden
/// Ring, cualquier factory builder— llena el cupo entero en una sesión, y si
/// todas las versiones compiten por el mismo hueco, esa ráfaga de copias
/// automáticas se lleva por delante la que el usuario hizo a propósito antes de
/// un jefe. Que es justo la que quería conservar. Con el origen anotado, cada
/// clase tiene su presupuesto y una ráfaga automática sólo puede desplazar a
/// otras automáticas.
///
/// Viaja en el campo `notes` del snapshot, que existía sin usarse desde el
/// principio. `Automatic` no escribe nada: así todas las filas de antes de esto
/// —que tienen `notes` a nulo— se leen como automáticas, que es lo que son, y
/// no hace falta migrar nada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionOrigin {
    /// La hizo el motor solo: el temporizador, el cierre del juego, el barrido.
    Automatic,
    /// La pidió el usuario ("copia ahora").
    Manual,
    /// Red de seguridad antes de sobrescribir la carpeta al restaurar. Cuenta
    /// como manual: es la copia que permite deshacer un restore equivocado, y
    /// perderla es perder exactamente lo que la hacía valiosa.
    PreRestore,
}

impl VersionOrigin {
    /// Lo que se guarda en `notes`. `None` para las automáticas.
    pub fn as_note(self) -> Option<&'static str> {
        match self {
            Self::Automatic => None,
            Self::Manual => Some("manual"),
            Self::PreRestore => Some("pre-restore"),
        }
    }

    /// Lee el origen de un `notes`. Cualquier cosa que no reconozca es
    /// automática: es lo que eran las filas viejas y lo que debe ser una nota
    /// escrita por una versión futura que este cliente no entiende.
    pub fn from_note(note: Option<&str>) -> Self {
        match note.map(str::trim) {
            Some("manual") => Self::Manual,
            Some("pre-restore") => Self::PreRestore,
            _ => Self::Automatic,
        }
    }

    /// ¿Cuenta contra el presupuesto de las que el usuario hizo a propósito?
    pub fn is_deliberate(self) -> bool {
        matches!(self, Self::Manual | Self::PreRestore)
    }
}

/// Resumen de un snapshot (una versión de un save).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_id: Option<SaveId>,
    pub version_num: i64,
    /// Padre en el DAG (`None` = raíz). La arista que hace detectable la
    /// divergencia. Los servers viejos lo omiten → `None`.
    #[serde(default)]
    pub parent_version: Option<i64>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub total_size_bytes: i64,
    pub file_count: i64,
    pub is_pinned: bool,
    #[serde(default, with = "ts::option")]
    pub deleted_at: Option<OffsetDateTime>,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
}

/// Snapshot + su listado de ficheros (`GET /v1/saves/{id}/snapshots/{n}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDetail {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    pub files: Vec<SnapshotFile>,
}

/// Un fichero dentro de un snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub relative_path: String,
    pub size_bytes: i64,
    /// `None` = **no se conoce**, no "hash malo". Pasa con las versiones legacy
    /// de archivo entero, donde el listado se sintetiza leyendo el tar y no hay
    /// digest por fichero. En el JSON eso es la cadena vacía, que es lo que
    /// emitía la release: ver [`sha_opt`].
    #[serde(default, with = "sha_opt")]
    pub sha256: Option<Sha256>,
}

/// `Option<Sha256>` con `""` como forma de `None` en el JSON.
///
/// La release ya usaba la cadena vacía para "sin digest", así que relajar la
/// puerta de [`Sha256`] para dejarla pasar habría sido meter un valor imposible
/// dentro del tipo. El sitio correcto para "no aplica" es el `Option`, y este
/// módulo es el traductor entre las dos representaciones — sin tocar los bytes
/// que cruzan la red.
mod sha_opt {
    use super::Sha256;
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Sha256>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(value.as_ref().map(Sha256::as_str).unwrap_or(""))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Sha256>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        match raw.as_deref() {
            None | Some("") => Ok(None),
            Some(s) => Sha256::parse(s).map(Some).map_err(D::Error::custom),
        }
    }
}

// ---------------------------------------------------------------------------
// /v1/saves/{id}/cas/*  ·  subida direccionada por contenido (self-hosted)
// ---------------------------------------------------------------------------
//
// Hasta la 1.1.2 self-hosted sólo sabía subir en multipart: la carpeta **entera**
// en cada copia, aunque el server ya tuviera el 99% de los bytes. Deduplicaba al
// guardar (blobs por sha, ADR 0018), no al transmitir. Una partida de 3 GB que
// cambia 10 MB costaba 3 GB de subida, y encima chocaba contra
// `storage.max_snapshot_size_mb` y contra cualquier proxy con un límite de
// cuerpo por delante.
//
// Estas tres llamadas son la negociación que Hoard Cloud ya tenía: declarar el
// manifiesto, subir sólo lo que falta, confirmar. La diferencia con cloud es
// dónde aterrizan los bytes — cloud firma URLs de R2 y el cliente escribe en el
// bucket; aquí el cliente **nunca** habla con el almacenamiento (ADR 0020), así
// que cada blob que falta se sube al propio server y es él quien lo coloca.

/// Un fichero del manifiesto: qué ruta, qué contenido, cuánto ocupa.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasFile {
    pub relative_path: String,
    pub sha256: Sha256,
    pub size_bytes: i64,
}

/// Cuerpo de `POST /v1/saves/{id}/cas/init`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasInit {
    /// La versión sobre la que el cliente cree estar construyendo. Igual que en
    /// el multipart: si ya no es la cabeza, otro equipo se adelantó y esto se
    /// rechaza — aquí **antes** de mover un byte, que es la gracia.
    #[serde(default)]
    pub base_version: Option<i64>,
    pub files: Vec<CasFile>,
}

/// Un blob que el server no tiene y el cliente debe subir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasMissing {
    pub sha256: Sha256,
    pub size_bytes: i64,
}

/// Respuesta de `cas/init`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasInitOut {
    /// Identifica el área de staging de esta subida
    /// (`PUT /v1/cas/blobs/{upload_id}/{sha}`). La mina el server; caduca con el
    /// barrido de `tmp/` (`retention.tmp_cleanup_hours`), así que una subida
    /// abandonada se limpia sola.
    pub upload_id: String,
    /// La versión que saldrá del commit **si nadie se adelanta**. Orientativa:
    /// el número de verdad lo asigna el commit, bajo la misma transacción que
    /// comprueba la cabeza.
    pub version_num: i64,
    /// Lo que hay que subir. Todo lo demás del manifiesto ya está guardado.
    pub missing: Vec<CasMissing>,
    /// Bytes que se van a transmitir de verdad (suma de `missing`), frente al
    /// tamaño lógico de la partida. La diferencia es lo que ahorra el dedup, y
    /// el cliente la usa para que la barra de progreso mida la subida real.
    pub missing_bytes: i64,
}

/// Cuerpo de `POST /v1/saves/{id}/cas/commit`. Repite el manifiesto: el server
/// no guarda nada entre init y commit salvo los bytes en staging, así que un
/// commit es autosuficiente y un init perdido no deja una fila a medias en la
/// base (que es lo que obliga a cloud a llevar versiones "pendientes").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasCommit {
    pub upload_id: String,
    #[serde(default)]
    pub base_version: Option<i64>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub files: Vec<CasFile>,
}

// ---------------------------------------------------------------------------
// GET /v1/devices  ·  POST /v1/presence/heartbeat
// ---------------------------------------------------------------------------
//
// Los dos despliegues montan **estas mismas rutas** (no viven bajo
// `/v1/cloud/`), así que aquí sí compilan las dos puntas contra una única
// definición. El cliente las usaba ya contra cloud; self-hosted las sirve desde
// la 1.1.3.
//
// Qué contestan, y por qué importa las tres cosas a la vez: de qué máquina salió
// cada versión (eso ya lo lleva `Snapshot::device_name`), qué máquinas de la
// misma cuenta existen, y cuáles están encendidas ahora mismo y jugando a qué.

/// Un juego en un latido: slug + segundos que lleva corriendo.
///
/// Duración y no timestamp a propósito: el server la ancla a **su** reloj
/// (`ahora - secs`), así un cliente con la hora desviada no puede declarar que
/// lleva jugando desde dentro de tres minutos.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayingBeat {
    pub slug: String,
    pub for_secs: u64,
}

/// Cuerpo de `POST /v1/presence/heartbeat`. Todo opcional: un keepalive de una
/// máquina ociosa es `{}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Juegos corriendo ahora, el más reciente primero. Vacío = ociosa.
    #[serde(default)]
    pub playing: Vec<PlayingBeat>,
    /// Latido final del apagado ordenado: apaga el punto al instante en vez de
    /// esperar a que la máquina envejezca fuera de la ventana.
    #[serde(default)]
    pub closing: bool,
}

/// Un juego corriendo en un dispositivo (`GET /v1/devices`): slug + RFC3339 del
/// arranque de la sesión.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePlaying {
    pub slug: String,
    #[serde(default)]
    pub since: Option<String>,
}

/// Un dispositivo de la cuenta con su presencia en vivo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceOut {
    pub id: String,
    pub device_name: String,
    #[serde(default)]
    pub device_kind: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Latido fresco y sin latido de cierre. Se calcula **al leer**, no se
    /// guarda: así una máquina que murió sin despedirse se apaga sola en vez de
    /// quedarse encendida para siempre.
    #[serde(default)]
    pub online: bool,
    /// Juegos corriendo ahora mismo (el más reciente primero); sólo viene si
    /// `online`. Vacío = ociosa.
    #[serde(default)]
    pub playing: Vec<DevicePlaying>,
    /// True en la fila que corresponde al fingerprint de quien pregunta, para
    /// que la UI se reconozca sin conocer su propio id.
    #[serde(default)]
    pub this_device: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceListOut {
    pub devices: Vec<DeviceOut>,
}

// ---------------------------------------------------------------------------
// POST /v1/logs  (el namespace cloud monta este mismo cuerpo)
// ---------------------------------------------------------------------------

/// El `target` de las **desmentidas**: los eventos que dicen dónde se equivocó
/// la detección y qué hizo el humano para arreglarlo (`hoard_agent::telemetry`).
///
/// Vive aquí porque es contrato de los dos lados: el cliente lo exime de su
/// filtro por nivel y el server lo acepta aunque esté por debajo del mínimo que
/// anuncia. Un `where target = 'hoard::telemetry'` es toda la consulta.
pub const TELEMETRY_TARGET: &str = "hoard::telemetry";

/// El `target` de la telemetría de **Hoard Screen**: cuándo se abre el overlay,
/// cuánto se tiene puesto y qué se monta dentro (`hoard_desktop::screen_telemetry`).
///
/// Va aparte de [`TELEMETRY_TARGET`] a propósito y no como un `verdict` más: son
/// dos preguntas distintas —una es "dónde falla la detección", la otra "usa
/// alguien el overlay"— y mezclarlas obliga a filtrar por `fields` en cada
/// consulta de las dos. Con un target propio, cada panel es un `where target = …`.
pub const SCREEN_TARGET: &str = "hoard::screen";

/// Targets que viajan sea cual sea su nivel.
///
/// Los dos son INFO y los dos tienen que entrar en Cloud, cuyo mínimo es WARN.
/// Se listan aquí, en el contrato, para que añadir un tercero no exija tocar el
/// cliente y el server por separado — que es exactamente cómo se rompió esto la
/// primera vez.
pub const EXEMPT_TARGETS: [&str; 2] = [TELEMETRY_TARGET, SCREEN_TARGET];

/// La versión de los Términos que el cliente pide aceptar, tal y como la
/// enseña la web: una **fecha**, no un semver.
///
/// Es contrato de los dos lados. El cliente la manda a `POST /v1/me/terms` al
/// entrar y el server la guarda tal cual; `GET /v1/me` devuelve la última
/// aceptada, y si no coincide con ésta el cliente vuelve a pedir la casilla.
/// Por eso es una fecha: lo que hay que poder casar el día de una disputa es
/// "qué texto estaba publicado cuando esta persona dijo que sí", y la fecha es
/// justo lo que la página enseña.
///
/// Súbela **sólo** cuando cambie el fondo del documento. Un cambio de coma que
/// mueva este literal vuelve a interrumpir a todo el mundo para nada. Va en
/// pareja con `TERMS_VERSION` de `web/src/lib/legal.ts`.
pub const TERMS_VERSION: &str = "2026-08-11";

/// Metadatos del dispositivo, una vez por lote.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    /// Huella de la máquina. Es la misma que usa el alta de dispositivos, así
    /// que una fila de log y el contador de dispositivos casan por este campo.
    #[serde(default)]
    pub fingerprint: Option<MachineId>,
    #[serde(default)]
    pub app_version: Option<String>,
}

/// Una línea de log enviada por un cliente.
///
/// `target` y `ts` son `Option` porque así los declaraba el server: el cliente
/// los mandaba siempre, pero el contrato tolera su ausencia y romper eso
/// rechazaría lotes de clientes viejos.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    #[serde(default)]
    pub target: Option<String>,
    pub message: String,
    /// Campos estructurados como objeto JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Value>,
    /// Timestamp del cliente (RFC3339). Se queda como `String`: es dato de
    /// diagnóstico de una punta que puede tener el reloj torcido, y el server
    /// lo guarda tal cual sin interpretarlo.
    #[serde(default)]
    pub ts: Option<String>,
}

/// Un lote de logs (`POST /v1/logs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogBatch {
    pub device: DeviceMeta,
    pub entries: Vec<LogEntry>,
}

/// Respuesta del ingest de logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogIngestResponse {
    pub accepted: usize,
}

/// Orden de los niveles, para comparar sin depender de `tracing` (el server no
/// lo tiene). Un nivel desconocido vale INFO: nunca se tira por no reconocerlo.
pub fn level_rank(level: &str) -> u8 {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "warn" => 3,
        "error" => 4,
        _ => 2, // info y cualquier cosa que no conozcamos
    }
}

/// Mínimo que Cloud guarda: WARN. Con toda la población enviando, el INFO
/// operativo llena la tabla de ruido y la poda a 14 días se lleva por delante
/// los casos raros, que son los que sirven.
pub const CLOUD_MIN_RANK: u8 = 3;

/// **La** regla de qué línea viaja y cuál se guarda, una sola vez para los dos
/// lados: el cliente la usa para filtrar en origen y el server para no fiarse
/// del cliente.
///
/// Estaba escrita tres veces —agente, server, cloud— y eso es una fuga
/// esperando: si el cliente filtra a un nivel y el server a otro, o se envía lo
/// que el server tira (ancho de banda para nada) o se tira lo que el cliente
/// manda (datos que nadie vuelve a ver), y en los dos casos en silencio. Es el
/// mismo motivo por el que `LogEntry` vive aquí y no duplicado (ADR 0021 C.6).
///
/// La excepción por `target` es el corazón del asunto: las desmentidas de
/// detección y la telemetría de Screen son INFO y tienen que entrar en Cloud,
/// cuyo mínimo es WARN. Ver [`EXEMPT_TARGETS`].
pub fn ships_at(entry: &LogEntry, min_rank: u8) -> bool {
    entry
        .target
        .as_deref()
        .is_some_and(|t| EXEMPT_TARGETS.contains(&t))
        || level_rank(&entry.level) >= min_rank
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, target: &str) -> LogEntry {
        LogEntry {
            level: level.to_string(),
            target: Some(target.to_string()),
            message: String::new(),
            fields: None,
            ts: None,
        }
    }

    /// La matriz entera de la regla compartida. Cliente y server llaman aquí,
    /// así que este test es el que impide que vuelvan a separarse.
    #[test]
    fn one_rule_decides_what_travels_and_what_is_stored() {
        // Cloud (WARN): el ruido operativo se queda fuera.
        for level in ["trace", "debug", "info", "notice", "TRACE"] {
            assert!(
                !ships_at(&entry(level, "hoard_agent::agent"), CLOUD_MIN_RANK),
                "{level} no debería entrar en Cloud"
            );
        }
        for level in ["warn", "WARN", " error ", "error"] {
            assert!(
                ships_at(&entry(level, "hoard_agent::agent"), CLOUD_MIN_RANK),
                "{level} sí debería entrar en Cloud"
            );
        }
        // Las desmentidas entran en Cloud aunque sean INFO, que es su nivel.
        assert!(ships_at(&entry("info", TELEMETRY_TARGET), CLOUD_MIN_RANK));
        // …y hasta un DEBUG con ese target, por si algún día baja de nivel.
        assert!(ships_at(&entry("debug", TELEMETRY_TARGET), CLOUD_MIN_RANK));
        // Lo mismo para Screen, y por el mismo motivo: si su INFO no entra en
        // Cloud, el panel del overlay se queda a cero sin que nada falle.
        for level in ["info", "debug", "trace"] {
            assert!(ships_at(&entry(level, SCREEN_TARGET), CLOUD_MIN_RANK));
        }
        // Un target parecido pero que no es el nuestro no se cuela.
        assert!(!ships_at(&entry("info", "hoard::screenshot"), CLOUD_MIN_RANK));
        // Self-hosted (DEBUG) se queda con casi todo, pero no con TRACE.
        assert!(ships_at(&entry("debug", "hoard_agent::agent"), 1));
        assert!(!ships_at(&entry("trace", "hoard_agent::agent"), 1));
        // Sin target (cliente viejo o entrada a medias) manda el nivel.
        let mut naked = entry("info", "x");
        naked.target = None;
        assert!(!ships_at(&naked, CLOUD_MIN_RANK));
    }

    /// Un nivel que no conocemos no se tira por no conocerlo: cuenta como INFO.
    #[test]
    fn an_unknown_level_ranks_as_info() {
        assert_eq!(level_rank("notice"), level_rank("info"));
        assert_eq!(level_rank(""), level_rank("info"));
        assert_eq!(level_rank(" WARN "), level_rank("warn"));
    }

    /// Los timestamps salen con el sufijo `Z` de la release, y vuelven a entrar
    /// tanto en esa forma como en la que emite el namespace cloud (`+00:00`).
    #[test]
    fn timestamps_keep_the_release_shape() {
        let json = r#"{
            "id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "game_slug": "stardew-valley",
            "label": "default",
            "local_path_hint": null,
            "client_os": null,
            "latest_version_num": 7,
            "snapshot_count": 3,
            "total_size_bytes": 1024,
            "created_at": "2026-07-24T12:34:56Z",
            "updated_at": "2026-07-24T12:34:56+00:00"
        }"#;
        let save: Save = serde_json::from_str(json).unwrap();
        let out = serde_json::to_value(&save).unwrap();
        assert_eq!(out["created_at"], "2026-07-24T12:34:56Z");
        assert_eq!(out["updated_at"], "2026-07-24T12:34:56Z");
        // `user_id` ausente no se emite: los bytes self-hosted no cambian.
        assert!(out.get("user_id").is_none());
    }

    /// Un save con el slug envenenado no entra por el wire. Es la mitad
    /// "estricta" de la ADR C.3: los datos nuevos pasan por la puerta.
    #[test]
    fn poisoned_slug_never_crosses_the_wire() {
        let json = r#"{
            "id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "game_slug": "GSE Saves",
            "label": "default",
            "created_at": "2026-07-24T12:34:56Z",
            "updated_at": "2026-07-24T12:34:56Z"
        }"#;
        assert!(serde_json::from_str::<Save>(json).is_err());
    }

    /// Un JSON de una versión vieja (sin los campos que se añadieron después)
    /// sigue deserializando. Es la disciplina append-only en forma de test.
    #[test]
    fn older_payloads_still_deserialize() {
        let old_health = r#"{"status":"ok","version":"0.2.0"}"#;
        let h: Health = serde_json::from_str(old_health).unwrap();
        assert_eq!(h.uptime_secs, 0);
        assert!(h.mode.is_none());

        let old_whoami = r#"{"user_id":"u1","username":"jacka","is_admin":false}"#;
        let w: Whoami = serde_json::from_str(old_whoami).unwrap();
        assert_eq!(w.storage_quota_bytes, 0);
        assert!(w.max_versions.is_none());

        let old_snapshot = r#"{
            "id":"s1","version_num":1,"total_size_bytes":10,"file_count":2,
            "is_pinned":false,"created_at":"2026-07-24T12:34:56Z"
        }"#;
        let s: Snapshot = serde_json::from_str(old_snapshot).unwrap();
        assert!(s.parent_version.is_none());
        assert!(s.deleted_at.is_none());
        assert!(s.save_id.is_none());
    }

    /// `SnapshotDetail` aplana el resumen: los campos del snapshot viven al
    /// mismo nivel que `files`, como en la release.
    #[test]
    fn snapshot_detail_stays_flat() {
        let json = r#"{
            "id":"s1","version_num":1,"total_size_bytes":10,"file_count":1,
            "is_pinned":false,"created_at":"2026-07-24T12:34:56Z",
            "files":[{"relative_path":"a.sav","size_bytes":10,
                      "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]
        }"#;
        let d: SnapshotDetail = serde_json::from_str(json).unwrap();
        assert_eq!(d.files.len(), 1);
        let out = serde_json::to_value(&d).unwrap();
        assert_eq!(out["version_num"], 1);
        assert!(out["files"].is_array());
    }
}
