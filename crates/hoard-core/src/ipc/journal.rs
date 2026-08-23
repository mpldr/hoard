//! Journal append-only con cursor — la mitad "el cliente que no estaba" de la
//! entrega de eventos (ADR 0021, D.14.2).
//!
//! El push por el socket sirve al cliente conectado *ahora*. No sirve al que
//! arranca tarde: sólo-push es exactamente el bug de las campanas mudas (la UI
//! sin snapshot ni backlog). Así que el daemon guarda lo que pasa y el cliente
//! pide "todo lo posterior al cursor N" antes de escuchar en vivo. Misma forma
//! que ya tienen Supabase Realtime + el airbag del poll.
//!
//! ## Lo que NO se guarda: reposos repetidos
//!
//! Guardar cada decisión de cada tick amplifica escritura de forma absurda —
//! medido en este repo el 2026-07-25: **3015 `cloud state stale` en 36 minutos**
//! (~84/min, >100k/día) con tick de 2 s, y el disco a proteger es el SSD del
//! Deck. La regla de la ADR es «guardar transiciones y acciones, no reposos
//! repetidos; y colapsar rachas del mismo motivo en una fila con contador».
//!
//! Eso es [`collapse_key`]: los eventos de *reposo/veto* (el motor está
//! esperando por algo, y sigue esperando por lo mismo) colapsan sobre la fila
//! de la cola con un contador; las **transiciones** (juego arranca/para) y las
//! **acciones** (subida, restore) siempre generan fila propia. Un colapso no se
//! empuja en vivo: por definición no hay información nueva que contar.
//!
//! ## Frontera con el Slice 5 (SQLite)
//!
//! Aquí no hay IO: [`JournalEntry`] es el tipo de wire, [`collapse_key`] es la
//! política y [`Journal`] es un anillo en memoria con tope. El Slice 5 cambia
//! **sólo** el almacén (tabla-anillo en la SQLite privada del daemon, que es
//! también el log de decisiones de C.5): el tipo y la política se quedan tal
//! cual, y `Journal` pasa a ser la implementación en memoria o desaparece. No
//! meter aquí nada que necesite disco.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::events::AgentEvent;

/// Filas retenidas por el anillo en memoria. Dimensionado para el hueco que de
/// verdad tiene que cubrir: un cliente que se reconecta tarda segundos, no
/// horas. Si un cliente pide más atrás de lo que queda, se le dice
/// ([`Backlog::gap`]) en vez de mentirle con un historial parcial.
pub const DEFAULT_CAPACITY: usize = 1024;

/// Una fila del journal. `seq` es el cursor: monótono, sin huecos, por
/// ejecución del daemon (ver `epoch` en [`super::Welcome`] — un daemon
/// reiniciado empieza de nuevo en 1, y el epoch es lo que le dice al cliente
/// que su cursor viejo ya no vale).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: u64,
    /// Cuándo se vio la **primera** ocurrencia de esta fila.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Cuándo se vio la **última** (igual a `at` mientras `repeat == 1`).
    #[serde(with = "time::serde::rfc3339")]
    pub last_at: OffsetDateTime,
    /// Ocurrencias colapsadas en esta fila, empezando en 1. `> 1` sólo puede
    /// pasarle a un evento de reposo (ver [`collapse_key`]).
    pub repeat: u32,
    pub event: AgentEvent,
}

/// Resultado de [`Journal::append`].
#[derive(Debug, Clone)]
pub enum Appended {
    /// Fila nueva. **Esto es lo que se empuja en vivo.**
    Recorded(JournalEntry),
    /// Racha del mismo reposo: se sumó al contador de la fila `seq`. No se
    /// empuja (el cliente ya sabe que el motor está esperando por eso).
    Collapsed { seq: u64, repeat: u32 },
}

