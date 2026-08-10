//! `hoard devices` — las máquinas de la cuenta y cuál está encendida.
//!
//! Paridad con el panel del Ojo del escritorio (ADR: la lógica vive en
//! `hoard-agent`, los dos frontends sólo pintan). Importa más aquí que allí: un
//! self-hoster suele tener el servidor en una máquina sin pantalla, y hasta
//! ahora la única forma de ver el censo era abrir la app en otra.

use anyhow::Result;

use hoard_agent::api::{ApiClient, ApiError};
use hoard_agent::config::CliConfig;

pub async fn run() -> Result<()> {
    let (cfg, _) = CliConfig::load_default()?;
    let token = cfg.require_token()?;
    let client = ApiClient::new(cfg.server.url.clone(), token)?;

    let list = match client.list_devices().await {
        Ok(l) => l,
        // Server anterior a la 1.1.3: no lleva censo. Decirlo, en vez de
        // imprimir una lista vacía que se lee como "no tienes máquinas".
        Err(e) if matches!(e.downcast_ref::<ApiError>(), Some(ApiError::NotFound)) => {
            println!("this server doesn't keep a device list (needs Hoard 1.1.3 or newer)");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    if list.devices.is_empty() {
        println!("(no devices)");
        return Ok(());
    }

    println!(
        "{:<24} {:<9} {:<9} {:<20} LAST SEEN",
        "DEVICE", "OS", "STATE", "PLAYING"
    );
    for d in &list.devices {
        let state = if d.online { "online" } else { "offline" };
        let playing = d
            .playing
            .first()
            .map(|g| g.slug.as_str())
            .unwrap_or(if d.online { "-" } else { "" });
        // La máquina desde la que se pregunta se marca, para no tener que
        // adivinar cuál es la propia en una lista de nombres parecidos.
        let name = if d.this_device {
            format!("{} *", d.device_name)
        } else {
            d.device_name.clone()
        };
        println!(
            "{:<24} {:<9} {:<9} {:<20} {}",
            name,
            d.os.as_deref().unwrap_or("-"),
            state,
            playing,
            d.last_seen_at.as_deref().unwrap_or("-"),
        );
    }
    println!("\n* this machine");
    Ok(())
}
