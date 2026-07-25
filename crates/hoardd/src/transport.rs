//! Socket local: bind exclusivo, permisos solo-usuario, accept y connect.
//!
//! Dos implementaciones detrás del mismo par de tipos — `UnixListener` en
//! Linux/macOS, named pipe en Windows —, ambas cumpliendo tres cosas que la ADR
//! 0021 pide explícitamente:
//!
//! 1. **La propiedad del socket es el árbitro.** «El pidfile desaparece: el
//!    árbitro pasa a ser la *propiedad del socket* del servicio — un mutex real
//!    con liveness, no un pidfile que se consulta una vez.» Aquí eso es
//!    literal: [`Listener::bind`] devuelve [`BindError::AlreadyRunning`] cuando
//!    otro daemon lo tiene, y el que pierde el bind se conecta al que ganó en
//!    vez de arrancar un segundo motor.
//! 2. **Liveness de verdad.** En unix el mutex es un `flock` sobre
//!    `hoardd.lock`: el kernel lo suelta cuando el proceso muere, pase lo que
//!    pase, así que un daemon que crashea no deja el lock tomado (el fallo
//!    clásico del pidfile). En Windows lo hace `FILE_FLAG_FIRST_PIPE_INSTANCE`,
//!    que es atómico y cuya vida es la del handle.
//! 3. **Solo-usuario.** 0700 en el directorio + 0600 en el socket; en Windows
//!    una ACL construida desde el SID del usuario (ver [`crate::winsec`]).
//!
//! Un socket **stale** (fichero que quedó de un daemon muerto) no se detecta por
//! heurística: se borra siempre justo después de ganar el lock. Con el mutex en
//! la mano no puede haber nadie escuchando ahí, así que el fichero es basura por
//! construcción. Ese orden —lock, luego unlink, luego bind— es lo que hace que
//! dos daemons arrancando a la vez no puedan pisarse el socket.

use crate::endpoint::Endpoint;

