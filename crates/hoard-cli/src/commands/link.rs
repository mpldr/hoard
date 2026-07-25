//! El enlace de la CLI con `hoardd` (ADR 0021, Parte A — Slice 4c).
//!
//! Hasta el 4b la CLI **embebía** el motor: `hoard sync` hacía `agent::spawn`,
//! tomaba el pidfile y rotaba el refresh token de Cloud por su cuenta. Desde este
//! slice no hay motor aquí: la CLI manda comandos al servicio por el socket local
//! e imprime lo que el servicio reporta, igual que el desktop pinta lo mismo en
//! una ventana. Un frontend con ventana y otro sin ella, ahora también en la
//! topología de procesos.
//!
//! ## Quién arranca el servicio, y quién no
//!
//! [`ensure`] es "conéctate; si no hay servicio, arráncalo" — el handshake
//! idempotente de la ADR. Lo usa **sólo** [`super::daemon::run`] (`hoard sync
//! run`), porque ése es el comando cuyo trabajo *es* que el sync esté corriendo.
//!
//! Todo lo demás usa [`attached`], que conecta pero **no arranca nada**. Un
//! `hoard whoami` o un `hoard save pause` no pueden convertir la máquina en una
//! máquina que sincroniza como efecto secundario; el modo explícito de pedir eso
//! es `hoard sync start`. La contrapartida es que sin servicio no hay quien rote
//! el token Cloud, y eso se degrada, no se rompe: ver [`resolve_session`].

use std::time::Duration;

use anyhow::{Context, Result};
use hoard_agent::session::{self, Active};
use hoard_core::ipc::{CloudToken, DaemonStatus, IpcError, Payload, Request};
use hoardd::client::Client;
use hoardd::endpoint::Endpoint;

/// Cómo nos presentamos en el log del daemon.
fn client_name(role: &str) -> String {
    format!("hoard {} ({role})", env!("CARGO_PKG_VERSION"))
}

/// Tope de una petición ya conectada. Un servicio que acepta y luego calla no
/// puede colgar un comando de terminal para siempre.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Tope para *conectar* cuando el comando sólo quiere mirar (status, banner). Es
/// un socket local: si no contesta en esto, no hay nadie.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

fn endpoint() -> Result<Endpoint> {
    Endpoint::resolve().context("resolving the hoardd endpoint")
}

/// Conéctate al servicio y, si no hay, arráncalo. Para `hoard sync run`.
pub async fn ensure(role: &str) -> Result<Client> {
    let endpoint = endpoint()?;
    Client::ensure_running(&endpoint, &client_name(role))
        .await
        .with_context(|| format!("connecting to the Hoard service at {endpoint}"))
}

/// Conéctate al servicio **si ya está arriba**. `None` = no hay servicio (o no
/// contesta), y eso no es un error: es la respuesta.
pub async fn attached(role: &str) -> Option<Client> {
    let endpoint = endpoint().ok()?;
    let name = client_name(role);
    match tokio::time::timeout(PROBE_TIMEOUT, Client::connect(&endpoint, &name)).await {
        Ok(Ok(client)) => Some(client),
        Ok(Err(err)) => {
            tracing::debug!(error = %format!("{err:#}"), "cli: no Hoard service listening");
            None
        }
        Err(_) => {
            tracing::warn!("cli: the Hoard service accepted nothing within {PROBE_TIMEOUT:?}");
            None
        }
    }
}

/// Una petición con tope sobre una conexión ya hecha.
pub async fn ask(client: &mut Client, request: Request) -> Result<Payload> {
    tokio::time::timeout(REQUEST_TIMEOUT, client.request(request))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "the Hoard service didn't answer in {}s",
                REQUEST_TIMEOUT.as_secs()
            )
        })?
}

/// Estado del servicio, o `None` si no hay ninguno. Para pintar (`hoard`,
/// `hoard sync`): no arranca nada.
pub async fn status() -> Option<DaemonStatus> {
    let mut client = attached("status").await?;
    match ask(&mut client, Request::Status).await {
        Ok(Payload::Status(status)) => Some(status),
        Ok(other) => {
            tracing::warn!("cli: unexpected answer to status: {other:?}");
            None
        }
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "cli: couldn't read the service status");
            None
        }
    }
}

