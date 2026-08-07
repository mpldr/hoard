//! La sesión activa: qué servidor, con qué token, **y quién lo renueva**.
//!
//! Implementación **única** compartida por el servicio (`hoardd`) y la CLI
//! (ADR 0021, Slice 4c). Hasta el 4b había dos copias: la de la CLI y el port que
//! el 4a hizo en `hoardd/src/session.rs` para no tocarla. Con la CLI convertida
//! en cliente, esa duplicación no tiene excusa y vive aquí, donde manda la regla
//! de `CLAUDE.md`: la lógica va en `hoard-agent` y los frontends son vistas.
//!
//! ## Dos caminos, y sólo uno rota
//!
//! - [`resolve_owned`] — **el servicio**. Resuelve las credenciales, refresca el
//!   JWT de arranque y se queda con el par rotado. Lo acompaña [`refresh_loop`],
//!   que renueva antes de que expire, y [`lend_token`], que presta un token
//!   válido a quien lo pida por IPC.
//! - [`resolve_borrowed`] — **un cliente** (la CLI en un one-shot). Nunca llama a
//!   GoTrue: usa el token que el servicio le presta y, si no hay servicio, el que
//!   haya en disco tal cual.
//!
//! Que dos procesos pudieran rotar el mismo refresh token de `cloud.toml` es la
//! causa raíz de una familia entera de bugs cloud (401 por reuse-detection,
//! realtime enmudecido); el pidfile lo evitaba por exclusión, no por diseño. Aquí
//! la separación es de tipos: el único que rota es el que llama a
//! [`resolve_owned`]/[`refresh_loop`]/[`lend_token`], y eso es el daemon.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hoard_core::ipc::CloudToken;

use crate::api::ApiClient;
use crate::cloud_auth;
use crate::config::CliConfig;
use crate::credentials::{self, Credentials};
use crate::state;
use crate::supervisor::Finished;

/// Cadencia normal del refresher: renovar el JWT (~1 h de vida) con margen.
const REFRESH_EVERY: Duration = Duration::from_secs(45 * 60);

/// Cadencia con la sesión muerta. Sólo relee disco esperando un login nuevo, así
/// que comprobar a menudo no cuesta nada — y **sin red**: repetir un token
/// revocado cada pocos minutos es lo que llenaba el journal del sistema con el
/// mismo WARN durante días.
const RELOGIN_RECHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// Cuánto insiste el arranque con un fallo *transitorio* del refresh inicial
/// antes de arrancar con el token guardado.
const BOOT_REFRESH_GRACE: Duration = Duration::from_secs(60);

/// Cuánto espera el arranque a que un servidor self-hosted conteste `/v1/health`.
///
/// El servicio arranca en el boot, y un server self-hosted **en la misma caja**
/// puede estar aún levantándose: sin esta espera el primer auto-restore falla con
/// "connection refused" para todos los saves. Acotado, para no quedarnos colgados
/// cuando simplemente no hay server (el motor reintenta por save de todas formas).
const SERVER_WAIT: Duration = Duration::from_secs(60);

/// Vida mínima que debe quedarle a un token para prestarlo sin rotar.
///
/// Load-bearing que sea **mayor** que el margen con el que los clientes deciden
/// "este token está a punto de caducar" (120 s en el realtime del desktop): si
/// fuese menor, el cliente pediría, recibiría el mismo token y volvería a pedir
/// cada pocos segundos hasta cruzar nuestro umbral.
const LEND_MIN_TTL: i64 = 5 * 60;

/// Sesión activa resuelta.
pub struct Active {
    pub client: ApiClient,
    pub is_cloud: bool,
    /// Descripción legible del destino (banners, logs, `Status` del IPC).
    pub server: String,
    /// Credenciales Cloud para las llamadas REST que van por fuera del
    /// `ApiClient` (`hoard cloud`, el refresher). `None` en self-hosted.
    pub cloud: Option<CloudEndpoint>,
}

/// A qué Cloud y con qué token. El refresh token **sólo** viaja aquí cuando
/// quien resolvió es el dueño ([`resolve_owned`]): un cliente no rota, así que no
/// lo recibe, y sin él no puede rotar aunque se despiste. La regla de "un único
/// rotador" es así una propiedad de los tipos y no un comentario.
pub struct CloudEndpoint {
    pub server_url: String,
    pub access: String,
    pub refresh: Option<String>,
}

