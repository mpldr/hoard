//! Arranque en boot como **servicio de usuario** (ADR 0021, Parte A → Slice 4d).
//!
//! El servicio arranca por dos vías. Una es "spawn if absent": un cliente que no
//! encuentra servicio lo levanta ([`crate::client::Client::ensure_running`]).
//! La otra es ésta — el gestor de servicios del SO lo arranca al iniciar sesión,
//! y entonces el sync corre **sin que nadie abra nada**, que es el punto del
//! Slice 4. Un backend por plataforma, los mismos comandos en las tres:
//!
//! - **Linux**: unidad `systemd --user` (`hoard-sync.service`). Además se intenta
//!   `loginctl enable-linger`, para que una máquina headless (NAS / SteamOS /
//!   servidor casero) siga sincronizando sin sesión gráfica abierta.
//! - **macOS**: LaunchAgent de `launchd` (`com.hoard.sync`).
//! - **Windows**: tarea del Task Scheduler al inicio de sesión (`HoardSync`).
//!
//! **Por usuario, nunca system-wide**, y no es una preferencia estética: el token
//! Cloud vive en el almacén de secretos de *tu* sesión (Secret Service / Keychain
//! / DPAPI), que un servicio de root no puede leer. Un servicio de máquina
//! tampoco sabría de quién son los saves.
//!
//! ## Qué ejecuta la unidad, y por qué cambió en el 4d
//!
//! El `ExecStart` es **el binario `hoardd`**. Hasta el 4c era `hoard sync run`,
//! porque el motor vivía dentro de ese proceso; desde el 4b/4c ese comando es un
//! cliente, así que dejarlo de `ExecStart` significaba que el gestor de servicios
//! supervisaba a un espectador y no al servicio. Ahora `systemctl --user stop`
//! manda la señal **al daemon**, que se despide de sus clientes y para el motor
//! (ver [`hoard_core::ipc::ServerFrame::Goodbye`]).
//!
//! ## El traspaso: un cliente pudo arrancarlo antes que la unidad
//!
//! Sólo hay un daemon por usuario y el árbitro es el bind del socket, así que si
//! ya hay uno corriendo (lo levantó la app al abrirse) el que lance systemd
//! **perderá el bind y saldrá con 0** — dejando a la unidad marcada como muerta
//! aunque el sync funcione. Por eso [`install`] y [`restart`] paran primero el
//! que haya y esperan a que suelte el socket: es la forma de que el dueño del
//! proceso pase a ser el gestor de servicios de verdad.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::client::Client;
use crate::endpoint::Endpoint;

/// Cuánto se espera a que el daemon anterior suelte el socket antes de arrancar
/// el de la unidad. Su apagado limpio incluye el último latido de presencia (va
/// por red), así que no es instantáneo.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(20);

/// Cuánto se espera a que el servicio recién arrancado escuche.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Nombre de la unidad / label / tarea de este SO. Lo necesita un frontend que
/// quiera preguntarle al gestor de servicios directamente (`systemctl status`,
/// `launchctl print`, `schtasks /Query`) — enseñar esa salida tal cual es cosa de
/// la terminal, no de esta capa.
pub const UNIT_ID: &str = platform::UNIT;

/// Dónde quedó instalado, para que el frontend pueda decirlo.
#[derive(Debug, Clone)]
pub struct Installed {
    /// Gestor de servicios usado (`"systemd --user"`, `"launchd"`, …).
    pub manager: &'static str,
    /// Nombre de la unidad / label / tarea.
    pub id: &'static str,
    /// Fichero escrito, si el backend usa uno.
    pub path: Option<PathBuf>,
}

/// El binario que la unidad ejecuta: el `hoardd` que viaja junto a quien la
/// instala (el bundle del desktop lo empaqueta como `externalBin`), y si no, el
/// del `PATH`. Mismo criterio que "spawn if absent", para que las dos vías de
/// arranque no puedan acabar ejecutando binarios distintos.
pub fn service_binary() -> PathBuf {
    crate::client::daemon_binary()
}