/// Por qué no se pudo escuchar. La distinción es la que decide el
/// comportamiento del daemon: `AlreadyRunning` es un final **correcto** (ya hay
/// servicio, no hacía falta este proceso), cualquier otra cosa es un fallo.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("another hoardd already owns {address}")]
    AlreadyRunning { address: String },
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    use tokio::net::{UnixListener, UnixStream};

    use super::BindError;
    use crate::endpoint::Endpoint;

    pub type ServerStream = UnixStream;
    pub type ClientStream = UnixStream;

    /// El `flock` que hace de mutex del servicio. Vive mientras viva el
    /// [`Listener`]; el kernel lo suelta si el proceso muere sin llegar a
    /// `Drop`.
    #[derive(Debug)]
    struct LockFile {
        /// Sólo se guarda: el lock vive mientras viva el fichero abierto. Nadie
        /// lo lee, y eso es el punto — a diferencia del pidfile, aquí no hay
        /// nada que interpretar.
        _file: File,
    }

    impl LockFile {
        fn acquire(path: &Path) -> Result<Self, BindError> {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(path)
                .with_context(|| format!("opening the lock file {}", path.display()))?;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            // SAFETY: `fd` sale de un `File` vivo y `flock` sólo lo usa durante
            // la llamada.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(BindError::AlreadyRunning {
                        address: path.display().to_string(),
                    });
                }
                return Err(BindError::Failed(
                    anyhow::Error::new(err).context(format!("locking {} (flock)", path.display())),
                ));
            }
            let mut file = file;
            // El pid es diagnóstico (`ps`, un log, un bug report). El lock NO
            // depende de leerlo: eso era el pidfile, y por eso un pid reciclado
            // o un fichero rancio nos daban un dueño fantasma.
            let _ = file.set_len(0);
            let _ = writeln!(file, "{}", std::process::id());
            let _ = file.flush();
            Ok(Self { _file: file })
        }
    }

    #[derive(Debug)]
    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
        _lock: LockFile,
    }

    impl Listener {
        pub fn bind(endpoint: &Endpoint) -> Result<Self, BindError> {
            let path = endpoint.path().to_path_buf();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                // 0700: nadie más entra en el directorio del socket. Es la
                // primera de las dos vallas; la otra es el 0600 de abajo.
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
            let lock = LockFile::acquire(&endpoint.lock_path())?;
            // Con el lock en la mano, cualquier socket que quede aquí es de un
            // daemon muerto.
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing the stale socket {}", path.display()))?;
            }
            let inner =
                UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
            // `bind` crea el nodo con 0755 & !umask, así que el 0600 se pone
            // después. La ventana la cubre el 0700 del directorio.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("tightening permissions on {}", path.display()))?;
            Ok(Self {
                inner,
                path,
                _lock: lock,
            })
        }

        pub async fn accept(&mut self) -> Result<ServerStream> {
            let (stream, _addr) = self.inner.accept().await.context("accepting a client")?;
            Ok(stream)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Salida limpia: el socket no queda para que el siguiente arranque
            // tenga que barrerlo. El `flock` lo suelta el cierre del fichero.
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Conecta con el daemon. `Err` con `NotFound`/`ConnectionRefused` es
    /// "no hay daemon", que es información, no un fallo.
    pub async fn connect(endpoint: &Endpoint) -> std::io::Result<ClientStream> {
        UnixStream::connect(endpoint.path()).await
    }

    /// ¿Merece la pena reintentar la conexión (el daemon está arrancando)?
    pub fn is_transient(err: &std::io::Error) -> bool {
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::WouldBlock
        )
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY};

    use super::BindError;
    use crate::endpoint::Endpoint;
    use crate::winsec::SecurityDescriptor;

    pub type ServerStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    pub struct Listener {
        name: String,
        security: SecurityDescriptor,
        /// Instancia creada y esperando cliente. Siempre `Some` entre accepts.
        pending: Option<NamedPipeServer>,
    }

    fn create(
        name: &str,
        security: &SecurityDescriptor,
        first: bool,
    ) -> std::io::Result<NamedPipeServer> {
        let mut attrs = security.attributes();
        let mut options = ServerOptions::new();
        options.first_pipe_instance(first);
        // SAFETY: `attrs` (y el SD al que apunta) viven hasta que la llamada
        // retorna; Windows copia el descriptor de seguridad al objeto.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                &mut attrs as *mut _ as *mut std::ffi::c_void,
            )
        }
    }

    impl Listener {
        pub fn bind(endpoint: &Endpoint) -> Result<Self, BindError> {
            let name = endpoint.as_str().to_string();
            let security = SecurityDescriptor::user_only().map_err(BindError::Failed)?;
            // `first_pipe_instance` es el mutex: si el nombre ya existe, esto
            // falla con ACCESS_DENIED y quien pierde se conecta al que ganó. No
            // hay ventana entre "comprobar" y "crear" porque no hay comprobación.
            match create(&name, &security, true) {
                Ok(pending) => Ok(Self {
                    name,
                    security,
                    pending: Some(pending),
                }),
                Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                    Err(BindError::AlreadyRunning { address: name })
                }
                Err(err) => Err(BindError::Failed(
                    anyhow::Error::new(err).context(format!("creating the named pipe {name}")),
                )),
            }
        }

        pub async fn accept(&mut self) -> Result<ServerStream> {
            let server = self
                .pending
                .take()
                .context("the named pipe listener has no pending instance")?;
            server.connect().await.context("accepting a client")?;
            // La siguiente instancia se crea *después* de que el cliente entre:
            // durante ese instante un cliente nuevo ve ERROR_PIPE_BUSY, que
            // `is_transient` marca como reintentable (es lo que hace el bucle de
            // `connect_with_deadline`).
            self.pending = Some(
                create(&self.name, &self.security, false)
                    .with_context(|| format!("re-arming the named pipe {}", self.name))?,
            );
            Ok(server)
        }
    }

    pub async fn connect(endpoint: &Endpoint) -> std::io::Result<ClientStream> {
        ClientOptions::new().open(endpoint.as_str())
    }

    pub fn is_transient(err: &std::io::Error) -> bool {
        if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
            return true;
        }
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    }
}

