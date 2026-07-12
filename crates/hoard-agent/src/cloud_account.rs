//! Cuenta Cloud: llamadas REST portables (export, storage/caja negra,
//! archive/reactivate, borrar/reactivar cuenta, entitlements, features,
//! playtime). Es la lógica que antes vivía atrapada en
//! `hoard-desktop/src/commands/cloud.rs`; aquí no hay Tauri ni keyring.
//!
//! Cada función recibe `(base, token)` ya resueltos y devuelve **datos**
//! (`Result<_, CloudError>`). El desktop resuelve las credenciales por su
//! sesión Supabase/keyring y mapea el error a i18n; la CLI las resuelve por
//! [`crate::cloud_auth`] y lo imprime. El reintento-tras-401 se queda en cada
//! frontend porque cada uno refresca el JWT de forma distinta.

use serde::{Deserialize, Serialize};

use crate::playtime::{PlaytimeRow, PlaytimeSummary};

// ---- error ------------------------------------------------------------

/// Error de una llamada Cloud. Conserva `status`+`body` en el caso HTTP para
/// que el desktop reproduzca el mensaje exacto (incluido su mapeo `i18n:<key>`)
/// que ya mostraba, mientras la CLI se queda con [`CloudError::message`].
#[derive(Debug)]
pub enum CloudError {
    /// 401 — el JWT caducó. El llamador puede refrescar y reintentar.
    Unauthorized,
    /// Cualquier otro no-2xx (incluido 402 payment-required). `status` es el
    /// código HTTP crudo.
    Http { status: u16, body: String },
    /// Error de red / transporte.
    Network(String),
    /// Respuesta ilegible (JSON que no parsea).
    Parse(String),
}

impl CloudError {
    /// Texto humano neutral (sin i18n). Lo usa la CLI y es el fallback del
    /// desktop para los casos que no intercepta por código.
    pub fn message(&self) -> String {
        match self {
            CloudError::Unauthorized => "la sesión Cloud caducó — vuelve a iniciar sesión".into(),
            CloudError::Http { status, body } => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                    if let Some(msg) = v.get("error").and_then(|x| x.as_str()) {
                        return format!("Hoard Cloud: {msg} ({status})");
                    }
                }
                format!("Hoard Cloud devolvió {status}: {body}")
            }
            CloudError::Network(m) | CloudError::Parse(m) => m.clone(),
        }
    }
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for CloudError {}

// ---- helpers HTTP -----------------------------------------------------

fn http_client() -> Result<reqwest::Client, CloudError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("hoard-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))
}

/// Convierte una respuesta no-exitosa en [`CloudError`], distinguiendo el 401.
async fn into_error(resp: reqwest::Response) -> CloudError {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return CloudError::Unauthorized;
    }
    let body = resp.text().await.unwrap_or_default();
    CloudError::Http {
        status: status.as_u16(),
        body,
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, CloudError> {
    serde_json::from_str::<T>(body)
        .map_err(|e| CloudError::Parse(format!("parseando respuesta Cloud: {e}: {body}")))
}

// ---- export -----------------------------------------------------------

/// Job de exportación server-side. El worker construye el ZIP y el cliente
/// sondea [`export_status`] hasta que aparece el enlace de descarga.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportJob {
    pub job_id: String,
    pub status: String,
}

/// Estado del último job de exportación, con `download_url` presignada cuando el
/// ZIP está listo. Todos los campos son `None` si el usuario nunca exportó.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExportStatus {
    pub job_id: Option<String>,
    pub status: Option<String>,
    pub requested_at: Option<String>,
    pub size_bytes: Option<i64>,
    pub expires_at: Option<String>,
    pub download_url: Option<String>,
    pub error: Option<String>,
}

/// `POST {base}/v1/me/export` — lanza el job de exportación.
pub async fn export_all(base: &str, token: &str) -> Result<ExportJob, CloudError> {
    let url = format!("{base}/v1/me/export");
    let resp = http_client()?
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    let body = resp.text().await.unwrap_or_default();
    parse_json(&body)
}

/// `GET {base}/v1/me/export` — estado del último job.
pub async fn export_status(base: &str, token: &str) -> Result<ExportStatus, CloudError> {
    let url = format!("{base}/v1/me/export");
    let resp = http_client()?
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    let body = resp.text().await.unwrap_or_default();
    parse_json(&body)
}

// ---- caja negra: storage / archived games -----------------------------

/// Huella liberable de una partida. Espeja `GameFootprint` del server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageGame {
    pub save_id: String,
    pub game_slug: String,
    pub label: String,
    /// Bytes que baja la cuota si se archiva (blobs exclusivos deduplicados).
    pub freeable_bytes: i64,
    #[serde(default)]
    pub archived: bool,
    /// Instante RFC3339 de purga definitiva, presente solo mientras está archivada.
    #[serde(default)]
    pub purge_after: Option<String>,
}

/// `GET {base}/v1/cloud/storage/games` — huella por partida + cifras de cuota.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageGames {
    pub plan: String,
    pub used_bytes: u64,
    pub limit_bytes: u64,
    /// Bytes por encima del límite (0 si dentro).
    pub over_bytes: u64,
    pub games: Vec<StorageGame>,
}