/// Instala la unidad y **arranca el servicio ahora**. Idempotente.
pub async fn install() -> Result<Installed> {
    let installed = ensure_installed().await?;
    // El dueño del proceso pasa a ser el gestor de servicios: para el daemon que
    // hubiera levantado un cliente y espera a que suelte el socket, o el que
    // lance la unidad perderá el bind y saldrá (unidad muerta, sync vivo — el
    // peor de los dos mundos para diagnosticar).
    hand_over().await;
    start_now().await?;
    Ok(installed)
}

/// Escribe/actualiza la unidad y la deja habilitada para el próximo inicio de
/// sesión, **sin tocar** un servicio que ya esté corriendo. Idempotente: la
/// llama el desktop en cada arranque (igual que reafirma su propio autostart),
/// donde parar el sync para reinstalarlo sería absurdo.
pub async fn ensure_installed() -> Result<Installed> {
    let (installed, changed) = platform::declare()?;
    // Con la definición intacta y el servicio ya instalado no hay nada que
    // hacer: esto lo llama el desktop en cada arranque, y dos subprocesos por
    // arranque para reafirmar lo que ya está es peaje sin contrapartida.
    if changed || !platform::installed().await {
        platform::enable().await?;
    }
    Ok(installed)
}

/// Quita el arranque automático y para el servicio del gestor. **No** manda
/// `Shutdown` por IPC: eso es una orden aparte (`hoard sync stop` hace las dos,
/// por si el servicio lo levantó un cliente y no la unidad). Devuelve `false` si
/// no había nada instalado.
pub async fn uninstall() -> Result<bool> {
    if !installed().await {
        return Ok(false);
    }
    platform::disable().await?;
    Ok(true)
}

/// Reinicia el servicio bajo el gestor (tras un `hoard upgrade`, para que releve
/// el binario nuevo). Si no estaba instalado, lo instala.
pub async fn restart() -> Result<Installed> {
    if !installed().await {
        return install().await;
    }
    let (installed, _) = platform::declare()?;
    hand_over().await;
    platform::restart().await?;
    wait_until_serving().await?;
    Ok(installed)
}

/// ¿Hay unidad instalada para este usuario?
pub async fn installed() -> bool {
    platform::installed().await
}

