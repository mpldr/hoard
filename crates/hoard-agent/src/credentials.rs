//! Persistent storage for the desktop client's session.
//!
//! Two pieces are kept on disk:
//!
//! * The bearer token, which is sensitive and should live in the OS keychain
//!   when one is available (Secret Service on Linux, Credential Manager on
//!   Windows, Keychain on macOS — all surfaced by the `keyring` crate).
//! * The server URL and a cached copy of the last-seen user info, which are
//!   not sensitive and live in a TOML file at `<config>/desktop/session.toml`
//!   so we can show the username without hitting the network on startup. These
//!   are also mirrored into the keychain blob, so a lost or unreadable cache
//!   file no longer signs the user out.
//!
//! When the OS keychain is unavailable (e.g. headless Linux without
//! libsecret) the token falls back into the same TOML file, which is created
//! with `0600` permissions on Unix.
//!
//! The desktop app uses a separate file from `hoard-cli`'s `config.toml` so
//! that running the CLI does not stomp the GUI's session and vice versa.
//!
//! ## Quién escribe, quién lee (D.20)
//!
//! **El dueño es el daemon**, igual que en `cloud_auth`: [`save`] y [`clear`]
//! tocan el llavero, y en macOS un ítem del llavero sólo autoriza al binario que
//! lo creó — con la app escribiendo y `hoardd` leyendo, cada lectura del servicio
//! era un diálogo de contraseña. Un cliente que acaba de validar un token lo
//! **entrega** (`Request::AdoptServerSession`) y lo pide prestado cuando lo
//! necesita (`Request::ServerToken`).
//!
//! Un cliente, por tanto, **no llama a [`load`]**: usa [`current`], que devuelve
//! el préstamo que le hayan puesto en el hueco ([`set_lent`]) y sólo cae al
//! almacén cuando nadie lo ha rellenado — que es el caso del daemon, el dueño.
//! Para lo que no es secreto (URL y usuario) está [`load_public`], que lee el
//! fichero y no toca el llavero.
//!
//! Sin servicio a quien entregar existen [`save_unlocked`] y
//! [`forget_unlocked`]: fichero 0600 y nunca el llavero.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::api::Whoami;
use crate::config::CliConfig;
use crate::keychain::{keyring_op, KeyringTimeout, KeyringUnreadable, KEYRING_TIMEOUT};

const KEYRING_SERVICE: &str = "hoard-desktop";
const KEYRING_USER: &str = "default";

/// In-memory view of the desktop client's saved session.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub url: String,
    pub token: String,
    pub user: Option<UserSection>,
}

/// Where the token actually ended up after `save`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStorage {
    /// Stored via the OS secret service (preferred).
    Keyring,
    /// Stored in the TOML file at 0600 because the keyring was unavailable.
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Session {
    #[serde(default)]
    server: ServerSection,
    #[serde(default)]
    user: Option<UserSection>,
    /// Filesystem fallback when the OS keyring is unavailable. In normal
    /// operation this is `None` and the token lives in the keyring.
    #[serde(default)]
    auth: Option<AuthSection>,
    /// El usuario cerró sesión y **nadie ha podido borrar el ítem del llavero**
    /// (un cliente sin servicio al alcance: borrarlo es del dueño).
    ///
    /// Es load-bearing que exista. [`load`] recupera la sesión del blob del
    /// llavero cuando el fichero se ha perdido —el arreglo de la ACL que un build
    /// viejo de Windows dejaba clavada— y eso, sin esta marca, resucitaría la
    /// sesión que el usuario acaba de cerrar. Un fichero borrado y un fichero
    /// ilegible se parecen demasiado para distinguirlos por su ausencia, así que
    /// el logout deja dicho lo que hizo. [`save`] escribe un `Session` nuevo, así
    /// que el siguiente login la limpia sin acordarse de ella.
    #[serde(default, skip_serializing_if = "is_false")]
    signed_out: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ServerSection {
    #[serde(default)]
    url: String,
}

/// Subset of `/v1/auth/whoami` we cache locally so the dashboard can show the
/// username without an extra round-trip on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSection {
    pub user_id: String,
    pub username: String,
    pub is_admin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthSection {
    token: String,
}

/// What we stash in the OS keychain. Historically this was the bare token
/// string; it's now a small TOML document so the keychain alone can restore a
/// session (token + server URL + cached user) even when the on-disk cache is
/// missing or unreadable. Reads tolerate the legacy bare-token form.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyringBlob {
    #[serde(default)]
    token: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    user: Option<UserSection>,
}

