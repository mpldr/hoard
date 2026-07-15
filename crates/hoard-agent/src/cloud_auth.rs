//! Hoard Cloud auth, sin navegador y **compartido** entre desktop y CLI.
//!
//! El login web del desktop (redirect por browser → `hoard://auth/callback`)
//! no sirve en una terminal (SteamOS modo gaming, NAS, servidor por SSH). Aquí
//! vive el camino headless: Supabase GoTrue por **password grant** o por
//! **código OTP por email** (`/auth/v1/otp` + `/auth/v1/verify`), más el
//! refresh del JWT y la persistencia de la sesión.
//!
//! La persistencia es **interoperable con el desktop a propósito**: mismo
//! `keyring` (servicio `hoard-desktop-cloud`), mismo fichero
//! `<config>/desktop/cloud.toml` y misma forma `AuthSection { access_token,
//! refresh_token }`. Así una sesión iniciada por el CLI la ve el desktop y al
//! revés — una única sesión Cloud por máquina. El campo `user` (snapshot de
//! `/v1/me`) se **preserva tal cual** (raw TOML) en las reescrituras del CLI,
//! para no pisar el cache de cuenta del desktop.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

const CLOUD_DEFAULT_URL: &str = "https://api.hoard.services";
const KEYRING_SERVICE: &str = "hoard-desktop-cloud";
const KEYRING_USER: &str = "default";

// Proyecto Supabase GoTrue público — el mismo que usan la web y el desktop. La
// anon key es una credencial pública (viaja en el bundle estático de la web),
// así que embeberla aquí no expone nada nuevo. Todo overridable por env para
// que un build de dev apunte a otro proyecto.
const SUPABASE_DEFAULT_URL: &str = "https://zddepgqdiuhhzqdimsks.supabase.co";
const SUPABASE_DEFAULT_ANON_KEY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InpkZGVwZ3FkaXVoaHpxZGltc2tzIiwicm9sZSI6ImFub24iLCJpYXQiOjE3Nzk2MzM2MTksImV4cCI6MjA5NTIwOTYxOX0.3nZebGwCzFO1byTqhowq9ip89GE9fMRxPscgYSlPzFk";

pub fn cloud_base_url() -> String {
    std::env::var("HOARD_CLOUD_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CLOUD_DEFAULT_URL.to_string())
}

pub fn supabase_url() -> String {
    std::env::var("HOARD_SUPABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("HOARD_SUPABASE_URL").map(str::to_string))
        .unwrap_or_else(|| SUPABASE_DEFAULT_URL.to_string())
}

pub fn supabase_anon_key() -> String {
    std::env::var("HOARD_SUPABASE_ANON_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("HOARD_SUPABASE_ANON_KEY").map(str::to_string))
        .unwrap_or_else(|| SUPABASE_DEFAULT_ANON_KEY.to_string())
}

/// Par de tokens de una sesión Supabase. `access` es el JWT corto (~1h);
/// `refresh` es el de larga vida que lo renueva.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
}

/// Sesión Cloud activa cargada de disco: a qué servidor apunta y sus tokens.
#[derive(Debug, Clone)]
pub struct Session {
    pub server_url: String,
    pub access: String,
    pub refresh: String,
}

/// Sentinela: Supabase rechazó el refresh porque ya se había rotado
/// (`refresh_token_already_used`/`not_found`). Se distingue por tipo para que
/// quien refresca pueda auto-sanar adoptando los tokens que otra ejecución ya
/// dejó en disco, en vez de tratarlo como sesión muerta.
#[derive(Debug)]
pub struct RefreshTokenStale;

impl std::fmt::Display for RefreshTokenStale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refresh token ya rotado por otra ejecución")
    }
}

impl std::error::Error for RefreshTokenStale {}

fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("hoard-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("construyendo cliente HTTP")
}

// ---- login sin navegador ----------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

/// Extrae `(access, refresh)` de una respuesta de GoTrue, o un error legible
/// con el mensaje que devuelva Supabase.
async fn parse_token_response(resp: reqwest::Response) -> Result<Tokens> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let parsed: TokenResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "parseando respuesta de login (status {status}, {} bytes)",
                body.len()
            )
        })?;
        return Ok(Tokens {
            access: parsed.access_token,
            refresh: parsed.refresh_token,
        });
    }
    bail!("{}", supabase_error_message(status, &body));
}

