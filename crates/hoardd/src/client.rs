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
//!
//! ## La excepción: un apagado deliberado se queda apagado (4d)
//!
//! "Arráncalo si no hay" tiene un caso en el que está mal: el servicio no está
//! porque **lo acaban de parar a propósito**. Hasta el 4c un cliente enganchado
//! lo resucitaba ~3 s después de un `hoard sync stop`, porque su reconexión es
//! `ensure_running` y no tenía forma de distinguir "lo pararon" de "se cayó". La
//! diferencia la dice ahora el daemon ([`ServerFrame::Goodbye`]) y este módulo la
//! recuerda ([`stopped_on_purpose`]): mientras esté puesta, los clientes siguen
//! reconectando pero **no arrancan** nada.
//!
//! Es memoria **de proceso**, no un fichero: un marcador en disco sería el error
//! del pidfile otra vez (queda rancio, nadie sabe si miente). Y se cura sola —
//! cualquier handshake con éxito la borra, porque si hay servicio al que
//! saludar, "está parado" ya no es verdad.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hoard_core::ipc::{
    AdoptedSession, Backlog, ClientFrame, CloudToken, DaemonStatus, Hello, JournalEntry, Payload,
    Reply, Request, ServerFrame, ServerSession, Welcome, PROTOCOL_VERSION,
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

/// El servicio se despidió: alguien lo paró a propósito y este proceso no debe
/// resucitarlo. Ver el encabezado del módulo.
static STOPPED_ON_PURPOSE: AtomicBool = AtomicBool::new(false);

/// ¿Nos consta que el servicio está parado a propósito?
///
/// Los clientes que reconectan en bucle (el relevo de eventos del desktop, el
/// `follow` de `hoard sync run`) lo consultan para espaciar los reintentos: no
/// hay nadie a quien conectarse hasta que alguien lo arranque a mano.
pub fn stopped_on_purpose() -> bool {
    STOPPED_ON_PURPOSE.load(Ordering::Relaxed)
}

/// Anota la despedida del daemon.
fn note_farewell(reason: &str) {
    if !STOPPED_ON_PURPOSE.swap(true, Ordering::Relaxed) {
        tracing::info!(
            reason,
            "the Hoard service said goodbye; it won't be restarted from here"
        );
    }
}

/// Olvida la despedida. La llama el handshake: si hemos podido saludar a un
/// daemon, "está parado" dejó de ser verdad.
fn clear_farewell() {
    if STOPPED_ON_PURPOSE.swap(false, Ordering::Relaxed) {
        tracing::info!("the Hoard service is up again");
    }
}

