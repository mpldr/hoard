//! Subida direccionada por contenido, self-hosted (`/v1/saves/{id}/cas/*`).
//!
//! # Por qué existe
//!
//! El almacenamiento de Hoard deduplica desde la ADR 0018: cada fichero se
//! guarda una vez por usuario bajo su sha256 y una versión no es más que su
//! lista de referencias. Lo que **no** deduplicaba self-hosted era la
//! transmisión. El único camino de entrada era el multipart de
//! `POST /v1/saves/{id}/snapshots`, que se traga la carpeta entera en cada
//! copia: el server recibía 3 GB, los escribía en `tmp/`, los hasheaba, y
//! descubría que ya tenía 2,99 GB. Los tiraba y se quedaba con los 10 MB
//! nuevos.
//!
//! Eso cuesta tres cosas a la vez: el ancho de banda de subida del usuario, el
//! espacio en `tmp/` para una copia completa, y —la que de verdad rompía— el
//! límite de cuerpo de la petición. `storage.max_snapshot_size_mb` y cualquier
//! proxy inverso por delante ven una única petición del tamaño de la partida
//! entera, así que una partida grande devolvía un 413 que no se podía arreglar
//! sin subir el tope. Hoard Cloud no tenía ese problema porque negocia el
//! contenido desde el principio; self-hosted arrastraba el protocolo viejo.
//!
//! # El protocolo
//!
//! 1. `POST /v1/saves/{id}/cas/init` — el cliente declara el manifiesto
//!    (ruta, sha, tamaño de cada fichero). El server contesta **qué shas no
//!    tiene** y abre un área de staging.
//! 2. `PUT /v1/cas/blobs/{upload_id}/{sha}` — un blob que falta, un cuerpo.
//!    El server lo escribe en staging verificando el hash mientras entra.
//! 3. `POST /v1/saves/{id}/cas/commit` — el manifiesto otra vez; el server
//!    coloca los blobs nuevos, escribe las filas y avanza la cabeza.
//!
//! # Dónde se aparta de cloud (y por qué)
//!
//! Cloud firma URLs presignadas de R2 y el cliente escribe **directamente en el
//! bucket**. Aquí no: la ADR 0020 dice que el cliente self-hosted nunca habla
//! con el almacenamiento, porque el backend puede ser disco local, MinIO, un
//! `rclone serve s3` sobre OneDrive… El server siempre está en medio, así que
//! el paso 2 es un PUT contra el propio server.
//!
//! Cloud además reserva la versión en el `init` con una fila pendiente
//! (`sha256 = ''`) y la confirma después. Aquí el `init` **no escribe nada en la
//! base**: es una consulta. El manifiesto vuelve a viajar en el commit, que es
//! quien asigna el número de versión bajo la misma transacción que comprueba la
//! cabeza. Un init abandonado no deja fila pendiente que limpiar ni versión
//! fantasma en el historial; lo único que deja son bytes en `tmp/`, que ya
//! barre `retention.tmp_cleanup_hours`.
//!
//! # Trocear sigue siendo cosa del server
//!
//! Un fichero por encima de `chunking::CHUNK_THRESHOLD` se parte en trozos de
//! contenido igual que en el multipart (ADR 0019), y por el mismo motivo: una
//! partida monolítica que reescribe unos KB por versión no debe re-almacenar el
//! fichero entero. El cliente no lo sabe ni le hace falta — negocia por
//! fichero completo, y el server decide cómo lo guarda.
//!
//! La otra cara: un fichero ya troceado no tiene fila en `blobs`, así que
//! preguntar sólo esa tabla lo daría por ausente y el cliente lo volvería a
//! subir entero cada vez. [`stored_representation`] mira también los
//! `snapshot_files` del usuario, y el commit **copia la lista de trozos** de la
//! versión vieja en vez de pedir los bytes.

use axum::{
    body::Body,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::Json,
};
use futures::StreamExt;
use hoard_core::wire::{CasCommit, CasFile, CasInit, CasInitOut, CasMissing, Snapshot};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::routes::health::ServerState;
use crate::routes::snapshots::{
    blob_in_db, chunk_in_db, err, internal, internal_logged, is_safe_relative_path,
    ownership_check, prune_over_version_cap, snapshot_too_large, snapshot_too_large_declared,
};

type ApiError = (StatusCode, Json<serde_json::Value>);

/// Tope de ficheros por versión. El mismo que el modo empaquetado del multipart:
/// aquí tampoco hay un handle ni un round-trip por fichero, así que lo que se
/// acota es el tamaño de la transacción, no el coste de la transferencia.
const MAX_FILES: usize = 50_000;

/// Cómo tiene el server guardado ya un contenido concreto.
#[derive(Debug, Clone)]
enum Stored {
    /// Blob de fichero entero, con fila en `blobs`.
    Blob { size_bytes: i64 },
    /// Troceado (ADR 0019). `file_id` es una fila de `snapshot_files` de la que
    /// copiar la lista ordenada de trozos.
    Chunks { size_bytes: i64, file_id: String },
}

impl Stored {
    fn size_bytes(&self) -> i64 {
        match self {
            Stored::Blob { size_bytes } | Stored::Chunks { size_bytes, .. } => *size_bytes,
        }
    }
}