impl From<Whoami> for UserSection {
    fn from(w: Whoami) -> Self {
        Self {
            user_id: w.user_id,
            username: w.username.into_inner(),
            is_admin: w.is_admin,
        }
    }
}

/// Resolve the on-disk path of the session metadata file.
pub fn session_path() -> Result<PathBuf> {
    let dirs = CliConfig::project_dirs()?;
    Ok(dirs.config_dir().join("desktop").join("session.toml"))
}

/// Persist credentials. Token goes to the OS keychain when available, with a
/// transparent file fallback otherwise.
/// **Sólo el daemon.** Es la escritura que crea el ítem del llavero, y en macOS
/// su ACL autoriza únicamente al binario que lo crea: si lo escribiera un cliente,
/// cada lectura del servicio le pediría la contraseña al usuario (D.20). Un
/// cliente entrega la sesión por IPC (`Request::AdoptServerSession`), o usa
/// [`save_unlocked`] si no hay servicio a quien entregarla.
pub fn save(creds: &Credentials) -> Result<TokenStorage> {
    let session = Session {
        server: ServerSection {
            url: creds.url.clone(),
        },
        user: creds.user.clone(),
        auth: None,
        signed_out: false,
    };
    write_session(&session)?;

    match try_keyring_set(creds) {
        Ok(()) => {
            // Belt and braces: if the file had a stale token from a previous
            // fallback run, scrub it now that the keyring took over.
            scrub_file_token().ok();
            Ok(TokenStorage::Keyring)
        }
        Err(_) => {
            let mut session = read_session()?.unwrap_or_default();
            session.auth = Some(AuthSection {
                token: creds.token.clone(),
            });
            write_session(&session)?;
            Ok(TokenStorage::File)
        }
    }
}

/// Load credentials if any are stored. Returns `Ok(None)` when no session is
/// present yet (e.g. fresh install) — that is not an error. Un llavero
/// bloqueado, en cambio, **sí** lo es: ver [`pick_token`].
pub fn load() -> Result<Option<Credentials>> {
    Ok(load_detailed()?.map(|(creds, _)| creds))
}

/// Como [`load`], pero diciendo **de dónde** salió el token.
///
/// Lo necesita el daemon: un token que venía del fichero significa que el ítem del
/// llavero no existe o no es suyo, y entonces le toca subirlo él
/// ([`promote_to_keyring`]) para ser el dueño de la ACL. Sin esa distinción la
/// promoción tendría que reescribir el llavero en cada arranque —una escritura
/// inútil por arranque, y en macOS otro diálogo— o no hacerse nunca, que es lo que
/// deja al usuario de macOS con un ítem que su servicio no puede leer.
pub fn load_detailed() -> Result<Option<(Credentials, TokenStorage)>> {
    // Un logout que no pudo borrar el ítem del llavero (cliente sin servicio) lo
    // deja dicho aquí. Se comprueba **antes** que nada: la recuperación de más
    // abajo resucitaría la sesión desde el blob huérfano.
    if matches!(read_session(), Ok(Some(s)) if s.signed_out) {
        return Ok(None);
    }
    match read_session() {
        // Normal path: the on-disk cache is readable and has a server URL. The
        // token comes from the keychain, falling back to the file copy.
        Ok(Some(session)) if !session.server.url.is_empty() => {
            let from_file = session.auth.as_ref().map(|a| a.token.clone());
            match pick_token(try_keyring_get(), from_file)? {
                Some((token, storage)) => Ok(Some((
                    Credentials {
                        url: session.server.url,
                        token,
                        user: session.user,
                    },
                    storage,
                ))),
                None => Ok(None),
            }
        }
        // Cache absent, empty, or unreadable (e.g. an ACL a previous Windows
        // build clamped down and `read_session` couldn't repair). Don't drop
        // the session over a disk hiccup: the keychain now carries the URL too,
        // so it can restore everything on its own.
        //
        // Aquí un fallo del llavero **sí** se traga, al revés que arriba: sin
        // fichero de sesión nadie ha entrado nunca en esta máquina (`save` lo
        // escribe siempre, incluso cuando el llavero no está), así que su
        // ausencia es la respuesta y `Ok(None)` manda al usuario al asistente —
        // que es lo que quiere en una primera ejecución, con llavero bloqueado o
        // sin él. Un fichero ilegible sigue devolviendo su error de lectura.
        read => {
            if let Ok(Some(blob)) = try_keyring_get() {
                if !blob.token.is_empty() && !blob.url.is_empty() {
                    let creds = Credentials {
                        url: blob.url,
                        token: blob.token,
                        user: blob.user,
                    };
                    // Best-effort: rewrite the cache so it's healthy again, with
                    // sane inherited permissions.
                    let _ = write_session(&Session {
                        server: ServerSection {
                            url: creds.url.clone(),
                        },
                        user: creds.user.clone(),
                        auth: None,
                        signed_out: false,
                    });
                    return Ok(Some((creds, TokenStorage::Keyring)));
                }
            }
            // Nothing recoverable from the keychain. Surface a real read error;
            // treat "absent/empty" as simply not-logged-in.
            match read {
                Err(e) => Err(e),
                _ => Ok(None),
            }
        }
    }
}

