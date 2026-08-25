//! `hoard devices` — las máquinas de la cuenta y cuál está encendida.
//!
//! Paridad con el panel del Ojo del escritorio (ADR: la lógica vive en
//! `hoard-agent`, los dos frontends sólo pintan). Importa más aquí que allí: un
//! self-hoster suele tener el servidor en una máquina sin pantalla, y hasta
//! ahora la única forma de ver el censo era abrir la app en otra.

use anyhow::Result;
use serde::Serialize;

use hoard_agent::api::{ApiClient, ApiError};
use hoard_agent::config::CliConfig;

use crate::output;

#[derive(Serialize)]
pub struct DeviceRow {
    pub device_name: String,
    pub os: Option<String>,
    pub online: bool,
    /// Every game, not the first: two sessions at once are two, and keeping
    /// one makes them indistinguishable from a single one.
    pub playing: Vec<String>,
    pub last_seen_at: Option<String>,
    /// The machine this command ran on.
    pub this_device: bool,
}

#[derive(Serialize)]
pub struct DevicesOut {
    /// False on servers older than 1.1.3, which keep no device list. An empty
    /// `devices` would read as "you have no machines", which is a different
    /// thing and the reason this flag exists.
    pub supported: bool,
    pub devices: Vec<DeviceRow>,
}

pub async fn run() -> Result<()> {
    let (cfg, _) = CliConfig::load_default()?;
    let token = output::require_token(&cfg)?;
    let client = ApiClient::new(cfg.server.url.clone(), token)?;

    let out = match client.list_devices().await {
        Ok(l) => DevicesOut {
            supported: true,
            devices: l
                .devices
                .into_iter()
                .map(|d| DeviceRow {
                    device_name: d.device_name,
                    os: d.os,
                    online: d.online,
                    playing: d.playing.into_iter().map(|g| g.slug).collect(),
                    last_seen_at: d.last_seen_at,
                    this_device: d.this_device,
                })
                .collect(),
        },
        // Server anterior a la 1.1.3: no lleva censo. Decirlo, en vez de
        // imprimir una lista vacía que se lee como "no tienes máquinas".
        Err(e) if matches!(e.downcast_ref::<ApiError>(), Some(ApiError::NotFound)) => DevicesOut {
            supported: false,
            devices: Vec::new(),
        },
        Err(e) => return Err(e),
    };

    output::emit(&out, |out| {
        if !out.supported {
            println!("this server doesn't keep a device list (needs Hoard 1.1.3 or newer)");
            return;
        }
        if out.devices.is_empty() {
            println!("(no devices)");
            return;
        }

        println!(
            "{:<24} {:<9} {:<9} {:<20} LAST SEEN",
            "DEVICE", "OS", "STATE", "PLAYING"
        );
        for d in &out.devices {
            let state = if d.online { "online" } else { "offline" };
            let playing = if d.playing.is_empty() {
                (if d.online { "-" } else { "" }).to_string()
            } else {
                d.playing.join(", ")
            };
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
    })
}