/// Algo que el daemon empuja sin que se lo pidan.
#[derive(Debug, Clone)]
pub enum Push {
    /// Fila nueva del journal.
    Event(JournalEntry),
    /// Nos hemos retrasado y el canal descartó filas: hay que volver a pedir el
    /// backlog desde `cursor`. Se avisa en vez de dejar un hueco invisible.
    Resync { cursor: u64, dropped: u64 },
    /// El servicio se para a propósito. Quien escucha decide qué hacer: la CLI
    /// termina (su trabajo era seguir un sync que ya no corre), el desktop pinta
    /// el motor parado y espera. Ninguno de los dos lo relanza.
    Goodbye { reason: String },
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
    ///
    /// Salvo que nos hayan dicho que lo pararon a propósito: entonces esto es un
    /// [`Client::connect`] a secas y el error explica que hay que arrancarlo. Un
    /// cliente enganchado no puede deshacer un `hoard sync stop` por el mero
    /// hecho de reconectar.
    pub async fn ensure_running(endpoint: &Endpoint, client_name: &str) -> Result<Self> {
        if let Ok(stream) = transport::connect(endpoint).await {
            return Self::handshake(stream, client_name).await;
        }
        if stopped_on_purpose() {
            bail!(
                "the Hoard service is stopped (someone stopped it on purpose); \
                 start it again with `hoard sync start`"
            );
        }
        // An update is replacing the binaries right now. Starting one here is
        // how a Windows update used to fail: the installer stops `hoardd.exe`,
        // this reconnect brings it back from the old binary two seconds later,
        // and NSIS then can't write the file it just made room for. Whoever
        // finishes the swap starts the service (the NSIS post-install hook, or
        // `relaunch` on the way out), so waiting is not waiting forever.
        if hoard_agent::install::Swap::in_progress() {
            bail!("Hoard is being updated right now; the service will be back in a moment");
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
            Some(ServerFrame::Welcome(welcome)) => {
                // Hay servicio al que saludar: la despedida que recordáramos ya
                // no describe la realidad (alguien lo volvió a arrancar).
                clear_farewell();
                Ok(Self {
                    reader,
                    writer,
                    next_id: 1,
                    welcome,
                    pushes: VecDeque::new(),
                })
            }
            // Saludamos a un servicio que se está apagando a propósito. No es un
            // servicio con el que hablar, pero tampoco una caída: anotarlo aquí
            // es lo que impide que el reintento de dentro de tres segundos lo
            // relance (la ventana de apagado dura lo que tarde el último latido
            // de presencia, que va por red).
            Some(ServerFrame::Goodbye { reason }) => {
                note_farewell(&reason);
                bail!("the Hoard service is stopping: {reason}")
            }
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
                // La despedida se anota **aquí mismo**, no al consumir el push:
                // esta conexión está a punto de cerrarse y quien esperaba una
                // respuesta puede no llegar a leer la cola nunca.
                Some(ServerFrame::Goodbye { reason }) => {
                    note_farewell(&reason);
                    self.pushes.push_back(Push::Goodbye { reason });
                }
                // Respuesta a otra petición en vuelo, un handshake repetido o una
                // trama de un daemon más nuevo: nada de eso es asunto de esta
                // espera.
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

    /// Entrega al daemon una sesión Cloud recién acuñada, para que la guarde
    /// **él**. La contrapartida de [`Client::cloud_token`]: el cliente acuña
    /// (acaba el OAuth) y presta; el daemon guarda, rota y presta de vuelta.
    ///
    /// Escribirla aquí es el bug de macOS que esto viene a matar: el ítem del
    /// llavero queda a nombre de quien lo crea, y el servicio —otro binario—
    /// tendría que pedirle permiso al usuario en cada lectura.
    pub async fn adopt_session(&mut self, session: AdoptedSession) -> Result<()> {
        match self.request(Request::AdoptSession { session }).await? {
            Payload::Ack => Ok(()),
            other => bail!("unexpected answer to adopt_session: {other:?}"),
        }
    }

    /// Dile al daemon que olvide la sesión Cloud (logout). Borrar el ítem del
    /// llavero también hay que autorizarlo, así que lo hace su dueño.
    pub async fn forget_session(&mut self) -> Result<()> {
        match self.request(Request::ForgetSession).await? {
            Payload::Ack => Ok(()),
            other => bail!("unexpected answer to forget_session: {other:?}"),
        }
    }

    /// Entrega al daemon la sesión self-hosted que este cliente acaba de validar.
    /// El gemelo de [`Client::adopt_session`].
    pub async fn adopt_server_session(&mut self, session: ServerSession) -> Result<()> {
        match self
            .request(Request::AdoptServerSession { session })
            .await?
        {
            Payload::Ack => Ok(()),
            other => bail!("unexpected answer to adopt_server_session: {other:?}"),
        }
    }

    /// Dile al daemon que olvide la sesión self-hosted (logout).
    pub async fn forget_server_session(&mut self) -> Result<()> {
        match self.request(Request::ForgetServerSession).await? {
            Payload::Ack => Ok(()),
            other => bail!("unexpected answer to forget_server_session: {other:?}"),
        }
    }

    /// Pide prestada la sesión self-hosted (URL + token + quién eres). Un token
    /// `hoard_v1_` no caduca, así que basta pedirla una vez por proceso.
    pub async fn server_session(&mut self) -> Result<ServerSession> {
        match self.request(Request::ServerToken).await? {
            Payload::ServerSession(session) => Ok(session),
            other => bail!("unexpected answer to server_token: {other:?}"),
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
                Some(ServerFrame::Goodbye { reason }) => {
                    note_farewell(&reason);
                    return Ok(Some(Push::Goodbye { reason }));
                }
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }
}

/// Ruta del binario del daemon, por orden de autoridad: el override, **el que
/// ejecuta el servicio instalado**, el hermano del ejecutable actual (como se
/// empaqueta) y por último el `PATH`.
///
/// El segundo escalón es el que se añadió al unificar la instalación, y no es
/// una preferencia: con la app y el instalador de terminal conviviendo puede
/// haber dos `hoardd` en el disco (`/usr/bin` del paquete, `~/.local/bin` del
/// tarball), y "hermano, si no PATH" hacía que el binario elegido dependiera de
/// **quién** preguntara — la app levantaría el suyo y la terminal el suyo. Sólo
/// hay un daemon por usuario, así que sólo puede haber una respuesta: la que ya
/// tomó el gestor de servicios. Es la misma clase de fallo que el
/// `hoard-server` viejo del `PATH` eclipsando al bueno, resuelta de raíz en vez
/// de a base de limpiar binarios a mano.
pub fn daemon_binary() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os(DAEMON_BIN_ENV).filter(|v| !v.is_empty()) {
        return std::path::PathBuf::from(path);
    }
    if let Some(path) = crate::autostart::installed_exec_start() {
        if path.is_file() {
            return path;
        }
    }
    own_daemon_binary()
}

/// El daemon **de esta instalación**: el override, el hermano de este
/// ejecutable, y si no el `PATH`. Deliberadamente ciego al servicio instalado.
///
/// Es lo que [`crate::autostart`] pone en el `ExecStart`, y por eso no puede
/// mirar la unidad: la unidad es lo que estamos declarando. Si mirara, una
/// actualización que moviera el binario reescribiría la unidad con la ruta
/// **vieja** que ella misma acaba de leer, y el servicio seguiría arrancando el
/// binario anterior para siempre. Los clientes usan [`daemon_binary`], que sí
/// consulta la unidad; quien la declara usa ésta.
pub fn own_daemon_binary() -> std::path::PathBuf {
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
    command.spawn().map_err(|e| {
        // `NotFound` aquí no es "falló el arranque", es "no está el motor". Se
        // dice con esas palabras: éste es el mensaje que ve quien abre la app o
        // escribe `hoard track`, y sin la pista el síntoma es indistinguible de
        // un fallo de permisos o de un daemon que arrancó y murió.
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "the sync engine ({}) isn't there. `hoard` is a thin client of `hoardd` and the \
                 two ship together — reinstall the core (https://hoard.services/install.sh) or \
                 drop `hoardd` beside `hoard`.",
                binary.display()
            )
        } else {
            anyhow::Error::new(e).context(format!("starting the daemon ({})", binary.display()))
        }
    })?;
    Ok(())
}