/// Qué token vale: el del llavero cuando contesta, el del fichero 0600 cuando el
/// llavero falla por algo **reparable** (bloqueado, sin D-Bus en una sesión
/// headless) — eso no es "no hay sesión".
///
/// Es el gemelo de `cloud_auth::pick_auth`, y por el mismo motivo: tragarse el
/// `Err` como si fuese `NoEntry` devolvía `Ok(None)` con el token intacto en el
/// llavero, o sea un usuario que aparece deslogueado sin una línea que lo
/// explique. Con el fichero de sesión delante (hay URL: aquí **sí** se entró
/// alguna vez) un llavero mudo y sin fallback en disco tiene que salir entero:
/// es la única pista de que está bloqueado.
fn pick_token(
    from_keyring: Result<Option<KeyringBlob>>,
    from_file: Option<String>,
) -> Result<Option<(String, TokenStorage)>> {
    let from_file = from_file.filter(|t| !t.is_empty());
    match from_keyring {
        Ok(Some(blob)) if !blob.token.is_empty() => Ok(Some((blob.token, TokenStorage::Keyring))),
        // Sin entrada (o con una vacía): no hay fallo que contar, cae al fichero.
        Ok(_) => Ok(from_file.map(|t| (t, TokenStorage::File))),
        Err(e) => match from_file {
            Some(token) => {
                tracing::debug!(error = %e, "keyring ilegible; usando el token del fichero");
                Ok(Some((token, TokenStorage::File)))
            }
            // Un tope agotado ya se explica solo; a cualquier otro fallo se le
            // añade el motivo tipado, que es lo que la UI mira para decir "vuelve
            // a entrar" en vez del banner genérico.
            None if e.is::<KeyringTimeout>() => Err(e),
            None => Err(e.context(KeyringUnreadable {
                doing: "reading the self-hosted session",
            })),
        },
    }
}

/// Sube al llavero una sesión que estaba sólo en el fichero, **como dueño**.
///
/// La llama el daemon después de arrancar con un token que venía del fichero
/// 0600: el que dejó ahí un cliente sin servicio ([`save_unlocked`]), o el que
/// quedó cuando el llavero estaba bloqueado. A partir de la siguiente lectura el
/// ítem es suyo, que es lo único que en macOS evita el diálogo de contraseña por
/// arranque (la ACL autoriza al binario que **crea** el ítem, D.20).
///
/// Best-effort de verdad: devuelve `false` y no toca nada más si el llavero no
/// está. Un servicio que se negara a sincronizar porque no pudo guardar el token
/// donde prefiere sería mucho peor que uno que sigue leyendo del fichero.
///
/// **No borra la copia del fichero**, a diferencia de [`save`]. Aquí el llavero
/// acaba de demostrar que no era legible o no existía, así que quitar el único
/// respaldo que funciona es exactamente la jugada que deja al usuario sin sync la
/// próxima vez que se bloquee. El fichero es 0600 y ya contenía ese token.
pub fn promote_to_keyring(creds: &Credentials) -> bool {
    match try_keyring_set(creds) {
        Ok(()) => {
            tracing::info!(
                "credentials: la sesión self-hosted pasa al llavero a nombre del servicio"
            );
            true
        }
        Err(err) => {
            tracing::debug!(error = %format!("{err:#}"), "credentials: el llavero no acepta la sesión; se queda en el fichero");
            false
        }
    }
}