/// Para el daemon que esté corriendo y espera a que suelte el socket, para que
/// el siguiente arranque (el de la unidad) gane el bind.
///
/// Best-effort de principio a fin: si no hay servicio no hay nada que traspasar,
/// y si no se aparta a tiempo seguimos igual — el arranque de la unidad se
/// encontrará el socket ocupado y saldrá con 0, que es feo pero no rompe nada.
async fn hand_over() {
    let Ok(endpoint) = Endpoint::resolve() else {
        return;
    };
    let Ok(mut client) = Client::connect(&endpoint, "hoard autostart (handover)").await else {
        return;
    };
    tracing::info!("autostart: stopping the running service so the unit can own it");
    if let Err(err) = client.request(hoard_core::ipc::Request::Shutdown).await {
        tracing::warn!(error = %format!("{err:#}"), "autostart: the service didn't acknowledge the stop");
    }
    drop(client);
    let deadline = Instant::now() + HANDOVER_TIMEOUT;
    while Instant::now() < deadline {
        // Se sondea el **socket**, no el handshake: lo que tiene que quedar libre
        // es el bind. Un daemon que ya se despidió sigue teniéndolo tomado
        // mientras apaga el motor, y darlo por ido ahí es justo lo que dejaría al
        // siguiente arranque perdiendo la carrera.
        if crate::transport::connect(&endpoint).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::warn!("autostart: the previous service is still holding the socket");
}

/// Arranca el servicio por el gestor y confirma que **escucha**. Que la unidad
/// arranque no basta: si perdió el bind, salió con 0 y no hay servicio nuevo.
async fn start_now() -> Result<()> {
    platform::start().await?;
    wait_until_serving().await
}

async fn wait_until_serving() -> Result<()> {
    let endpoint = Endpoint::resolve().context("resolving the hoardd endpoint")?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(client) = Client::connect(&endpoint, "hoard autostart (probe)").await {
            tracing::info!(
                pid = client.welcome().pid,
                "autostart: the service is serving"
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the Hoard service was installed but never started listening on {endpoint} \
                 — see `hoard sync logs`"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---- helpers de proceso compartidos -----------------------------------

/// Ejecuta un comando tragándose su salida; devuelve si tuvo éxito.
async fn run_quiet(program: &str, args: &[&str]) -> Result<bool> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running `{program}`"))?;
    Ok(out.status.success())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("no HOME/USERPROFILE in the environment")
}

// =======================================================================
// Linux — systemd --user
// =======================================================================

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub const UNIT: &str = "hoard-sync.service";

    /// True si `name` está en el `PATH` (se comprueba antes de invocarlo).
    fn bin_exists(name: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file()))
            .unwrap_or(false)
    }

    pub fn unit_path() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(|| home().map(|h| h.join(".config")))?;
        Ok(base.join("systemd").join("user").join(UNIT))
    }

    fn ensure_systemd() -> Result<()> {
        if bin_exists("systemctl") {
            return Ok(());
        }
        anyhow::bail!(
            "systemd not found. On a non-systemd init, run the service under your own \
             supervisor (e.g. an OpenRC/runit service, or `nohup hoardd &`)."
        )
    }

    /// El texto de la unidad. Puro y testeable: es el contrato con systemd.
    pub fn unit_text(exe: &str) -> String {
        format!(
            "[Unit]\n\
             Description=Hoard game-save sync service\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart=\"{exe}\"\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
        )
    }

    /// Escribe la unidad si hace falta. El `bool` dice si cambió, para que
    /// reafirmarla en cada arranque del desktop no cueste dos subprocesos.
    pub fn declare() -> Result<(Installed, bool)> {
        ensure_systemd()?;
        let exe = service_binary();
        // Dentro de un AppImage el binario vive en un punto de montaje efímero
        // (`/tmp/.mount_XXXX/...`) que desaparece al cerrar la app: una unidad
        // apuntando ahí arrancaría en el siguiente login contra una ruta que ya
        // no existe. Es la clase de fallo que sólo se ve semanas después, así
        // que se dice ahora y en voz alta. El sync sigue funcionando — la app
        // levanta el servicio al abrirse ("spawn if absent") —; lo que no hay es
        // arranque sin abrirla.
        if std::env::var_os("APPIMAGE").is_some() {
            anyhow::bail!(
                "an AppImage has no stable path for the service ({} lives in a temporary \
                 mount), so it can't start at login. The sync still runs whenever Hoard is \
                 open; for login start use the .deb/.rpm package, or install the CLI and run \
                 `hoard sync start`.",
                exe.display()
            );
        }
        let path = unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // Entrecomillado para que una ruta con espacios sobreviva al tokenizador
        // de systemd (un AppImage en `~/Mis programas/`, por ejemplo).
        let unit = unit_text(&exe.to_string_lossy());
        let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(unit.as_str());
        if changed {
            std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok((
            Installed {
                manager: "systemd --user",
                id: UNIT,
                path: Some(path),
            },
            changed,
        ))
    }

    pub async fn enable() -> Result<()> {
        ensure_systemd()?;
        run_quiet("systemctl", &["--user", "daemon-reload"]).await?;
        if !run_quiet("systemctl", &["--user", "enable", UNIT]).await? {
            anyhow::bail!("`systemctl --user enable {UNIT}` failed");
        }
        // Que siga sincronizando sin sesión activa (NAS / SteamOS / servidor).
        // Best-effort: puede pedir un polkit que no hay a quien enseñar.
        let _ = run_quiet("loginctl", &["enable-linger"]).await;
        Ok(())
    }

    pub async fn start() -> Result<()> {
        if !run_quiet("systemctl", &["--user", "start", UNIT]).await? {
            anyhow::bail!("`systemctl --user start {UNIT}` failed — see `hoard sync`");
        }
        Ok(())
    }

    pub async fn restart() -> Result<()> {
        if !run_quiet("systemctl", &["--user", "restart", UNIT]).await? {
            anyhow::bail!("`systemctl --user restart {UNIT}` failed — see `hoard sync`");
        }
        Ok(())
    }

    pub async fn disable() -> Result<()> {
        ensure_systemd()?;
        run_quiet("systemctl", &["--user", "disable", "--now", UNIT]).await?;
        let path = unit_path()?;
        let _ = std::fs::remove_file(&path);
        run_quiet("systemctl", &["--user", "daemon-reload"]).await?;
        Ok(())
    }

    pub async fn installed() -> bool {
        unit_path().map(|p| p.exists()).unwrap_or(false)
    }
}

