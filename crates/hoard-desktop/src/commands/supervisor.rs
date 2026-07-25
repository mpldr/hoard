//! Alias del supervisor compartido — la implementación vive en
//! `hoard_agent::supervisor` desde el Slice 4a (ADR 0021).
//!
//! Se movió porque el daemon (`hoardd`) tiene que cumplir la misma regla de
//! D.12 («si vive más que una petición, va bajo `supervise`») y no puede usar un
//! módulo privado del desktop. Este alias se queda para que la ruta que la ADR
//! nombra —`commands/supervisor.rs`— siga siendo la que encuentra quien busque
//! dónde se supervisa una tarea del desktop.

pub use hoard_agent::supervisor::{supervise, Finished};
