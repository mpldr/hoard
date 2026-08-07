//! Saves inventados con la *forma* de los reales.
//!
//! Lo que hace lento (o rápido) a un restore no son los bytes totales sino la
//! forma: 5000 ficheros de 4 KB pagan latencia de GET por fichero, y un zip de
//! 200 MB paga ancho de banda. Un banco que solo probara "400 MB" mediría una
//! de las dos y creería haber medido las dos.
//!
//! Todo se genera desde una semilla: la misma `(shape, seed)` produce byte a
//! byte el mismo save, así que dos ejecuciones son comparables y el dedup
//! content-addressed del motor ve exactamente lo que esperamos que vea.

use anyhow::{Context, Result};
use rand::{RngCore, SeedableRng};
use std::path::{Path, PathBuf};

/// La forma de un save. Cada una imita un caso real observado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Shape {
    /// Factorio: una decena de zips gordos y poco más. Ancho de banda puro.
    Factorio,
    /// Skyrim / Fallout: ~40 `.ess` medianos con sus `.skse` al lado.
    Skyrim,
    /// Muchos ficheros diminutos (mods, config, un mundo por chunks): la forma
    /// donde manda la latencia por fichero, no el tamaño.
    Swarm,
    /// Un único fichero enorme. El caso peor para la concurrencia: no hay nada
    /// que paralelizar.
    Monolith,
    /// Save de bolsillo (unos pocos KB). Mide el suelo: lo que cuesta un
    /// backup/restore cuando los bytes no cuestan nada.
    Tiny,
}

impl Shape {
    /// Slug de juego con el que se registra en el server. Prefijo `pruebas-`
    /// para que un save de banco nunca se confunda con uno real del usuario.
    pub fn slug(&self) -> &'static str {
        match self {
            Shape::Factorio => "pruebas-factorio",
            Shape::Skyrim => "pruebas-skyrim",
            Shape::Swarm => "pruebas-swarm",
            Shape::Monolith => "pruebas-monolith",
            Shape::Tiny => "pruebas-tiny",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Shape::Factorio => "factorio",
            Shape::Skyrim => "skyrim",
            Shape::Swarm => "swarm",
            Shape::Monolith => "monolith",
            Shape::Tiny => "tiny",
        }
    }

    /// El plano de ficheros: `(ruta relativa, bytes)`.
    ///
    /// `scale` toca **los tamaños, no las cuentas**. Es deliberado: lo que
    /// distingue a una forma de otra es cuántos ficheros tiene, y una escala
    /// que encogiera también la cuenta convertiría un Factorio de 10 zips en
    /// un monolito de 1 — mediría otra cosa y encima lo diría con el nombre
    /// equivocado. Un `swarm` a escala 0,01 sigue pagando 4000 round-trips,
    /// que es justo la propiedad por la que existe.
    fn plan(&self, scale: f64) -> Vec<(String, u64)> {
        let mib = 1024 * 1024;
        let kib = 1024;
        let n = |base: usize| base;
        let sz = |base: u64| ((base as f64 * scale).round() as u64).max(1);

        match self {
            Shape::Factorio => {
                let mut v: Vec<(String, u64)> = (0..n(9))
                    .map(|i| (format!("saves/_autosave{}.zip", i + 1), sz(38 * mib)))
                    .collect();
                v.push(("saves/partida-principal.zip".into(), sz(52 * mib)));
                v.push(("mods/mod-settings.dat".into(), sz(6 * kib)));
                v.push(("player-data.json".into(), sz(2 * kib)));
                v
            }
            Shape::Skyrim => {
                let mut v = Vec::new();
                for i in 0..n(40) {
                    v.push((format!("Saves/Save{}.ess", i + 1), sz(9 * mib)));
                    v.push((format!("Saves/Save{}.skse", i + 1), sz(220 * kib)));
                }
                v.push(("Skyrim.ini".into(), sz(4 * kib)));
                v
            }
            Shape::Swarm => {
                let mut v = Vec::new();
                for i in 0..n(4000) {
                    // Repartidos en subcarpetas: un solo dir con 4000 entradas
                    // no es lo que hace un juego, y el walk se comporta distinto.
                    v.push((
                        format!("region/r{:02}/chunk-{:05}.dat", i % 32, i),
                        sz(3 * kib + (i as u64 % 11) * kib),
                    ));
                }
                v.push(("level.dat".into(), sz(48 * kib)));
                v
            }
            Shape::Monolith => vec![("world/world.sav".into(), sz(700 * mib))],
            Shape::Tiny => vec![
                ("save0.dat".into(), sz(12 * kib)),
                ("settings.cfg".into(), sz(900)),
            ],
        }
    }
}

