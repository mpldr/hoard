//! Rutas HTTP self-hosted.
//!
//! # Puerta indulgente para los valores que salen de la DB (ADR 0021, C.3)
//!
//! Los tipos de [`hoard_core::wire`] llevan la puerta estricta en `serde`, así
//! que nada envenenado **entra** por la red. Pero estas respuestas se construyen
//! leyendo la SQLite del server, que es estado persistido con años encima: una
//! fila con un slug que hoy no pasaría la puerta debe **repararse o marcarse**,
//! nunca tumbar la petición. Rechazar en duro aquí sería exactamente el
//! "brickeo" que la ADR prohíbe, sólo que server-side y para todos los saves del
//! usuario a la vez.

pub mod admin;
pub mod auth;
pub mod cas;
pub mod devices;
pub mod events;
pub mod games;
pub mod health;
pub mod logs;
pub mod playtime;
pub mod saves;
pub mod snapshots;

use hoard_core::ids::{GameSlug, Repair, SaveId, Username};
use time::OffsetDateTime;

/// Slug de una fila de la DB, listo para el wire.
///
/// Válido → tal cual. Recuperable → re-derivado con `slugify` (con aviso, para
/// que el operador vea que tiene una fila vieja). Irrecuperable → el marcador
/// [`GameSlug::unknown`], que no casa con ningún juego.
///
/// Ojo: sólo entra por [`GameSlug::repair`] lo que la puerta estricta rechaza.
/// Un slug *degenerado* (`users`, el nombre de usuario de Windows) es
/// sintácticamente válido y sale tal cual: quién decide ignorarlo para
/// correlacionar es el cliente, y cambiarlo aquí rompería la identidad
/// `(user_id, game_slug, label)` que ese save ya tiene en la DB.
pub(crate) fn repair_slug(raw: &str) -> GameSlug {
    match GameSlug::parse(raw) {
        Ok(v) => v,
        Err(e) => match GameSlug::repair(raw) {
            Repair::Clean(v) | Repair::Repaired { value: v, .. } => {
                tracing::warn!(raw, repaired = %v, error = %e, "db: slug reparado al servirlo");
                v
            }
            Repair::Quarantined { reason, .. } => {
                tracing::warn!(raw, %reason, "db: slug irrecuperable, se sirve marcado");
                GameSlug::unknown()
            }
        },
    }
}

/// Username de la tabla `users`, listo para el wire. Mismo contrato que
/// [`repair_slug`]: se repara o se marca, nunca 500 (ver [`Username::unknown`]).
pub(crate) fn repair_username(raw: &str) -> Username {
    match Username::parse(raw) {
        Ok(v) => v,
        Err(e) => match Username::repair(raw) {
            Repair::Clean(v) | Repair::Repaired { value: v, .. } => {
                tracing::warn!(error = %e, "db: username reparado al servirlo");
                v
            }
            Repair::Quarantined { reason, .. } => {
                tracing::warn!(%reason, "db: username irrecuperable, se sirve marcado");
                Username::unknown()
            }
        },
    }
}

/// Id de un save leído de la DB. A diferencia del slug **no se puede reparar ni
/// marcar**: un id inventado apunta a otro save. Los ids son PKs que minta el
/// propio server (`Uuid::new_v4()`), así que un `None` aquí significa DB editada
/// a mano; se registra y la fila se omite, que es lo único seguro.
pub(crate) fn parse_save_id(raw: &str) -> Option<SaveId> {
    match SaveId::repair(raw).into_value() {
        Some(v) => Some(v),
        None => {
            tracing::error!(raw, "db: save_id que no es un UUID; fila omitida");
            None
        }
    }
}

/// Timestamp de una columna TEXT. El server los escribe con
/// `strftime('%Y-%m-%dT%H:%M:%SZ')`, así que esto sólo falla con una DB tocada a
/// mano: se avisa y se cae al epoch en vez de tumbar la respuesta.
pub(crate) fn repair_ts(raw: &str) -> OffsetDateTime {
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).unwrap_or_else(|e| {
        tracing::warn!(raw, error = %e, "db: timestamp ilegible, se sirve el epoch");
        OffsetDateTime::UNIX_EPOCH
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Una fila vieja con el slug sucio se sirve reparada en vez de tumbar el
    /// listado del usuario.
    #[test]
    fn poisoned_db_rows_are_repaired_not_rejected() {
        assert_eq!(repair_slug("stardew-valley").as_str(), "stardew-valley");
        assert_eq!(repair_slug("GSE Saves").as_str(), "gse-saves");
        assert_eq!(repair_slug("   ").as_str(), "unknown-game");
        // Degenerado pero bien formado: se respeta, porque es la identidad que
        // el save ya tiene en la DB.
        assert_eq!(repair_slug("users").as_str(), "users");

        assert_eq!(repair_username("jacka").as_str(), "jacka");
        assert_eq!(repair_username("  jacka  ").as_str(), "jacka");
        assert_eq!(repair_username("").as_str(), "unknown");
    }
}
