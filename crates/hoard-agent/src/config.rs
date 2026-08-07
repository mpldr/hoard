use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CliConfig {
    pub server: ServerSection,
    #[serde(default)]
    pub auth: AuthSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSection {
    pub url: String,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            url: "http://localhost:12421".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthSection {
    /// Bearer token in plaintext (`hoard_v1_<hex>`).
    /// Stored in the user's config dir; permissions tightened to 0600 on Unix.
    pub token: Option<String>,
}

/// Resuelve el directorio de estado en Windows, mudando el antiguo la primera
/// vez que alguien pregunta.
///
/// Reglas, en orden, y todas con el mismo criterio: **nunca devolver una
/// carpeta vacía mientras los datos estén en otra.**
///
/// 1. Si el destino ya existe, es el bueno (ya se migró, o la instalación nació
///    aquí).
/// 2. Si el origen no existe, tampoco hay nada que mudar.
/// 3. Si el `rename` falla, se sigue usando el origen. Que la mudanza no salga
///    es un incordio; perder de vista los datos es el bug que esto arregla.
///
/// El `rename` es atómico —origen y destino cuelgan los dos de `AppData`, mismo
/// volumen— así que no hay estado intermedio en el que los ficheros estén a
/// medias. Y si dos procesos arrancan a la vez (el servicio y la app es lo
/// normal), el que pierde la carrera ve fallar su `rename` y se encuentra el
/// destino ya creado, que es la respuesta correcta.
/// Sólo se llama en Windows, pero se compila en todas partes **a propósito**:
/// código bajo `cfg(windows)` no lo mira el compilador de esta máquina, y esto
/// decide dónde están los datos del usuario. Compilarlo y probarlo siempre es lo
/// que evita que un error tonto viaje hasta el único sitio donde corre.
#[cfg_attr(not(windows), allow(dead_code))]
fn relocated_state_dir(old: &Path, new: &Path) -> PathBuf {
    if new.is_dir() {
        return new.to_path_buf();
    }
    if !old.is_dir() {
        return new.to_path_buf();
    }
    if let Some(parent) = new.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                error = %e,
                path = %parent.display(),
                "no se pudo preparar el nuevo directorio de estado; se sigue con el antiguo"
            );
            return old.to_path_buf();
        }
    }
    match std::fs::rename(old, new) {
        Ok(()) => {
            tracing::info!(
                from = %old.display(),
                to = %new.display(),
                "estado movido fuera de la carpeta de instalación"
            );
            new.to_path_buf()
        }
        Err(e) => {
            // Puede ser la carrera de arranque: si el otro proceso ya lo movió,
            // el destino existe y es la respuesta buena.
            if new.is_dir() {
                return new.to_path_buf();
            }
            tracing::warn!(
                error = %e,
                from = %old.display(),
                to = %new.display(),
                "no se pudo mover el estado; se sigue usando el antiguo"
            );
            old.to_path_buf()
        }
    }
}

impl CliConfig {
    pub fn project_dirs() -> Result<ProjectDirs> {
        ProjectDirs::from("dev", "hoard", "hoard")
            .context("could not determine user config directory")
    }

    pub fn default_path() -> Result<PathBuf> {
        let pd = Self::project_dirs()?;
        Ok(pd.config_dir().join("config.toml"))
    }

    /// Dónde vive el estado del usuario: partidas vigiladas, horas jugadas,
    /// caché de detección, preferencias.
    ///
    /// En Windows **no** es `data_local_dir()`, y el motivo le costó a un
    /// usuario su historial. `ProjectDirs` lo resuelve a
    /// `%LOCALAPPDATA%\hoard\hoard\data`, y el instalador NSIS —`productName`
    /// "Hoard", `installMode` `currentUser`— instala en `%LOCALAPPDATA%\Hoard`.
    /// Windows no distingue mayúsculas: **los datos del usuario quedaban dentro
    /// de la carpeta de instalación**, así que reinstalar o actualizar podía
    /// llevárselos. Y no se quedaba en local: el cliente arrancaba con las
    /// horas a cero y su siguiente subida propagaba ese vacío a la nube.
    ///
    /// Pasa a `%APPDATA%` (Roaming), que es donde ya vivía la configuración y
    /// donde ningún instalador escarba. La caché se queda en Local, que es
    /// justo el reparto que Windows pide: Roaming para el estado pequeño del
    /// usuario, Local para lo reconstruible. En Linux y macOS `data_dir()` y
    /// `data_local_dir()` son la misma ruta, así que fuera de Windows no
    /// cambia nada.
    pub fn state_dir() -> Result<PathBuf> {
        let pd = Self::project_dirs()?;
        #[cfg(not(windows))]
        {
            Ok(pd.data_local_dir().to_path_buf())
        }
        #[cfg(windows)]
        {
            Ok(relocated_state_dir(pd.data_local_dir(), pd.data_dir()))
        }
    }

    /// Where rotating log files live. Distinct from `state_dir` so the user
    /// (or a packager's `clean cache` step) can wipe logs without nuking
    /// their tracked-saves mapping.
    pub fn cache_dir() -> Result<PathBuf> {
        let pd = Self::project_dirs()?;
        Ok(pd.cache_dir().to_path_buf())
    }

    pub fn logs_dir() -> Result<PathBuf> {
        Ok(Self::cache_dir()?.join("logs"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: CliConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn load_default() -> Result<(Self, PathBuf)> {
        let path = Self::default_path()?;
        let cfg = Self::load(&path)?;
        Ok((cfg, path))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(())
    }

    pub fn require_token(&self) -> Result<&str> {
        self.auth
            .token
            .as_deref()
            .filter(|s| !s.is_empty())
            .context("not logged in: run `hoard login --token <TOKEN>` first")
    }
}
