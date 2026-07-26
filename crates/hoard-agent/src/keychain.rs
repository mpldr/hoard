//! El llavero del sistema, **siempre acotado**.
//!
//! Toda llamada a `keyring` del cliente pasa por aquí: la sesión Cloud
//! ([`crate::cloud_auth`]) y el token self-hosted ([`crate::credentials`]). El
//! motivo es el fallo de D.19 (ADR 0021): un llavero bloqueado no falla, se queda
//! esperando, y una llamada síncrona que no vuelve nunca cuelga a quien la hizo.
//!
//! Las dos sesiones comparten **el mismo hilo y la misma cola** a propósito. No
//! hay dos llaveros: si `org.freedesktop.secrets` está bloqueado lo está para las
//! dos, así que un hilo por módulo sólo daría dos hilos colgados en vez de uno.
//! Con un llavero sano las operaciones tardan milisegundos y la cola no se nota.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Result};

/// Tope de espera de **cualquier** operación del llavero.
///
/// Un llavero bloqueado no falla: `org.freedesktop.secrets` se queda esperando a
/// que alguien conteste el prompt de desbloqueo, y en una sesión sin escritorio
/// (SSH, NAS, el dogfooding de D.19) nadie lo va a contestar nunca. Esa espera sin
/// tope dejaba el motor en `starting` para siempre y **sin una línea de log**
/// (`last_error` en `None`, indistinguible de "arrancando") y, peor, hacía que el
/// daemon no se pudiera parar: `abort()` no desaloja una llamada síncrona, así que
/// `systemctl --user stop` se quedaba en `deactivating` hasta el SIGKILL.
///
/// Un llavero sano contesta en milisegundos, así que cinco segundos no dan falsos
/// positivos; y si el usuario tarda en teclear su contraseña, el reintento del
/// keeper lo recoge en cuanto quede desbloqueado.
pub const KEYRING_TIMEOUT: Duration = Duration::from_secs(5);

/// El llavero no contestó dentro del tope. Es un tipo propio para que "está
/// bloqueado" no se confunda con "no hay sesión": confundirlos es exactamente lo
/// que hacía invisible el fallo.
#[derive(Debug)]
pub struct KeyringTimeout {
    /// Qué se estaba haciendo, en una frase, para el log y el `last_error`.
    pub doing: &'static str,
    pub after: Duration,
}

impl std::fmt::Display for KeyringTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the system keyring didn't answer in {}s while {} — it is most likely \
             locked, waiting for an unlock nobody can answer; unlock the login \
             keyring (or sign in again with `hoard login`)",
            self.after.as_secs(),
            self.doing
        )
    }
}

impl std::error::Error for KeyringTimeout {}

type KeyringJob = Box<dyn FnOnce() + Send>;

/// Cola del **único** hilo que habla con el llavero.
///
/// Un hilo por llamada bastaría para no esperar de más, pero la llamada colgada no
/// se puede cancelar: con el llavero bloqueado y el keeper reintentando cada pocos
/// minutos, cada intento dejaría un hilo más colgado para siempre. Serializando,
/// lo que se acumula es la cola (un `Box` por intento) y no los hilos.
///
/// Y es un hilo **suelto**, no del pool de `spawn_blocking`: al soltarse el
/// runtime, tokio espera a que terminen sus hilos de bloqueo, así que uno colgado
/// ahí volvería a impedir que el proceso muera — que es justo la mitad del bug.
/// A un hilo propio y sin `join` no lo espera nadie al salir.
fn keyring_queue() -> Option<&'static Mutex<mpsc::Sender<KeyringJob>>> {
    static QUEUE: OnceLock<Option<Mutex<mpsc::Sender<KeyringJob>>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<KeyringJob>();
            std::thread::Builder::new()
                .name("hoard-keyring".to_string())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                })
                .map_err(|err| {
                    tracing::error!(error = %err, "keyring: couldn't start the keyring thread")
                })
                .ok()?;
            Some(Mutex::new(tx))
        })
        .as_ref()
}