/// ¿Tiene ya el server los bytes de este sha para este usuario, y en qué forma?
///
/// Consulta `blobs` primero (el caso normal) y, si no está, busca un
/// `snapshot_files` del usuario con ese sha que tenga trozos. Se incluyen a
/// propósito los snapshots en la papelera: un snapshot borrado sigue sujetando
/// sus bytes contra la cuota hasta que la purga los libera, así que su contenido
/// está disponible y referenciarlo es correcto.
async fn stored_representation(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    sha: &str,
) -> Result<Option<Stored>, sqlx::Error> {
    if let Some(size) =
        sqlx::query_scalar::<_, i64>("SELECT size_bytes FROM blobs WHERE user_id=? AND sha256=?")
            .bind(user_id)
            .bind(sha)
            .fetch_optional(pool)
            .await?
    {
        return Ok(Some(Stored::Blob { size_bytes: size }));
    }

    let row = sqlx::query(
        "SELECT sf.id AS id, sf.size_bytes AS size_bytes
           FROM snapshot_files sf
           JOIN snapshots s ON s.id = sf.snapshot_id
           JOIN saves sv ON sv.id = s.save_id
          WHERE sv.user_id = ? AND sf.sha256 = ?
            AND EXISTS (SELECT 1 FROM snapshot_file_chunks c WHERE c.snapshot_file_id = sf.id)
          LIMIT 1",
    )
    .bind(user_id)
    .bind(sha)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| Stored::Chunks {
        size_bytes: r.get("size_bytes"),
        file_id: r.get("id"),
    }))
}

/// Carpeta de staging de una subida. Vive al ras de `tmp/` para que el barrido
/// por antigüedad de `cleanup::purge_tmp` (que sólo mira las entradas de primer
/// nivel) recoja las subidas abandonadas.
fn staging_dir(data_dir: &std::path::Path, upload_id: &str) -> PathBuf {
    data_dir.join("tmp").join(format!("cas-{upload_id}"))
}

/// El dueño de un área de staging, escrito al abrirla.
///
/// El `upload_id` es un UUID v4 que mina el server, así que adivinarlo no es
/// realista — pero "no es realista" no es un control de acceso. Con el dueño en
/// disco, subir a la subida de otro es imposible por construcción y no depende
/// de que nadie filtre un id.
fn owner_file(dir: &std::path::Path) -> PathBuf {
    dir.join("owner")
}

/// Valida un `upload_id` **antes** de meterlo en una ruta de fichero. Sólo
/// UUIDs: nada de `..`, separadores ni sorpresas.
fn valid_upload_id(s: &str) -> bool {
    Uuid::parse_str(s).is_ok()
}

/// sha256 canónico en hexadecimal minúsculo. Se interpola en claves de
/// almacenamiento y en rutas de staging, así que se comprueba antes.
fn valid_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Manifiesto → shas únicos, con el tamaño declarado de cada uno. Un mismo
/// contenido repetido en varias rutas se sube una sola vez.
fn unique_shas(files: &[CasFile]) -> Vec<(String, i64)> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for f in files {
        if seen.insert(f.sha256.as_str()) {
            out.push((f.sha256.as_str().to_string(), f.size_bytes.max(0)));
        }
    }
    out
}

/// Comprobaciones que valen para el init y para el commit: manifiesto no vacío,
/// dentro del tope de ficheros, rutas seguras.
fn validate_manifest(files: &[CasFile]) -> Result<(), ApiError> {
    if files.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty manifest"));
    }
    if files.len() > MAX_FILES {
        return Err(err(StatusCode::BAD_REQUEST, "too many files in snapshot"));
    }
    if let Some(bad) = files
        .iter()
        .find(|f| !is_safe_relative_path(&f.relative_path))
    {
        warn!(path = %bad.relative_path, "cas: unsafe relative path in manifest");
        return Err(err(StatusCode::BAD_REQUEST, "unsafe file path"));
    }
    Ok(())
}

// ─── POST /v1/saves/:save_id/cas/init ───────────────────────────────────────