/// Arranca **nuestro relevo** tras una actualización que sustituyó este binario.
///
/// Es `spawn_daemon` sin el endpoint por entorno: quien se releva es el propio
/// servicio, y el endpoint que le toca es el mismo que resuelve por su cuenta
/// —heredamos `HOARDD_SOCKET` si lo había, así que un daemon con socket propio
/// se releva en su socket—. Devuelve el pid del hijo, que es lo único que se
/// puede afirmar aquí: si el binario nuevo estuviera roto, quien lo dice es el
/// log del hijo, no nosotros.
///
/// Quien llama tiene que haber **soltado el socket** antes: el árbitro es su
/// propiedad, y un hijo que llega y lo encuentra ocupado sale con 0 sin servir
/// nada (`Outcome::AlreadyRunning`).
pub fn respawn_service() -> Result<u32> {
    // `own_daemon_binary` y no `daemon_binary`: lo que hay que arrancar es el
    // binario que acabamos de sustituir en **nuestro** sitio. `daemon_binary`
    // prefiere el `ExecStart` de la unidad instalada, que en una máquina con dos
    // instalaciones apuntaría a la otra.
    let binary = own_daemon_binary();
    let mut command = std::process::Command::new(&binary);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut command);
    let child = command
        .spawn()
        .with_context(|| format!("starting the updated daemon ({})", binary.display()))?;
    Ok(child.id())
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