/// Mensaje humano a partir del cuerpo de error de GoTrue. Cubre las formas
/// habituales (`error_description`, `msg`, `error`) sin volcar JSON crudo.
fn supabase_error_message(status: StatusCode, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["error_description", "msg", "error", "message"] {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                return s.to_string();
            }
        }
    }
    format!("Supabase devolvió {status}")
}

/// Login por email + contraseña (`grant_type=password`). Directo si la cuenta
/// tiene contraseña; las creadas solo con Google/GitHub no la tienen → usa OTP.
pub async fn login_password(email: &str, password: &str) -> Result<Tokens> {
    let url = format!("{}/auth/v1/token?grant_type=password", supabase_url());
    let resp = http_client()?
        .post(&url)
        .header("apikey", supabase_anon_key())
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    parse_token_response(resp).await
}

/// Pide a Supabase que envíe un código OTP al email (`/auth/v1/otp`). Que el
/// email traiga un código de 6 dígitos (además del enlace mágico) depende de la
/// plantilla de correo del proyecto; si solo trae enlace, este camino no
/// completa y hay que usar el password grant.
pub async fn otp_start(email: &str) -> Result<()> {
    let url = format!("{}/auth/v1/otp", supabase_url());
    let resp = http_client()?
        .post(&url)
        .header("apikey", supabase_anon_key())
        .json(&serde_json::json!({ "email": email, "create_user": true }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    bail!(
        "no pude enviar el código: {}",
        supabase_error_message(status, &body)
    );
}

/// Canjea el código OTP recibido por email por una sesión (`/auth/v1/verify`).
pub async fn otp_verify(email: &str, code: &str) -> Result<Tokens> {
    let url = format!("{}/auth/v1/verify", supabase_url());
    let resp = http_client()?
        .post(&url)
        .header("apikey", supabase_anon_key())
        .json(&serde_json::json!({ "type": "email", "email": email, "token": code.trim() }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    parse_token_response(resp).await
}

// ---- emparejamiento por móvil (device flow) ---------------------------

/// Respuesta de `/v1/cloud/device/start`. El CLI enseña `user_code` +
/// `verification_uri` (o el `_complete` con el código ya puesto) y hace polling
/// con `device_code` hasta que el móvil lo aprueba.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default = "default_poll_interval")]
    pub interval_secs: u64,
    #[serde(default)]
    pub expires_in_secs: u64,
}

fn default_poll_interval() -> u64 {
    3
}

/// Estado de un emparejamiento al hacer polling.
pub enum DeviceStatus {
    Pending,
    Approved(Tokens),
    Denied,
    Expired,
}

/// Arranca un emparejamiento en el server Cloud (no en Supabase): es el server
/// quien acuña la sesión cuando el móvil aprueba.
///
/// Devuelve `Ok(None)` cuando el server **no soporta** el emparejamiento: una
/// versión previa sin las rutas `/v1/cloud/device/*` (404), un server que las
/// deja tras auth (401), o un Cloud sin `service_role` configurado ("not
/// configured"). En esos casos quien llama debe caer al login por email, no
/// reventar. `Err` queda para fallos de transporte/parseo reales.
pub async fn device_start(hostname: Option<&str>) -> Result<Option<DeviceStart>> {
    let url = format!("{}/v1/cloud/device/start", cloud_base_url());
    let resp = http_client()?
        .post(&url)
        .json(&serde_json::json!({ "hostname": hostname }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    // Server sin la feature: 404 (ruta inexistente), 401 (tras auth en una
    // versión previa) o 501. Tratar como "no soportado" → fallback a email.
    if matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::NOT_IMPLEMENTED
    ) {
        return Ok(None);
    }
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Cloud alcanzable pero sin `service_role`: `approve` responde "not
        // configured"; algunos despliegues lo devuelven ya en `start`.
        if body.contains("not configured") {
            return Ok(None);
        }
        bail!(
            "no pude iniciar el emparejamiento: {}",
            supabase_error_message(status, &body)
        );
    }
    let start = serde_json::from_str(&body).context("parseando respuesta de device/start")?;
    Ok(Some(start))
}

/// Consulta si el emparejamiento ya fue aprobado (`/v1/cloud/device/poll`).
/// Cuando lo está, devuelve los tokens una única vez (el server borra la fila).
pub async fn device_poll(device_code: &str) -> Result<DeviceStatus> {
    #[derive(Deserialize)]
    struct PollBody {
        status: String,
        access_token: Option<String>,
        refresh_token: Option<String>,
    }
    let url = format!("{}/v1/cloud/device/poll", cloud_base_url());
    let resp = http_client()?
        .post(&url)
        .json(&serde_json::json!({ "device_code": device_code }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "error consultando el emparejamiento: {}",
            supabase_error_message(status, &body)
        );
    }
    let p: PollBody = serde_json::from_str(&body).context("parseando respuesta de device/poll")?;
    Ok(match p.status.as_str() {
        "approved" => match (p.access_token, p.refresh_token) {
            (Some(access), Some(refresh)) if !access.is_empty() && !refresh.is_empty() => {
                DeviceStatus::Approved(Tokens { access, refresh })
            }
            _ => DeviceStatus::Expired,
        },
        "denied" => DeviceStatus::Denied,
        "expired" => DeviceStatus::Expired,
        _ => DeviceStatus::Pending,
    })
}

/// Canjea el refresh token por un par nuevo (`grant_type=refresh_token`).
///
/// Reintenta ante fallos transitorios (red/timeout/5xx/429) dentro de la ventana
/// de gracia de reuse de GoTrue: si la rotación llegó al server pero perdimos la
/// respuesta, reclamar el mismo token en ~10s recupera el par ya minteado en vez
/// de dejarlo huérfano (que luego dispararía reuse-detection y cerraría sesión).
/// Un rechazo genuino (`already_used`/`not_found`) se devuelve como
/// [`RefreshTokenStale`] sin reintentar.
pub async fn refresh(refresh_token: &str) -> Result<Tokens> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        bail!("no hay refresh token guardado — vuelve a iniciar sesión");
    }
    let url = format!("{}/auth/v1/token?grant_type=refresh_token", supabase_url());
    let client = Client::builder()
        .timeout(Duration::from_secs(7))
        .user_agent(concat!("hoard-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("construyendo cliente de refresh")?;

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let sent = client
            .post(&url)
            .header("apikey", supabase_anon_key())
            .json(&serde_json::json!({ "refresh_token": refresh_token }))
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    continue;
                }
                return Err(anyhow::Error::new(e).context(format!("POST {url}")));
            }
        };
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            let parsed: TokenResponse = serde_json::from_str(&body).with_context(|| {
                format!("parseando refresh (status {status}, {} bytes)", body.len())
            })?;
            return Ok(Tokens {
                access: parsed.access_token,
                refresh: parsed.refresh_token,
            });
        }
        let low = body.to_lowercase();
        if low.contains("already_used")
            || low.contains("already used")
            || low.contains("not_found")
            || low.contains("not found")
        {
            return Err(anyhow::Error::new(RefreshTokenStale));
        }
        if (status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS) && attempt < 3 {
            tokio::time::sleep(Duration::from_millis(600)).await;
            continue;
        }
        bail!("no pude renovar la sesión ({status}): {body}");
    }
}