/// Resultado de archivar. Espeja `ArchiveOut` del server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveResult {
    pub save_id: String,
    pub archived: bool,
    /// RFC3339 — cuándo se purga la copia congelada (instante + 7d).
    pub purge_after: String,
    pub freed_bytes: i64,
}

/// `GET {base}/v1/cloud/storage/games`.
pub async fn storage_games(base: &str, token: &str) -> Result<StorageGames, CloudError> {
    let url = format!("{base}/v1/cloud/storage/games");
    let resp = http_client()?
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    let body = resp.text().await.unwrap_or_default();
    parse_json(&body)
}

/// `POST {base}/v1/cloud/saves/:id/archive` — aparca una partida en la caja
/// negra: libera cuota ya, la deja descargable 7 días, luego un cron la purga.
pub async fn archive_save(
    base: &str,
    token: &str,
    save_id: &str,
) -> Result<ArchiveResult, CloudError> {
    let url = format!("{base}/v1/cloud/saves/{save_id}/archive");
    let resp = http_client()?
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    let body = resp.text().await.unwrap_or_default();
    parse_json(&body)
}

/// `POST {base}/v1/cloud/saves/:id/reactivate` — recupera una partida archivada.
pub async fn reactivate_save(base: &str, token: &str, save_id: &str) -> Result<(), CloudError> {
    let url = format!("{base}/v1/cloud/saves/{save_id}/reactivate");
    let resp = http_client()?
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    Ok(())
}

// ---- cuenta -----------------------------------------------------------

/// `DELETE {base}/v1/me` — soft-delete + freeze de la cuenta (gracia 30 días).
/// Limpiar la sesión local es glue de cada frontend.
pub async fn delete_account(base: &str, token: &str) -> Result<(), CloudError> {
    let url = format!("{base}/v1/me");
    let resp = http_client()?
        .delete(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    Ok(())
}

/// `POST {base}/v1/me/reactivate` — cancela un soft-delete pendiente. El
/// frontend re-lee `/v1/me` después para refrescar su snapshot de cuenta.
pub async fn reactivate_account(base: &str, token: &str) -> Result<(), CloudError> {
    let url = format!("{base}/v1/me/reactivate");
    let resp = http_client()?
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    Ok(())
}

// ---- entitlements / features ------------------------------------------

/// Acceso Pro por feature, espejo de `GET /v1/cloud/entitlements`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudEntitlements {
    pub plan: String,
    pub features: CloudFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudFeatures {
    pub screen: FeatureState,
    pub wrapple: FeatureState,
}

/// Estado de acceso de una feature. `tag = "state"` para casar con el enum del
/// server (`entitled` / `trial_available` / `trial` / `trial_expired`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FeatureState {
    Entitled,
    TrialAvailable { days: i64 },
    Trial { expires_at: String },
    TrialExpired,
}

/// `GET {base}/v1/cloud/entitlements` — snapshot de solo lectura (no arranca
/// trial). Un solo intento; el reintento-tras-401 lo hace el llamador.
pub async fn entitlements(base: &str, token: &str) -> Result<CloudEntitlements, CloudError> {
    let url = format!("{base}/v1/cloud/entitlements");
    let resp = http_client()?
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    let body = resp.text().await.unwrap_or_default();
    parse_json(&body)
}

/// `POST {base}/v1/cloud/features/:feature/activate` — abre una feature Pro:
/// arranca el trial de un mes en el primer uso (el server es idempotente). Un
/// `402` (bloqueada: sin Pro, trial agotado) se traduce a `TrialExpired` para
/// que la UI mantenga el candado, no a error.
pub async fn activate_feature(
    base: &str,
    token: &str,
    feature: &str,
) -> Result<FeatureState, CloudError> {
    let url = format!("{base}/v1/cloud/features/{feature}/activate");
    let resp = http_client()?
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(CloudError::Unauthorized);
    }
    if status == reqwest::StatusCode::PAYMENT_REQUIRED {
        return Ok(FeatureState::TrialExpired);
    }
    if !status.is_success() {
        return Err(into_error(resp).await);
    }
    let body = resp.text().await.unwrap_or_default();
    parse_json(&body)
}

// ---- playtime ---------------------------------------------------------

/// Cuerpo de subida: el desglose `(día, juego, secs)` de este equipo + su
/// fingerprint para que el server separe las filas por máquina.
#[derive(Debug, Serialize)]
pub struct PlaytimeUploadBody {
    pub device_fp: String,
    pub rows: Vec<PlaytimeRow>,
}

/// `POST {base}{path}` — sube el desglose de playtime de este equipo. `path` es
/// `/v1/cloud/playtime` (Cloud) o `/v1/playtime` (self-hosted).
pub async fn push_playtime(
    base: &str,
    path: &str,
    token: &str,
    body: &PlaytimeUploadBody,
) -> Result<(), CloudError> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let resp = http_client()?
        .post(&url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    Ok(())
}

/// `GET {base}{path}` — lee el agregado de playtime fusionado por dispositivo.
pub async fn fetch_playtime(
    base: &str,
    path: &str,
    token: &str,
) -> Result<PlaytimeSummary, CloudError> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let resp = http_client()?
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("Network error: {e}")))?;
    if !resp.status().is_success() {
        return Err(into_error(resp).await);
    }
    let body = resp.text().await.unwrap_or_default();
    parse_json(&body)
}
