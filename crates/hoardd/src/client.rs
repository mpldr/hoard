//! Cliente IPC + **"spawn if absent"**.
//!
//! Lo que usarán el desktop (4b) y la CLI (4c) para hablar con el servicio. La
//! parte interesante es [`Client::ensure_running`], que es idempotente por
//! diseño (ADR 0021, Parte A): «ambos clientes hacen lo mismo — conéctate al
//! servicio; si no hay, arráncalo. Bajo carrera, el que pierde el arranque
//! simplemente se conecta al que ganó (el bind del socket resuelve el empate)».
//!
//! Aquí no hay comprobación de "¿ya hay un daemon?" seguida de un arranque: eso
//! es un TOCTOU y produciría dos motores. El árbitro es el bind, dentro del
//! daemon; este lado se limita a lanzar el proceso y volver a conectar. Lanzar
//! dos daemons a la vez es **correcto**: uno gana el socket y el otro sale sin
//! hacer nada.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hoard_core::ipc::{
    Backlog, ClientFrame, CloudToken, DaemonStatus, Hello, JournalEntry, Payload, Reply, Request,
    ServerFrame, Welcome, PROTOCOL_VERSION,
};
use tokio::io::{ReadHalf, WriteHalf};

use crate::codec::{read_frame, write_frame};
use crate::endpoint::{Endpoint, ENDPOINT_ENV};
use crate::transport::{self, ClientStream};

/// Override de la ruta del binario del daemon. El empaquetado lo pone junto al
/// ejecutable del cliente (eso es lo que busca [`daemon_binary`]); esto es para
/// desarrollo y tests.
pub const DAEMON_BIN_ENV: &str = "HOARDD_BIN";

/// Cuánto se espera a que un daemon recién lanzado abra su socket.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Algo que el daemon empuja sin que se lo pidan.
#[derive(Debug, Clone)]
pub enum Push {
    /// Fila nueva del journal.
    Event(JournalEntry),
    /// Nos hemos retrasado y el canal descartó filas: hay que volver a pedir el
    /// backlog desde `cursor`. Se avisa en vez de dejar un hueco invisible.
    Resync { cursor: u64, dropped: u64 },
}

/// Conexión con el daemon.
pub struct Client {
    reader: ReadHalf<ClientStream>,
    writer: WriteHalf<ClientStream>,
    next_id: u64,
    welcome: Welcome,
    /// Pushes que llegaron mientras esperábamos la respuesta a una petición. No
    /// se descartan: un evento perdido por haber pedido el estado a la vez es el
    /// tipo de agujero que este protocolo existe para no tener.
    pushes: VecDeque<Push>,
}

impl Client {
    /// Conecta con un daemon que ya esté arriba. Falla si no hay ninguno — para
    /// arrancarlo, [`Client::ensure_running`].
    pub async fn connect(endpoint: &Endpoint, client_name: &str) -> Result<Self> {
        let stream = transport::connect(endpoint)
            .await
            .with_context(|| format!("connecting to {endpoint}"))?;
        Self::handshake(stream, client_name).await
    }

    /// Conéctate; si no hay servicio, lánzalo y vuelve a conectar.
    pub async fn ensure_running(endpoint: &Endpoint, client_name: &str) -> Result<Self> {
        if let Ok(stream) = transport::connect(endpoint).await {
            return Self::handshake(stream, client_name).await;
        }
        spawn_daemon(endpoint)?;
        let stream = transport::connect_with_deadline(endpoint, Instant::now() + SPAWN_TIMEOUT)
            .await
            .with_context(|| {
                format!("waiting for the hoardd we just started to listen on {endpoint}")
            })?;
        Self::handshake(stream, client_name).await
    }

    async fn handshake(stream: ClientStream, client_name: &str) -> Result<Self> {
        let (reader, mut writer) = tokio::io::split(stream);
        write_frame(
            &mut writer,
            &ClientFrame::Hello(Hello {
                protocol: PROTOCOL_VERSION,
                client: client_name.to_string(),
            }),
        )
        .await
        .context("sending the hello")?;
        let mut reader = reader;
        match read_frame::<_, ServerFrame>(&mut reader).await? {
            Some(ServerFrame::Welcome(welcome)) => Ok(Self {
                reader,
                writer,
                next_id: 1,
                welcome,
                pushes: VecDeque::new(),
            }),
            // El handshake versionado en acción: el daemon dice su versión, así
            // que el cliente puede pedir que se actualice o se reinicie el
            // servicio en vez de mostrar un error de parseo.
            Some(ServerFrame::Rejected(rejected)) => bail!(
                "the daemon refused the connection: {} (daemon {} speaks protocol {})",
                rejected.reason,
                rejected.daemon_version,
                rejected.daemon_protocol
            ),
            Some(other) => bail!("the daemon answered the hello with {other:?}"),
            None => bail!("the daemon closed the connection during the handshake"),
        }
    }

