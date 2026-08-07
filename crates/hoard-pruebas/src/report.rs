//! Muestras, estadística y salida.
//!
//! Una sola medición no dice nada cuando el problema es justamente la
//! varianza ("a veces 15 s, a veces 25, a veces nada"): lo que hay que mirar
//! es la mediana **y** la cola. Por eso todo lo que sale de aquí lleva p50 y
//! p95, y el JSON conserva las muestras crudas para poder rehacer cualquier
//! cuenta después sin repetir el experimento.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// Qué se midió. Un paso del guion, no una función del motor: lo que le
/// importa a quien lee es "restaurar sobre una carpeta que ya tenía casi
/// todo", no `restore_cloud_cas`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// Primera subida del save entero.
    BackupCold,
    /// Subida tras mutarlo: el server ya tiene casi todos los blobs.
    BackupIncremental,
    /// Restore a carpeta vacía: no hay nada que deduplicar, todo viaja.
    RestoreCold,
    /// Restore sobre la carpeta que ya tiene exactamente ese contenido: el
    /// caso "instantáneo".
    RestoreWarm,
    /// Restore de la versión siguiente sobre la anterior: casi todo se
    /// reutiliza y viaja lo que cambió. El caso real del día a día.
    RestoreIncremental,
}

impl Step {
    pub fn label(&self) -> &'static str {
        match self {
            Step::BackupCold => "backup frío",
            Step::BackupIncremental => "backup incremental",
            Step::RestoreCold => "restore frío",
            Step::RestoreWarm => "restore caliente",
            Step::RestoreIncremental => "restore incremental",
        }
    }
}

/// Una medición.
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub shape: String,
    pub step: Step,
    pub concurrency: usize,
    pub round: usize,
    pub ms: u64,
    pub files: u64,
    pub bytes: u64,
    /// Solo en restores: cuánto salió del disco en vez de la red.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_reused: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_reused: Option<u64>,
    /// Desglose por fases (solo la ruta content-addressed lo rellena).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer_ms: Option<u64>,
}

impl Sample {
    pub fn mib_per_s(&self) -> f64 {
        if self.ms == 0 {
            return 0.0;
        }
        (self.bytes as f64 / (1024.0 * 1024.0)) / (self.ms as f64 / 1000.0)
    }
}

/// Un fallo. El banco no aborta cuando algo se rompe: lo apunta y sigue, que
/// para eso corre a gran escala. Un error que solo pasa una vez de cada
/// cuarenta es exactamente el que hay que cazar, y abortar en la primera lo
/// convertiría en irreproducible.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub shape: String,
    pub step: Step,
    pub concurrency: usize,
    pub round: usize,
    pub error: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub server: String,
    pub started_at: String,
    pub samples: Vec<Sample>,
    pub failures: Vec<Failure>,
}

/// Resumen de un grupo de muestras comparables.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub n: usize,
    pub min_ms: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
    pub mib_s_p50: f64,
    /// Cuánto se desvía la cola de la mediana. >2 es "funciona a ratos".
    pub spread: f64,
}

pub fn stats(samples: &[&Sample]) -> Option<Stats> {
    if samples.is_empty() {
        return None;
    }
    let mut ms: Vec<u64> = samples.iter().map(|s| s.ms).collect();
    ms.sort_unstable();
    let pick = |q: f64| -> u64 {
        // Percentil por índice más cercano: con 5 repeticiones interpolar es
        // fingir una precisión que no hay.
        let idx = ((ms.len() as f64 - 1.0) * q).round() as usize;
        ms[idx.min(ms.len() - 1)]
    };
    let p50 = pick(0.50);
    let p95 = pick(0.95);
    let mut rate: Vec<f64> = samples.iter().map(|s| s.mib_per_s()).collect();
    rate.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(Stats {
        n: ms.len(),
        min_ms: ms[0],
        p50_ms: p50,
        p95_ms: p95,
        max_ms: ms[ms.len() - 1],
        mib_s_p50: rate[rate.len() / 2],
        spread: if p50 == 0 {
            0.0
        } else {
            p95 as f64 / p50 as f64
        },
    })
}