pub async fn init(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
    Json(body): Json<CasInit>,
) -> Result<Json<CasInitOut>, ApiError> {
    let user_id = user.user_id.to_string();
    ownership_check(&state.pool, &save_id, &user_id)
        .await
        .map_err(|e| internal_logged("ownership lookup", e))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "save not found"))?;

    validate_manifest(&body.files)?;

    // El tope por versión se mide sobre el tamaño **lógico** de la partida, no
    // sobre lo que se vaya a transmitir. Es la misma promesa que hace el
    // multipart y la que el operador cree estar configurando: "no guardes
    // versiones de más de X". Que ahora los bytes viajen troceados no cambia lo
    // que ocupa la versión.
    let logical: i64 = body.files.iter().map(|f| f.size_bytes.max(0)).sum();
    let max_per_snapshot = (state.config.storage.max_snapshot_size_mb as i64) * 1024 * 1024;
    if logical > max_per_snapshot {
        // Aquí sí se sabe el tamaño real (lo declara el manifiesto), a
        // diferencia del multipart, que aborta a media transmisión y sólo puede
        // decir hasta dónde llegó. Va como `actual_bytes` justamente por eso: de
        // aquí no ha salido ni un byte todavía, así que darlo como "recibido"
        // hacía que el cliente dijera "3,6 GB enviados antes de parar".
        return Err(snapshot_too_large_declared(max_per_snapshot, logical));
    }

    // Rechazar el non-fast-forward **antes** de mover un byte. En el multipart
    // esta comprobación llega después de haber subido la partida entera; aquí
    // es lo primero, que es media razón para tener un `init`.
    let head: i64 = sqlx::query_scalar("SELECT latest_version_num FROM saves WHERE id=?")
        .bind(&save_id)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| internal_logged("reading the save's latest version", e))?;
    if let Some(base) = body.base_version {
        // Una base que no cuadra con la cabeza se rechaza para que un push no
        // entierre una versión que no llegó a ver. Pero un manifiesto que trae
        // *entera* esa versión no puede enterrarla: su contenido sigue ahí,
        // fichero a fichero, en la que se va a escribir. Es el mismo juicio que
        // hace el agente cuando reconcilia y descubre que su carpeta ya
        // contiene la cabeza — hecho aquí, con el manifiesto que ya viene en la
        // petición, para los clientes que no saben hacerlo solos: leer la
        // cabeza del cuerpo de un 409 es de ago-2026, y antes de eso un rechazo
        // los dejaba sabiendo que divergían pero no de qué.
        if base != head && !manifest_covers_head(&state.pool, &save_id, head, &body.files).await? {
            return Err(non_fast_forward(&save_id, head, base));
        }
        if base != head {
            tracing::warn!(
                %save_id,
                head_version = head,
                base_version = base,
                "cas init: la base diverge pero el manifiesto trae la cabeza entera — se deja pasar"
            );
        }
    }

    // Qué falta. Los tamaños que se apuntan son los que el cliente declara —
    // sólo se usan para la barra de progreso y para el aviso de cuota de aquí
    // abajo; el cargo de verdad lo hace el commit con lo que haya aterrizado.
    let mut missing = Vec::new();
    let mut missing_bytes: i64 = 0;
    for (sha, size) in unique_shas(&body.files) {
        if !valid_sha256(&sha) {
            return Err(err(StatusCode::BAD_REQUEST, "invalid sha256 in manifest"));
        }
        if stored_representation(&state.pool, &user_id, &sha)
            .await
            .map_err(|e| internal_logged("blob dedup lookup", e))?
            .is_none()
        {
            missing_bytes += size;
            missing.push(CasMissing {
                sha256: body
                    .files
                    .iter()
                    .find(|f| f.sha256.as_str() == sha)
                    .map(|f| f.sha256.clone())
                    .expect("sha came from this manifest"),
                size_bytes: size,
            });
        }
    }

    // Aviso temprano de cuota, con los tamaños declarados. No es la puerta —esa
    // está en el commit, contra los bytes reales— pero evita que alguien suba
    // 8 GB para que se los rechacen al final.
    let (quota, used): (i64, i64) =
        sqlx::query_as("SELECT storage_quota_bytes, storage_used_bytes FROM users WHERE id=?")
            .bind(&user_id)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| internal_logged("quota lookup", e))?;
    if used + missing_bytes > quota {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "storage quota exceeded"));
    }

    let upload_id = Uuid::new_v4().to_string();
    let dir = staging_dir(&state.config.storage.data_dir, &upload_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| internal_logged("creating the upload tmp dir", e))?;
    tokio::fs::write(owner_file(&dir), &user_id)
        .await
        .map_err(|e| internal_logged("creating the upload tmp dir", e))?;

    info!(
        user = %user.username,
        save_id = %save_id,
        files = body.files.len(),
        missing = missing.len(),
        logical_bytes = logical,
        missing_bytes,
        "cas init"
    );

    Ok(Json(CasInitOut {
        upload_id,
        version_num: head + 1,
        missing,
        missing_bytes,
    }))
}

/// ¿Trae este push todo lo que tiene la cabeza?
///
/// Superconjunto **estricto**: tiene que traer la cabeza entera *y* algo suyo.
/// Quitar un fichero que la cabeza tiene es justo el entierro que el 409 existe
/// para impedir, y lo sigue recibiendo. Traer la cabeza y nada más es un cliente
/// que no tiene contenido nuevo que escribir — el agente se asienta sobre la
/// cabeza en vez de re-subirla, y acuñar aquí una versión idéntica sólo engorda
/// el historial de un equipo que lo único que perdió fue su sitio. Una cabeza
/// sin ficheros (versión a medias, o anterior al content-addressing) tampoco
/// concede nada: no hay con qué comparar.
async fn manifest_covers_head(
    pool: &sqlx::SqlitePool,
    save_id: &str,
    head: i64,
    files: &[CasFile],
) -> Result<bool, ApiError> {
    if head <= 0 {
        return Ok(false);
    }
    let head_files: Vec<(String, String)> = sqlx::query_as(
        "SELECT sf.relative_path, sf.sha256
           FROM snapshot_files sf
           JOIN snapshots s ON s.id = sf.snapshot_id
          WHERE s.save_id = ? AND s.version_num = ?",
    )
    .bind(save_id)
    .bind(head)
    .fetch_all(pool)
    .await
    .map_err(|e| internal_logged("reading the head's manifest", e))?;
    if head_files.is_empty() {
        return Ok(false);
    }
    let incoming: std::collections::HashSet<(&str, &str)> = files
        .iter()
        .map(|f| (f.relative_path.as_str(), f.sha256.as_str()))
        .collect();
    let covers_all = head_files
        .iter()
        .all(|(path, sha)| incoming.contains(&(path.as_str(), sha.as_str())));
    Ok(covers_all && incoming.len() > head_files.len())
}