// ---- /v1/me -----------------------------------------------------------

/// Forma mínima de `/v1/me` que necesita el CLI. `serde` ignora los campos que
/// no listamos, así que no hace falta espejar todo `CloudAccount` del desktop.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub user_id: String,
    pub email: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub storage_used_bytes: i64,
    #[serde(default)]
    pub storage_limit_bytes: i64,
}

/// GET `{base}/v1/me` con el JWT. Valida el token y trae la cuenta.
pub async fn fetch_me(base: &str, access: &str) -> Result<Me> {
    let url = format!("{}/v1/me", base.trim_end_matches('/'));
    let resp = http_client()?
        .get(&url)
        .bearer_auth(access)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        bail!("la sesión Cloud caducó — vuelve a iniciar sesión");
    }
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("/v1/me devolvió {status}: {body}");
    }
    serde_json::from_str::<Me>(&body).with_context(|| format!("parseando /v1/me: {body}"))
}

// ---- persistencia (interoperable con el desktop) ----------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SessionFile {
    #[serde(default)]
    server_url: String,
    /// Snapshot de `/v1/me` que escribe el desktop. Lo preservamos tal cual
    /// (raw TOML) para no pisar su cache; el CLI no lo necesita tipado.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<toml::Value>,
    /// Fallback cuando el keyring no está disponible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<AuthSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthSection {
    access_token: String,
    refresh_token: String,
}

