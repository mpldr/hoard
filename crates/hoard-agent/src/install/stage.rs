//! **Bajar antes de decidir, aplicar todo de una vez.**
//!
//! [`super::auto`] decide *qué* toca; esto lo hace. Son dos operaciones y la
//! separación no es cosmética:
//!
//! - [`stage`] baja y verifica los ficheros de una versión en un directorio
//!   aparte. No toca nada instalado, así que puede correr con un juego abierto,
//!   con la app cerrada, y fallar a medias sin consecuencias.
//! - [`apply`] los pone en su sitio. Es la parte corta —renombrar dos binarios,
//!   o correr un instalador— y es la única que necesita que el momento sea
//!   bueno.
//!
//! Partirlo así es lo que convierte "actualizar al abrir" en algo que se puede
//! prometer: al abrir ya no queda descarga, queda un `rename`. Es lo mismo que
//! hacen Steam y Discord, y por el mismo motivo.
//!
//! ## Una versión, un directorio
//!
//! Lo bajado vive en `<cache>/staged/<versión>/`. Con la versión en la ruta, un
//! reinicio del servicio en mitad de una descarga no deja ficheros de dos
//! releases mezclados en la misma carpeta, y [`sweep`] puede borrar lo viejo sin
//! mirar dentro. Cada fichero se comprueba contra la clave de release **antes**
//! de escribirse, así que en `staged/` no hay nada sin firmar.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::fetch;
use super::{Component, Delivery, Manifest};

/// Dónde se guarda lo bajado de una versión.
pub fn dir(version: &str) -> Result<PathBuf> {
    Ok(crate::config::CliConfig::cache_dir()?
        .join("staged")
        .join(version.trim_start_matches('v')))
}

/// Los ficheros de una versión, ya en disco y ya verificados.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub version: String,
    /// El tarball del núcleo. `None` cuando el núcleo no es nuestro (viaja
    /// dentro del bundle de la app, que lo releva su instalador).
    pub core: Option<PathBuf>,
    /// El instalador de la app. `None` en una máquina sin app.
    pub desktop: Option<PathBuf>,
}

impl Staged {
    /// ¿Hay algo que aplicar? Un `Staged` vacío significa que esta instalación
    /// no tiene ninguna pieza nuestra, y aplicarlo sería subir el número de
    /// versión del manifiesto sin haber tocado un solo binario.
    pub fn is_empty(&self) -> bool {
        self.core.is_none() && self.desktop.is_none()
    }
}

/// Qué ficheros necesita esta instalación para pasar a `version`.
fn wanted(version: &str, manifest: &Manifest) -> Result<(bool, Option<Delivery>)> {
    // El núcleo se releva por nuestra cuenta salvo que lo traiga el bundle.
    let core = !manifest.core_from_bundle;
    let desktop = if manifest.has(Component::Desktop) {
        let d = manifest
            .delivery
            .context("the manifest says there's an app here but not how it got here")?;
        if !d.is_ours() {
            bail!("this install is managed by your package manager ({})", d.as_str());
        }
        Some(d)
    } else {
        None
    };
    if !core && desktop.is_none() {
        bail!("nothing here is ours to update for {version}");
    }
    Ok((core, desktop))
}

/// Lo que ya está bajado para `version`, si está **entero**.
///
/// Entero importa: una descarga cortada a la mitad deja el tarball del núcleo y
/// no el instalador de la app, y darla por buena aplicaría media actualización.
/// Falta un fichero → como si no hubiera nada, y se vuelve a bajar.
pub fn already_staged(version: &str, manifest: &Manifest) -> Option<Staged> {
    let (want_core, want_desktop) = wanted(version, manifest).ok()?;
    let dir = dir(version).ok()?;

    let core = if want_core {
        let name = fetch::core_asset_name(version)?;
        let path = dir.join(name);
        if !path.is_file() {
            return None;
        }
        Some(path)
    } else {
        None
    };

    let desktop = match want_desktop {
        Some(_) => {
            // No se sabe el nombre exacto del bundle sin listar la release, así
            // que se acepta el único fichero que no sea el tarball del núcleo.
            let core_name = core.as_ref().and_then(|p| p.file_name().map(|s| s.to_owned()));
            let mut others = std::fs::read_dir(&dir)
                .ok()?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| p.file_name().map(|n| Some(n.to_owned()) != core_name) == Some(true));
            let found = others.next()?;
            if others.next().is_some() {
                // Dos candidatos: no se puede saber cuál es. Rebajar es barato y
                // adivinar mal es ejecutar un instalador que no era.
                return None;
            }
            Some(found)
        }
        None => None,
    };

    Some(Staged {
        version: version.trim_start_matches('v').to_string(),
        core,
        desktop,
    })
}