/// Respuesta a "dame todo lo posterior al cursor N".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backlog {
    pub entries: Vec<JournalEntry>,
    /// Cursor tras aplicar `entries` (el `seq` de la última fila del journal,
    /// haya o no filas nuevas).
    pub cursor: u64,
    /// El journal ya no tiene todo lo que el cliente pedía: se cayó del anillo,
    /// o el cursor es de otra ejecución del daemon. El cliente debe re-sembrar
    /// su estado con [`super::Request::Status`] en vez de asumir continuidad —
    /// mentirle aquí es cómo se pierde un historial sin que nadie se entere.
    #[serde(default)]
    pub gap: bool,
}

/// Anillo append-only en memoria.
#[derive(Debug)]
pub struct Journal {
    entries: VecDeque<JournalEntry>,
    capacity: usize,
    /// Próximo `seq` a asignar. Empieza en 1 para que `cursor == 0` signifique
    /// sin ambigüedad "nunca he visto nada".
    next_seq: u64,
    dropped: u64,
}

impl Default for Journal {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl Journal {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            next_seq: 1,
            dropped: 0,
        }
    }

    /// Cursor actual: `seq` de la última fila, o 0 si el journal está vacío.
    pub fn cursor(&self) -> u64 {
        self.next_seq - 1
    }

    /// Filas caídas del anillo desde el arranque. Diagnóstico (y la señal de
    /// que el tope está mal dimensionado).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Registra `event`. Si es un reposo idéntico al de la fila de la cola,
    /// suma al contador de esa fila en vez de crear una nueva.
    pub fn append(&mut self, at: OffsetDateTime, event: AgentEvent) -> Appended {
        if let Some(key) = collapse_key(&event) {
            if let Some(tail) = self.entries.back_mut() {
                if collapse_key(&tail.event).as_deref() == Some(key.as_str()) {
                    tail.repeat = tail.repeat.saturating_add(1);
                    tail.last_at = at;
                    return Appended::Collapsed {
                        seq: tail.seq,
                        repeat: tail.repeat,
                    };
                }
            }
        }
        let entry = JournalEntry {
            seq: self.next_seq,
            at,
            last_at: at,
            repeat: 1,
            event,
        };
        self.next_seq += 1;
        self.entries.push_back(entry.clone());
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
            self.dropped += 1;
        }
        Appended::Recorded(entry)
    }

    /// Todo lo posterior a `cursor`. Marca [`Backlog::gap`] cuando no puede
    /// servir el tramo completo.
    pub fn since(&self, cursor: u64) -> Backlog {
        let entries: Vec<JournalEntry> = self
            .entries
            .iter()
            .filter(|e| e.seq > cursor)
            .cloned()
            .collect();
        // Hueco por dos motivos distintos: el tramo pedido ya se cayó del
        // anillo, o el cursor viene del futuro (típicamente el cliente guardó
        // el cursor de un daemon anterior; el `epoch` del handshake es la
        // detección buena, esto es el cinturón).
        let oldest = self.entries.front().map(|e| e.seq);
        let lost = match oldest {
            Some(first) => cursor + 1 < first,
            None => self.dropped > 0 && cursor < self.cursor(),
        };
        let from_the_future = cursor > self.cursor();
        Backlog {
            entries,
            cursor: self.cursor(),
            gap: lost || from_the_future,
        }
    }
}