pub use imp::{connect, is_transient, ClientStream, Listener, ServerStream};

/// Conecta reintentando hasta `deadline`. Es lo que usa "spawn if absent" para
/// esperar a que el daemon que acabamos de lanzar (o el que ganó la carrera)
/// abra su socket.
pub async fn connect_with_deadline(
    endpoint: &Endpoint,
    deadline: std::time::Instant,
) -> std::io::Result<ClientStream> {
    loop {
        match connect(endpoint).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if !is_transient(&err) || std::time::Instant::now() >= deadline {
                    return Err(err);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// El segundo bind del mismo endpoint no arranca un segundo servicio: se le
    /// dice que ya hay dueño. Es la mitad del daemon de la idempotencia de
    /// "spawn if absent" (la otra mitad, que el perdedor se conecta al ganador,
    /// la prueba `tests/spawn_if_absent.rs` con procesos de verdad).
    #[tokio::test]
    async fn a_second_bind_reports_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let ep = Endpoint::new(
            dir.path()
                .join("hoardd.sock")
                .to_string_lossy()
                .into_owned(),
        );
        let _first = Listener::bind(&ep).expect("first bind wins");
        match Listener::bind(&ep) {
            Err(BindError::AlreadyRunning { .. }) => {}
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    /// Permisos: 0600 el socket, 0700 su directorio. Otro usuario local no
    /// maneja tu sync ni te lee los eventos.
    #[tokio::test]
    async fn the_socket_is_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("run");
        let ep = Endpoint::new(sub.join("hoardd.sock").to_string_lossy().into_owned());
        let _listener = Listener::bind(&ep).unwrap();
        let sock_mode = std::fs::metadata(ep.path()).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(sock_mode, 0o600, "socket must be user-only");
        assert_eq!(dir_mode, 0o700, "socket dir must be user-only");
    }

    /// Un socket que quedó de un daemon muerto no bloquea el arranque: se borra
    /// al ganar el lock. Sin esto, un crash dejaba el servicio inarrancable
    /// hasta que alguien borrara el fichero a mano.
    #[tokio::test]
    async fn a_stale_socket_file_does_not_block_the_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hoardd.sock");
        let ep = Endpoint::new(path.to_string_lossy().into_owned());
        {
            let listener = Listener::bind(&ep).unwrap();
            // Simula la muerte sucia: el fichero se queda, el lock lo suelta el
            // cierre del fd. `mem::forget` evita el `Drop` que lo limpiaría.
            std::mem::forget(listener);
        }
        assert!(path.exists(), "the stale socket must still be there");
        // El `flock` sigue tomado por este mismo proceso (lo hemos filtrado), y
        // flock es por *fichero abierto*: un segundo `open` + `flock` desde el
        // mismo proceso también bloquea, así que aquí sólo comprobamos lo que
        // toca — que el fichero rancio se borra cuando el lock se puede tomar.
        std::fs::remove_file(ep.lock_path()).unwrap();
        let _second = Listener::bind(&ep).expect("a stale socket must not block a new daemon");
        assert!(path.exists());
    }

    /// Sin daemon, conectar falla con un error *transitorio* (que es lo que hace
    /// que "spawn if absent" decida lanzarlo en vez de rendirse).
    #[tokio::test]
    async fn connecting_to_nothing_is_transient() {
        let dir = tempfile::tempdir().unwrap();
        let ep = Endpoint::new(dir.path().join("nope.sock").to_string_lossy().into_owned());
        let err = connect(&ep).await.expect_err("nothing is listening");
        assert!(is_transient(&err), "unexpected error: {err}");
    }
}