impl CloudEndpoint {
    /// La sesión completa, que es lo que [`refresh_loop`] necesita. `None` para
    /// un endpoint prestado.
    pub fn owned(&self) -> Option<cloud_auth::Session> {
        Some(cloud_auth::Session {
            server_url: self.server_url.clone(),
            access: self.access.clone(),
            refresh: self.refresh.clone()?,
        })
    }
}

// ---- el servicio: resolver rotando ------------------------------------

/// Resuelve la sesión **del dueño del token**: Cloud si hay sesión, si no
/// self-hosted por el token de config. Fija el **contexto de sync**
/// (`state::set_active_context`) antes de que nadie lea `state.json`: sin eso el
/// daemon cargaría el mapa de saves de otra cuenta.
///
/// Un fallo transitorio del refresh inicial reintenta durante
/// [`BOOT_REFRESH_GRACE`] y luego arranca con lo que haya en disco. El servicio
/// arranca en el boot, rutinariamente antes de que el DNS conteste; morir ahí
/// deja la máquina sin sincronizar nada hasta el siguiente intento, y el
/// refresher repara el token en cuanto hay red. Una sesión terminalmente
/// expirada sí es fatal: sólo un login nuevo la arregla y esperar no ayuda.
///
/// **Sólo el servicio llama a esto.** Es el camino que rota.
pub async fn resolve_owned() -> Result<Active> {
    // `load_session_async` y no `load_session`: la lectura del llavero es síncrona
    // y este camino corre en la task del keeper, la que el apagado aborta. Con la
    // lectura en el hilo de la task, un llavero bloqueado dejaba el motor en
    // `starting` y el daemon sin poder pararse (D.19).
    if let Some(sess) = cloud_auth::load_session_async().await? {
        return resolve_cloud_owned(sess).await;
    }
    let active = selfhosted_owned().await?;
    // Cloud siempre está arriba; esperar sólo tiene sentido con un server propio.
    wait_for_server(&active).await;
    Ok(active)
}

async fn resolve_cloud_owned(sess: cloud_auth::Session) -> Result<Active> {
    let refreshed = initial_refresh().await?;
    let degraded = refreshed.is_none();
    let (access, refresh) = match refreshed {
        Some(t) => (t.access, t.refresh),
        None => (sess.access.clone(), sess.refresh.clone()),
    };

    let client = ApiClient::new(sess.server_url.clone(), access.clone())?;
    let cloud = Some(CloudEndpoint {
        server_url: sess.server_url.clone(),
        access: access.clone(),
        refresh: Some(refresh),
    });
    lend_to_logship(&sess.server_url, &access);

    match cloud_auth::fetch_me(&sess.server_url, &access).await {
        Ok(me) => {
            state::set_active_context(Some(state::cloud_context(&me.user_id)));
            Ok(Active {
                client,
                is_cloud: true,
                server: format!("Cloud · {} ({})", me.email, me.plan),
                cloud,
            })
        }
        // El mismo corte de red que hundió el refresh. Se fija el contexto desde
        // el `sub` del JWT guardado — sin red, y es el mismo id que habría dado
        // `/v1/me`. Si no se puede leer, se aborta: correr bajo el contexto
        // equivocado sincronizaría el mapa de saves de otra cuenta.
        Err(err) if degraded => {
            let user_id = cloud_auth::session_user_id()?.context(
                "Cloud is unreachable and the stored session is unreadable — run `hoard login`",
            )?;
            state::set_active_context(Some(state::cloud_context(&user_id)));
            tracing::warn!(error = %err, "session: Cloud unreachable; starting on the stored session");
            Ok(Active {
                client,
                is_cloud: true,
                server: format!("Cloud · {} (unverified)", sess.server_url),
                cloud,
            })
        }
        Err(err) => Err(err),
    }
}