/// The divergence 409. It carries `save_id` even though here it is always the
/// id the client asked for — self-hosted never relabels rows, the route already
/// names one — so the body has a single shape across both deployments: on Cloud
/// that field is the canonical row the push was rejected against, which may not
/// be the one the client thought it was writing to, and the client parses one
/// structure instead of branching on which server answered.
fn non_fast_forward(save_id: &str, head: i64, base: i64) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "non-fast-forward: another device advanced this save since your base version",
            "code": "non_fast_forward",
            "head_version": head,
            "base_version": base,
            "save_id": save_id,
        })),
    )
}

// ─── PUT /v1/cas/blobs/:upload_id/:sha256 ───────────────────────────────────

/// How much body we swallow as a courtesy before answering an error, while the
/// client is still writing.
///
/// This exists because answering and closing without reading is not free: hyper
/// closes the socket with data unconsumed, TCP sends RST, and **Windows throws
/// away the response already sitting in its receive buffer**. The client never
/// gets to see the 404 or the 413 — only an `error writing a body to
/// connection` (os error 10053/10054) that says nothing. Half of issue #17.
///
/// Capped, because the body can be gigabytes and swallowing all of it just to
/// be able to say "no" is exactly the work the error was trying to avoid. Past
/// the cap we stop and the client gets the same reset as before: no worse than
/// it was.
const MAX_DRAIN_BYTES: u64 = 8 * 1024 * 1024;

/// Empty whatever is left of the body (up to [`MAX_DRAIN_BYTES`]) and return the
/// error unchanged, so the response leaves through a socket the client can
/// still read. See [`MAX_DRAIN_BYTES`].
async fn drain_then(
    stream: &mut axum::body::BodyDataStream,
    error: ApiError,
    already_read: u64,
) -> ApiError {
    let mut drained = already_read;
    while drained < MAX_DRAIN_BYTES {
        match stream.next().await {
            Some(Ok(chunk)) => drained += chunk.len() as u64,
            // A body that dies on its own is no longer in the way: nothing to
            // drain.
            Some(Err(_)) | None => break,
        }
    }
    error
}