/// Qué se generó, para que el informe pueda decir contra qué se midió.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Fixture {
    pub shape: Shape,
    pub root: PathBuf,
    pub files: usize,
    pub bytes: u64,
}

/// Escribe el save en `root`. Si ya existe con el mismo plano no reescribe
/// nada: regenerar un fixture de 700 MB en cada repetición mediría el disco de
/// pruebas, no el motor.
pub fn generate(shape: Shape, scale: f64, seed: u64, root: &Path) -> Result<Fixture> {
    let plan = shape.plan(scale);
    std::fs::create_dir_all(root).with_context(|| format!("creando {}", root.display()))?;

    let mut bytes = 0u64;
    for (rel, size) in &plan {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creando {}", parent.display()))?;
        }
        // Ya está y mide lo que toca: lo damos por bueno. El contenido es
        // función de (seed, rel), así que si el tamaño coincide el contenido
        // coincide salvo que alguien lo haya tocado a mano.
        let up_to_date = std::fs::metadata(&path)
            .map(|m| m.len() == *size)
            .unwrap_or(false);
        if !up_to_date {
            write_pseudo_random(&path, *size, seed_for(seed, rel))?;
        }
        bytes += size;
    }

    Ok(Fixture {
        shape,
        root: root.to_path_buf(),
        files: plan.len(),
        bytes,
    })
}

/// Cómo cambia un save entre dos partidas. Es lo que decide cuánto puede
/// deduplicar el restore, o sea la diferencia entre "instantáneo" y "25s".
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum Mutation {
    /// Rota el autosave: el más viejo desaparece, entra uno nuevo. Es lo que
    /// hace Factorio, y el caso que D.13 vino a arreglar.
    Rotate,
    /// Reescribe un porcentaje de los ficheros con contenido nuevo.
    Touch,
    /// Añade un fichero nuevo sin tocar los que ya estaban.
    Grow,
    /// Cambia solo mtime, no contenido: el save "parece" nuevo y no lo es.
    /// Sirve para comprobar que la firma barata no dispara una subida.
    Bump,
}

/// Aplica la mutación y devuelve una línea legible de lo que cambió.
pub fn mutate(root: &Path, kind: Mutation, percent: u8, seed: u64) -> Result<String> {
    let mut files = walk(root)?;
    files.sort();
    if files.is_empty() {
        anyhow::bail!("no hay nada que mutar en {}", root.display());
    }

    match kind {
        Mutation::Rotate => {
            // Se va el fichero MÁS GRANDE, que es el que de verdad rota (el
            // autosave, el `.ess`, el mundo). Elegir "el primero que no sea
            // config" por orden alfabético parecía equivalente y no lo es: en
            // Factorio eso es `mods/mod-settings.dat`, 184 bytes, y entonces el
            // restore incremental sale 100 % reutilizado y el banco concluye
            // que rotar un autosave es gratis. Lo era, pero porque no había
            // rotado ningún autosave.
            let victim = files
                .iter()
                .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
                .cloned()
                .unwrap_or_else(|| files[0].clone());
            let size = std::fs::metadata(&victim)?.len();
            std::fs::remove_file(&victim)?;
            let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
            let newcomer = victim.with_file_name(format!(
                "rotado-{}-{}",
                stamp,
                victim.file_name().unwrap_or_default().to_string_lossy()
            ));
            write_pseudo_random(&newcomer, size, seed ^ stamp as u64)?;
            Ok(format!(
                "rotado: -{} +{} ({} bytes)",
                victim.file_name().unwrap_or_default().to_string_lossy(),
                newcomer.file_name().unwrap_or_default().to_string_lossy(),
                size
            ))
        }
        Mutation::Touch => {
            let how_many = ((files.len() as f64) * (percent as f64 / 100.0)).ceil() as usize;
            let how_many = how_many.clamp(1, files.len());
            let mut changed = 0u64;
            for (i, path) in files.iter().take(how_many).enumerate() {
                let size = std::fs::metadata(path)?.len();
                write_pseudo_random(path, size, seed ^ (i as u64).wrapping_mul(0x9E37_79B9))?;
                changed += size;
            }
            Ok(format!("reescritos {how_many} ficheros ({changed} bytes)"))
        }
        Mutation::Grow => {
            let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
            let path = root.join(format!("nuevo-{stamp}.dat"));
            let size = std::fs::metadata(&files[0])?.len();
            write_pseudo_random(&path, size, seed ^ stamp as u64)?;
            Ok(format!(
                "añadido {} ({} bytes)",
                path.file_name().unwrap_or_default().to_string_lossy(),
                size
            ))
        }
        Mutation::Bump => {
            let now = std::time::SystemTime::now();
            let how_many = ((files.len() as f64) * (percent as f64 / 100.0)).ceil() as usize;
            let how_many = how_many.clamp(1, files.len());
            for path in files.iter().take(how_many) {
                let f = std::fs::File::options().write(true).open(path)?;
                f.set_modified(now)?;
            }
            Ok(format!(
                "mtime tocado en {how_many} ficheros (contenido intacto)"
            ))
        }
    }
}

