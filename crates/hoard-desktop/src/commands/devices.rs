//! El censo de dispositivos de la sesión activa, para el panel del Ojo.
//!
//! Existe porque el camino que ya había es exclusivo de la nube: `cloud_feed`
//! pide `/v1/devices` con las credenciales de Supabase y lo dispara Realtime
//! cuando otra máquina late. Un servidor propio no tiene ninguna de las dos
//! cosas, así que necesita preguntar por su cuenta.
//!
//! Esto sirve a los dos: `current_client` elige la sesión activa (self-hosted
//! gana, si no la nube) y `/v1/devices` es la misma ruta en ambos despliegues.
//! La UI llama a esto mientras el panel está abierto; con sesión de nube el
//! empuje de Realtime sigue llegando igual por su lado.

use hoard_agent::api::DeviceListOut;
use tauri::{AppHandle, State};

use crate::state::AppState;

#[tauri::command]
pub async fn devices_list(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DeviceListOut, String> {
    let client = crate::commands::library::current_client(&app, &state).await?;
    match client.list_devices().await {
        Ok(list) => {
            tracing::debug!(devices = list.devices.len(), "devices: listed");
            Ok(list)
        }
        // Server anterior a la 1.1.3: la ruta no existe. El panel se queda con
        // esta máquina, como hasta ahora, y no pinta un error por algo que el
        // usuario no puede arreglar desde aquí.
        //
        // Se pregunta por el 404 en vez de consultar antes la capacidad a
        // propósito: `current_client` construye un `ApiClient` nuevo en cada
        // llamada, así que su sonda de `/v1/health` no está cacheada y
        // preguntar costaría **una petición de más cada 15 segundos**, para
        // enterarse de lo mismo que ya cuenta la respuesta.
        Err(e) => match e.downcast_ref::<hoard_agent::api::ApiError>() {
            Some(hoard_agent::api::ApiError::NotFound) => Ok(DeviceListOut {
                devices: Vec::new(),
            }),
            _ => Err(format!("{e:#}")),
        },
    }
}