// =======================================================================
// macOS — launchd LaunchAgent
// =======================================================================

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub const UNIT: &str = "com.hoard.sync";

    pub fn plist_path() -> Result<PathBuf> {
        Ok(home()?
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{UNIT}.plist")))
    }

    fn log_path() -> Result<PathBuf> {
        Ok(home()?.join("Library").join("Logs").join("hoard-sync.log"))
    }

    async fn current_uid() -> Result<String> {
        let out = tokio::process::Command::new("id")
            .arg("-u")
            .output()
            .await
            .context("running `id -u`")?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// El plist. Puro y testeable: es el contrato con launchd.
    pub fn plist_text(exe: &str, log: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\t<string>{label}</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n\
             \t<key>KeepAlive</key>\n\t<true/>\n\
             \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
             \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
             </dict>\n\
             </plist>\n",
            label = UNIT,
        )
    }

    pub fn declare() -> Result<(Installed, bool)> {
        let exe = service_binary();
        let log = log_path()?;
        let path = plist_path()?;
        for dir in [path.parent(), log.parent()].into_iter().flatten() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let plist = plist_text(&exe.to_string_lossy(), &log.to_string_lossy());
        let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(plist.as_str());
        if changed {
            std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok((
            Installed {
                manager: "launchd",
                id: UNIT,
                path: Some(path),
            },
            changed,
        ))
    }

    /// launchd no distingue "instalar" de "cargar": el plist en
    /// `~/Library/LaunchAgents` ya se carga en el siguiente inicio de sesión, así
    /// que escribirlo **es** habilitarlo.
    pub async fn enable() -> Result<()> {
        Ok(())
    }

    pub async fn start() -> Result<()> {
        let uid = current_uid().await?;
        let domain = format!("gui/{uid}");
        let plist = plist_path()?;
        let plist = plist.to_string_lossy().to_string();
        // Recarga limpia si ya estaba cargado.
        let _ = run_quiet("launchctl", &["bootout", &domain, &plist]).await;
        if !run_quiet("launchctl", &["bootstrap", &domain, &plist]).await? {
            anyhow::bail!("`launchctl bootstrap {domain}` failed");
        }
        Ok(())
    }

    pub async fn restart() -> Result<()> {
        let uid = current_uid().await?;
        let target = format!("gui/{uid}/{UNIT}");
        if !run_quiet("launchctl", &["kickstart", "-k", &target]).await? {
            anyhow::bail!("`launchctl kickstart {target}` failed");
        }
        Ok(())
    }

    pub async fn disable() -> Result<()> {
        let uid = current_uid().await?;
        let domain = format!("gui/{uid}");
        let plist = plist_path()?;
        let _ = run_quiet("launchctl", &["bootout", &domain, &plist.to_string_lossy()]).await;
        let _ = std::fs::remove_file(&plist);
        Ok(())
    }

    pub async fn installed() -> bool {
        plist_path().map(|p| p.exists()).unwrap_or(false)
    }
}

// =======================================================================
// Windows — Task Scheduler (por usuario, al inicio de sesión)
// =======================================================================

#[cfg(target_os = "windows")]
mod platform {
    use super::*;

    pub const UNIT: &str = "HoardSync";

    pub async fn installed() -> bool {
        run_quiet("schtasks", &["/Query", "/TN", UNIT])
            .await
            .unwrap_or(false)
    }

    /// La cuenta del que llama, como `DOMINIO\usuario`: a eso se acotan el
    /// disparador y el principal. Sin dominio vale el nombre a secas (el Task
    /// Scheduler lo resuelve contra la máquina local).
    fn current_account() -> Result<String> {
        let user = std::env::var("USERNAME")
            .ok()
            .filter(|s| !s.is_empty())
            .context("no USERNAME in the environment")?;
        let domain = std::env::var("USERDOMAIN").ok().filter(|s| !s.is_empty());
        Ok(match domain {
            Some(d) => format!("{d}\\{user}"),
            None => user,
        })
    }

    /// La tarea no es un fichero que podamos comparar, así que se declara
    /// siempre "cambiada" y `enable` la reescribe (`/F`). Es lo que hace que una
    /// actualización que mueva el `.exe` re-apunte la tarea sola, igual que el
    /// desktop reafirma su propia entrada de autostart en cada arranque.
    pub fn declare() -> Result<(Installed, bool)> {
        Ok((
            Installed {
                manager: "Task Scheduler",
                id: UNIT,
                path: None,
            },
            true,
        ))
    }

    pub async fn enable() -> Result<()> {
        let exe = service_binary();
        let account = current_account()?;
        let xml = super::task_xml(&exe.to_string_lossy(), &account);

        // `/XML` lee la definición de un fichero; se escribe junto a los demás
        // temporales del proceso y con el pid en el nombre, para que dos shells
        // no se pisen.
        let path = std::env::temp_dir().join(format!("hoard-sync-{}.xml", std::process::id()));
        std::fs::write(&path, super::to_utf16le_with_bom(&xml))
            .with_context(|| format!("writing {}", path.display()))?;
        let created = run_quiet(
            "schtasks",
            &[
                "/Create",
                "/TN",
                UNIT,
                "/XML",
                &path.to_string_lossy(),
                "/F",
            ],
        )
        .await;
        let _ = std::fs::remove_file(&path);

        if !created? {
            anyhow::bail!(
                "`schtasks /Create` failed. Re-run it from an elevated PowerShell \
                 (right-click → \"Run as administrator\")."
            );
        }
        Ok(())
    }

    pub async fn start() -> Result<()> {
        if !run_quiet("schtasks", &["/Run", "/TN", UNIT]).await? {
            anyhow::bail!("`schtasks /Run /TN {UNIT}` failed — see `hoard sync`");
        }
        Ok(())
    }

    pub async fn restart() -> Result<()> {
        let _ = run_quiet("schtasks", &["/End", "/TN", UNIT]).await;
        start().await
    }

    pub async fn disable() -> Result<()> {
        let _ = run_quiet("schtasks", &["/End", "/TN", UNIT]).await;
        if !run_quiet("schtasks", &["/Delete", "/TN", UNIT, "/F"]).await? {
            anyhow::bail!("`schtasks /Delete /TN {UNIT}` failed");
        }
        Ok(())
    }
}

// =======================================================================
// Cualquier otro SO
// =======================================================================

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::*;

    pub const UNIT: &str = "hoard-sync";

    pub fn declare() -> Result<(Installed, bool)> {
        anyhow::bail!("no service backend for this OS — run `hoardd` under your own supervisor")
    }
    pub async fn enable() -> Result<()> {
        anyhow::bail!("no service backend for this OS")
    }
    pub async fn start() -> Result<()> {
        anyhow::bail!("no service backend for this OS")
    }
    pub async fn restart() -> Result<()> {
        anyhow::bail!("no service backend for this OS")
    }
    pub async fn disable() -> Result<()> {
        anyhow::bail!("no service backend for this OS")
    }
    pub async fn installed() -> bool {
        false
    }
}