/// Aviso best-effort al servicio, **sin arrancarlo**: si no hay servicio no hay
/// nada que avisar (cuando arranque leerá el disco de cero). Devuelve `true` si
/// llegó.
async fn notify(what: &str, request: Request) -> bool {
    let Some(mut client) = attached("notify").await else {
        return false;
    };
    match ask(&mut client, request).await {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "cli: couldn't {what}");
            false
        }
    }
}

/// El conjunto de saves vigilados cambió en disco: que el servicio lo relea. El
/// cliente **avisa**, no manda la lista — el dueño del estado es el servicio.
///
/// Devuelve la frase que la CLI le enseña al usuario: antes de este slice todos
/// estos comandos decían "reinicia `hoard sync` para aplicarlo", que ya no hace
/// falta (ni funcionaría: reiniciar un cliente no reinicia el motor).
pub async fn notify_reload() -> &'static str {
    if notify("ask the service to reload its watch list", Request::Reload).await {
        "the sync service picked it up"
    } else {
        "it applies when the sync service starts"
    }
}

/// La sesión en disco cambió (login/logout): que el servicio resuelva de cero.
/// Un cambio de cuenta invalida su `ApiClient`, su contexto y su rotador de
/// token, y ninguno de los tres se arregla releyendo los saves.
pub async fn notify_session_changed() {
    notify(
        "tell the service the session changed",
        Request::RestartEngine,
    )
    .await;
}

/// Pide prestado un token Cloud al servicio. `None` cuando no hay servicio a
/// quien pedírselo.
///
/// Un `CloudSessionExpired` **no** se traga: si GoTrue revocó la familia de
/// tokens, seguir con el de disco sólo produce un 401 con peor mensaje.
pub async fn borrow_cloud_token(rejected: Option<String>) -> Result<Option<CloudToken>> {
    let Some(mut client) = attached("cloud token").await else {
        return Ok(None);
    };
    match ask(&mut client, Request::CloudToken { rejected }).await {
        Ok(Payload::CloudToken(token)) => Ok(Some(token)),
        Ok(other) => anyhow::bail!("unexpected answer to cloud_token: {other:?}"),
        Err(err) => {
            if matches!(
                err.downcast_ref::<IpcError>(),
                Some(IpcError::CloudSessionExpired { .. })
            ) {
                return Err(err.context("the Hoard service couldn't renew the Cloud session"));
            }
            // Transitorio (red, GoTrue de mal humor): el token de disco puede
            // seguir valiendo, así que se intenta con él en vez de abortar.
            tracing::warn!(error = %format!("{err:#}"), "cli: couldn't borrow a Cloud token");
            Ok(None)
        }
    }
}

/// Sesión activa para un one-shot que necesita hablar con el servidor.
///
/// Pide el token al servicio y resuelve con él. Si no hay servicio se usa el de
/// disco **sin rotarlo**: rotar aquí es exactamente lo que este slice elimina
/// (dos procesos rotando el mismo refresh token = reuse-detection de GoTrue =
/// sesión revocada). Un token caducado y sin servicio da un 401, y ahí se enseña
/// la pista de que quien renueva es el servicio.
pub async fn resolve_session() -> Result<Active> {
    // Sin sesión Cloud no hay nada que pedir prestado: esto es un usuario
    // self-hosted, y el servicio contestaría "no hay sesión Cloud" — un error
    // inventado sobre algo que a este comando no le hace falta.
    if hoard_agent::cloud_auth::load_session()?.is_none() {
        return session::resolve_borrowed(None).await;
    }
    let lent = borrow_cloud_token(None).await?;
    let borrowed = lent.is_some();
    match session::resolve_borrowed(lent).await {
        Ok(active) => Ok(active),
        Err(err) if !borrowed => Err(hint_stale_session(err)),
        Err(err) => Err(err),
    }
}

/// Añade al error la pista de que el arreglo es levantar el servicio, no volver a
/// entrar — pero sólo si el token de disco está de verdad caducado.
fn hint_stale_session(err: anyhow::Error) -> anyhow::Error {
    let Ok(Some(sess)) = hoard_agent::cloud_auth::load_session() else {
        return err;
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match session::stale_token_hint(&sess.access, now) {
        Some(hint) => err.context(hint),
        None => err,
    }
}