/// El refresh de arranque, reintentado dentro de la gracia. `Ok(None)` = se
/// agotó la gracia con un fallo transitorio (arranca con el token guardado).
async fn initial_refresh() -> Result<Option<cloud_auth::Tokens>> {
    let deadline = Instant::now() + BOOT_REFRESH_GRACE;
    let mut backoff = Duration::from_secs(2);
    loop {
        match cloud_auth::refresh_freshest().await {
            Ok(tokens) => return Ok(Some(tokens)),
            // Reuse-detection sin nada que adoptar: reintentar no lo arregla.
            Err(err) if cloud_auth::is_session_expired(&err) => return Err(err),
            Err(err) => {
                if Instant::now() >= deadline {
                    tracing::warn!(error = %err, "session: couldn't renew the Cloud session at boot");
                    return Ok(None);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

/// Sondea `/v1/health` hasta que el server conteste, con tope. Si se agota,
/// avisa y sigue: el motor lo intentará igual.
async fn wait_for_server(active: &Active) {
    let deadline = Instant::now() + SERVER_WAIT;
    let mut announced = false;
    loop {
        if active.client.health().await.is_ok() {
            if announced {
                tracing::info!(server = %active.server, "session: the server is up");
            }
            return;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                server = %active.server,
                secs = SERVER_WAIT.as_secs(),
                "session: the server is still unreachable; continuing anyway"
            );
            return;
        }
        if !announced {
            tracing::info!(server = %active.server, "session: waiting for the server to come online");
            announced = true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ---- un cliente: resolver con un token prestado -----------------------

/// Resuelve la sesión de un **cliente**: igual que [`resolve_owned`] en cuanto a
/// qué servidor y qué contexto, pero **sin rotar nada**.
///
/// `lent` es el token Cloud que el servicio ha prestado (`Request::CloudToken`) y
/// `lent_server` la sesión self-hosted (`Request::ServerToken`); `None` en
/// cualquiera de los dos significa "no había servicio a quien pedirlo": Cloud usa
/// el token de disco tal cual y self-hosted cae a `config.toml`. Puede estar caducado —nadie lo ha renovado, justamente porque
/// el rotador es el servicio— y entonces la llamada fallará con un 401 legible;
/// [`stale_token_hint`] es la pista que la CLI enseña en ese caso. Degradar así
/// es a propósito: la alternativa era que un `hoard whoami` arrancara el servicio
/// de sync como efecto secundario.
pub async fn resolve_borrowed(
    lent: Option<CloudToken>,
    lent_server: Option<hoard_core::ipc::ServerSession>,
) -> Result<Active> {
    if let Some(sess) = cloud_auth::load_session()? {
        let (server_url, access) = match lent {
            Some(token) => (token.server_url, token.access_token),
            None => (sess.server_url.clone(), sess.access.clone()),
        };
        let client = ApiClient::new(server_url.clone(), access.clone())?;
        // `/v1/me` sigue siendo la comprobación de que el token vale y de quién
        // es —lo mismo que hacía la CLI antes de este slice—; lo que ya no hay
        // es un refresh antes. Si falla, el contexto se fija igual desde el `sub`
        // del JWT: un comando que aborta no debe dejar el contexto de otra cuenta.
        match cloud_auth::fetch_me(&server_url, &access).await {
            Ok(me) => {
                state::set_active_context(Some(state::cloud_context(&me.user_id)));
                Ok(Active {
                    client,
                    is_cloud: true,
                    server: format!("Cloud · {} ({})", me.email, me.plan),
                    cloud: Some(CloudEndpoint {
                        server_url,
                        access,
                        // Prestado: sin refresh token, porque un cliente no rota.
                        refresh: None,
                    }),
                })
            }
            Err(err) => {
                let user_id = cloud_auth::session_user_id()?
                    .context("the stored Cloud session is unreadable — run `hoard login`")?;
                state::set_active_context(Some(state::cloud_context(&user_id)));
                Err(err)
            }
        }
    } else {
        selfhosted_borrowed(lent_server)
    }
}

/// Sesión self-hosted **del dueño** (ni Cloud ni tokens que rotar): del almacén de
/// sesión y, si ahí no hay nada, de `config.toml`.
///
/// El orden es el arreglo de D.20, y era un bug de los gordos: hasta aquí esto
/// leía **sólo** `config.toml`, que escribe únicamente `hoard login --token`. La
/// app guarda su sesión en `credentials` (llavero + `session.toml`), así que quien
/// entraba a su server sólo por la app tenía un motor que no resolvía sesión
/// ninguna: "no session, sign in with `hoard login`" en el `last_error`, cero
/// sincronización, y una UI que mientras tanto decía "conectado". Dos almacenes
/// disjuntos y ningún puente.
///
/// Manda [`credentials`] porque es la sesión que el usuario ve en la app y la que
/// tocan los logins nuevos (el de la app y el de la CLI, que la entrega también).
/// `config.toml` se queda como el camino headless de siempre —texto plano, sin
/// llavero, el que documenta la guía de self-hosting— y sirve de fallback para las
/// instalaciones que ya lo tenían.
///
/// Es `async` porque el llavero es síncrono y bloquea el hilo mientras espera: en
/// la task del keeper —la que el apagado aborta— eso es media mitad del fallo de
/// D.19, así que la lectura va al pool de bloqueo igual que la de Cloud.
async fn selfhosted_owned() -> Result<Active> {
    let stored = tokio::task::spawn_blocking(credentials::load_detailed)
        .await
        .map_err(|join| anyhow::Error::new(join).context("leyendo la sesión self-hosted"))??;

    // El token venía del fichero 0600: o lo dejó ahí un cliente sin servicio, o el
    // llavero estaba mudo cuando se guardó. Subirlo **ahora**, desde el daemon, es
    // lo que le da la propiedad del ítem — y en macOS la propiedad es la diferencia
    // entre leerlo callando y un diálogo de contraseña en cada arranque del motor.
    // Best-effort y en el pool de bloqueo, como toda escritura del llavero.
    if let Some((creds, credentials::TokenStorage::File)) = &stored {
        let creds = creds.clone();
        let _ = tokio::task::spawn_blocking(move || credentials::promote_to_keyring(&creds)).await;
    }

    let creds = pick_selfhosted(stored.map(|(creds, _)| creds), config_session()?)?;
    state::set_active_context(Some(state::selfhosted_context(&creds.url)));
    let client = ApiClient::new(creds.url.clone(), creds.token)?;
    Ok(Active {
        client,
        is_cloud: false,
        server: creds.url,
        cloud: None,
    })
}

/// **La precedencia, y nada más.** Pura y con tests porque este `or` *es* el bug
/// que rompió el self-hosted en la 1.1.0: el orden vivía implícito en un `if` que
/// sólo miraba `config.toml`, no había nada que lo fijara, y romperlo no ponía
/// rojo a nadie. Un test que compila el orden en la suite es lo que impide que
/// vuelva sin que CI se entere.
fn pick_selfhosted(
    stored: Option<Credentials>,
    from_config: Option<Credentials>,
) -> Result<Credentials> {
    stored
        .or(from_config)
        .ok_or_else(|| anyhow::Error::new(NoSession))
}

/// La sesión de `config.toml`, si la hay. `None` no es un error: es el caso normal
/// de quien nunca ha usado la CLI.
fn config_session() -> Result<Option<Credentials>> {
    let (cfg, _) = CliConfig::load_default()?;
    Ok(cfg
        .auth
        .token
        .filter(|t| !t.is_empty())
        .map(|token| Credentials {
            url: cfg.server.url,
            token,
            user: None,
        }))
}

/// La sesión self-hosted de un **cliente**: la que el servicio le presta, y si no
/// hay servicio la de `config.toml`.
///
/// Nunca el llavero, y eso es el punto: el ítem es del daemon, así que un cliente
/// que lo leyera volvería a pedirle la contraseña al usuario en macOS (D.20). Sin
/// servicio se degrada a `config.toml` —el camino headless de siempre— y quien
/// entró sólo por la app verá "no session" hasta que el servicio esté arriba, que
/// es quien tiene su sesión.
fn selfhosted_borrowed(lent: Option<hoard_core::ipc::ServerSession>) -> Result<Active> {
    if let Some(lent) = lent {
        state::set_active_context(Some(state::selfhosted_context(&lent.server_url)));
        let client = ApiClient::new(lent.server_url.clone(), lent.token)?;
        return Ok(Active {
            client,
            is_cloud: false,
            server: lent.server_url,
            cloud: None,
        });
    }
    selfhosted_from_config()
}

/// `config.toml`: el camino headless, texto plano y sin llavero. Lo escribe
/// `hoard login --token` y lo documenta la guía de self-hosting.
fn selfhosted_from_config() -> Result<Active> {
    let creds = config_session()?.ok_or_else(|| anyhow::Error::new(NoSession))?;
    state::set_active_context(Some(state::selfhosted_context(&creds.url)));
    let client = ApiClient::new(creds.url.clone(), creds.token)?;
    Ok(Active {
        client,
        is_cloud: false,
        server: creds.url,
        cloud: None,
    })
}

/// No hay sesión que usar en esta máquina.
///
/// Tipo propio y no un `anyhow!("no session")` porque el daemon lo clasifica por
/// downcast para que la ventana pueda decir *esto* en vez de "el servicio está
/// desconectado" — el banner genérico que costó dos hilos de soporte en julio de
/// 2026, con dos usuarios que no tenían forma de saber que les faltaba la sesión.
/// El texto se mantiene igual que el de antes: es el que sale en el `last_error`
/// y en el log del servicio.
#[derive(Debug)]
pub struct NoSession;

impl std::fmt::Display for NoSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "no session. Sign in with `hoard login` (Cloud) or \
             `hoard login --token <token>` (self-host).",
        )
    }
}

impl std::error::Error for NoSession {}

/// El token self-hosted que el daemon presta a un cliente
/// (`Request::ServerToken`). `None` = no hay sesión self-hosted en esta máquina.
///
/// Sirve las dos fuentes que resuelve [`selfhosted`], en el mismo orden, para que
/// lo que el cliente usa y lo que el motor usa no puedan divergir.
pub fn lend_server_session() -> Result<Option<hoard_core::ipc::ServerSession>> {
    if let Some(creds) = credentials::load()? {
        return Ok(Some(hoard_core::ipc::ServerSession {
            server_url: creds.url,
            token: creds.token,
            user: creds.user.map(|u| hoard_core::ipc::ServerUser {
                user_id: u.user_id,
                username: u.username,
                is_admin: u.is_admin,
            }),
        }));
    }
    let (cfg, _) = CliConfig::load_default()?;
    Ok(cfg
        .auth
        .token
        .filter(|t| !t.is_empty())
        .map(|token| hoard_core::ipc::ServerSession {
            server_url: cfg.server.url.clone(),
            token,
            // `config.toml` no cachea el whoami: el cliente que lo necesite lo
            // pregunta al server, que es de donde salía antes.
            user: None,
        }))
}

/// Fija el contexto de sync **sin red**: Cloud por el `sub` del JWT guardado, si
/// no self-hosted por la URL de config. Para comandos locales (`hoard saves`) que
/// deben funcionar sin conexión. Best-effort: si no puede, deja el contexto por
/// defecto.
pub fn set_context_offline() {
    if let Ok(Some(user_id)) = cloud_auth::session_user_id() {
        state::set_active_context(Some(state::cloud_context(&user_id)));
        return;
    }
    if let Ok((cfg, _)) = CliConfig::load_default() {
        state::set_active_context(Some(state::selfhosted_context(&cfg.server.url)));
    }
}

/// Pista para el usuario cuando un cliente se queda sin servicio y el token de
/// disco ya no vale: el arreglo no es volver a entrar, es levantar el servicio
/// (que es quien renueva). `None` cuando el token sigue siendo usable.
pub fn stale_token_hint(access: &str, now_unix: i64) -> Option<&'static str> {
    let expired = match cloud_auth::jwt_expiry(access) {
        Some(exp) => exp <= now_unix,
        // Ilegible: no afirmamos nada. Si de verdad no vale, el 401 lo dirá.
        None => false,
    };
    expired.then_some(
        "the Cloud session token has expired and the Hoard service —the only thing that \
         renews it— isn't running. Start it with `hoard sync start`.",
    )
}

// ---- el servicio: prestar el token ------------------------------------

/// Por qué no se pudo prestar un token.
#[derive(Debug, thiserror::Error)]
pub enum LendError {
    /// No hay nada que prestar y rotar no lo arreglaría: sin sesión en disco, o
    /// GoTrue revocó la familia. Sólo un login nuevo.
    #[error("{0}")]
    Gone(String),
    /// Bache de red / GoTrue de mal humor: el token sigue vivo, reintentar tiene
    /// sentido. **No** debe hacer que un cliente cierre sesión.
    #[error(transparent)]
    Transient(anyhow::Error),
}

/// Presta un token Cloud válido, rotando **sólo si hace falta**. Responde a
/// `Request::CloudToken`; **sólo el servicio la llama**, que es lo que hace de
/// ella el único rotador.
///
/// `rejected` es el token que al cliente le devolvió un 401. Ver
/// [`needs_rotation`] para la decisión, que es pura y está testeada.
pub async fn lend_token(rejected: Option<&str>) -> Result<CloudToken, LendError> {
    let session = cloud_auth::load_session()
        .map_err(LendError::Transient)?
        .ok_or_else(|| LendError::Gone("no Cloud session on this machine".to_string()))?;

    let now = now_unix();
    let ttl = cloud_auth::jwt_expiry(&session.access).map(|exp| exp - now);
    if !needs_rotation(ttl, &session.access, rejected) {
        return Ok(CloudToken {
            expires_at: ttl.map(|t| now + t),
            access_token: session.access,
            server_url: session.server_url,
            rotated: false,
        });
    }

    match cloud_auth::refresh_freshest().await {
        Ok(tokens) => Ok(CloudToken {
            expires_at: cloud_auth::jwt_expiry(&tokens.access),
            access_token: tokens.access,
            server_url: session.server_url,
            rotated: true,
        }),
        Err(err) if cloud_auth::is_session_expired(&err) => {
            Err(LendError::Gone(format!("{err:#}")))
        }
        Err(err) => Err(LendError::Transient(err)),
    }
}

/// ¿Hay que rotar antes de prestar? Pura, para que la política sea un test y no
/// un comentario.
///
/// - Un token que el cliente ya comió con un 401 **no se le devuelve**: sería un
///   bucle de reintentos con el mismo token muerto. Si el que tenemos ya es otro,
///   alguien rotó por nosotros y ese sirve.
/// - Por debajo de [`LEND_MIN_TTL`] se rota: prestar un token que caduca en
///   segundos es prestar un 401.
/// - Caducidad ilegible ⇒ rotar. No sabemos qué le queda, y la ventana de reuso
///   de `refresh_freshest` colapsa la ráfaga si varios preguntan a la vez.
fn needs_rotation(ttl_secs: Option<i64>, stored: &str, rejected: Option<&str>) -> bool {
    if rejected.is_some_and(|r| r == stored) {
        return true;
    }
    match ttl_secs {
        Some(ttl) => ttl < LEND_MIN_TTL,
        None => true,
    }
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

// ---- el servicio: refresher de fondo ----------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Sesión viva: renovar en la cadencia normal.
    Normal,
    /// GoTrue revocó la familia de tokens. No hay nada que renovar hasta que
    /// alguien vuelva a entrar, así que sólo se vigila disco.
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Renewed,
    /// Bache de red/GoTrue — el token sigue bueno, reintentar luego.
    Transient,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Announce {
    Nothing,
    Expired,
    Restored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    phase: Phase,
    sleep: Duration,
    announce: Announce,
}

/// La cadencia del refresher, como función pura: "dilo una vez y luego calla" es
/// comprobable sin sesión Cloud y sin esperar 45 minutos.
fn next_step(phase: Phase, outcome: Outcome) -> Step {
    match (phase, outcome) {
        (Phase::Normal, Outcome::Expired) => Step {
            phase: Phase::Expired,
            sleep: RELOGIN_RECHECK_EVERY,
            announce: Announce::Expired,
        },
        (Phase::Expired, Outcome::Renewed) => Step {
            phase: Phase::Normal,
            sleep: REFRESH_EVERY,
            announce: Announce::Restored,
        },
        (Phase::Expired, _) => Step {
            phase: Phase::Expired,
            sleep: RELOGIN_RECHECK_EVERY,
            announce: Announce::Nothing,
        },
        (Phase::Normal, _) => Step {
            phase: Phase::Normal,
            sleep: REFRESH_EVERY,
            announce: Announce::Nothing,
        },
    }
}

/// Mete el par renovado en el cliente vivo y en nuestra copia.
fn adopt(client: &ApiClient, sess: &mut cloud_auth::Session, tokens: cloud_auth::Tokens) {
    client.set_token(&tokens.access);
    sess.access = tokens.access;
    sess.refresh = tokens.refresh;
    // El enviador de logs no puede pedir nada por IPC y no ve `cloud.toml`: se
    // le deja puesto el token recién rotado, o se quedaría enviando con el
    // viejo hasta comerse un 401.
    lend_to_logship(&sess.server_url, &sess.access);
}

/// Deja la sesión Cloud en el hueco que lee `logship`. La llama **el dueño** (el
/// servicio) en cuanto tiene un JWT válido: al arrancar y en cada rotación.
fn lend_to_logship(url: &str, token: &str) {
    credentials::set_lent_cloud(Some(credentials::CloudLease {
        url: url.to_string(),
        token: token.to_string(),
    }));
}

/// Una sesión en disco cuyo refresh token no es el muerto — o sea, el usuario
/// volvió a entrar (aquí o en el desktop: comparten fichero de sesión).
fn relogin_tokens(dead: Option<&str>) -> Option<cloud_auth::Tokens> {
    let s = cloud_auth::load_session().ok().flatten()?;
    if s.refresh.trim().is_empty() || Some(s.refresh.as_str()) == dead {
        return None;
    }
    Some(cloud_auth::Tokens {
        access: s.access,
        refresh: s.refresh,
    })
}

/// Bucle que renueva el JWT antes de que expire y lo empuja al `ApiClient` vivo.
/// Sin esto el motor empieza a devolver 401 una hora después de arrancar.
///
/// La sesión va en un `Arc<Mutex<…>>` para que el bucle pueda reiniciarse bajo
/// `supervise` sin perder los tokens ya rotados: reiniciar tras un pánico y
/// volver al par de disco reintroduciría la reuse-detection que este módulo
/// existe para matar.
///
/// **Sólo el servicio.** Es la otra mitad del único rotador.
pub async fn refresh_loop(
    client: ApiClient,
    session: Arc<tokio::sync::Mutex<cloud_auth::Session>>,
) -> Finished {
    let mut phase = Phase::Normal;
    let mut sleep_for = REFRESH_EVERY;
    // El refresh token que GoTrue dio por muerto, para distinguir un login nuevo
    // de la misma sesión muerta seguir en disco.
    let mut dead: Option<String> = None;

    loop {
        tokio::time::sleep(sleep_for).await;

        let outcome = match phase {
            Phase::Normal => match cloud_auth::refresh_freshest().await {
                Ok(tokens) => {
                    adopt(&client, &mut *session.lock().await, tokens);
                    Outcome::Renewed
                }
                Err(err) if cloud_auth::is_session_expired(&err) => {
                    let ours = session.lock().await.refresh.clone();
                    dead = cloud_auth::load_session()
                        .ok()
                        .flatten()
                        .map(|s| s.refresh)
                        .or(Some(ours));
                    Outcome::Expired
                }
                Err(err) => {
                    tracing::warn!(error = %err, "session: periodic Cloud refresh failed");
                    Outcome::Transient
                }
            },
            // Sin red a propósito: repetir un token revocado cada pocos minutos
            // es lo que llenaba el log del sistema con el mismo WARN durante
            // días. Sólo un login nuevo ayuda, así que sólo se vigila disco.
            Phase::Expired => match relogin_tokens(dead.as_deref()) {
                Some(tokens) => {
                    dead = None;
                    adopt(&client, &mut *session.lock().await, tokens);
                    Outcome::Renewed
                }
                None => Outcome::Expired,
            },
        };

        let step = next_step(phase, outcome);
        match step.announce {
            Announce::Expired => {
                tracing::error!("session: the Cloud session expired — run `hoard login`")
            }
            Announce::Restored => tracing::info!("session: the Cloud session is back"),
            Announce::Nothing => {}
        }
        phase = step.phase;
        sleep_for = step.sleep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(url: &str, token: &str) -> Credentials {
        Credentials {
            url: url.to_string(),
            token: token.to_string(),
            user: None,
        }
    }

    /// **El test del bug de la 1.1.0.** El motor resolvía la sesión self-hosted
    /// mirando sólo `config.toml`, que escribe únicamente `hoard login --token`,
    /// mientras la app guardaba la suya en `credentials`. Quien entraba a su
    /// servidor sólo por la app tenía un motor sin sesión, cero backups, y una
    /// ventana que decía "el servicio está desconectado" sin más.
    ///
    /// Que el almacén gane no es una preferencia: es la sesión que el usuario ve
    /// en la app y la que tocan todos los logins nuevos.
    #[test]
    fn the_session_store_beats_config_toml() {
        let picked = pick_selfhosted(
            Some(creds("https://saves.example", "hoard_v1_de-la-app")),
            Some(creds("http://localhost:12421", "hoard_v1_del-config")),
        )
        .expect("hay sesión");
        assert_eq!(picked.token, "hoard_v1_de-la-app");
        assert_eq!(picked.url, "https://saves.example");
    }

    /// Y `config.toml` sigue siendo el camino headless: sin almacén (una máquina
    /// donde sólo se ha usado la CLI, que es lo que documenta la guía de
    /// self-hosting) manda él. Arreglar el bug no podía romper esto.
    #[test]
    fn config_toml_still_serves_the_headless_path() {
        let picked = pick_selfhosted(None, Some(creds("http://nas.local:12421", "hoard_v1_cli")))
            .expect("hay sesión");
        assert_eq!(picked.token, "hoard_v1_cli");
    }

    /// Sin ninguna de las dos, el motivo va **tipado**: es lo que el daemon
    /// clasifica para que la ventana diga "no hay sesión, vuelve a entrar" en vez
    /// del banner genérico.
    #[test]
    fn no_session_anywhere_is_typed() {
        let err = pick_selfhosted(None, None).expect_err("no hay sesión");
        assert!(err.downcast_ref::<NoSession>().is_some(), "{err:#}");
        assert!(format!("{err:#}").contains("no session"), "{err:#}");
    }

    #[test]
    fn announces_the_death_once_and_then_stays_quiet() {
        let died = next_step(Phase::Normal, Outcome::Expired);
        assert_eq!(died.phase, Phase::Expired);
        assert_eq!(died.announce, Announce::Expired);
        assert_eq!(died.sleep, RELOGIN_RECHECK_EVERY);

        // Cada comprobación posterior sin login pendiente: mismo estado, callado.
        let again = next_step(died.phase, Outcome::Expired);
        assert_eq!(again.phase, Phase::Expired);
        assert_eq!(again.announce, Announce::Nothing);
        assert_eq!(again.sleep, RELOGIN_RECHECK_EVERY);
    }

    #[test]
    fn a_relogin_restores_the_normal_cadence() {
        let back = next_step(Phase::Expired, Outcome::Renewed);
        assert_eq!(back.phase, Phase::Normal);
        assert_eq!(back.announce, Announce::Restored);
        assert_eq!(back.sleep, REFRESH_EVERY);
    }

    #[test]
    fn a_transient_failure_neither_announces_nor_changes_phase() {
        let step = next_step(Phase::Normal, Outcome::Transient);
        assert_eq!(step.phase, Phase::Normal);
        assert_eq!(step.announce, Announce::Nothing);
        assert_eq!(step.sleep, REFRESH_EVERY);
    }

    #[test]
    fn a_transient_failure_while_expired_keeps_waiting_for_a_login() {
        let step = next_step(Phase::Expired, Outcome::Transient);
        assert_eq!(step.phase, Phase::Expired);
        assert_eq!(step.announce, Announce::Nothing);
    }

    #[test]
    fn the_happy_path_holds_the_normal_cadence() {
        let step = next_step(Phase::Normal, Outcome::Renewed);
        assert_eq!(step.phase, Phase::Normal);
        assert_eq!(step.announce, Announce::Nothing);
        assert_eq!(step.sleep, REFRESH_EVERY);
    }

    /// Un token con vida de sobra se presta tal cual: prestar no es rotar, y
    /// rotar de más gasta el refresh token (cada rotación revoca el anterior).
    #[test]
    fn a_healthy_token_is_lent_without_rotating() {
        assert!(!needs_rotation(Some(LEND_MIN_TTL + 1), "tok", None));
        assert!(!needs_rotation(Some(3600), "tok", None));
    }

    /// El margen es lo que impide prestar un 401: por debajo, se rota.
    #[test]
    fn a_token_about_to_die_is_rotated_first() {
        assert!(needs_rotation(Some(LEND_MIN_TTL - 1), "tok", None));
        assert!(needs_rotation(Some(0), "tok", None));
        assert!(needs_rotation(Some(-10), "tok", None));
    }

    /// El caso que el `rejected` existe para cerrar: el cliente comió un 401 con
    /// un token que **todavía no ha caducado** (revocado server-side, reloj
    /// desfasado). Sin esto recibiría el mismo token y reintentaría en bucle.
    #[test]
    fn a_rejected_token_is_never_handed_back() {
        assert!(needs_rotation(Some(3600), "tok", Some("tok")));
    }

    /// Pero si el que tenemos ya no es el rechazado, alguien rotó por nosotros:
    /// servirlo es gratis y ahorra una rotación.
    #[test]
    fn a_rejection_of_someone_elses_token_doesnt_force_a_rotation() {
        assert!(!needs_rotation(Some(3600), "newer", Some("older")));
    }

    /// Caducidad ilegible: no fingimos saber. Rotar es la dirección segura.
    #[test]
    fn an_unreadable_expiry_rotates() {
        assert!(needs_rotation(None, "tok", None));
    }

    /// La pista sólo aparece cuando el token está de verdad caducado; con uno
    /// vivo (o ilegible) callamos, para no mandar al usuario a arreglar algo que
    /// no está roto.
    #[test]
    fn the_stale_hint_only_fires_on_an_expired_token() {
        use base64::Engine;
        let jwt = |exp: i64| {
            let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
            format!("h.{body}.s")
        };
        assert!(stale_token_hint(&jwt(1_000), 2_000).is_some());
        assert!(stale_token_hint(&jwt(3_000), 2_000).is_none());
        assert!(stale_token_hint("opaque", 2_000).is_none());
    }
}