// ---- Windows: XML de la tarea (puro, testeable en cualquier SO) --------

/// Escapa un valor para contenido/atributo XML.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// El XML de `HoardSync`: ejecuta `hoardd` al inicio de sesión de `user`, como
/// `user` y sin elevar.
///
/// `schtasks /Create /SC ONLOGON` exige consola elevada aunque se pase
/// `/RL LIMITED`; registrar este XML —cuyo disparador y principal están acotados
/// a la propia cuenta del que llama— no. (Las dos cosas, comprobadas contra una
/// máquina Windows real con token filtrado: ONLOGON → "Acceso denegado", este
/// XML → tarea creada.)
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn task_xml(exe: &str, user: &str) -> String {
    let exe = xml_escape(exe);
    let user = xml_escape(user);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <Triggers>\n\
         \x20   <LogonTrigger>\n\
         \x20     <UserId>{user}</UserId>\n\
         \x20   </LogonTrigger>\n\
         \x20 </Triggers>\n\
         \x20 <Principals>\n\
         \x20   <Principal id=\"Author\">\n\
         \x20     <UserId>{user}</UserId>\n\
         \x20     <LogonType>InteractiveToken</LogonType>\n\
         \x20     <RunLevel>LeastPrivilege</RunLevel>\n\
         \x20   </Principal>\n\
         \x20 </Principals>\n\
         \x20 <Settings>\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
         \x20   <StartWhenAvailable>true</StartWhenAvailable>\n\
         \x20   <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n\
         \x20   <Exec>\n\
         \x20     <Command>{exe}</Command>\n\
         \x20   </Exec>\n\
         \x20 </Actions>\n\
         </Task>\n",
    )
}