/// Corre `op` en el hilo del llavero y **deja de esperarla** pasado `wait`.
/// Devuelve lo que devuelva `op`, o [`KeyringTimeout`] si no contestó a tiempo.
pub(crate) fn keyring_op<T: Send + 'static>(
    doing: &'static str,
    wait: Duration,
    op: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let Some(queue) = keyring_queue() else {
        bail!("no keyring thread available");
    };
    let (tx, rx) = mpsc::channel();
    let job: KeyringJob = Box::new(move || {
        // Si quien preguntó ya se rindió, el envío falla y el resultado se
        // descarta: nadie se queda esperando a nadie.
        let _ = tx.send(op());
    });
    // Igual que en el journal y en la ranura del motor: un pánico ajeno no puede
    // dejar el llavero inaccesible para siempre.
    let sender = queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if sender.send(job).is_err() {
        bail!("the keyring thread is gone");
    }
    drop(sender);
    match rx.recv_timeout(wait) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            Err(anyhow::Error::new(KeyringTimeout { doing, after: wait }))
        }
        // El hilo se fue con la operación a medias (un pánico dentro de `keyring`).
        Err(RecvTimeoutError::Disconnected) => bail!("the keyring call died while {doing}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Lo que hace un llavero bloqueado: esperar un desbloqueo que nadie va a
    /// contestar. Se simula con una operación que tarda mucho más que el tope (no
    /// con una infinita, para no dejar el hilo del llavero ocupado el resto de la
    /// suite).
    fn a_locked_keyring() -> impl FnOnce() -> Result<Option<String>> + Send + 'static {
        || {
            std::thread::sleep(Duration::from_millis(300));
            // A estas alturas ya nadie escucha: el resultado se descarta y el hilo
            // del llavero queda libre para el siguiente test.
            Ok(None)
        }
    }

    /// El fallo de D.19: la llamada al llavero no volvía nunca. Ahora se deja de
    /// esperar, y con un motivo tipado — el que aterriza en `last_error` y en el
    /// log del servicio.
    #[test]
    fn a_keyring_that_never_answers_gives_up_with_a_reason() {
        let started = Instant::now();
        let err = keyring_op(
            "reading the Cloud session",
            Duration::from_millis(20),
            a_locked_keyring(),
        )
        .expect_err("tiene que rendirse, no esperar");
        // Lo que importa no es el número sino que la espera esté acotada: quien
        // llamó recupera el control muchísimo antes de que la operación termine.
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "esperó de más: {:?}",
            started.elapsed()
        );

        let timeout = err
            .downcast_ref::<KeyringTimeout>()
            .expect("el motivo va tipado, no sólo en el texto");
        assert_eq!(timeout.doing, "reading the Cloud session");
        let text = err.to_string();
        assert!(
            text.contains("keyring") && text.contains("locked"),
            "{text}"
        );
    }

    /// Un llavero que sí contesta pasa tal cual, y su fallo propio (sin D-Bus,
    /// entrada corrupta) no se disfraza de tope: el motivo tiene que ser el de
    /// verdad.
    #[test]
    fn a_keyring_that_answers_is_passed_through_verbatim() {
        let got = keyring_op("reading the Cloud session", KEYRING_TIMEOUT, || {
            Ok(Some("jwt".to_string()))
        })
        .expect("contesta");
        assert_eq!(got.as_deref(), Some("jwt"));

        let err = keyring_op::<()>("reading the Cloud session", KEYRING_TIMEOUT, || {
            bail!("no D-Bus session bus")
        })
        .expect_err("el fallo del llavero se propaga");
        assert!(err.downcast_ref::<KeyringTimeout>().is_none());
        assert_eq!(err.to_string(), "no D-Bus session bus");
    }

    /// El tope vale para las dos sesiones, que es el motivo de que el hilo sea
    /// uno solo: la del token self-hosted se rinde igual que la Cloud, y con su
    /// propio motivo.
    #[test]
    fn the_self_hosted_session_is_bounded_by_the_same_thread() {
        let err = keyring_op(
            "reading the self-hosted session",
            Duration::from_millis(20),
            a_locked_keyring(),
        )
        .expect_err("tiene que rendirse, no esperar");
        let timeout = err.downcast_ref::<KeyringTimeout>().expect("motivo tipado");
        assert_eq!(timeout.doing, "reading the self-hosted session");
    }

    /// La otra mitad de D.19: la espera no puede vivir en el hilo de la task que
    /// el apagado aborta. Con la lectura en el pool de bloqueo, la task que la
    /// aguarda se cancela en el momento — el runtime queda libre y el daemon
    /// parable, aunque el llavero siga sin contestar.
    #[tokio::test(flavor = "current_thread")]
    async fn a_task_waiting_on_the_keyring_can_be_aborted_at_once() {
        let task = tokio::spawn(async {
            match tokio::task::spawn_blocking(|| {
                keyring_op(
                    "reading the Cloud session",
                    Duration::from_secs(30),
                    a_locked_keyring(),
                )
            })
            .await
            {
                Ok(result) => result,
                Err(join) => Err(anyhow::Error::new(join)),
            }
        });
        // Un runtime de un solo hilo: si la espera estuviera en él, este `yield`
        // no volvería y el `abort` no llegaría a ejecutarse.
        tokio::task::yield_now().await;
        let started = Instant::now();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "el apagado esperó al llavero: {:?}",
            started.elapsed()
        );
    }
}
