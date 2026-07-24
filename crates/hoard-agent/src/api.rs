use anyhow::{anyhow, bail, Context, Result};
use hoard_core::ids::GameSlug;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::OnceCell;

/// How long a snapshot download may go without a single byte arriving before we
/// call it stalled. Not a budget for the transfer — it resets on every chunk —
/// so it bounds a dead stream without capping a big, slow, healthy one.
const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("authentication failed: token rejected by server (401)")]
    Unauthorized,
    #[error("forbidden (403)")]
    Forbidden,
    #[error("not found (404)")]
    NotFound,
    /// HTTP 413. On Hoard Cloud the body carries the structured per-save cap
    /// (`code:"save_too_large"` with `plan` / `limit_bytes` / `actual_bytes`),
    /// so we can tell the user exactly which limit they hit and how big the
    /// save was. Self-hosted 413s (raw quota) leave `0` and fall back to the
    /// generic message.
    #[error("{}", .0.human())]
    TooLarge(SaveTooLarge),
    /// HTTP 403 with `code:"save_archived"` — the game is parked in the
    /// server-side archive ("caja negra"). Uploading it would revive its frozen
    /// blobs and re-inflate the quota, so the client must stop trying and treat
    /// the local save as frozen, not errored. Distinct from the generic
    /// `Forbidden` so the backup path can settle it silently instead of painting
    /// a red "falló".
    #[error("game is archived on the server")]
    Archived,
    #[error("conflict (409): {0}")]
    Conflict(String),
    #[error("bad request (400): {0}")]
    BadRequest(String),
    #[error("server error ({status}): {body}")]
    Server { status: u16, body: String },
    /// 429 from the rolling bandwidth limiter. Carries the server's
    /// `retry_after_seconds` so the caller can wait the *exact* window-slide
    /// time instead of a short exponential backoff that would just burn every
    /// retry inside the still-over-quota window. `body` keeps the raw JSON for
    /// logging/diagnostics.
    #[error("rate limited (429): retry after {retry_after_seconds}s")]
    RateLimited {
        retry_after_seconds: u32,
        body: String,
    },
}

/// Structured body of a Hoard Cloud `save_too_large` 413. All fields default
/// to zero/empty so a self-hosted 413 (or an unparseable body) still yields a
/// usable [`ApiError::TooLarge`] via [`SaveTooLarge::human`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SaveTooLarge {
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub limit_bytes: u64,
    #[serde(default)]
    pub actual_bytes: u64,
    #[serde(default)]
    pub upgrade_url: Option<String>,
}

impl SaveTooLarge {
    /// A human, diagnosable one-liner. Falls back to a generic sentence when
    /// the structured fields are absent (self-hosted / unparseable body). The
    /// desktop re-localizes the cloud case from the structured fields via the
    /// `BackupTooLarge` agent event; this string is the log/CLI/self-hosted
    /// surface.
    pub fn human(&self) -> String {
        if self.limit_bytes == 0 {
            return "payload too large (413): exceeds the server's per-save size limit".into();
        }
        format!(
            "save too large: {} exceeds the {} plan limit of {} per save",
            fmt_bytes(self.actual_bytes),
            if self.plan.is_empty() {
                "current"
            } else {
                &self.plan
            },
            fmt_bytes(self.limit_bytes),
        )
    }
}

/// Coarse human byte size for error copy. Binary units, one decimal.
fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.0} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

impl ApiError {
    pub async fn from_response(resp: reqwest::Response) -> Self {
        let status = resp.status();
        // Grab the Retry-After header before consuming the body — it's our
        // fallback for `retry_after_seconds` if the JSON is unparseable.
        let retry_after_header = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u32>().ok());
        let body = resp.text().await.unwrap_or_default();
        match status {
            StatusCode::UNAUTHORIZED => ApiError::Unauthorized,
            StatusCode::FORBIDDEN => {
                if extract_code(&body).as_deref() == Some("save_archived") {
                    ApiError::Archived
                } else {
                    ApiError::Forbidden
                }
            }
            StatusCode::NOT_FOUND => ApiError::NotFound,
            StatusCode::PAYLOAD_TOO_LARGE => {
                ApiError::TooLarge(serde_json::from_str::<SaveTooLarge>(&body).unwrap_or_default())
            }
            StatusCode::CONFLICT => ApiError::Conflict(extract_message(&body)),
            StatusCode::BAD_REQUEST => ApiError::BadRequest(extract_message(&body)),
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after_seconds = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("retry_after_seconds")
                            .and_then(|x| x.as_u64())
                            .map(|n| n as u32)
                    })
                    .or(retry_after_header)
                    // Defensive floor: a 429 with no usable hint still waits a
                    // sensible spell rather than hammering immediately.
                    .unwrap_or(60);
                ApiError::RateLimited {
                    retry_after_seconds,
                    body,
                }
            }
            _ => ApiError::Server {
                status: status.as_u16(),
                body,
            },
        }
    }
}