fn session_path() -> Result<PathBuf> {
    let dirs = crate::config::CliConfig::project_dirs()?;
    Ok(dirs.config_dir().join("desktop").join("cloud.toml"))
}

fn read_session_file() -> Result<Option<SessionFile>> {
    let path = session_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("leyendo {}", path.display()))?;
    let s: SessionFile =
        toml::from_str(&text).with_context(|| format!("parseando {}", path.display()))?;
    Ok(Some(s))
}

fn write_session_file(s: &SessionFile) -> Result<()> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creando {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(s).context("serializando sesión Cloud")?;
    // Escritura atómica: temp + rename, para que un corte a media escritura no
    // deje un TOML truncado que al arrancar parezca "sesión rota".
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("escribiendo {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renombrando {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn keyring_set(access: &str, refresh: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    let blob = toml::to_string(&AuthSection {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
    })?;
    entry.set_password(&blob)?;
    Ok(())
}

fn keyring_get() -> Result<Option<AuthSection>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    match entry.get_password() {
        Ok(blob) => Ok(Some(toml::from_str(&blob)?)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn keyring_delete() -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Carga la sesión Cloud activa (keyring primero, fichero como fallback), o
/// `None` si no hay sesión.
pub fn load_session() -> Result<Option<Session>> {
    let Some(file) = read_session_file()? else {
        return Ok(None);
    };
    let auth = match keyring_get() {
        Ok(Some(a)) => Some(a),
        _ => file.auth.clone(),
    };
    let Some(auth) = auth else {
        return Ok(None);
    };
    if auth.access_token.is_empty() {
        return Ok(None);
    }
    Ok(Some(Session {
        server_url: if file.server_url.is_empty() {
            cloud_base_url()
        } else {
            file.server_url
        },
        access: auth.access_token,
        refresh: auth.refresh_token,
    }))
}

/// Persiste el par de tokens conservando el `user` y el `server_url` ya en
/// disco (read-modify-write). Keyring primero; si no hay, cae al fichero 0600.
pub fn store_tokens(tokens: &Tokens, server_url: &str) -> Result<()> {
    let mut session = read_session_file()?.unwrap_or_default();
    if session.server_url.is_empty() {
        session.server_url = server_url.to_string();
    }
    match keyring_set(&tokens.access, &tokens.refresh) {
        Ok(()) => session.auth = None,
        Err(_) => {
            session.auth = Some(AuthSection {
                access_token: tokens.access.clone(),
                refresh_token: tokens.refresh.clone(),
            })
        }
    }
    write_session_file(&session)
}

/// Borra la sesión Cloud (keyring + fichero).
pub fn clear_session() -> Result<()> {
    let _ = keyring_delete();
    let path = session_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("borrando {}", path.display()))?;
    }
    Ok(())
}

/// `user_id` (claim `sub`) del JWT de la sesión guardada, **sin red**. Deja que
/// comandos locales (`hoard saves`) fijen el contexto Cloud correcto sin tener
/// que refrescar el token ni llamar a `/v1/me`. `None` si no hay sesión o el
/// JWT no es decodificable.
pub fn session_user_id() -> Result<Option<String>> {
    let Some(sess) = load_session()? else {
        return Ok(None);
    };
    Ok(jwt_sub(&sess.access))
}

/// Decodifica el claim `sub` de un JWT (segundo segmento, base64url sin
/// padding). No verifica la firma — solo lee el `user_id`, que el server
/// revalida en cada request.
fn jwt_sub(jwt: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("sub")?.as_str().map(String::from)
}

// ---- refresh centralizado ---------------------------------------------

/// Ventana en la que un refresh recién completado se reutiliza en vez de pedir
/// otro. GoTrue rota el refresh token en cada uso y revoca el anterior, así que
/// una ráfaga de llamantes (el refresher periódico y un token rechazado por el
/// realtime, p.ej.) tiene que colapsar en un solo viaje: el segundo replay
/// dispararía reuse-detection sobre un token ya rotado.
const REFRESH_REUSE_WINDOW: Duration = Duration::from_secs(30);

