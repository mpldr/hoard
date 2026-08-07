//! Contra qué servidor se mide y con qué credencial.
//!
//! Tres caminos, en este orden: lo que digan `--server`/`--token`, el token
//! que preste el servicio, o el que haya en disco. El préstamo importa: el
//! daemon es el **único rotador** del refresh token (ADR 0021, Slice 4c), y un
//! banco que rotara por su cuenta dispararía la reuse-detection de GoTrue y
//! revocaría la sesión de verdad del usuario. Medir no puede costar la cuenta.

use anyhow::{Context, Result};
use hoard_agent::api::ApiClient;
use hoard_agent::session::Active;

pub async fn resolve(server: Option<String>, token: Option<String>) -> Result<Active> {
    if let Some(url) = server {
        let token = token.unwrap_or_default();
        let client = ApiClient::new(url.clone(), token)?;
        let is_cloud = client.is_cloud().await;
        return Ok(Active {
            client,
            is_cloud,
            server: url,
            cloud: None,
        });
    }

    // Sin sesión Cloud no hay nada que pedir prestado: es un self-hosted y el
    // token vive en `config.toml`.
    if hoard_agent::cloud_auth::load_session()?.is_none() {
        return hoard_agent::session::resolve_borrowed(None, None)
            .await
            .context("resolviendo la sesión self-hosted");
    }

    let lent = borrow_token().await;
    hoard_agent::session::resolve_borrowed(lent, None)
        .await
        .context("resolviendo la sesión Cloud")
}

/// Pide el token al servicio si está levantado. Si no lo está **no lo levanta**:
/// arrancar un daemon como efecto de correr un banco es meter un motor a
/// sincronizar de verdad en mitad de una medición.
async fn borrow_token() -> Option<hoard_core::ipc::CloudToken> {
    let endpoint = hoardd::endpoint::Endpoint::resolve().ok()?;
    let mut client = match hoardd::client::Client::connect(&endpoint, "hoard-pruebas").await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "sin servicio: se usa el token de disco");
            return None;
        }
    };
    match client.cloud_token(None).await {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "el servicio no prestó token");
            None
        }
    }
}