/// Wipe stored credentials. Idempotent — clearing twice is fine.
///
/// **Del daemon**, por lo mismo que [`save`]: borrar un ítem del llavero también
/// se autoriza. Un cliente manda `Request::ForgetServerSession` y, si no hay
/// servicio, [`forget_unlocked`].
pub fn clear() -> Result<()> {
    // Best-effort: errors here mean the entry didn't exist, which is fine.
    let _ = try_keyring_delete();
    let path = session_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Persiste la sesión **sin tocar el llavero**: fichero 0600 y nada más.
///
/// El camino de un cliente que acaba de validar un token y no tiene servicio a
/// quien entregarlo. Escribir el llavero aquí sería el bug de D.20 —el ítem
/// quedaría a nombre del cliente y el servicio pediría permiso en cada lectura—,
/// mientras que dejándolo en el fichero el daemon lo lee tal cual al arrancar y lo
/// sube al llavero él mismo, ya como dueño.
pub fn save_unlocked(creds: &Credentials) -> Result<()> {
    write_session(&Session {
        server: ServerSection {
            url: creds.url.clone(),
        },
        user: creds.user.clone(),
        auth: Some(AuthSection {
            token: creds.token.clone(),
        }),
        signed_out: false,
    })
}

/// Cierra sesión **sin tocar el llavero**: deja la tumba (`signed_out`) en el
/// fichero.
///
/// No basta con borrar el fichero, y ésa es la diferencia con Cloud: [`load`]
/// recupera la sesión del blob del llavero cuando el fichero no está, así que
/// borrarlo la resucitaría. La marca dice "esto no es un fichero perdido, es un
/// logout", y el ítem huérfano que quede en el llavero no autoriza nada por su
/// cuenta — lo pisa el siguiente login, y hasta entonces nadie lo lee.
pub fn forget_unlocked() -> Result<()> {
    write_session(&Session {
        server: ServerSection::default(),
        user: None,
        auth: None,
        signed_out: true,
    })
}

/// Lo que **no** es secreto de la sesión: a qué server y quién. Sale del fichero,
/// sin tocar el llavero.
///
/// Es lo que un cliente puede leer por su cuenta, y con lo que le basta para
/// arrancar: el desktop pinta el usuario y la URL al abrir (síncrono, antes de que
/// exista el enlace con el servicio) y pide el token prestado cuando de verdad va
/// a llamar al server.
pub fn load_public() -> Result<Option<(String, Option<UserSection>)>> {
    match read_session()? {
        Some(s) if s.signed_out => Ok(None),
        Some(s) if !s.server.url.is_empty() => Ok(Some((s.server.url, s.user))),
        _ => Ok(None),
    }
}

/// El hueco del préstamo: la sesión que el servicio nos ha prestado.
///
/// Existe porque hay un lector que **no puede** pedirla por IPC: el enviador de
/// logs (`logship`), que corre en su propio hilo con su propio runtime y relee la
/// sesión cada pocos segundos. En el daemon el hueco está vacío y lee el almacén,
/// que es suyo; en un cliente lo rellena quien pide el préstamo y así nadie toca
/// el llavero ajeno.
static LENT: std::sync::RwLock<Option<Credentials>> = std::sync::RwLock::new(None);

/// Guarda (o borra, con `None`) la sesión prestada. La llama el cliente en cuanto
/// el servicio se la presta, y con `None` al cerrar sesión.
pub fn set_lent(creds: Option<Credentials>) {
    let mut slot = LENT.write().unwrap_or_else(|p| p.into_inner());
    *slot = creds;
}

/// El gemelo Cloud del hueco de arriba, y existe por el mismo lector: `logship`.
///
/// La sesión Cloud **no vive aquí** —vive en `cloud_auth`/`cloud.toml`, y su JWT
/// lo rota el servicio— así que un lector que sólo mirase [`current`] no la ve
/// nunca. Ése era el bug: con la app en Cloud, el enviador de logs resolvía
/// `None` en cada vuelta y no ha mandado una sola línea desde que existe.
///
/// Lo rellena quien tiene un token fresco: el servicio en cada rotación
/// ([`crate::session::refresh_loop`]) y un cliente en cuanto se lo prestan. Con
/// `None` al cerrar sesión.
static LENT_CLOUD: std::sync::RwLock<Option<CloudLease>> = std::sync::RwLock::new(None);

/// A qué Cloud y con qué JWT, para el lector que no puede pedirlo por IPC.
#[derive(Debug, Clone)]
pub struct CloudLease {
    pub url: String,
    pub token: String,
}

/// Guarda (o borra, con `None`) el token Cloud prestado.
pub fn set_lent_cloud(lease: Option<CloudLease>) {
    let mut slot = LENT_CLOUD.write().unwrap_or_else(|p| p.into_inner());
    *slot = lease;
}

/// El token Cloud prestado, si hay sesión Cloud viva en este proceso.
pub fn lent_cloud() -> Option<CloudLease> {
    LENT_CLOUD.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Este proceso es un **cliente**: no toca el almacén, sólo el préstamo.
static CLIENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Declara este proceso cliente del servicio, para que [`current`] no caiga nunca
/// al almacén.
///
/// Lo llama el desktop al arrancar. Sin esto, un lector que corre en los dos
/// procesos —`logship`— leería el llavero en el cliente durante la ventana en la
/// que el préstamo aún no está puesto, y en macOS esa lectura **es** el diálogo de
/// contraseña que D.20 viene a matar. Con la marca, un cliente sin préstamo se
/// queda sin sesión (y no envía logs) en vez de pedir permiso: perder un lote de
/// diagnóstico opcional es infinitamente mejor que un diálogo.
///
/// `hoardd` no la llama nunca: él es el dueño.
pub fn mark_client() {
    CLIENT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// La sesión prestada, sin caer al almacén. La usa un cliente que **no** puede
/// tocar el llavero: si el hueco está vacío, lo suyo es pedir el préstamo.
pub fn lent() -> Option<Credentials> {
    LENT.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// La sesión que este proceso puede usar: la prestada si la hay, el almacén si
/// no.
///
/// Para lectores que corren en los dos procesos y no pueden pedir nada por IPC
/// (`logship`): en un cliente el hueco está puesto, y en el daemon está vacío y
/// entonces el almacén es el suyo. Un cliente que llegue aquí con el hueco vacío
/// leería el llavero ajeno, así que quien pueda esperar debe usar el préstamo.
pub fn current() -> Result<Option<Credentials>> {
    if let Some(creds) = lent() {
        return Ok(Some(creds));
    }
    if CLIENT.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(None);
    }
    load()
}

/// Cheap shape check on a token string — `hoard_v1_` followed by 64 lowercase
/// hex characters. Avoids round-tripping obviously-wrong input through the
/// network.
pub fn is_valid_token(token: &str) -> bool {
    const PREFIX: &str = "hoard_v1_";
    if token.len() != PREFIX.len() + 64 {
        return false;
    }
    if !token.starts_with(PREFIX) {
        return false;
    }
    token[PREFIX.len()..]
        .chars()
        .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

// ---- internals ---------------------------------------------------------

fn read_session() -> Result<Option<Session>> {
    let path = session_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // A session file written by an older build can carry a broken ACL
        // (icacls /inheritance:r granted to a principal that doesn't resolve to
        // this process's identity) → the file exists but reads back "access
        // denied". The owner can always rewrite the DACL, so reset inherited
        // permissions and retry once before giving up.
        #[cfg(windows)]
        Err(_) if reset_acl_windows(&path) => std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} after ACL reset", path.display()))?,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let s: Session =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(s))
}

fn write_session(s: &Session) -> Result<()> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(s).context("serializing session")?;

    // Atomic write: a plain truncate+write leaves the session file half-written
    // if the process dies mid-write, and a truncated TOML fails to parse on next
    // launch → spurious sign-out. Write to a sibling temp file then rename over the
    // target (atomic on the same filesystem), so a reader only ever sees the old or
    // the new file. Solves Windows issues with inherited ACLs on partially-written
    // files and sync-folder interference (OneDrive, Dropbox).
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }

    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;

    Ok(())
}

/// Repair a session file a previous build's ACL-hardening left unreadable.
///
/// Older versions ran `icacls /inheritance:r /grant:r %USERNAME%:F` on the
/// file. When `%USERNAME%` didn't resolve to the process's actual identity
/// (Microsoft accounts, a same-named local account, roaming/redirected
/// profiles) the file ended up owned by the user but granting access to the
/// wrong principal, so a later launch reads it back as "access denied". The
/// owner keeps the implicit right to rewrite the DACL, so `icacls /reset`
/// restores the inherited, per-user permissions and the retry read then
/// succeeds. Best-effort — returns whether the reset ran cleanly so the caller
/// only retries the read when it's worth it.
#[cfg(windows)]
fn reset_acl_windows(path: &std::path::Path) -> bool {
    match std::process::Command::new("icacls")
        .arg(path)
        .arg("/reset")
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(path = %path.display(), "credentials: reset stale ACL on session file");
            true
        }
        Ok(out) => {
            tracing::warn!(
                status = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "credentials: icacls /reset did not repair the session file",
            );
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "credentials: failed to run icacls /reset");
            false
        }
    }
}