impl Report {
    /// Tabla por (forma, paso, concurrencia). Ordenada de forma estable para
    /// que dos ejecuciones se puedan diffear.
    pub fn print_table(&self) {
        let mut groups: BTreeMap<(String, Step, usize), Vec<&Sample>> = BTreeMap::new();
        for s in &self.samples {
            groups
                .entry((s.shape.clone(), s.step, s.concurrency))
                .or_default()
                .push(s);
        }

        println!();
        println!(
            "{:<10} {:<20} {:>4} {:>3} {:>8} {:>8} {:>8} {:>8} {:>9}",
            "forma", "paso", "conc", "n", "p50", "p95", "max", "MiB/s", "dispersión"
        );
        println!("{}", "-".repeat(88));
        for ((shape, step, conc), rows) in &groups {
            let Some(st) = stats(rows) else { continue };
            println!(
                "{:<10} {:<20} {:>4} {:>3} {:>8} {:>8} {:>8} {:>8.1} {:>9}",
                shape,
                step.label(),
                conc,
                st.n,
                fmt_ms(st.p50_ms),
                fmt_ms(st.p95_ms),
                fmt_ms(st.max_ms),
                st.mib_s_p50,
                format!("{:.1}x", st.spread),
            );
        }

        // Con dos muestras p50 y p95 son la misma fila ordenada, así que la
        // dispersión sale 1.0x pase lo que pase. Decirlo evita leer "estable"
        // donde solo hay "no hay datos suficientes para saberlo".
        if groups.values().map(|r| r.len()).max().unwrap_or(0) < 3 {
            println!(
                "\n(pocas vueltas: con n<3 los percentiles y la dispersión no \
                 significan nada — usa --vueltas 5 o más)"
            );
        }

        self.print_phases();

        if !self.failures.is_empty() {
            println!();
            println!("fallos ({}):", self.failures.len());
            for f in &self.failures {
                println!(
                    "  [{} · {} · conc {} · vuelta {}] {}",
                    f.shape,
                    f.step.label(),
                    f.concurrency,
                    f.round,
                    f.error.lines().next().unwrap_or("")
                );
            }
        }
    }

    /// El desglose que responde "¿en qué se fueron los 25 s?".
    fn print_phases(&self) {
        // Solo la ruta content-addressed (Cloud) tiene fases. En self-hosted
        // el tar entero las deja todas a cero, y una tabla de ceros parece un
        // banco roto en vez de "aquí no hay fases que repartir".
        let with_phases: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|s| {
                s.manifest_ms.unwrap_or(0) + s.index_ms.unwrap_or(0) + s.transfer_ms.unwrap_or(0)
                    > 0
            })
            .collect();
        if with_phases.is_empty() {
            return;
        }
        let mut groups: BTreeMap<(String, Step, usize), Vec<&Sample>> = BTreeMap::new();
        for s in with_phases {
            groups
                .entry((s.shape.clone(), s.step, s.concurrency))
                .or_default()
                .push(s);
        }
        println!();
        println!(
            "{:<10} {:<20} {:>4} {:>10} {:>10} {:>10} {:>14}",
            "forma", "paso", "conc", "manifiesto", "índice", "transfer", "reutilizado"
        );
        println!("{}", "-".repeat(82));
        for ((shape, step, conc), rows) in &groups {
            let avg = |f: fn(&Sample) -> u64| -> u64 {
                rows.iter().map(|s| f(s)).sum::<u64>() / rows.len() as u64
            };
            let reused: u64 = rows.iter().filter_map(|s| s.bytes_reused).sum();
            let total: u64 = rows.iter().map(|s| s.bytes).sum();
            let pct = if total == 0 {
                0.0
            } else {
                reused as f64 * 100.0 / total as f64
            };
            println!(
                "{:<10} {:<20} {:>4} {:>10} {:>10} {:>10} {:>13.0}%",
                shape,
                step.label(),
                conc,
                fmt_ms(avg(|s| s.manifest_ms.unwrap_or(0))),
                fmt_ms(avg(|s| s.index_ms.unwrap_or(0))),
                fmt_ms(avg(|s| s.transfer_ms.unwrap_or(0))),
                pct,
            );
        }
    }

    pub fn write_json(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("escribiendo {}", path.display()))?;
        Ok(())
    }
}

pub fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

pub fn fmt_bytes(b: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = b as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{b:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ms: u64) -> Sample {
        Sample {
            shape: "factorio".into(),
            step: Step::RestoreCold,
            concurrency: 4,
            round: 0,
            ms,
            files: 10,
            bytes: 1024 * 1024,
            bytes_reused: None,
            files_reused: None,
            manifest_ms: None,
            index_ms: None,
            transfer_ms: None,
        }
    }

    #[test]
    fn la_dispersion_delata_el_funciona_a_ratos() {
        // El caso del usuario: 0 s, 15 s, 25 s sobre el mismo save.
        let rows = [sample(200), sample(15_000), sample(25_000)];
        let refs: Vec<&Sample> = rows.iter().collect();
        let st = stats(&refs).unwrap();
        assert_eq!(st.min_ms, 200);
        assert_eq!(st.max_ms, 25_000);
        assert!(st.spread > 1.5, "una cola así tiene que saltar: {st:?}");
    }

    #[test]
    fn sin_muestras_no_hay_estadistica() {
        assert!(stats(&[]).is_none());
    }
}