/// Serializa **todos** los refresh del proceso y recuerda el último par rotado.
fn refresh_gate() -> &'static tokio::sync::Mutex<Option<(Instant, Tokens)>> {
    static GATE: OnceLock<tokio::sync::Mutex<Option<(Instant, Tokens)>>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// Refresca la sesión Cloud con **el token más fresco que haya en disco** y
/// persiste el par rotado. Único camino de refresh del proceso, a propósito.
///
/// El motivo de que no acepte una `Session`: cada llamante refrescaba antes con
/// su propia copia en memoria, y una copia capturada minutos atrás (el realtime
/// la toma al conectar y su conexión vive hasta `CONNECTION_MAX_SECS`) podía
/// replayar un token que el refresher periódico ya había rotado. Fuera de la
/// ventana de gracia de GoTrue eso no es un reintento sino reuse-detection, y
/// la respuesta es revocar la **familia entera** de tokens: sesión muerta sin
/// recuperación posible, ni siquiera reiniciando.
///
/// Tres capas: el mutex serializa los refresh concurrentes, la relectura de
/// disco *dentro* del lock impide mandar una copia vieja, y
/// [`REFRESH_REUSE_WINDOW`] colapsa las ráfagas. El heal cubre lo que queda
/// fuera del proceso (el desktop comparte el mismo fichero de sesión): si GoTrue
/// dice stale, se relee disco y se adopta la rotación ajena.
pub async fn refresh_freshest() -> Result<Tokens> {
    // Sostener el lock a través de la llamada de red es lo que serializa.
    let mut last = refresh_gate().lock().await;

    if let Some((at, tokens)) = last.as_ref() {
        if at.elapsed() < REFRESH_REUSE_WINDOW {
            return Ok(tokens.clone());
        }
    }

    let Some(sess) = load_session()? else {
        bail!("no hay sesión Cloud — vuelve a iniciar sesión");
    };
    let attempted = sess.refresh.clone();
    match refresh(&sess.refresh).await {
        Ok(tokens) => {
            store_tokens(&tokens, &sess.server_url)?;
            *last = Some((Instant::now(), tokens.clone()));
            Ok(tokens)
        }
        Err(e) if e.downcast_ref::<RefreshTokenStale>().is_some() => {
            match adoptable(&attempted, load_session().ok().flatten().as_ref()) {
                Some(tokens) => {
                    tracing::debug!("cloud: el refresh token lo rotó otra ejecución; adopto el de disco");
                    *last = Some((Instant::now(), tokens.clone()));
                    Ok(tokens)
                }
                None => Err(e.context("la sesión Cloud caducó — vuelve a iniciar sesión")),
            }
        }
        Err(e) => Err(e),
    }
}

/// Decide si la sesión en disco sirve para sanar un [`RefreshTokenStale`]: solo
/// si trae un refresh token no vacío y **distinto** del que se intentó. Si fuera
/// el mismo, reintentarlo daría stale otra vez — no hay nada que adoptar.
fn adoptable(attempted: &str, on_disk: Option<&Session>) -> Option<Tokens> {
    let s = on_disk?;
    (!s.refresh.trim().is_empty() && s.refresh != attempted).then(|| Tokens {
        access: s.access.clone(),
        refresh: s.refresh.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(refresh: &str) -> Session {
        Session {
            server_url: "https://api.hoard.services".to_string(),
            access: "fresh-jwt".to_string(),
            refresh: refresh.to_string(),
        }
    }

    #[test]
    fn adopts_a_rotation_left_by_another_process() {
        let got = adoptable("ours", Some(&disk("rotated-by-the-desktop"))).expect("adoptable");
        assert_eq!(got.refresh, "rotated-by-the-desktop");
        assert_eq!(got.access, "fresh-jwt");
    }

    #[test]
    fn refuses_to_replay_the_token_that_just_failed() {
        // Mismo token en disco: nadie rotó nada, la sesión está muerta de verdad.
        assert!(adoptable("ours", Some(&disk("ours"))).is_none());
    }

    #[test]
    fn ignores_an_empty_or_absent_session() {
        assert!(adoptable("ours", Some(&disk("   "))).is_none());
        assert!(adoptable("ours", None).is_none());
    }
}