fn scrub_file_token() -> Result<()> {
    let Some(mut session) = read_session()? else {
        return Ok(());
    };
    if session.auth.is_some() {
        session.auth = None;
        write_session(&session)?;
    }
    Ok(())
}

// Las tres operaciones van por `keychain::keyring_op`: hilo propio, tope de
// [`KEYRING_TIMEOUT`] y [`KeyringTimeout`] como motivo cuando se agota. Un
// llavero bloqueado no falla, se queda esperando un desbloqueo que en una sesión
// sin escritorio nadie va a contestar, y una llamada síncrona sin tope cuelga a
// quien la hizo (ADR 0021 D.19 — lo mismo que ya pasaba con la sesión Cloud).

fn try_keyring_set(creds: &Credentials) -> Result<()> {
    // Store the whole session (token + URL + cached user) as TOML so the
    // keychain can restore it without the on-disk cache. See `KeyringBlob`.
    // Se serializa aquí, fuera del hilo del llavero: la operación tiene que ser
    // `'static` y `creds` es prestado.
    let blob = toml::to_string(&KeyringBlob {
        token: creds.token.clone(),
        url: creds.url.clone(),
        user: creds.user.clone(),
    })
    .context("serializing keychain blob")?;
    keyring_op(
        "saving the self-hosted session",
        KEYRING_TIMEOUT,
        move || {
            let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
            entry.set_password(&blob)?;
            Ok(())
        },
    )
}