    /// Lo que el daemon dijo al conectar: versión, pid, epoch y cursor.
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Manda una petición y espera **su** respuesta, encolando por el camino
    /// cualquier push que llegue.
    pub async fn request(&mut self, request: Request) -> Result<Payload> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.writer, &ClientFrame::Request { id, request })
            .await
            .context("sending a request")?;
        loop {
            match read_frame::<_, ServerFrame>(&mut self.reader).await? {
                Some(ServerFrame::Reply { id: got, reply }) if got == id => {
                    return match reply {
                        Reply::Ok(payload) => Ok(payload),
                        // Tipado, no `{err:?}`: este mensaje acaba delante del
                        // usuario (un toast del desktop, una línea de la CLI).
                        Reply::Error(err) => Err(anyhow::Error::new(err)),
                    };
                }
                Some(ServerFrame::Event(entry)) => self.pushes.push_back(Push::Event(entry)),
                Some(ServerFrame::Resync { cursor, dropped }) => {
                    self.pushes.push_back(Push::Resync { cursor, dropped })
                }
                // Respuesta a otra petición en vuelo, o un handshake repetido:
                // ni lo uno ni lo otro es asunto de esta espera.
                Some(_) => continue,
                None => bail!("the daemon closed the connection"),
            }
        }
    }

    pub async fn ping(&mut self) -> Result<(String, u32)> {
        match self.request(Request::Ping).await? {
            Payload::Pong {
                daemon_version,
                pid,
            } => Ok((daemon_version, pid)),
            other => bail!("unexpected answer to ping: {other:?}"),
        }
    }

    pub async fn status(&mut self) -> Result<DaemonStatus> {
        match self.request(Request::Status).await? {
            Payload::Status(status) => Ok(status),
            other => bail!("unexpected answer to status: {other:?}"),
        }
    }

    /// Pide prestado un token Cloud válido. `rejected` es el token que a este
    /// cliente le acaban de devolver un 401, para que el daemon sepa que
    /// devolverle el mismo no sirve de nada.
    ///
    /// El cliente **no** persiste nada de esto: el par completo lo escribe el
    /// daemon, que es el único rotador (ADR 0021, Parte A).
    pub async fn cloud_token(&mut self, rejected: Option<String>) -> Result<CloudToken> {
        match self.request(Request::CloudToken { rejected }).await? {
            Payload::CloudToken(token) => Ok(token),
            other => bail!("unexpected answer to cloud_token: {other:?}"),
        }
    }

    /// Pide el backlog desde `since` y queda suscrito al push en vivo.
    pub async fn subscribe(&mut self, since: Option<u64>) -> Result<Backlog> {
        match self.request(Request::Subscribe { since }).await? {
            Payload::Backlog(backlog) => Ok(backlog),
            other => bail!("unexpected answer to subscribe: {other:?}"),
        }
    }

    /// Siguiente push. Devuelve primero lo que se encoló durante una petición.
    /// `None` = el daemon cerró.
    pub async fn next_push(&mut self) -> Result<Option<Push>> {
        if let Some(push) = self.pushes.pop_front() {
            return Ok(Some(push));
        }
        loop {
            match read_frame::<_, ServerFrame>(&mut self.reader).await? {
                Some(ServerFrame::Event(entry)) => return Ok(Some(Push::Event(entry))),
                Some(ServerFrame::Resync { cursor, dropped }) => {
                    return Ok(Some(Push::Resync { cursor, dropped }))
                }
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }
}

/// Ruta del binario del daemon: el override, si no el hermano del ejecutable
/// actual (como se empaqueta), si no el `PATH`.
pub fn daemon_binary() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os(DAEMON_BIN_ENV).filter(|v| !v.is_empty()) {
        return std::path::PathBuf::from(path);
    }
    let name = format!("hoardd{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sibling) = exe.parent().map(|d| d.join(&name)) {
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from(name)
}

/// Lanza el daemon desasido de nosotros. Que dos clientes lo lancen a la vez es
/// correcto: uno gana el socket y el otro sale.
fn spawn_daemon(endpoint: &Endpoint) -> Result<()> {
    let binary = daemon_binary();
    let mut command = std::process::Command::new(&binary);
    // El endpoint viaja por entorno para que un daemon lanzado por un cliente con
    // socket propio (tests, dos instalaciones) escuche donde el cliente mira.
    command
        .env(ENDPOINT_ENV, endpoint.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut command);
    command
        .spawn()
        .with_context(|| format!("starting the daemon ({})", binary.display()))?;
    Ok(())
}

/// El servicio tiene que sobrevivir a quien lo arrancó — es el punto de todo el
/// slice: cerrar la app (o Ctrl-C en la CLI) no puede matar el sync.
#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` es async-signal-safe y no toca memoria del padre, que es
    // exactamente lo que `pre_exec` exige.
    unsafe {
        command.pre_exec(|| {
            // Sesión propia: el Ctrl-C del terminal del cliente no llega aquí.
            // Si falla (ya somos líder de sesión) seguimos: no es fatal.
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, DETACHED_PROCESS};
    // Sin consola y sin heredar la del cliente: si el desktop lo lanza no debe
    // aparecer una ventana negra, y si lo lanza la CLI no debe morir con ella.
    command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El override del binario manda: es lo que permite a un test lanzar el
    /// daemon recién compilado en vez de uno instalado.
    #[test]
    fn the_binary_override_wins() {
        let name = daemon_binary();
        // Sin override, el nombre acaba en `hoardd` (con sufijo de la plataforma).
        assert!(
            name.to_string_lossy().contains("hoardd"),
            "unexpected daemon path: {}",
            name.display()
        );
    }
}