/// Clave de colapso: `Some` para eventos de **reposo** (el motor sigue
/// esperando por lo mismo), `None` para transiciones y acciones, que siempre
/// generan fila.
///
/// La clave es el JSON del propio evento, así que sólo colapsan repeticiones
/// **idénticas**: si cambia el motivo del veto, el error o el juego, la fila es
/// nueva. Usar la serialización en vez de listar campos a mano es a propósito —
/// añadir un campo a una variante no puede olvidarse de la clave.
///
/// Qué entra y por qué:
///
/// - `RestoreDeferred` — el veto mid-session. El sweep lo re-evalúa cada tick y
///   lo re-emite mientras el juego siga vivo: es el caso 3015-en-36-minutos en
///   miniatura.
/// - `SaveAutoRestoreFailed` — el mismo error una y otra vez es el sweep
///   reintentando, no N incidencias distintas (fue lo que llenó el feed en el
///   incidente de julio-2026).
/// - `SaveAutoRestoreStuck` — de por sí one-shot por (save, versión); el
///   colapso lo hace idempotente si el shell lo re-emite.
/// - `BackupThrottled` — esperas por la ventana de banda del server. Reposo con
///   motivo; sólo cambia de fila si cambia el `retry_after_secs`.
/// - `BackupQuotaFull` — la cuenta está llena. Cada save lo descubre por su
///   cuenta y el park lo re-emite cada hora: son N informes del mismo hecho,
///   no N incidencias. Colapsa por cifras, así que la fila se refresca cuando
///   el usuario libera algo y sigue sin llegar.
/// - `BackupFilesUnreadable` — el mismo fichero que no se deja leer sale otra
///   vez en cada copia mientras dure la causa (un proveedor de ficheros bajo
///   demanda parado puede durar semanas). Un aviso, no uno por copia. Colapsa
///   por contenido, así que si aparece otro fichero —o cambia el error— la fila
///   es nueva.
/// - `HeavyProcessDetected` — el mismo proceso pesado visto otra vez no es un
///   descubrimiento nuevo.
///
/// Qué NO entra, aunque se repita: `BackupScheduled`. Parece reposo pero cada
/// emisión es información nueva — el debounce **se reinició** y la cuenta atrás
/// de la UI arranca otra vez. Colapsarlo silenciaría ese refresco (los
/// colapsos no se empujan). Las transiciones (`GameStarted`/`GameStopped`) y
/// las acciones (`Backup*` de verdad, `SaveAutoRestored`,
/// `SaveConflictsBackedUp`) tampoco: son el historial.
pub fn collapse_key(event: &AgentEvent) -> Option<String> {
    // `BackupQuotaFull` es el único que colapsa **entre saves distintos**: el
    // hecho es de la cuenta, no del save, así que veinte juegos chocando contra
    // el mismo muro son una fila, no veinte. Por eso su clave se construye a
    // mano en vez de serializar el evento entero (que incluiría el `save_id` y
    // los volvería a separar).
    if let AgentEvent::BackupQuotaFull {
        plan,
        used_bytes,
        limit_bytes,
        ..
    } = event
    {
        return Some(format!("quota_full:{plan}:{used_bytes}:{limit_bytes}"));
    }
    let restful = matches!(
        event,
        AgentEvent::RestoreDeferred { .. }
            | AgentEvent::SaveAutoRestoreFailed { .. }
            | AgentEvent::SaveAutoRestoreStuck { .. }
            | AgentEvent::BackupThrottled { .. }
            | AgentEvent::BackupFilesUnreadable { .. }
            | AgentEvent::HeavyProcessDetected { .. }
    );
    if !restful {
        return None;
    }
    serde_json::to_string(event).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_753_000_000 + secs).unwrap()
    }

    fn deferred(save: &str, reason: &str) -> AgentEvent {
        AgentEvent::RestoreDeferred {
            save_id: save.to_string(),
            game_slug: "factorio".to_string(),
            reason: reason.to_string(),
        }
    }

    fn started(save: &str) -> AgentEvent {
        AgentEvent::GameStarted {
            save_id: save.to_string(),
            game_slug: "factorio".to_string(),
        }
    }

    #[test]
    fn cursor_starts_empty_and_advances_by_one() {
        let mut j = Journal::default();
        assert_eq!(j.cursor(), 0);
        j.append(ts(0), started("a"));
        assert_eq!(j.cursor(), 1);
        j.append(ts(1), started("b"));
        assert_eq!(j.cursor(), 2);
    }

    /// El caso medido: una racha de reposos idénticos es UNA fila con contador.
    /// 3015 holds en 36 min no pueden ser 3015 escrituras.
    #[test]
    fn a_run_of_the_same_rest_collapses_into_one_row() {
        let mut j = Journal::default();
        let first = j.append(ts(0), deferred("s1", "game is running"));
        assert!(matches!(first, Appended::Recorded(_)));
        for i in 1..3015 {
            match j.append(ts(i), deferred("s1", "game is running")) {
                Appended::Collapsed { seq, repeat } => {
                    assert_eq!(seq, 1);
                    assert_eq!(repeat as i64, i + 1);
                }
                Appended::Recorded(_) => panic!("identical rest must not open a new row"),
            }
        }
        assert_eq!(j.len(), 1);
        assert_eq!(j.cursor(), 1);
        let row = &j.since(0).entries[0];
        assert_eq!(row.repeat, 3015);
        assert_eq!(row.at, ts(0));
        assert_eq!(row.last_at, ts(3014));
    }

    /// Cambiar el motivo es información nueva: fila nueva.
    #[test]
    fn a_different_reason_opens_a_new_row() {
        let mut j = Journal::default();
        j.append(ts(0), deferred("s1", "game is running"));
        j.append(ts(1), deferred("s1", "local changes pending"));
        assert_eq!(j.len(), 2);
    }

    /// Transiciones y acciones nunca colapsan: son el historial que el cliente
    /// tardío viene a buscar.
    #[test]
    fn transitions_never_collapse() {
        let mut j = Journal::default();
        for i in 0..3 {
            assert!(matches!(
                j.append(ts(i), started("s1")),
                Appended::Recorded(_)
            ));
        }
        assert_eq!(j.len(), 3);
        assert!(collapse_key(&started("s1")).is_none());
        assert!(collapse_key(&AgentEvent::BackupScheduled {
            save_id: "s1".into(),
            delay_ms: 5000,
            reason: crate::ipc::events::BackupReason::FilesystemSettled,
        })
        .is_none());
    }

    /// Un reposo entre medias no impide que el siguiente igual colapse, pero sí
    /// rompe la racha cuando hay otra fila detrás (sólo se colapsa contra la
    /// cola, nunca contra una fila enterrada — eso reordenaría el historial).
    #[test]
    fn collapsing_only_looks_at_the_tail() {
        let mut j = Journal::default();
        j.append(ts(0), deferred("s1", "game is running"));
        j.append(ts(1), started("s1"));
        j.append(ts(2), deferred("s1", "game is running"));
        assert_eq!(j.len(), 3);
        assert_eq!(j.since(0).entries[2].repeat, 1);
    }

    #[test]
    fn since_returns_only_newer_rows() {
        let mut j = Journal::default();
        j.append(ts(0), started("a"));
        j.append(ts(1), started("b"));
        j.append(ts(2), started("c"));
        let back = j.since(1);
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].seq, 2);
        assert_eq!(back.cursor, 3);
        assert!(!back.gap);
        // Al día: nada nuevo, y sin hueco.
        let none = j.since(3);
        assert!(none.entries.is_empty());
        assert!(!none.gap);
    }

    /// Lo que el anillo tira se reporta como hueco, no se disimula.
    #[test]
    fn dropping_old_rows_reports_a_gap() {
        let mut j = Journal::with_capacity(2);
        j.append(ts(0), started("a"));
        j.append(ts(1), started("b"));
        j.append(ts(2), started("c"));
        assert_eq!(j.len(), 2);
        assert_eq!(j.dropped(), 1);
        let from_scratch = j.since(0);
        assert!(from_scratch.gap);
        assert_eq!(from_scratch.entries.len(), 2);
        // Un cliente que ya tenía la fila 1 no perdió nada.
        assert!(!j.since(1).gap);
    }

    /// Cursor del futuro (típicamente de un daemon anterior): hueco, para que
    /// el cliente re-siembre en vez de quedarse esperando eventos que ya pasaron.
    #[test]
    fn a_cursor_from_the_future_is_a_gap() {
        let mut j = Journal::default();
        j.append(ts(0), started("a"));
        assert!(j.since(99).gap);
    }
}