fn extract_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(s) = v.get("message").and_then(|x| x.as_str()) {
            return s.to_string();
        }
        if let Some(s) = v.get("error").and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    body.to_string()
}

/// Pull the stable machine-readable `code` out of a cloud error body, if any.
fn extract_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("code").and_then(|x| x.as_str()).map(String::from))
}

#[derive(Debug, Clone)]
pub struct ApiClient {
    base_url: String,
    /// Bearer token, shared+swappable across every clone of this client.
    ///
    /// Self-hosted bearer tokens are stable for the process lifetime, but a
    /// Hoard Cloud session uses a short-lived Supabase JWT (~1h). The desktop's
    /// long-lived agent holds a single `ApiClient` for the whole session, so a
    /// frozen token would start answering 401 the moment the JWT expired —
    /// which is exactly what made the auto-restore sweep spam "no se pudo
    /// restaurar" once an hour in. Storing the token behind a shared `RwLock`
    /// lets the desktop's token-refresh path push a fresh JWT into the running
    /// agent's client via [`ApiClient::set_token`] without rebuilding it.
    token: Arc<RwLock<String>>,
    /// Client for the small request/response JSON endpoints. Its 60 s total
    /// timeout covers the whole request *including the body*, so nothing that
    /// streams snapshot bytes may use it — see `upload_http` / `download_http`.
    http: Client,
    /// Streaming client for snapshot **uploads** (`snapshot_upload`,
    /// `put_presigned`). Same headers as `http` but with **no per-request total
    /// timeout** — a multi-GB save (Paradox grand-strategy is the worst case)
    /// on a residential upload link blows past any fixed timeout, which
    /// previously killed the request mid-flight and silently hung the dashboard
    /// "Subiendo…" pill. A TCP keepalive surfaces a genuinely dead connection;
    /// a slow-but-progressing upload is left to finish.
    upload_http: Client,
    /// Streaming client for snapshot **downloads** (`snapshot_download`,
    /// `get_presigned`). Same no-total-timeout rationale as `upload_http`, plus
    /// a `read_timeout` that bounds a genuine stall.
    ///
    /// The read timeout is deliberately *not* on `upload_http`: reqwest arms it
    /// once when the request starts and polls it while waiting for the response
    /// head, only handing it to the body (where it becomes per-read and resets
    /// on progress) once the head arrives. A download's head lands immediately,
    /// so the timeout only ever sees body reads; an upload's head arrives after
    /// the whole body is sent, so the same setting would kill any upload slower
    /// than the timeout — exactly the bug `upload_http` exists to avoid.
    download_http: Client,
    /// Lazily-probed `/v1/health` `mode` (`Some("cloud")` on the SaaS
    /// deployment, `None`/absent self-hosted). Cached behind an `Arc` so the
    /// many `ApiClient` clones in flight share a single probe. Only cached on
    /// a successful probe — a transient health failure leaves the cell empty
    /// so the next call retries instead of wedging the client into the wrong
    /// protocol forever.
    mode: Arc<OnceCell<Option<String>>>,
}