/// El Task Scheduler sólo ingiere el XML de forma fiable como UTF-16 LE con BOM
/// — un fichero UTF-8 (aun con la declaración correspondiente) muere dentro de
/// `schtasks /Create /XML` con "unable to switch the encoding", comprobado
/// contra una máquina Windows real. La declaración de [`task_xml`] dice UTF-16
/// para casar.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn to_utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La unidad tiene que ejecutar **el daemon**, no un cliente: desde el 4b/4c
    /// `hoard sync run` es un espectador, y supervisar a un espectador significa
    /// que `systemctl --user stop` no para el sync.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_unit_execs_the_daemon_itself() {
        let unit = platform::unit_text("/usr/local/bin/hoardd");
        assert!(
            unit.contains("ExecStart=\"/usr/local/bin/hoardd\"\n"),
            "unexpected ExecStart:\n{unit}"
        );
        assert!(
            !unit.contains("sync run"),
            "the unit must not exec a client"
        );
        // Sin `WantedBy` no hay arranque en boot, que es el punto del módulo.
        assert!(unit.contains("WantedBy=default.target"));
    }

    /// Una ruta con espacios (un AppImage en `~/Mis programas/`) sobrevive al
    /// tokenizador de systemd gracias a las comillas.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_path_with_spaces_survives_the_unit() {
        let unit = platform::unit_text("/home/ada/Mis programas/hoardd");
        assert!(unit.contains("ExecStart=\"/home/ada/Mis programas/hoardd\""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_agent_execs_the_daemon_itself() {
        let plist = platform::plist_text("/Applications/Hoard.app/Contents/MacOS/hoardd", "/tmp/l");
        assert!(plist.contains("<string>/Applications/Hoard.app/Contents/MacOS/hoardd</string>"));
        assert!(!plist.contains("<string>sync</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn escapes_the_five_xml_metacharacters() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(
            xml_escape(r"C:\Program Files\hoardd.exe"),
            r"C:\Program Files\hoardd.exe"
        );
    }

    /// La tarea de Windows: acotada a la cuenta que la crea (nunca máquina) y
    /// ejecutando el daemon **sin argumentos** — ya no hay un `sync run` de por
    /// medio.
    #[test]
    fn task_xml_scopes_the_trigger_and_principal_to_the_account() {
        let xml = task_xml(r"C:\Program Files\Hoard\hoardd.exe", r"CORP\ada");
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-16\"?>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<LogonTrigger>\n      <UserId>CORP\\ada</UserId>"));
        assert!(xml.contains("<Principal id=\"Author\">\n      <UserId>CORP\\ada</UserId>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<Command>C:\\Program Files\\Hoard\\hoardd.exe</Command>"));
        assert!(
            !xml.contains("<Arguments>"),
            "the daemon takes no arguments"
        );
    }

    #[test]
    fn task_xml_escapes_the_exe_path() {
        let xml = task_xml(r"C:\R&D\hoardd.exe", "ada");
        assert!(xml.contains("<Command>C:\\R&amp;D\\hoardd.exe</Command>"));
    }

    #[test]
    fn utf16le_bom_encoding_round_trips() {
        let bytes = to_utf16le_with_bom("<a>ñ</a>");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "BOM must lead the file");
        assert_eq!(bytes.len() % 2, 0, "UTF-16 LE is an even byte count");
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "<a>ñ</a>");
    }

    /// La unidad ejecuta el mismo binario que "spawn if absent" lanzaría: dos
    /// vías de arranque que resolvieran binarios distintos serían dos versiones
    /// del servicio según quién lo arrancara.
    #[test]
    fn the_unit_and_spawn_if_absent_agree_on_the_binary() {
        assert_eq!(service_binary(), crate::client::daemon_binary());
    }
}