pub fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("leyendo {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Semilla estable por fichero: misma `(seed, ruta)` ⇒ mismos bytes.
fn seed_for(seed: u64, rel: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(seed.to_le_bytes());
    h.update(rel.as_bytes());
    let d = h.finalize();
    u64::from_le_bytes(d[..8].try_into().unwrap())
}

/// Bytes pseudoaleatorios: incompresibles a propósito.
///
/// Un save de ceros se comprimiría a nada y el banco mediría zstd en vez de la
/// red. Los saves reales (zips de Factorio, `.ess` comprimidos) ya vienen
/// comprimidos, así que el caso realista es justo este.
fn write_pseudo_random(path: &Path, size: u64, seed: u64) -> Result<()> {
    use std::io::Write;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let file =
        std::fs::File::create(path).with_context(|| format!("creando {}", path.display()))?;
    let mut w = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut buf = vec![0u8; 256 * 1024];
    let mut left = size;
    while left > 0 {
        let n = (left as usize).min(buf.len());
        rng.fill_bytes(&mut buf[..n]);
        w.write_all(&buf[..n])?;
        left -= n as u64;
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_misma_semilla_da_el_mismo_save() {
        let tmp = std::env::temp_dir().join(format!("hoard-pruebas-test-{}", std::process::id()));
        let a = tmp.join("a");
        let b = tmp.join("b");
        generate(Shape::Tiny, 1.0, 42, &a).unwrap();
        generate(Shape::Tiny, 1.0, 42, &b).unwrap();
        let ha = std::fs::read(a.join("save0.dat")).unwrap();
        let hb = std::fs::read(b.join("save0.dat")).unwrap();
        assert_eq!(ha, hb, "la semilla tiene que ser determinista");
        // Y una semilla distinta, contenido distinto.
        let c = tmp.join("c");
        generate(Shape::Tiny, 1.0, 43, &c).unwrap();
        let hc = std::fs::read(c.join("save0.dat")).unwrap();
        assert_ne!(ha, hc);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rotate_se_lleva_el_fichero_grande_no_el_de_config() {
        // El defecto real: rotaba `mods/mod-settings.dat` (184 bytes) en vez
        // de un autosave, y el banco medía un incremental que no lo era.
        let tmp = std::env::temp_dir().join(format!("hoard-pruebas-vic-{}", std::process::id()));
        let root = tmp.join("save");
        generate(Shape::Factorio, 0.001, 7, &root).unwrap();
        let cambio = mutate(&root, Mutation::Rotate, 0, 7).unwrap();
        assert!(
            cambio.contains("autosave") || cambio.contains("partida-principal"),
            "tiene que rotar un save, no un fichero de config: {cambio}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rotate_quita_uno_y_pone_otro() {
        let tmp = std::env::temp_dir().join(format!("hoard-pruebas-rot-{}", std::process::id()));
        let root = tmp.join("save");
        generate(Shape::Factorio, 0.001, 7, &root).unwrap();
        let antes = walk(&root).unwrap().len();
        mutate(&root, Mutation::Rotate, 0, 7).unwrap();
        let despues = walk(&root).unwrap().len();
        assert_eq!(antes, despues, "rotar mantiene la cuenta de ficheros");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
