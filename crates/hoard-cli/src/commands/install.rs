//! `hoard install` — deja esta máquina con **todo el Hoard que le toca**, a una
//! sola versión.
//!
//! Es la segunda mitad del instalador de terminal: el script pone el núcleo
//! (`hoardd` + `hoard`) y llama aquí, que es donde vive la decisión de si esta
//! máquina además quiere la app y por qué vía. Que la decisión esté en Rust y no
//! en el `.sh` no es limpieza: la misma función la usan `hoard upgrade` y el
//! updater de la app, y tenerla escrita tres veces en tres lenguajes es tenerla
//! escrita mal.
//!
//! También es el comando que se ejecuta a mano cuando algo quedó a medias —
//! sin red al instalar, `pkexec` cancelado— sin tener que volver a la web.

use anyhow::{Context, Result};

use hoard_agent::install::{self, Component, Delivery, Manifest, Probe};

/// Qué componentes quiere el usuario, cuando quiere opinar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Lo que decida la máquina (lo normal).
    Detect,
    /// Sólo el núcleo, aunque haya entorno gráfico. Para una NAS con escritorio
    /// instalado, o para quien no quiere la app y punto.
    Headless,
    /// La app también, aunque no se detecte. Para instalar por SSH la máquina
    /// que luego se usará con pantalla.
    Desktop,
}

pub async fn run(want: Want, version: Option<String>) -> Result<()> {
    let probe = Probe::read();
    // El manifiesto manda sobre la detección **si ya existe**: reinstalar por
    // SSH no puede concluir "aquí no hay pantalla" y dejar sin app a una máquina
    // que la tiene. La detección es para la primera vez.
    let existing = Manifest::load()?;
    let mut manifest = match (&existing, want) {
        (Some(m), Want::Detect) => m.clone(),
        _ => Manifest::planned(hoard_agent::update::current(), &probe),
    };
    match want {
        Want::Headless => {
            manifest.components.retain(|c| *c != Component::Desktop);
            manifest.delivery = None;
        }
        Want::Desktop => {
            manifest.add(Component::Desktop);
            if manifest.delivery.is_none() {
                manifest.delivery = Some(install::resolve_delivery(&probe));
            }
        }
        Want::Detect => {}
    }

    let target = match &version {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => hoard_agent::update::current().to_string(),
    };
    manifest.version = target.clone();

    println!(
        "hoard {target} — components: {}",
        manifest
            .components
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // ---- núcleo -----------------------------------------------------------
    // Ya está en disco (lo puso el script, o somos nosotros mismos). Lo que
    // falta es que corra: instalar el servicio es lo que convierte "hay
    // binarios" en "esto sincroniza solo".
    match hoardd::autostart::install().await {
        Ok(installed) => println!("  core:    service ready ({})", installed.manager),
        Err(err) => {
            // No es fatal: el sync sigue arrancando cuando se abre un cliente.
            // Lo que se pierde es el arranque en boot, y eso se dice claro.
            eprintln!("  core:    the service won't start at login — {err:#}");
        }
    }
    manifest.core_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()));

    // ---- app --------------------------------------------------------------
    if manifest.has(Component::Desktop) {
        let delivery = manifest
            .delivery
            .unwrap_or_else(|| install::resolve_delivery(&probe));
        manifest.delivery = Some(delivery);
        match install_desktop(delivery, &target).await {
            Ok(path) => {
                println!(
                    "  desktop: installed ({}) → {}",
                    delivery.as_str(),
                    path.display()
                );
                manifest.desktop_path = Some(path);
            }
            Err(err) => {
                // El núcleo instalado y funcionando es un resultado válido; la
                // app se puede reintentar. Anotarla como instalada cuando no lo
                // está sería peor que no tenerla: el próximo `upgrade` creería
                // que la actualiza.
                manifest.components.retain(|c| *c != Component::Desktop);
                manifest.delivery = None;
                manifest.save().ok();
                eprintln!("  desktop: not installed — {err:#}");
                anyhow::bail!(
                    "the core is installed and running; the desktop app is not. \
                     Re-run `hoard install` to retry just that part."
                );
            }
        }
    }

    // Quién es el dueño del núcleo, con todo ya resuelto. Se calcula igual que
    // en `install::observe`: mismo directorio **y** una vía que de verdad
    // empaqueta. El AppImage comparte carpeta con el núcleo sin contenerlo, y
    // darlo por empaquetado dejaría al núcleo fuera de toda actualización
    // posterior justo en la vía de SteamOS.
    manifest.core_from_bundle = match (
        &manifest.core_dir,
        &manifest.desktop_path,
        manifest.delivery,
    ) {
        (Some(core), Some(desktop), Some(d)) => {
            d != Delivery::AppImage && desktop.parent() == Some(core.as_path())
        }
        _ => false,
    };

    manifest.save().context("writing the install manifest")?;
    Ok(())
}

/// Baja el fichero que le toca a esta vía, verifica su firma y lo aplica.
async fn install_desktop(delivery: Delivery, target: &str) -> Result<std::path::PathBuf> {
    if !delivery.is_ours() {
        anyhow::bail!(
            "the desktop app here is managed by your package manager — update it with that"
        );
    }
    // Se pide **siempre** la release de `target`, nunca "la última". Con `latest`
    // esto se rompía solo: `hoard install` a secas —el camino de reparación que
    // documenta este módulo— resolvía la última publicada, la comparaba con la
    // versión de este binario y abortaba en cuanto hubiera salido una release
    // nueva. Lo que hay que instalar es la que casa con el núcleo que ya está
    // aquí; subir de versión es cosa de `hoard upgrade`.
    let (released, assets) = install::fetch::release_assets(Some(target)).await?;
    if released != target {
        // GitHub sirvió otra: instalar a ciegas rompería la única garantía que
        // da todo esto, que las piezas van a la par.
        anyhow::bail!("asked for {target} but the release is {released}");
    }
    let asset = install::fetch::asset_for(delivery, &assets).with_context(|| {
        format!(
            "release {released} publishes no {} package",
            delivery.as_str()
        )
    })?;
    let dir = hoard_agent::config::CliConfig::cache_dir()?.join("downloads");
    println!("  desktop: downloading {}…", asset.name);
    let file = install::fetch::download_verified(asset, &assets, &dir).await?;
    let noninteractive = std::env::var_os("HOARD_NONINTERACTIVE").is_some();
    let installed = install::fetch::apply_desktop(delivery, &file, noninteractive).await?;
    // El instalador ya no hace falta y pesa lo que pesa un bundle.
    let _ = std::fs::remove_file(&file);
    Ok(installed)
}
