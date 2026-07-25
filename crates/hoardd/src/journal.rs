//! El journal del daemon: el anillo de [`hoard_core::ipc::journal`] más el canal
//! de push en vivo.
//!
//! Las dos mitades de la entrega de eventos (ADR 0021 D.14.2) comparten una sola
//! escritura: [`EventLog::record`] guarda **y** empuja. Guardar sirve al cliente
//! que no estaba (pide su cursor al conectar); empujar sirve al que está
//! conectado. Lo que no se empuja es un colapso —una racha del mismo reposo—,
//! porque por definición no cuenta nada nuevo.
//!
//! ## Frontera con el Slice 5
//!
//! Todo el estado está detrás de esta fachada: `record` / `since` / `cursor` /
//! `subscribe`. Cuando el journal pase a la tabla-anillo de la SQLite privada
//! del daemon (Slice 5, que es también el log de decisiones de C.5), cambia el
//! cuerpo de estos cuatro métodos y nada más — el servidor IPC no sabe dónde
//! viven las filas.

use std::sync::Mutex;

use hoard_core::ipc::journal::{Appended, Backlog, Journal, JournalEntry, DEFAULT_CAPACITY};
use hoard_core::ipc::AgentEvent;
use time::OffsetDateTime;
use tokio::sync::broadcast;

/// Filas en vuelo por suscriptor antes de considerarlo retrasado. Un cliente
/// normal drena en microsegundos; el tope existe para que uno atascado no crezca
/// sin límite en memoria del daemon. Al pasarse se le manda un `Resync` y él
/// vuelve a pedir por cursor, así que quedarse atrás no pierde nada.
const PUSH_BUFFER: usize = 256;

pub struct EventLog {
    journal: Mutex<Journal>,
    tx: broadcast::Sender<JournalEntry>,
}

impl EventLog {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(PUSH_BUFFER);
        Self {
            journal: Mutex::new(Journal::with_capacity(capacity)),
            tx,
        }
    }

    /// Registra un evento del motor y, si abrió fila, lo empuja a los
    /// suscriptores.
    pub fn record(&self, at: OffsetDateTime, event: AgentEvent) {
        // El lock es sólo alrededor del append (sin await dentro), así que un
        // suscriptor lento no puede bloquear al motor.
        let appended = {
            let mut journal = self.lock();
            journal.append(at, event)
        };
        match appended {
            Appended::Recorded(entry) => {
                // `send` falla cuando no hay suscriptores: es el caso normal
                // (daemon sin clientes) y no un error.
                let _ = self.tx.send(entry);
            }
            Appended::Collapsed { seq, repeat } => {
                tracing::trace!(seq, repeat, "hoardd: collapsed a repeated rest event");
            }
        }
    }

    pub fn since(&self, cursor: u64) -> Backlog {
        self.lock().since(cursor)
    }

    pub fn cursor(&self) -> u64 {
        self.lock().cursor()
    }

    pub fn dropped(&self) -> u64 {
        self.lock().dropped()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JournalEntry> {
        self.tx.subscribe()
    }

    /// Recupera el mutex envenenado en vez de propagar el pánico. Un pánico
    /// dentro del append dejaría el journal envenenado y **todos** los eventos
    /// posteriores caerían — que es exactamente cómo enmudeció el poller de
    /// D.11 (`.lock().unwrap()` sobre un mutex envenenado). Las filas son
    /// append-only: lo peor que puede haber pasado es media fila a medio
    /// escribir, no un estado que envenene lo siguiente.
    fn lock(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(|poisoned| {
            tracing::error!("hoardd: the journal mutex was poisoned; recovering");
            poisoned.into_inner()
        })
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(save: &str) -> AgentEvent {
        AgentEvent::GameStarted {
            save_id: save.to_string(),
            game_slug: "factorio".to_string(),
        }
    }

    fn deferred() -> AgentEvent {
        AgentEvent::RestoreDeferred {
            save_id: "s1".to_string(),
            game_slug: "factorio".to_string(),
            reason: "game is running".to_string(),
        }
    }

    #[tokio::test]
    async fn recording_pushes_new_rows_and_swallows_collapses() {
        let log = EventLog::new();
        let mut rx = log.subscribe();
        let now = OffsetDateTime::now_utc();

        log.record(now, started("s1"));
        log.record(now, deferred());
        // Tres reposos idénticos: una sola fila, un solo push.
        log.record(now, deferred());
        log.record(now, deferred());

        let first = rx.try_recv().unwrap();
        assert!(matches!(first.event, AgentEvent::GameStarted { .. }));
        let second = rx.try_recv().unwrap();
        assert!(matches!(second.event, AgentEvent::RestoreDeferred { .. }));
        assert!(rx.try_recv().is_err(), "a collapse must not be pushed");

        // Pero el contador sí está en el journal, para el cliente que llegue
        // tarde.
        let backlog = log.since(0);
        assert_eq!(backlog.entries.len(), 2);
        assert_eq!(backlog.entries[1].repeat, 3);
        assert_eq!(log.cursor(), 2);
    }

    #[tokio::test]
    async fn a_late_subscriber_catches_up_by_cursor() {
        let log = EventLog::new();
        let now = OffsetDateTime::now_utc();
        log.record(now, started("a"));
        log.record(now, started("b"));

        // Se suscribe después: el push no le trae nada del pasado…
        let mut rx = log.subscribe();
        assert!(rx.try_recv().is_err());
        // …y el backlog sí. Es el bug de las campanas mudas, cerrado por
        // construcción.
        let backlog = log.since(0);
        assert_eq!(backlog.entries.len(), 2);
        assert!(!backlog.gap);

        log.record(now, started("c"));
        let live = rx.try_recv().unwrap();
        assert_eq!(live.seq, 3);
    }
}
