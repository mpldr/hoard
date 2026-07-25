//! Dónde escucha el daemon: ruta del socket (Linux/macOS) o nombre del named
//! pipe (Windows), y el fichero de lock que hace de árbitro.
//!
//! **Por usuario, nunca por máquina.** El daemon necesita el keyring del usuario
//! y su login cloud, así que en una máquina multiusuario hay un daemon por
//! sesión (ADR 0021, Parte A). En unix eso sale gratis: el socket vive bajo
//! `$XDG_RUNTIME_DIR` (o el `state_dir` del usuario), que ya es privado. En
//! Windows el namespace de pipes es global, así que el nombre lleva el usuario
//! dentro y la ACL hace el resto (ver `winsec`).

use anyhow::Result;

#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use anyhow::{bail, Context};

#[cfg(unix)]
use hoard_agent::config::CliConfig;

/// Override del endpoint, para tests y diagnóstico. Cliente y daemon tienen que
/// verlo igual, así que se exporta por nombre y no se inventa cada uno el suyo.
pub const ENDPOINT_ENV: &str = "HOARDD_SOCKET";

/// Dirección del socket del daemon. En unix es una ruta; en Windows el nombre
/// del pipe (`\\.\pipe\…`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(String);

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Endpoint {
    pub fn new(address: impl Into<String>) -> Self {
        Self(address.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// El override de [`ENDPOINT_ENV`], si está puesto y no vacío.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(ENDPOINT_ENV).ok()?;
        (!raw.trim().is_empty()).then(|| Self::new(raw))
    }

    /// El endpoint de este usuario: el override si existe, y si no el default de
    /// la plataforma. Es lo que llaman tanto el daemon como los clientes — que
    /// coincidan no es cortesía, es el mecanismo de exclusión mutua.
    pub fn resolve() -> Result<Self> {
        match Self::from_env() {
            Some(ep) => Ok(ep),
            None => Self::user_default(),
        }
    }

    #[cfg(unix)]
    pub fn path(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
    }

    /// Fichero de lock hermano del socket. Se deriva de la dirección (mismo
    /// directorio, extensión `.lock`) para que un endpoint de test tenga su
    /// propio lock y no pelee con el del daemon de verdad del usuario.
    #[cfg(unix)]
    pub fn lock_path(&self) -> PathBuf {
        self.path().with_extension("lock")
    }

    #[cfg(unix)]
    pub fn user_default() -> Result<Self> {
        let path = runtime_dir()?.join("hoardd.sock");
        // `sockaddr_un.sun_path` son 108 bytes en Linux y 104 en macOS. Pasarse
        // da un `bind` con error opaco, así que se dice aquí y con la ruta
        // delante.
        let len = path.as_os_str().len();
        if len > 100 {
            bail!(
                "the socket path is too long for a unix socket ({len} bytes): {}",
                path.display()
            );
        }
        Ok(Self::new(path.to_string_lossy().into_owned()))
    }

    #[cfg(windows)]
    pub fn user_default() -> Result<Self> {
        let user = std::env::var("USERNAME").unwrap_or_default();
        Ok(Self::new(windows_pipe_name(&user)))
    }

    /// Endpoint aislado con nombre propio: el del usuario, pero sin pisarlo.
    ///
    /// En unix cuelga de `dir` (un temporal, típicamente); en Windows el
    /// namespace de pipes es **plano y global a la máquina**, así que `dir` no
    /// pinta nada y la unicidad va en el nombre. Que la firma acepte las dos
    /// cosas es lo que permite escribir **un** test que corra igual en los dos
    /// sitios: el Slice 4a no lo tenía —sus tests componían rutas de fichero— y
    /// por eso la ruta del pipe se quedó verificada sólo a nivel de tipos.
    pub fn scoped(dir: &std::path::Path, name: &str) -> Self {
        #[cfg(unix)]
        {
            Self::new(
                dir.join(format!("hoardd-{name}.sock"))
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        #[cfg(windows)]
        {
            let _ = dir;
            Self::new(windows_pipe_name(&format!("test-{name}")))
        }
    }
}

/// Directorio del socket en unix: `$XDG_RUNTIME_DIR/hoard` cuando existe (tmpfs,
/// 0700 del usuario, se limpia al cerrar sesión — el sitio canónico de un
/// socket de servicio de usuario), y si no el `state_dir` de siempre (macOS no
/// define `XDG_RUNTIME_DIR`).
#[cfg(unix)]
fn runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("hoard"));
    }
    CliConfig::state_dir().context("resolving the state dir for the socket")
}

/// Nombre del named pipe de un usuario.
///
/// El namespace `\\.\pipe\` es **global a la máquina**: dos usuarios en la misma
/// sesión de servidor de terminal comparten espacio de nombres, así que el
/// nombre lleva el usuario. Va sanitizado (el nombre de un pipe no puede llevar
/// `\`) *y* con un hash del nombre original: sin el hash, `José` y `Jose`
/// colapsarían al mismo pipe y el segundo usuario no podría ni crear el suyo
/// (la ACL del primero le negaría el acceso) ni usarlo.
pub fn windows_pipe_name(user: &str) -> String {
    let mut safe: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect::<String>()
        .to_ascii_lowercase();
    if safe.is_empty() {
        safe.push_str("user");
    }
    format!("\\\\.\\pipe\\hoardd-{safe}-{:08x}", fnv1a(user.as_bytes()))
}

/// FNV-1a de 32 bits. Sólo se usa para desambiguar el nombre del pipe, así que
/// no hace falta arrastrar una dep de hashing por doce líneas.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_override_wins() {
        // Sin tocar el entorno del proceso (los tests corren en paralelo):
        // basta comprobar que `new` no reinterpreta la dirección.
        let ep = Endpoint::new("/tmp/whatever.sock");
        assert_eq!(ep.as_str(), "/tmp/whatever.sock");
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_sits_next_to_the_socket() {
        let ep = Endpoint::new("/run/user/1000/hoard/hoardd.sock");
        assert_eq!(
            ep.lock_path(),
            PathBuf::from("/run/user/1000/hoard/hoardd.lock")
        );
    }

    /// Dos usuarios distintos no pueden colisionar de nombre, ni cuando la
    /// sanitización los deja iguales.
    #[test]
    fn pipe_names_are_per_user_and_collision_free() {
        let a = windows_pipe_name("José");
        let b = windows_pipe_name("Jose");
        assert_ne!(a, b);
        assert!(a.starts_with("\\\\.\\pipe\\hoardd-jos"));
        assert_eq!(a, windows_pipe_name("José"));
    }

    /// Un nombre de usuario que la sanitización deja vacío (cirílico, CJK) sigue
    /// dando un nombre de pipe válido y propio.
    #[test]
    fn an_unsanitisable_user_still_gets_a_name() {
        let name = windows_pipe_name("Пользователь");
        assert!(name.starts_with("\\\\.\\pipe\\hoardd-user-"));
        assert_ne!(name, windows_pipe_name("用户"));
    }
}