impl ApiClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("hoard-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            // Long-lived stream uploads/downloads handle their own timeouts via streaming
            .build()?;
        let upload_http = Client::builder()
            .user_agent(concat!("hoard-agent/", env!("CARGO_PKG_VERSION")))
            // No total timeout: snapshot bodies are arbitrary size. The TCP
            // keepalive RSTs a connection that genuinely stopped flowing, while
            // a slow-but-progressing upload is left to finish.
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(None)
            .build()?;
        let download_http = Client::builder()
            .user_agent(concat!("hoard-agent/", env!("CARGO_PKG_VERSION")))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(None)
            // Per-read, not total: it resets on every chunk that arrives, so a
            // download stays alive as long as it progresses however long it
            // takes, while one that truly stalls fails instead of hanging.
            .read_timeout(STREAM_STALL_TIMEOUT)
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: Arc::new(RwLock::new(token.into())),
            http,
            upload_http,
            download_http,
            mode: Arc::new(OnceCell::new()),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Swap in a fresh bearer token. Every clone of this client shares the same
    /// token cell, so updating one updates them all — the mechanism the desktop
    /// uses to keep the long-lived agent client's Supabase JWT current after a
    /// refresh.
    pub fn set_token(&self, token: impl Into<String>) {
        if let Ok(mut guard) = self.token.write() {
            *guard = token.into();
        }
    }

    fn auth_header(&self) -> String {
        let token = self.token.read().map(|t| t.clone()).unwrap_or_default();
        format!("Bearer {token}")
    }

    async fn ok_or_err(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(ApiError::from_response(resp).await)
        }
    }

    /// Issue an authenticated GET to `path` (e.g. `/v1/manifest/version`) and
    /// return the response object on success. Other modules use this when
    /// their only interaction with the API is "GET this URL, decode JSON".
    pub async fn http_get(&self, path: &str) -> Result<reqwest::Response> {
        let resp = self
            .http
            .get(self.url(path))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Ok(Self::ok_or_err(resp).await?)
    }

    pub async fn whoami(&self) -> Result<Whoami> {
        let resp = self
            .http
            .get(self.url("/v1/auth/whoami"))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn health(&self) -> Result<Health> {
        let resp = self.http.get(self.url("/v1/health")).send().await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    /// Resolve (and cache) the server's deployment mode from `/v1/health`.
    /// `Some("cloud")` selects the Hoard Cloud protocol; `None` means
    /// self-hosted. A failed probe returns `None` *without* caching so the
    /// next call retries.
    pub async fn server_mode(&self) -> Option<String> {
        self.mode
            .get_or_try_init(|| async { self.health().await.map(|h| h.mode) })
            .await
            .ok()
            .cloned()
            .flatten()
    }

    /// True when the server is the SaaS (`api.hoard.services`) deployment,
    /// which speaks the `/v1/cloud/*` protocol instead of the self-hosted
    /// `/v1/saves` + multipart one.
    pub async fn is_cloud(&self) -> bool {
        self.server_mode().await.as_deref() == Some("cloud")
    }

    // ---- Cloud (SaaS) protocol -----------------------------------------

    /// `POST /v1/cloud/saves` — declare upload intent. The server validates
    /// plan + quota, mints a presigned R2 PUT URL, and returns the version
    /// number the client must `commit` against.
    pub async fn cloud_init_upload(&self, init: &CloudUploadInit) -> Result<CloudUploadInitOut> {
        let resp = self
            .http
            .post(self.url("/v1/cloud/saves"))
            .header("authorization", self.auth_header())
            .json(init)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// Upload bytes directly to a presigned R2 URL. No `Authorization`
    /// header — the presigned URL carries its own signature in the query
    /// string, and an extra auth header breaks the S3 v4 signature.
    pub async fn put_presigned(
        &self,
        presigned: &PresignedUrl,
        body: reqwest::Body,
        content_length: u64,
    ) -> Result<()> {
        let method = reqwest::Method::from_bytes(presigned.method.as_bytes())
            .unwrap_or(reqwest::Method::PUT);
        let resp = self
            .upload_http
            .request(method, &presigned.url)
            .header(reqwest::header::CONTENT_LENGTH, content_length)
            .body(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("storage upload failed ({status}): {text}");
        }
        Ok(())
    }

    /// `POST /v1/cloud/saves/:id/versions/:n/commit` — finalize an upload.
    /// The server verifies the object via R2 HEAD and records the sha256.
    pub async fn cloud_commit(
        &self,
        save_id: &str,
        version: i64,
        commit: &CloudUploadCommit,
    ) -> Result<CloudUploadCommitOut> {
        let resp = self
            .http
            .post(self.url(&format!(
                "/v1/cloud/saves/{save_id}/versions/{version}/commit"
            )))
            .header("authorization", self.auth_header())
            .json(commit)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// `GET /v1/cloud/saves/:id/versions/:n/download` — mint a presigned R2
    /// GET URL plus the version's sha256/size for verification.
    pub async fn cloud_download(&self, save_id: &str, version: i64) -> Result<CloudDownloadOut> {
        let resp = self
            .http
            .get(self.url(&format!(
                "/v1/cloud/saves/{save_id}/versions/{version}/download"
            )))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// `GET /v1/cloud/sync` — the manifest of the user's saves (latest
    /// version of each). The cloud analogue of `list_saves`; excludes
    /// `backup_only` saves. Sends the device fingerprint so the server's
    /// poll guard can rate-limit per machine instead of per account.
    pub async fn cloud_sync(&self) -> Result<CloudManifest> {
        let dev = crate::logship::device_identity();
        let resp = self
            .http
            .get(self.url("/v1/cloud/sync"))
            .header("authorization", self.auth_header())
            .header("x-hoard-device-fp", &dev.fingerprint)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// `GET /v1/cloud/saves/:save_id/versions` — the full version history of a
    /// cloud save (every committed version, newest first). The cloud analogue
    /// of `list_save_snapshots`; the sync manifest only carries the latest.
    pub async fn cloud_list_versions(
        &self,
        save_id: &str,
        include_deleted: bool,
    ) -> Result<Vec<Snapshot>> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/cloud/saves/{save_id}/versions")))
            .query(&[("include_deleted", include_deleted)])
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// `DELETE /v1/cloud/saves/:save_id/versions/:version` — drop a single
    /// version (blob + row) and repoint `latest_version_num` to the highest
    /// remaining version. Deletes the whole save only if none remain.
    pub async fn cloud_delete_version(&self, save_id: &str, version: i64) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/v1/cloud/saves/{save_id}/versions/{version}")))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(())
    }

    /// Current "max versions per save" cap for the logged-in user. `None` =
    /// unlimited. Self-hosted reads it off `whoami`; cloud reads the
    /// `max_versions` field of `/v1/me` (other fields ignored).
    pub async fn get_max_versions(&self) -> Result<Option<i64>> {
        if self.is_cloud().await {
            #[derive(Deserialize)]
            struct MeMaxVersions {
                #[serde(default)]
                max_versions: Option<i64>,
            }
            let resp = self.http_get("/v1/me").await?;
            let me: MeMaxVersions = resp.json().await?;
            return Ok(me.max_versions);
        }
        Ok(self.whoami().await?.max_versions)
    }

    /// `PUT /v1/me/max-versions` — set (`Some(n)`) or clear (`None`) the
    /// per-user cap on stored versions per save. Both server modes mount the
    /// same path; both prune immediately, so the freed space is visible on
    /// the next quota poll.
    pub async fn set_max_versions(&self, max_versions: Option<i64>) -> Result<()> {
        let resp = self
            .http
            .put(self.url("/v1/me/max-versions"))
            .header("authorization", self.auth_header())
            .json(&MaxVersionsBody {
                max_versions,
                dry_run: false,
            })
            .send()
            .await?;
        Self::ok_or_err(resp).await?;
        Ok(())
    }

    /// Dry-run of [`set_max_versions`]: how many stored versions a cap of
    /// `max_versions` would delete right now. Nothing is written. Frontends
    /// call this first and ask for confirmation when the count is > 0.
    pub async fn preview_max_versions(&self, max_versions: i64) -> Result<i64> {
        let resp = self
            .http
            .put(self.url("/v1/me/max-versions"))
            .header("authorization", self.auth_header())
            .json(&MaxVersionsBody {
                max_versions: Some(max_versions),
                dry_run: true,
            })
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        let out: MaxVersionsResponse = resp.json().await?;
        Ok(out.pruned as i64)
    }

    /// `DELETE /v1/cloud/saves/:save_id` — remove a cloud save and all of its
    /// versions so the user reclaims storage. The cloud analogue of deleting
    /// a whole tracked save.
    pub async fn cloud_save_delete(&self, save_id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/v1/cloud/saves/{save_id}")))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(())
    }

    /// GET the bytes behind a presigned download URL as a streaming response.
    /// No auth header, same rationale as [`put_presigned`].
    ///
    /// Streams the body, so it belongs on `download_http`: on `http` the 60 s
    /// total timeout also covered the streaming, and every Cloud restore of a
    /// save too big to land inside a minute died mid-body with "operation timed
    /// out" — forever, since the next attempt was no faster.
    pub async fn get_presigned(&self, presigned: &PresignedUrl) -> Result<reqwest::Response> {
        let method = reqwest::Method::from_bytes(presigned.method.as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let resp = self
            .download_http
            .request(method, &presigned.url)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("storage download failed ({status}): {text}");
        }
        Ok(resp)
    }

    /// `POST /v1/cloud/cas/init` — declare a content-addressed upload. Returns
    /// the new version number plus the subset of blobs the server is missing,
    /// each with a presigned PUT URL.
    pub async fn cloud_cas_init(&self, init: &CloudCasInit) -> Result<CloudCasInitOut> {
        let resp = self
            .http
            .post(self.url("/v1/cloud/cas/init"))
            .header("authorization", self.auth_header())
            .json(init)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// `POST /v1/cloud/saves/:id/versions/:n/cas/commit` — finalize a content-
    /// addressed upload once every missing blob has been PUT.
    pub async fn cloud_cas_commit(
        &self,
        save_id: &str,
        version: i64,
    ) -> Result<CloudUploadCommitOut> {
        let resp = self
            .http
            .post(self.url(&format!(
                "/v1/cloud/saves/{save_id}/versions/{version}/cas/commit"
            )))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// `GET /v1/cloud/saves/:id/versions/:n/manifest` — the per-file manifest
    /// of a content-addressed version. With `presign = true` each file carries
    /// a download URL (restore) and bandwidth is charged; with `false` it's a
    /// cheap listing (History detail). Returns `content_addressed = false` for
    /// legacy archive versions.
    pub async fn cloud_version_manifest(
        &self,
        save_id: &str,
        version: i64,
        presign: bool,
    ) -> Result<CloudVersionManifestOut> {
        let resp = self
            .http
            .get(self.url(&format!(
                "/v1/cloud/saves/{save_id}/versions/{version}/manifest"
            )))
            .query(&[("presign", presign)])
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    /// `POST /v1/presence/heartbeat` — latido de presencia (Cloud). Lleva los
    /// mismos headers de identidad de device que `/v1/me`, porque el server
    /// resuelve la fila de `devices` por `x-hoard-device-fp` (y puede hasta
    /// registrarla si el primer contacto de una máquina es este latido — el
    /// caso del daemon headless que nunca pasa por `/v1/me`).
    pub async fn presence_heartbeat(&self, playing: &[PlayingBeat], closing: bool) -> Result<()> {
        let body = serde_json::json!({ "closing": closing, "playing": playing });
        let dev = crate::logship::device_identity();
        let mut req = self
            .http
            .post(self.url("/v1/presence/heartbeat"))
            .header("authorization", self.auth_header())
            .header("x-hoard-device-fp", &dev.fingerprint)
            .header("x-hoard-device-os", &dev.os)
            .header("x-hoard-app-version", env!("CARGO_PKG_VERSION"));
        if let Some(name) = dev.name.as_deref() {
            req = req.header("x-hoard-device-name", name);
        }
        let resp = req.json(&body).send().await?;
        Self::ok_or_err(resp).await?;
        Ok(())
    }

    /// `GET /v1/devices` — los dispositivos de la cuenta con su presencia en
    /// vivo (online, jugando qué, desde cuándo). El header de fingerprint va
    /// para que el server marque `this_device` y la UI filtre sin conocer su
    /// propio UUID.
    pub async fn list_devices(&self) -> Result<DeviceListOut> {
        let dev = crate::logship::device_identity();
        let resp = self
            .http
            .get(self.url("/v1/devices"))
            .header("authorization", self.auth_header())
            .header("x-hoard-device-fp", &dev.fingerprint)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    /// `GET /v1/notifications` — broadcasts del operador para la campana.
    /// `since` es el cursor RFC3339 del cliente: solo vuelven filas
    /// estrictamente posteriores, así nada se re-entrega tras un reinicio.
    /// El fingerprint va para que el poll guard del server limite por
    /// máquina y no por cuenta.
    pub async fn list_notifications(&self, since: Option<&str>) -> Result<NotificationListOut> {
        let dev = crate::logship::device_identity();
        let mut req = self
            .http
            .get(self.url("/v1/notifications"))
            .header("authorization", self.auth_header())
            .header("x-hoard-device-fp", &dev.fingerprint);
        if let Some(s) = since {
            req = req.query(&[("since", s)]);
        }
        let resp = Self::ok_or_err(req.send().await?).await?;
        Ok(resp.json().await?)
    }

    pub async fn list_games(&self, query: Option<&str>) -> Result<Vec<Game>> {
        let mut req = self
            .http
            .get(self.url("/v1/games"))
            .header("authorization", self.auth_header());
        if let Some(q) = query {
            req = req.query(&[("search", q)]);
        }
        let resp = Self::ok_or_err(req.send().await?).await?;
        Ok(resp.json().await?)
    }

    /// Paginated catalog fetch. Used by detection to walk the full ~11k-entry
    /// games table without blowing the per-request size budget. The server
    /// caps `limit` at 1000.
    pub async fn list_games_paged(&self, limit: u32, offset: u32) -> Result<Vec<Game>> {
        let resp = self
            .http
            .get(self.url("/v1/games"))
            .header("authorization", self.auth_header())
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())])
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn get_game(&self, slug: &str) -> Result<Game> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/games/{}", slug)))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn list_saves(&self, game: Option<&str>) -> Result<Vec<Save>> {
        let mut req = self
            .http
            .get(self.url("/v1/saves"))
            .header("authorization", self.auth_header());
        if let Some(g) = game {
            req = req.query(&[("game_slug", g)]);
        }
        let resp = Self::ok_or_err(req.send().await?).await?;
        Ok(resp.json().await?)
    }

    pub async fn create_save(&self, game_slug: &str, label: &str) -> Result<Save> {
        self.create_save_with_meta(game_slug, label, None, None)
            .await
    }

    /// Create a Save and, optionally, hint to the server what the game's
    /// display name / Steam ID are. Used by the desktop client so that
    /// servers running an older Ludusavi catalog can self-heal a missing
    /// games row from the metadata the desktop already has at hand. Older
    /// servers ignore the extra fields (Serde tolerates unknown keys), so
    /// this is forward-compatible.
    pub async fn create_save_with_meta(
        &self,
        game_slug: &str,
        label: &str,
        display_name: Option<&str>,
        steam_app_id: Option<i64>,
    ) -> Result<Save> {
        let body = CreateSaveRequest {
            // La puerta de `GameSlug`: un slug envenenado no llega a crear una
            // fila server-side (ADR 0021 C.3). Los slugs del cliente salen todos
            // de `slugify`, así que esto sólo dispara con datos corruptos.
            game_slug: GameSlug::parse(game_slug)
                .with_context(|| format!("slug inválido al crear el save: {game_slug:?}"))?,
            label: Some(label.to_string()),
            local_path_hint: None,
            client_os: None,
            display_name: display_name.map(str::to_string),
            steam_app_id,
        };
        let resp = self
            .http
            .post(self.url("/v1/saves"))
            .header("authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn get_save(&self, save_id: &str) -> Result<Save> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/saves/{}", save_id)))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn delete_save(&self, save_id: &str) -> Result<()> {
        // Cloud speaks a different namespace (`/v1/cloud/saves/*`); the
        // self-hosted `DELETE /v1/saves/{id}` isn't mounted there and 404s,
        // which the UI mistranslates as "save no longer exists" ??? leaving
        // Cloud users unable to delete anything. Branch on the server mode.
        if self.is_cloud().await {
            return self.cloud_save_delete(save_id).await;
        }
        let resp = self
            .http
            .delete(self.url(&format!("/v1/saves/{}", save_id)))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Self::ok_or_err(resp).await?;
        Ok(())
    }

    /// Rename the label on an existing save. Surfaces 409 via
    /// [`ApiError::Conflict`] so the UI can show a "label already exists"
    /// message instead of a generic server error.
    pub async fn rename_save_label(&self, save_id: &str, new_label: &str) -> Result<Save> {
        // Same namespace split as `delete_save`: the self-hosted PATCH
        // isn't mounted on Cloud. Branch so both paths work.
        if self.is_cloud().await {
            return self.cloud_rename_save_label(save_id, new_label).await;
        }
        let body = PatchSaveRequest {
            label: Some(new_label.to_string()),
            ..PatchSaveRequest::default()
        };
        let resp = self
            .http
            .patch(self.url(&format!("/v1/saves/{}", save_id)))
            .header("authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    /// `PATCH /v1/cloud/saves/:save_id` ??? rename the label on a cloud save.
    /// The cloud analogue of `rename_save_label`; the server enforces
    /// `UNIQUE(user_id, game_slug, label)` and returns 409 on collision.
    pub async fn cloud_rename_save_label(&self, save_id: &str, new_label: &str) -> Result<Save> {
        let body = serde_json::json!({ "label": new_label });
        let resp = self
            .http
            .patch(self.url(&format!("/v1/cloud/saves/{save_id}")))
            .header("authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }

    pub async fn list_snapshots(
        &self,
        save_id: &str,
        include_deleted: bool,
    ) -> Result<Vec<Snapshot>> {
        let mut req = self
            .http
            .get(self.url(&format!("/v1/saves/{}/snapshots", save_id)))
            .header("authorization", self.auth_header());
        if include_deleted {
            req = req.query(&[("include_deleted", "true")]);
        }
        let resp = Self::ok_or_err(req.send().await?).await?;
        Ok(resp.json().await?)
    }

    pub async fn snapshot_detail(&self, save_id: &str, version: i64) -> Result<SnapshotDetail> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/saves/{}/snapshots/{}", save_id, version)))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn snapshot_download(
        &self,
        save_id: &str,
        version: i64,
    ) -> Result<reqwest::Response> {
        let resp = self
            .download_http
            .get(self.url(&format!(
                "/v1/saves/{}/snapshots/{}/download",
                save_id, version
            )))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Self::ok_or_err(resp)
            .await
            .map_err(|e| anyhow!(e))
            .context("download request failed")
    }

    pub async fn snapshot_delete(&self, save_id: &str, version: i64) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/v1/saves/{}/snapshots/{}", save_id, version)))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Self::ok_or_err(resp).await?;
        Ok(())
    }

    pub async fn snapshot_restore(&self, save_id: &str, version: i64) -> Result<()> {
        let resp = self
            .http
            .post(self.url(&format!(
                "/v1/saves/{}/snapshots/{}/restore",
                save_id, version
            )))
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Self::ok_or_err(resp).await?;
        Ok(())
    }

    /// Upload a multipart snapshot. Each part is a file: name=`relative/path`, body bytes.
    /// Returns the created snapshot summary.
    pub async fn snapshot_upload(
        &self,
        save_id: &str,
        form: reqwest::multipart::Form,
    ) -> Result<Snapshot> {
        let resp = self
            .upload_http
            .post(self.url(&format!("/v1/saves/{}/snapshots", save_id)))
            .header("authorization", self.auth_header())
            .multipart(form)
            .send()
            .await?;
        let resp = Self::ok_or_err(resp).await.map_err(|e| anyhow!(e))?;
        Ok(resp.json().await?)
    }
}

// ---- DTOs ---------------------------------------------------------------
//
// El contrato self-hosted vive en `hoard_core::wire` (ADR 0021 C.6): el server
// compila contra las mismas formas, así que un drift entre las dos puntas es un
// error de compilación en vez de un 422 en producción. Se re-exportan aquí para
// que `api::Save` y compañía sigan siendo las rutas públicas de siempre.

pub use hoard_core::wire::{
    CreateSaveRequest, Game, Health, MaxVersionsBody, MaxVersionsResponse, PatchSaveRequest, Save,
    Snapshot, SnapshotDetail, SnapshotFile,
};
pub use hoard_core::wire::{LogBatch, LogEntry, LogIngestResponse, Whoami};

// ---- Cloud (SaaS) protocol DTOs ----------------------------------------

/// Body for `POST /v1/cloud/saves`. Mirrors `hoard-server`'s `UploadInit`.
#[derive(Debug, Clone, Serialize)]
pub struct CloudUploadInit {
    pub save_id: String,
    pub game_slug: String,
    pub label: Option<String>,
    pub size_bytes: u64,
    /// Files inside the packed tar.zst. The server stores it verbatim so the
    /// History view can show "N archivos" (the blob is opaque server-side).
    pub file_count: i64,
    pub device_name: Option<String>,
    pub notes: Option<String>,
    pub backup_only: bool,
    /// Last-synced version for this save. Drives the server's fast-forward
    /// check: a mismatch means another device pushed since, → 409.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_version: Option<i64>,
}

/// A short-lived presigned R2 URL (PUT for upload, GET for download).
#[derive(Debug, Clone, Deserialize)]
pub struct PresignedUrl {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudQuotaInfo {
    pub plan: String,
    pub used_bytes: u64,
    pub limit_bytes: u64,
    #[serde(default)]
    pub devices_used: u32,
    #[serde(default)]
    pub devices_limit: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudUploadInitOut {
    pub version_num: i64,
    pub r2_key: String,
    pub upload: PresignedUrl,
    pub quota: CloudQuotaInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudUploadCommit {
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudUploadCommitOut {
    pub save_id: String,
    pub version_num: i64,
    pub committed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudDownloadOut {
    pub save_id: String,
    pub version_num: i64,
    pub sha256: String,
    pub size_bytes: i64,
    pub download: PresignedUrl,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudManifestEntry {
    pub save_id: String,
    pub game_slug: String,
    pub label: String,
    pub latest_version_num: i64,
    #[serde(default)]
    pub latest_parent_version: Option<i64>,
    #[serde(default)]
    pub latest_size_bytes: i64,
    /// Files in the latest version (0 = unknown / pre-file-count server).
    #[serde(default)]
    pub latest_file_count: i64,
    #[serde(default)]
    pub latest_sha256: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudManifest {
    #[serde(default)]
    pub generated_at: String,
    pub saves: Vec<CloudManifestEntry>,
}

// ---- Cloud content-addressed (per-file dedup) DTOs ---------------------

/// One file in a content-addressed upload manifest. Mirrors the server's
/// `CasFileEntry`.
#[derive(Debug, Clone, Serialize)]
pub struct CloudCasFileEntry {
    pub relative_path: String,
    pub sha256: String,
    pub size_bytes: i64,
    /// Source file mtime (unix seconds), preserved on restore. `None` if the
    /// FS didn't report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,
}

/// Body for `POST /v1/cloud/cas/init`. The client declares the whole-file
/// manifest; the server replies with the subset of blobs it doesn't have.
#[derive(Debug, Clone, Serialize)]
pub struct CloudCasInit {
    pub save_id: String,
    pub game_slug: String,
    pub label: Option<String>,
    pub device_name: Option<String>,
    pub notes: Option<String>,
    pub backup_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_version: Option<i64>,
    pub files: Vec<CloudCasFileEntry>,
}

/// A blob the server is missing — the client must PUT it to `upload`.
#[derive(Debug, Clone, Deserialize)]
pub struct CloudCasMissingBlob {
    pub sha256: String,
    pub size_bytes: i64,
    #[serde(default)]
    pub r2_key: String,
    pub upload: PresignedUrl,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudCasInitOut {
    /// Canonical cloud save id (servers ≥ 2.3.2). Differs from the requested
    /// id when (user, game_slug, label) already maps to another cloud save —
    /// the commit must target this id or it 404s. `None` on older servers.
    #[serde(default)]
    pub save_id: Option<String>,
    pub version_num: i64,
    pub missing: Vec<CloudCasMissingBlob>,
    #[allow(dead_code)]
    pub quota: CloudQuotaInfo,
}

/// One file in a version manifest. `download` is present only when the
/// manifest was requested with `presign=true` (the restore path).
#[derive(Debug, Clone, Deserialize)]
pub struct CloudManifestFile {
    pub relative_path: String,
    pub sha256: String,
    pub size_bytes: i64,
    #[serde(default)]
    pub modified_at: Option<i64>,
    #[serde(default)]
    pub download: Option<PresignedUrl>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudVersionManifestOut {
    /// False for legacy archive versions — the caller must fall back to the
    /// whole-archive `cloud_download` path.
    #[serde(default)]
    pub content_addressed: bool,
    #[serde(default)]
    pub files: Vec<CloudManifestFile>,
}

/// Un juego en un latido de presencia: slug + segundos que lleva corriendo.
/// Duración y no timestamp: el server la ancla a su propio reloj, inmune a
/// relojes de cliente desviados.
#[derive(Debug, Clone, Serialize)]
pub struct PlayingBeat {
    pub slug: String,
    pub for_secs: u64,
}

/// Un juego corriendo en un device (`GET /v1/devices`): slug + RFC3339 del
/// arranque de la sesión.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePlaying {
    pub slug: String,
    #[serde(default)]
    pub since: Option<String>,
}

/// Un dispositivo de la cuenta con su presencia en vivo (`GET /v1/devices`).
/// Serialize además de Deserialize: el desktop lo reemite tal cual como
/// payload del evento Tauri `hoard://devices`.
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
    /// Heartbeat fresco y sin beat de cierre.
    #[serde(default)]
    pub online: bool,
    /// Juegos corriendo ahora mismo (el más reciente primero); solo viene si
    /// `online`. Vacío = idle.
    #[serde(default)]
    pub playing: Vec<DevicePlaying>,
    /// True en la fila que corresponde al fingerprint del caller.
    #[serde(default)]
    pub this_device: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceListOut {
    pub devices: Vec<DeviceOut>,
}

/// Un broadcast del operador (`GET /v1/notifications`). Mismo shape que el
/// `ServerNotification` que espera la UI (stores/notifications.ts) más
/// `created_at` para el cursor del cliente.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationOut {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub action_url: Option<String>,
    #[serde(default)]
    pub action_label: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationListOut {
    pub notifications: Vec<NotificationOut>,
}
