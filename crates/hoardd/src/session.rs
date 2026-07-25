//! La sesión del daemon: qué servidor, con qué token, y quién lo renueva.
//!
//! **El daemon es el único rotador.** Que dos procesos pudieran rotar el mismo
//! refresh token de `cloud.toml` es la causa raíz de una familia entera de bugs
//! cloud (401 por reuse-detection, realtime enmudecido); el pidfile lo evitaba
//! por exclusión, no por diseño. Con el motor aquí, el refresh vive aquí y punto
//! (ADR 0021, Parte A).
//!
//! Este módulo es el port de `hoard-cli/src/commands/session.rs` para un host
//! **no interactivo**: misma resolución (Cloud si hay sesión, si no self-hosted
//! por token de config), misma máquina de fases del refresher, pero los avisos
//! van a `tracing` en vez de a stdout, que en un servicio de usuario es
//! `/dev/null`. La copia de la CLI muere en el Slice 4c, cuando `hoard sync`
//! pase a ser "asegura el daemon y engánchate": entonces esta pasa a ser la
//! única.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hoard_agent::api::ApiClient;
use hoard_agent::cloud_auth;
use hoard_agent::config::CliConfig;
use hoard_agent::state;
use hoard_agent::supervisor::Finished;

/// Cadencia normal: renovar el JWT (~1 h de vida) con margen.
const REFRESH_EVERY: Duration = Duration::from_secs(45 * 60);

/// Cadencia con la sesión muerta. Sólo relee disco esperando un login nuevo, así
/// que comprobar a menudo no cuesta nada — y **sin red**: repetir un token
/// revocado cada pocos minutos es lo que llenaba el journal del sistema con el
/// mismo WARN durante días.
const RELOGIN_RECHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// Cuánto insiste el arranque con un fallo *transitorio* del refresh inicial
/// antes de arrancar con el token guardado.
const BOOT_REFRESH_GRACE: Duration = Duration::from_secs(60);

/// Sesión activa resuelta.
pub struct Active {
    pub client: ApiClient,
    pub is_cloud: bool,
    /// Descripción legible del destino, para logs y para el `Status` del IPC.
    pub server: String,
    /// Sesión Cloud subyacente, si la hay: el daemon la usa para renovar el JWT.
    pub cloud: Option<cloud_auth::Session>,
}

/// Cloud gana si hay sesión; si no, self-hosted. Fija el **contexto de sync**
/// (`state::set_active_context`) antes de que nadie lea `state.json`: sin eso el
/// daemon cargaría el mapa de saves de otra cuenta.
///
/// Un fallo transitorio del refresh inicial reintenta durante
/// [`BOOT_REFRESH_GRACE`] y luego arranca con lo que haya en disco. El servicio
/// arranca en el boot, rutinariamente antes de que el DNS conteste; morir ahí
/// deja la máquina sin sincronizar nada hasta el siguiente intento, y el
/// refresher repara el token en cuanto hay red. Una sesión terminalmente
/// expirada sí es fatal: sólo un login nuevo la arregla y esperar no ayuda.
pub async fn resolve() -> Result<Active> {
    if let Some(sess) = cloud_auth::load_session()? {
        return resolve_cloud(sess).await;
    }

    let (cfg, _) = CliConfig::load_default()?;
    let token = cfg
        .require_token()
        .context(
            "no session. Sign in with `hoard login` (Cloud) or \
             `hoard login --token <token>` (self-host).",
        )?
        .to_string();
    state::set_active_context(Some(state::selfhosted_context(&cfg.server.url)));
    let client = ApiClient::new(cfg.server.url.clone(), token)?;
    Ok(Active {
        client,
        is_cloud: false,
        server: cfg.server.url,
        cloud: None,
    })
}

async fn resolve_cloud(sess: cloud_auth::Session) -> Result<Active> {
    let refreshed = initial_refresh().await?;
    let degraded = refreshed.is_none();
    let (access, refresh) = match refreshed {
        Some(t) => (t.access, t.refresh),
        None => (sess.access.clone(), sess.refresh.clone()),
    };

    let client = ApiClient::new(sess.server_url.clone(), access.clone())?;
    let cloud = Some(cloud_auth::Session {
        server_url: sess.server_url.clone(),
        access: access.clone(),
        refresh,
    });

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
            tracing::warn!(error = %err, "hoardd: Cloud unreachable; starting on the stored session");
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
            Err(err) if is_session_expired(&err) => return Err(err),
            Err(err) => {
                if Instant::now() >= deadline {
                    tracing::warn!(error = %err, "hoardd: couldn't renew the Cloud session at boot");
                    return Ok(None);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

/// Un fallo de refresh que sólo arregla un `hoard login` nuevo, frente a un
/// bache de red que merece reintento.
fn is_session_expired(err: &anyhow::Error) -> bool {
    err.downcast_ref::<cloud_auth::RefreshTokenStale>()
        .is_some()
}

// ---- refresher de fondo -----------------------------------------------

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
                Err(err) if is_session_expired(&err) => {
                    let ours = session.lock().await.refresh.clone();
                    dead = cloud_auth::load_session()
                        .ok()
                        .flatten()
                        .map(|s| s.refresh)
                        .or(Some(ours));
                    Outcome::Expired
                }
                Err(err) => {
                    tracing::warn!(error = %err, "hoardd: periodic Cloud refresh failed");
                    Outcome::Transient
                }
            },
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
                tracing::error!("hoardd: the Cloud session expired — run `hoard login`")
            }
            Announce::Restored => tracing::info!("hoardd: the Cloud session is back"),
            Announce::Nothing => {}
        }
        phase = step.phase;
        sleep_for = step.sleep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_the_death_once_and_then_stays_quiet() {
        let died = next_step(Phase::Normal, Outcome::Expired);
        assert_eq!(died.phase, Phase::Expired);
        assert_eq!(died.announce, Announce::Expired);
        assert_eq!(died.sleep, RELOGIN_RECHECK_EVERY);

        let again = next_step(died.phase, Outcome::Expired);
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
}