/// Baja y verifica todo lo que hace falta para pasar a `version`.
///
/// No aplica nada. Idempotente: lo que ya estuviera bajado no se vuelve a
/// bajar.
pub async fn stage(version: &str, manifest: &Manifest) -> Result<Staged> {
    let version = version.trim_start_matches('v').to_string();
    if let Some(done) = already_staged(&version, manifest) {
        return Ok(done);
    }

    let (want_core, want_desktop) = wanted(&version, manifest)?;
    let dir = dir(&version)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let (released, assets) = fetch::release_assets(Some(&version)).await?;
    if released != version {
        // La misma guarda que `hoard install`: bajar "otra cosa" rompe la única
        // garantía que da todo esto, que las piezas van a la par.
        bail!("asked GitHub for {version} but the release is {released}");
    }

    let core = if want_core {
        let asset = fetch::core_asset_for(&version, &assets).with_context(|| {
            format!(
                "release {version} publishes no core tarball for {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        Some(fetch::download_verified(asset, &assets, &dir).await?)
    } else {
        None
    };

    let desktop = match want_desktop {
        Some(delivery) => {
            let asset = fetch::asset_for(delivery, &assets).with_context(|| {
                format!("release {version} publishes no {} package", delivery.as_str())
            })?;
            Some(fetch::download_verified(asset, &assets, &dir).await?)
        }
        None => None,
    };

    Ok(Staged {
        version,
        core,
        desktop,
    })
}

/// Pone lo bajado en su sitio y anota la versión nueva en el manifiesto.
///
/// El orden es el de [`super`]: **el núcleo primero**, porque es de quien
/// dependen las demás piezas, y porque es el que se aplica sin poder fallar por
/// una cancelación humana. Si después la app falla —un `pkexec` que el usuario
/// cancela— el manifiesto **no** sube de versión: queda apuntando a la vieja y
/// el siguiente ciclo lo reintenta entero. Anotar una versión que sólo alcanzó
/// la mitad de las piezas es fabricar el desajuste mudo que esto viene a
/// impedir.
///
/// `noninteractive` corta cualquier vía que pudiera pararse a preguntar. El
/// servicio lo pone siempre: no tiene ventana donde pintar un diálogo, así que
/// un `pkexec` lanzado desde ahí se quedaría esperando para siempre.
pub async fn apply(staged: &Staged, manifest: &mut Manifest, noninteractive: bool) -> Result<()> {
    if staged.is_empty() {
        bail!("there is nothing staged for {}", staged.version);
    }

    if let Some(tarball) = &staged.core {
        let dir = manifest
            .core_dir
            .clone()
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(Path::to_path_buf))
            })
            .context("don't know where the core lives on this machine")?;
        let written = fetch::apply_core(tarball, &dir).await?;
        tracing::info!(
            version = %staged.version,
            files = ?written,
            "update: core replaced"
        );
        manifest.core_dir = Some(dir);
    }

    if let Some(installer) = &staged.desktop {
        let delivery = manifest
            .delivery
            .context("the manifest says there's an app here but not how it got here")?;
        let path = fetch::apply_desktop(delivery, installer, noninteractive).await?;
        tracing::info!(
            version = %staged.version,
            delivery = delivery.as_str(),
            path = %path.display(),
            "update: desktop replaced"
        );
        manifest.desktop_path = Some(path);
    }

    manifest.version.clone_from(&staged.version);
    manifest.save().context("writing the install manifest")?;
    sweep(&staged.version);
    Ok(())
}

/// Borra lo bajado de otras versiones. Se llama después de aplicar, cuando ya no
/// hace falta nada de lo anterior; un bundle son decenas de megas y dejarlos
/// acumularse en la caché es el fallo que nadie ve hasta que el disco se llena.
pub fn sweep(keep: &str) {
    let Ok(cache) = crate::config::CliConfig::cache_dir() else {
        return;
    };
    let root = cache.join("staged");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let keep = keep.trim_start_matches('v');
    for entry in entries.flatten() {
        if entry.file_name() == keep {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(desktop: Option<Delivery>, core_from_bundle: bool) -> Manifest {
        let mut components = vec![Component::Core];
        if desktop.is_some() {
            components.push(Component::Desktop);
        }
        Manifest {
            version: "1.0.0".into(),
            components,
            delivery: desktop,
            core_dir: None,
            desktop_path: None,
            core_from_bundle,
        }
    }

    #[test]
    fn a_headless_box_only_wants_the_core() {
        let (core, desktop) = wanted("1.1.0", &manifest(None, false)).unwrap();
        assert!(core);
        assert_eq!(desktop, None);
    }

    #[test]
    fn a_desktop_box_wants_both() {
        let (core, desktop) = wanted("1.1.0", &manifest(Some(Delivery::AppImage), false)).unwrap();
        assert!(core);
        assert_eq!(desktop, Some(Delivery::AppImage));
    }

    #[test]
    fn a_bundled_core_rides_the_app_installer() {
        let (core, desktop) = wanted("1.1.0", &manifest(Some(Delivery::Deb), true)).unwrap();
        assert!(!core, "the .deb brings the core with it");
        assert_eq!(desktop, Some(Delivery::Deb));
    }

    #[test]
    fn a_managed_install_has_nothing_to_stage() {
        assert!(wanted("1.1.0", &manifest(Some(Delivery::Managed), true)).is_err());
    }

    #[test]
    fn a_bundled_core_with_no_app_is_a_contradiction() {
        // `core_from_bundle` sin `Desktop` deja cero piezas nuestras: no hay
        // nada que bajar, y decirlo aquí evita crear un directorio vacío y
        // "aplicarlo" subiendo la versión del manifiesto a cambio de nada.
        assert!(wanted("1.1.0", &manifest(None, true)).is_err());
    }

    #[test]
    fn the_staging_dir_is_per_version_and_drops_the_tag_prefix() {
        let a = dir("1.1.0").unwrap();
        let b = dir("v1.1.0").unwrap();
        assert_eq!(a, b);
        assert!(a.ends_with("staged/1.1.0") || a.ends_with(r"staged\1.1.0"), "{a:?}");
        assert_ne!(a, dir("1.1.1").unwrap());
    }

    #[tokio::test]
    async fn applying_nothing_is_an_error_not_a_version_bump() {
        let mut m = manifest(None, false);
        let empty = Staged {
            version: "1.1.0".into(),
            core: None,
            desktop: None,
        };
        assert!(apply(&empty, &mut m, true).await.is_err());
        assert_eq!(m.version, "1.0.0");
    }
}