/// Un blob que falta. El cuerpo son los bytes en crudo; se escriben en staging
/// hasheando por el camino, y si el sha no cuadra con el que promete la URL el
/// fichero se borra y la petición se rechaza.
///
/// Verificar aquí y no en el commit no es cortesía: el cliente hashea el fichero
/// y **después** lo lee otra vez para mandarlo, y entre las dos lecturas el
/// juego puede haber rotado el save. Si nadie comprueba, el server acaba
/// guardando bytes nuevos bajo el sha de los viejos — un blob cuyo contenido no
/// es el que su nombre promete, que al restaurar devuelve otra partida sin que
/// nada se queje. Es la corrupción silenciosa de ago-2026; el cliente ya se
/// defiende hasheando lo que sale por el socket, y esta es la otra mitad.
pub async fn upload_blob(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path((upload_id, sha)): Path<(String, String)>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let user_id = user.user_id.to_string();
    // The body is taken as a stream before the first validation: every error
    // from here on has to empty it before answering, or the client never gets
    // to read the response. See `drain_then`.
    let mut stream = body.into_data_stream();

    if !valid_upload_id(&upload_id) {
        let e = err(StatusCode::BAD_REQUEST, "invalid upload id");
        return Err(drain_then(&mut stream, e, 0).await);
    }
    if !valid_sha256(&sha) {
        let e = err(StatusCode::BAD_REQUEST, "invalid sha256");
        return Err(drain_then(&mut stream, e, 0).await);
    }

    let dir = staging_dir(&state.config.storage.data_dir, &upload_id);
    let owner = match tokio::fs::read_to_string(owner_file(&dir)).await {
        Ok(o) => o,
        Err(_) => {
            let e = err(StatusCode::NOT_FOUND, "upload not found or expired");
            return Err(drain_then(&mut stream, e, 0).await);
        }
    };
    if owner != user_id {
        // Mismo cuerpo que "no existe": quien no es el dueño no debe poder
        // distinguir un id ajeno de uno inventado.
        let e = err(StatusCode::NOT_FOUND, "upload not found or expired");
        return Err(drain_then(&mut stream, e, 0).await);
    }

    let dest = dir.join(&sha);
    let max_per_blob = (state.config.storage.max_snapshot_size_mb as i64) * 1024 * 1024;

    let mut file = match tokio::fs::File::create(&dest).await {
        Ok(f) => f,
        Err(e) => {
            let e = internal_logged("creating the uploaded file", e);
            return Err(drain_then(&mut stream, e, 0).await);
        }
    };
    let mut hasher = Sha256::new();
    let mut size: i64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "cas blob stream error");
                let _ = tokio::fs::remove_file(&dest).await;
                return Err(err(StatusCode::BAD_REQUEST, "stream error"));
            }
        };
        size += chunk.len() as i64;
        if size > max_per_blob {
            let _ = tokio::fs::remove_file(&dest).await;
            // What we already read counts against the drain cap: a blob over
            // the server's limit is precisely the one not worth swallowing
            // whole just to reject it politely.
            let e = snapshot_too_large(max_per_blob, size);
            return Err(drain_then(&mut stream, e, size.max(0) as u64).await);
        }
        hasher.update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            warn!(error = %e, "cas blob write error");
            let _ = tokio::fs::remove_file(&dest).await;
            let e = internal_logged("writing the uploaded blob", e);
            return Err(drain_then(&mut stream, e, size.max(0) as u64).await);
        }
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(internal_logged("writing the uploaded blob", e));
    }
    drop(file);

    let actual = hex::encode(hasher.finalize());
    if actual != sha {
        warn!(
            declared = %sha,
            actual = %actual,
            bytes = size,
            "cas: uploaded blob does not hash to the sha it was announced under — rejected"
        );
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(err(
            StatusCode::BAD_REQUEST,
            "uploaded bytes do not match the declared sha256",
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

// ─── POST /v1/saves/:save_id/cas/commit ─────────────────────────────────────

/// Un objeto colocado en el almacén durante esta petición, para poder deshacerlo
/// si la transacción no llega a comprometerse.
struct Placed {
    key: String,
    sha: String,
    chunk: bool,
}

pub async fn commit(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
    Json(body): Json<CasCommit>,
) -> Result<(StatusCode, Json<Snapshot>), ApiError> {
    let user_id = user.user_id.to_string();
    ownership_check(&state.pool, &save_id, &user_id)
        .await
        .map_err(|e| internal_logged("ownership lookup", e))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "save not found"))?;
    validate_manifest(&body.files)?;
    if !valid_upload_id(&body.upload_id) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid upload id"));
    }

    let dir = staging_dir(&state.config.storage.data_dir, &body.upload_id);
    let owner = tokio::fs::read_to_string(owner_file(&dir))
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "upload not found or expired"))?;
    if owner != user_id {
        return Err(err(StatusCode::NOT_FOUND, "upload not found or expired"));
    }
    let cleanup_staging = || {
        let p = dir.clone();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_dir_all(&p).await;
        });
    };

    // ── Resolver cada sha: ¿lo acaban de subir, o ya lo teníamos? ───────────
    //
    // `staged` son los que hay que almacenar ahora; `reused` los que ya están y
    // sólo hay que referenciar. Cualquier otra cosa es un manifiesto que no
    // cuadra con lo subido, y se rechaza antes de tocar el almacén.
    let mut staged: HashMap<String, (PathBuf, i64)> = HashMap::new();
    let mut reused: HashMap<String, Stored> = HashMap::new();
    for (sha, _declared) in unique_shas(&body.files) {
        if !valid_sha256(&sha) {
            cleanup_staging();
            return Err(err(StatusCode::BAD_REQUEST, "invalid sha256 in manifest"));
        }
        let path = dir.join(&sha);
        match tokio::fs::metadata(&path).await {
            Ok(meta) => {
                // El tamaño que cuenta es el del fichero en disco, nunca el que
                // declaró el cliente: si no, bastaría con declarar 1 byte para
                // colarse por la cuota y subir un giga.
                staged.insert(sha, (path, meta.len() as i64));
            }
            Err(_) => {
                let Some(stored) = stored_representation(&state.pool, &user_id, &sha)
                    .await
                    .map_err(|e| {
                        cleanup_staging();
                        internal_logged("blob dedup lookup", e)
                    })?
                else {
                    cleanup_staging();
                    warn!(sha = %sha, save_id = %save_id, "cas commit: manifest references a blob that was never uploaded");
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "manifest references a blob that was not uploaded",
                    ));
                };
                reused.insert(sha, stored);
            }
        }
    }

    // ── Trocear lo nuevo que lo merezca (ADR 0019) ─────────────────────────
    // Sólo planifica (hashea), no escribe: si la cuota rechaza más abajo no hay
    // nada que deshacer.
    let mut chunk_plans: HashMap<String, Vec<crate::chunking::ChunkPlan>> = HashMap::new();
    for (sha, (path, size)) in &staged {
        if *size as u64 > crate::chunking::CHUNK_THRESHOLD {
            match crate::chunking::plan_chunks(path).await {
                Ok(plan) => {
                    chunk_plans.insert(sha.clone(), plan);
                }
                Err(e) => {
                    cleanup_staging();
                    return Err(internal_logged("chunk planning", e));
                }
            }
        }
    }

    // ── Bytes nuevos de verdad ─────────────────────────────────────────────
    // Un fichero troceado sólo cuesta los trozos que el usuario no tuviera ya,
    // así que dos versiones de una partida monolítica que cambia poco cuestan
    // poco aunque el fichero entero haya viajado.
    let mut new_bytes: i64 = 0;
    let mut new_chunks: HashSet<String> = HashSet::new();
    for (sha, (_, size)) in &staged {
        if let Some(plan) = chunk_plans.get(sha) {
            for c in plan {
                if new_chunks.contains(&c.sha256) {
                    continue;
                }
                if !chunk_in_db(&state.pool, &user_id, &c.sha256)
                    .await
                    .map_err(|e| {
                        cleanup_staging();
                        internal_logged("chunk dedup lookup", e)
                    })?
                {
                    new_chunks.insert(c.sha256.clone());
                    new_bytes += c.len as i64;
                }
            }
        } else {
            new_bytes += size;
        }
    }

    let (quota, used): (i64, i64) =
        sqlx::query_as("SELECT storage_quota_bytes, storage_used_bytes FROM users WHERE id=?")
            .bind(&user_id)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| {
                cleanup_staging();
                internal_logged("quota lookup", e)
            })?;
    if used + new_bytes > quota {
        cleanup_staging();
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "storage quota exceeded"));
    }

    // El tamaño de la versión lo pone el server sumando lo que sabe de cada
    // contenido (el fichero en staging o la fila que ya existía), no el
    // manifiesto del cliente.
    let mut size_by_sha: HashMap<&str, i64> = HashMap::new();
    for (sha, (_, size)) in &staged {
        size_by_sha.insert(sha.as_str(), *size);
    }
    for (sha, stored) in &reused {
        size_by_sha.insert(sha.as_str(), stored.size_bytes());
    }
    let total_size: i64 = body
        .files
        .iter()
        .map(|f| size_by_sha.get(f.sha256.as_str()).copied().unwrap_or(0))
        .sum();
    let max_per_snapshot = (state.config.storage.max_snapshot_size_mb as i64) * 1024 * 1024;
    if total_size > max_per_snapshot {
        cleanup_staging();
        return Err(snapshot_too_large(max_per_snapshot, total_size));
    }

    // ── Colocación: primero los bytes, después la base ─────────────────────
    // Igual que en el multipart (y por lo mismo): un `put_from_file` contra S3
    // es una subida por red, y hacerlo con la transacción abierta deja a todo el
    // server esperando el lock de escritura de SQLite.
    let store = state.store.clone();
    let mut placed: Vec<Placed> = Vec::new();
    let mut placed_chunks: HashSet<String> = HashSet::new();
    let rollback = {
        let store = store.clone();
        let pool = state.pool.clone();
        let user_id = user_id.clone();
        move |done: &[Placed]| {
            let keys: Vec<(String, String, bool)> = done
                .iter()
                .map(|p| (p.key.clone(), p.sha.clone(), p.chunk))
                .collect();
            let (store, pool, user_id) = (store.clone(), pool.clone(), user_id.clone());
            tokio::spawn(async move {
                for (key, sha, is_chunk) in keys {
                    // Sólo se borra lo que no referencia nadie: entre la
                    // colocación y esta vuelta atrás otra petición puede haber
                    // confirmado una versión que apunta a la misma clave. Ante
                    // un error de la base se asume referenciado — un huérfano
                    // cuesta espacio, un borrado de más cuesta datos.
                    let referenced = if is_chunk {
                        chunk_in_db(&pool, &user_id, &sha).await.unwrap_or(true)
                    } else {
                        blob_in_db(&pool, &user_id, &sha).await.unwrap_or(true)
                    };
                    if !referenced {
                        let _ = store.delete(&key).await;
                    }
                }
            });
        }
    };

    for (sha, (path, _size)) in &staged {
        if let Some(plan) = chunk_plans.get(sha) {
            for c in plan.iter() {
                if !new_chunks.contains(&c.sha256) || !placed_chunks.insert(c.sha256.clone()) {
                    continue;
                }
                let key = crate::store::chunk_key(&user_id, &c.sha256);
                let stage = dir.join("_stage").join(&c.sha256);
                if let Some(parent) = stage.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                if crate::chunking::place_chunk(path, c.offset, c.len, &stage)
                    .await
                    .is_err()
                    || store.put_from_file(&key, &stage).await.is_err()
                {
                    warn!(sha = %c.sha256, "cas: chunk placement failed");
                    rollback(&placed);
                    cleanup_staging();
                    return Err(internal());
                }
                placed.push(Placed {
                    key,
                    sha: c.sha256.clone(),
                    chunk: true,
                });
            }
            continue;
        }
        let key = crate::store::blob_key(&user_id, sha);
        if store.put_from_file(&key, path).await.is_err() {
            warn!(sha = %sha, "cas: blob placement failed");
            rollback(&placed);
            cleanup_staging();
            return Err(internal());
        }
        placed.push(Placed {
            key,
            sha: sha.clone(),
            chunk: false,
        });
    }

    // ── Transacción: sólo filas ────────────────────────────────────────────
    let snapshot_id = Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await.map_err(|e| {
        rollback(&placed);
        cleanup_staging();
        internal_logged("opening the commit transaction", e)
    })?;

    let head: i64 = sqlx::query_scalar("SELECT latest_version_num FROM saves WHERE id=?")
        .bind(&save_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            rollback(&placed);
            cleanup_staging();
            internal_logged("reading the save's latest version", e)
        })?;
    // El init ya lo miró, pero entre init y commit pueden pasar minutos y otro
    // equipo puede haber empujado. Esta es la comprobación que manda.
    if let Some(base) = body.base_version {
        if base != head {
            rollback(&placed);
            cleanup_staging();
            return Err(non_fast_forward(&save_id, head, base));
        }
    }
    let new_version = head + 1;
    let parent_version: Option<i64> = (head > 0).then_some(head);
    let file_count = body.files.len() as i64;

    let fail = |e: sqlx::Error, step: &'static str| internal_logged(step, e);

    sqlx::query(
        "INSERT INTO snapshots (id, save_id, version_num, device_name, notes,
                                total_size_bytes, file_count, parent_version)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(&snapshot_id)
    .bind(&save_id)
    .bind(new_version)
    .bind(&body.device_name)
    .bind(&body.notes)
    .bind(total_size)
    .bind(file_count)
    .bind(parent_version)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        rollback(&placed);
        cleanup_staging();
        fail(e, "recording the snapshot")
    })?;

    for f in &body.files {
        let file_id = Uuid::new_v4().to_string();
        let sha = f.sha256.as_str();
        let size = size_by_sha.get(sha).copied().unwrap_or(0);
        sqlx::query(
            "INSERT INTO snapshot_files (id, snapshot_id, relative_path, size_bytes, sha256, modified_at)
             VALUES (?,?,?,?,?,?)",
        )
        .bind(&file_id)
        .bind(&snapshot_id)
        .bind(&f.relative_path)
        .bind(size)
        .bind(sha)
        .bind(f.modified_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            rollback(&placed);
            cleanup_staging();
            fail(e, "recording a snapshot file")
        })?;

        // Trozos: los recién planificados, o los copiados de la versión que ya
        // tenía este contenido. En los dos casos se referencia trozo a trozo —
        // un fichero puede repetir el mismo trozo, y cada aparición cuenta.
        let chunks: Vec<(String, i64)> = if let Some(plan) = chunk_plans.get(sha) {
            plan.iter()
                .map(|c| (c.sha256.clone(), c.len as i64))
                .collect()
        } else if let Some(Stored::Chunks { file_id: src, .. }) = reused.get(sha) {
            sqlx::query(
                "SELECT c.chunk_sha256 AS sha, COALESCE(k.size_bytes, 0) AS size
                   FROM snapshot_file_chunks c
                   LEFT JOIN chunks k ON k.user_id = ? AND k.sha256 = c.chunk_sha256
                  WHERE c.snapshot_file_id = ?
                  ORDER BY c.ordinal",
            )
            .bind(&user_id)
            .bind(src)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| {
                rollback(&placed);
                cleanup_staging();
                fail(e, "copying a chunk list")
            })?
            .into_iter()
            .map(|r| (r.get::<String, _>("sha"), r.get::<i64, _>("size")))
            .collect()
        } else {
            Vec::new()
        };

        if chunks.is_empty() {
            sqlx::query(
                "INSERT INTO blobs (user_id, sha256, size_bytes, refcount)
                 VALUES (?,?,?,1)
                 ON CONFLICT(user_id, sha256) DO UPDATE SET refcount = refcount + 1",
            )
            .bind(&user_id)
            .bind(sha)
            .bind(size)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                rollback(&placed);
                cleanup_staging();
                fail(e, "reference-counting a blob")
            })?;
            continue;
        }

        for (ordinal, (csha, csize)) in chunks.iter().enumerate() {
            sqlx::query(
                "INSERT INTO snapshot_file_chunks (snapshot_file_id, ordinal, chunk_sha256)
                 VALUES (?,?,?)",
            )
            .bind(&file_id)
            .bind(ordinal as i64)
            .bind(csha)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                rollback(&placed);
                cleanup_staging();
                fail(e, "recording a file's chunks")
            })?;
            sqlx::query(
                "INSERT INTO chunks (user_id, sha256, size_bytes, refcount)
                 VALUES (?,?,?,1)
                 ON CONFLICT(user_id, sha256) DO UPDATE SET refcount = refcount + 1",
            )
            .bind(&user_id)
            .bind(csha)
            .bind(csize)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                rollback(&placed);
                cleanup_staging();
                fail(e, "reference-counting a chunk")
            })?;
        }
    }

    sqlx::query("UPDATE saves SET latest_version_num=? WHERE id=?")
        .bind(new_version)
        .bind(&save_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            rollback(&placed);
            cleanup_staging();
            fail(e, "advancing the save head")
        })?;

    let new_used = used + new_bytes;
    sqlx::query("UPDATE users SET storage_used_bytes=? WHERE id=?")
        .bind(new_used)
        .bind(&user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            rollback(&placed);
            cleanup_staging();
            fail(e, "updating storage accounting")
        })?;

    let metadata = serde_json::json!({
        "save_id": save_id,
        "version_num": new_version,
        "files": file_count,
        "bytes": total_size,
        "new_bytes": new_bytes,
        "transport": "cas",
    })
    .to_string();
    sqlx::query(
        "INSERT INTO audit_log (id, user_id, event_type, entity_id, metadata)
         VALUES (?,?,'snapshot.created',?,?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&user_id)
    .bind(&snapshot_id)
    .bind(&metadata)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        rollback(&placed);
        cleanup_staging();
        fail(e, "the commit transaction")
    })?;

    if let Err(e) = tx.commit().await {
        warn!(error = %e, "cas: transaction commit failed");
        rollback(&placed);
        cleanup_staging();
        return Err(internal());
    }
    cleanup_staging();

    info!(
        user = %user.username,
        save_id = %save_id,
        version = new_version,
        files = file_count,
        bytes = total_size,
        new_bytes,
        uploaded_blobs = staged.len(),
        reused_blobs = reused.len(),
        "cas commit"
    );

    {
        let pool = state.pool.clone();
        let uid = user_id.clone();
        let sid = save_id.clone();
        tokio::spawn(async move {
            if let Err(e) = prune_over_version_cap(&pool, &uid, Some(&sid)).await {
                warn!(error = %e, save_id = %sid, "version-cap prune after commit failed");
            }
        });
    }

    state.events.publish(
        user.user_id,
        crate::routes::events::SaveEvent {
            save_id: save_id.clone(),
            version_num: new_version,
        },
    );

    // What the history row will say. After the commit and never fatal: the
    // version is stored, and a row that fails to get a label is cosmetic.
    let insight = match crate::insight::record_selfhosted(&state.pool, &save_id, new_version).await
    {
        Ok(i) => i,
        Err(e) => {
            warn!(error = %e, save_id = %save_id, version = new_version, "insight: not recorded");
            None
        }
    };

    Ok((
        StatusCode::CREATED,
        Json(Snapshot {
            id: snapshot_id,
            save_id: None,
            version_num: new_version,
            parent_version,
            device_name: body.device_name,
            notes: body.notes,
            total_size_bytes: total_size,
            file_count,
            is_pinned: false,
            deleted_at: None,
            created_at: time::OffsetDateTime::now_utc(),
            insight,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::ids::Sha256 as Sha256Hex;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::SqlitePool;
    use std::str::FromStr;

    fn sha(prefix: &str) -> String {
        let mut s = prefix.to_string();
        while s.len() < 64 {
            s.push('0');
        }
        s
    }

    fn file(path: &str, s: &str, size: i64) -> CasFile {
        CasFile {
            relative_path: path.into(),
            sha256: Sha256Hex::parse(s).unwrap(),
            size_bytes: size,
            modified_at: None,
        }
    }

    async fn mem_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .pragma("foreign_keys", "ON");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[test]
    fn upload_ids_and_shas_are_gated_before_they_reach_a_path() {
        // Lo que se interpola en una ruta se valida antes. Sin esto, un
        // `upload_id` con `..` escribe fuera de `tmp/`.
        assert!(valid_upload_id(&Uuid::new_v4().to_string()));
        assert!(!valid_upload_id("../../etc"));
        assert!(!valid_upload_id(""));

        assert!(valid_sha256(&sha("ab")));
        assert!(!valid_sha256("../x"));
        assert!(!valid_sha256(&sha("AB")), "sólo hexadecimal en minúsculas");
        assert!(!valid_sha256("abc"), "longitud exacta");
    }

    #[test]
    fn a_repeated_content_is_negotiated_once() {
        let a = sha("aa");
        let b = sha("bb");
        let files = vec![
            file("save", &a, 10),
            file("save.bak", &a, 10),
            file("other", &b, 20),
        ];
        let u = unique_shas(&files);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0], (a, 10));
        assert_eq!(u[1], (b, 20));
    }

    #[test]
    fn manifests_that_could_write_outside_the_snapshot_are_refused() {
        let s = sha("aa");
        assert!(validate_manifest(&[]).is_err(), "manifiesto vacío");
        assert!(validate_manifest(&[file("../escape", &s, 1)]).is_err());
        assert!(validate_manifest(&[file("/abs", &s, 1)]).is_err());
        assert!(validate_manifest(&[file("saves/a.sav", &s, 1)]).is_ok());
    }

    /// Un blob con fila en `blobs` se reconoce; uno troceado también, aunque no
    /// tenga fila en `blobs` — y ese es el caso que, de olvidarse, haría que una
    /// partida monolítica se volviera a subir entera en cada copia.
    #[tokio::test]
    async fn stored_content_is_recognised_as_blob_or_as_chunks() {
        let pool = mem_pool().await;
        let whole = sha("aa");
        let chunked = sha("bb");
        let absent = sha("cc");
        let chunk1 = sha("c1");

        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1','user','x')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO games (slug, display_name) VALUES ('g','G')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO saves (id, user_id, game_slug, label, latest_version_num) VALUES ('sv','u1','g','default',1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO snapshots (id, save_id, version_num, total_size_bytes, file_count) VALUES ('s1','sv',1,300,2)")
            .execute(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO blobs (user_id, sha256, size_bytes, refcount) VALUES ('u1',?,100,1)",
        )
        .bind(&whole)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO snapshot_files (id, snapshot_id, relative_path, size_bytes, sha256) VALUES ('f1','s1','a',100,?)")
            .bind(&whole).execute(&pool).await.unwrap();

        sqlx::query("INSERT INTO snapshot_files (id, snapshot_id, relative_path, size_bytes, sha256) VALUES ('f2','s1','big',200,?)")
            .bind(&chunked).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO snapshot_file_chunks (snapshot_file_id, ordinal, chunk_sha256) VALUES ('f2',0,?)")
            .bind(&chunk1).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO chunks (user_id, sha256, size_bytes, refcount) VALUES ('u1',?,200,1)",
        )
        .bind(&chunk1)
        .execute(&pool)
        .await
        .unwrap();

        let got = stored_representation(&pool, "u1", &whole).await.unwrap();
        assert!(matches!(got, Some(Stored::Blob { size_bytes: 100 })));

        let got = stored_representation(&pool, "u1", &chunked).await.unwrap();
        match got {
            Some(Stored::Chunks {
                size_bytes,
                file_id,
            }) => {
                assert_eq!(size_bytes, 200);
                assert_eq!(file_id, "f2");
            }
            other => panic!("se esperaba troceado, salió {other:?}"),
        }

        assert!(stored_representation(&pool, "u1", &absent)
            .await
            .unwrap()
            .is_none());
        // El dedup no cruza cuentas: otro usuario no ve este contenido.
        assert!(stored_representation(&pool, "u2", &whole)
            .await
            .unwrap()
            .is_none());
    }
}