fn try_keyring_get() -> Result<Option<KeyringBlob>> {
    keyring_op("reading the self-hosted session", KEYRING_TIMEOUT, || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
        match entry.get_password() {
            Ok(raw) => Ok(Some(parse_keyring_blob(&raw))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
}

/// Parse a keychain payload, tolerating the legacy format where the entry was
/// just the bare token string (no TOML wrapper).
fn parse_keyring_blob(raw: &str) -> KeyringBlob {
    match toml::from_str::<KeyringBlob>(raw) {
        Ok(blob) if !blob.token.is_empty() => blob,
        _ => KeyringBlob {
            token: raw.trim().to_string(),
            url: String::new(),
            user: None,
        },
    }
}

fn try_keyring_delete() -> Result<()> {
    keyring_op("deleting the self-hosted session", KEYRING_TIMEOUT, || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation_accepts_canonical() {
        let good = format!("hoard_v1_{}", "a".repeat(64));
        assert!(is_valid_token(&good));
    }

    #[test]
    fn token_validation_rejects_wrong_prefix() {
        let bad = format!("hoard_v2_{}", "a".repeat(64));
        assert!(!is_valid_token(&bad));
    }

    #[test]
    fn token_validation_rejects_short() {
        assert!(!is_valid_token("hoard_v1_abcd"));
    }

    #[test]
    fn token_validation_rejects_uppercase_hex() {
        let bad = format!("hoard_v1_{}", "A".repeat(64));
        assert!(!is_valid_token(&bad));
    }

    #[test]
    fn token_validation_rejects_non_hex() {
        let bad = format!("hoard_v1_{}", "z".repeat(64));
        assert!(!is_valid_token(&bad));
    }

    // ---- el llavero bloqueado, ruta self-hosted (D.19) ----------------
    //
    // Que la espera esté acotada se prueba en `crate::keychain`, que es el hilo
    // que comparten las dos sesiones. Aquí, lo que es de ésta: qué token gana y
    // que "bloqueado" no se lea como "no hay sesión".

    fn stuck() -> anyhow::Error {
        anyhow::Error::new(KeyringTimeout {
            doing: "reading the self-hosted session",
            after: KEYRING_TIMEOUT,
        })
    }

    fn in_the_keyring(token: &str) -> Result<Option<KeyringBlob>> {
        Ok(Some(KeyringBlob {
            token: token.to_string(),
            url: "https://saves.example".to_string(),
            user: None,
        }))
    }

    /// El fallo de D.19 en la ruta self-hosted: con el token sólo en el llavero,
    /// uno bloqueado devolvía `Ok(None)` y el usuario aparecía deslogueado —
    /// indistinguible de una instalación nueva, y sin nada que mirar. Ahora el
    /// motivo sale entero y tipado.
    #[test]
    fn a_locked_keyring_is_not_a_logged_out_user() {
        let err = pick_token(Err(stuck()), None).expect_err("no puede ser Ok(None)");
        assert!(err.is::<KeyringTimeout>(), "{err:#}");
        assert!(format!("{err:#}").contains("locked"), "{err:#}");
    }

    /// Pero con token en el fichero (el fallback 0600 de cuando no hay llavero)
    /// un llavero bloqueado no desloguea a nadie: se sigue con lo que hay.
    #[test]
    fn a_locked_keyring_still_falls_back_to_the_file_token() {
        let got = pick_token(Err(stuck()), Some("hoard_v1_del-fichero".to_string()))
            .expect("el fichero salva la sesión")
            .expect("token");
        assert_eq!(
            got,
            ("hoard_v1_del-fichero".to_string(), TokenStorage::File)
        );
    }

    /// El origen viaja con el token, y no es cosmético: es lo que le dice al
    /// daemon que el ítem del llavero no es suyo (o no está) y que le toca
    /// subirlo con [`promote_to_keyring`]. Sin este dato la promoción sería una
    /// escritura por arranque, y en macOS un diálogo por arranque.
    #[test]
    fn the_token_says_where_it_came_from() {
        let (_, storage) = pick_token(in_the_keyring("hoard_v1_x"), None)
            .expect("ok")
            .expect("token");
        assert_eq!(storage, TokenStorage::Keyring);

        let (_, storage) = pick_token(Ok(None), Some("hoard_v1_x".to_string()))
            .expect("ok")
            .expect("token");
        assert_eq!(storage, TokenStorage::File);
    }

    /// Un llavero que contesta "no" (la ACL de macOS que autoriza a otro binario,
    /// una sesión sin D-Bus) sale con **su** motivo tipado, distinto del tope.
    /// La UI los pinta igual —"vuelve a entrar"— pero el log tiene que poder
    /// distinguir un llavero bloqueado de uno que deniega.
    #[test]
    fn a_refusing_keyring_is_typed_as_unreadable() {
        let err = pick_token(Err(anyhow::anyhow!("access denied")), None)
            .expect_err("sin fichero que salve, sale entero");
        assert!(err.downcast_ref::<KeyringUnreadable>().is_some(), "{err:#}");
        assert!(err.downcast_ref::<KeyringTimeout>().is_none());
    }

    /// Un fallo del llavero que no es el tope (sin D-Bus, entrada corrupta) llega
    /// igual de entero, con el contexto de dónde ocurrió.
    #[test]
    fn another_keyring_failure_also_surfaces() {
        let err = pick_token(Err(anyhow::anyhow!("no D-Bus session bus")), None)
            .expect_err("el fallo del llavero se propaga");
        assert!(err.downcast_ref::<KeyringTimeout>().is_none());
        assert!(
            format!("{err:#}").contains("no D-Bus session bus"),
            "{err:#}"
        );
    }

    /// Y un llavero sano gana al fichero; sin entrada (o con una vacía, que es
    /// como quedan las de una sesión a medio borrar) se cae al fichero.
    #[test]
    fn a_healthy_keyring_wins_and_an_empty_one_falls_back() {
        let (got, _) = pick_token(
            in_the_keyring("hoard_v1_del-llavero"),
            Some("hoard_v1_del-fichero".to_string()),
        )
        .expect("ok")
        .expect("token");
        assert_eq!(got, "hoard_v1_del-llavero");

        let from_file = Some("hoard_v1_del-fichero".to_string());
        assert_eq!(
            pick_token(Ok(None), from_file.clone())
                .expect("ok")
                .map(|(t, _)| t)
                .as_deref(),
            Some("hoard_v1_del-fichero")
        );
        assert_eq!(
            pick_token(in_the_keyring(""), from_file)
                .expect("ok")
                .map(|(t, _)| t)
                .as_deref(),
            Some("hoard_v1_del-fichero")
        );
        assert!(pick_token(Ok(None), None).expect("ok").is_none());
        // Un fichero con el token vacío es lo mismo que no tenerlo.
        assert!(pick_token(Ok(None), Some(String::new()))
            .expect("ok")
            .is_none());
    }

    /// Aísla el directorio de config. Sólo Linux, que es donde `ProjectDirs` mira
    /// `XDG_CONFIG_HOME`: en macOS y Windows la ruta sale del sistema y el test
    /// escribiría en la sesión de verdad de quien ejecuta los tests.
    #[cfg(target_os = "linux")]
    fn with_isolated_config(f: impl FnOnce()) {
        let _guard = crate::test_lock::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        f();
        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    /// El camino sin servicio de D.20: se guarda en el fichero 0600, sin llavero, y
    /// se lee de vuelta entero — incluido lo que un cliente puede leer solo
    /// ([`load_public`]).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_session_stored_without_a_service_lands_in_the_file() {
        with_isolated_config(|| {
            let creds = Credentials {
                url: "https://hoard.example".to_string(),
                token: format!("hoard_v1_{}", "a".repeat(64)),
                user: Some(UserSection {
                    user_id: "u1".to_string(),
                    username: "rai".to_string(),
                    is_admin: true,
                }),
            };
            save_unlocked(&creds).expect("escribe");

            let session = read_session().expect("lee").expect("hay fichero");
            assert_eq!(session.server.url, "https://hoard.example");
            assert_eq!(
                session.auth.expect("el token está en el fichero").token,
                creds.token
            );

            let (url, user) = load_public().expect("lee").expect("hay sesión");
            assert_eq!(url, "https://hoard.example");
            assert_eq!(user.expect("usuario").username, "rai");

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(session_path().unwrap())
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "modo {:o}", mode & 0o777);
            }
        });
    }

    /// La tumba del logout sin servicio, que es la diferencia con Cloud: aquí
    /// **borrar el fichero no basta**, porque [`load`] recupera la sesión del blob
    /// del llavero cuando el fichero no está (el arreglo de la ACL de Windows) y
    /// resucitaría justo lo que el usuario acaba de cerrar. Con la marca puesta,
    /// `load` contesta "no hay sesión" **sin llegar a mirar el llavero** — que es lo
    /// que hace este test determinista incluso en una máquina con su ítem de verdad.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_logout_without_a_service_cannot_be_resurrected_from_the_keyring() {
        with_isolated_config(|| {
            let creds = Credentials {
                url: "https://hoard.example".to_string(),
                token: format!("hoard_v1_{}", "b".repeat(64)),
                user: None,
            };
            save_unlocked(&creds).expect("escribe");
            assert!(load_public().expect("lee").is_some());

            forget_unlocked().expect("olvida");
            assert!(
                load_public().expect("lee").is_none(),
                "sigue habiendo sesión"
            );
            assert!(load().expect("lee").is_none(), "la tumba no se respetó");
            // Y el siguiente login la limpia sin acordarse de ella.
            save_unlocked(&creds).expect("vuelve a entrar");
            assert!(load_public().expect("lee").is_some());
        });
    }

    /// El hueco del préstamo: en un cliente, `current` no puede caer al almacén ni
    /// cuando está vacío — esa lectura es el diálogo de contraseña de macOS.
    #[test]
    fn a_client_without_a_loan_has_no_session_instead_of_reading_the_store() {
        let creds = Credentials {
            url: "https://hoard.example".to_string(),
            token: format!("hoard_v1_{}", "c".repeat(64)),
            user: None,
        };
        set_lent(Some(creds.clone()));
        assert_eq!(lent().expect("prestada").token, creds.token);
        assert_eq!(current().expect("ok").expect("prestada").token, creds.token);

        set_lent(None);
        mark_client();
        assert!(lent().is_none());
        assert!(
            current().expect("ok").is_none(),
            "un cliente sin préstamo no puede leer el almacén"
        );
    }
}
