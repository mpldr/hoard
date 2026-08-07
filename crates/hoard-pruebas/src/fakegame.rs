//! Juegos inventados: procesos que el motor debe reconocer como un juego y
//! correlacionar con la carpeta que escriben.
//!
//! No basta con dormir en un bucle. La detección tiene dos vetos duros que un
//! proceso de mentira falla sin darse cuenta (`hoard_agent::correlation`):
//!
//! - **CPU**: por debajo de `CORRELATION_SOURCE_MIN_CPU_PCT` (0,5 %) el
//!   proceso se descarta como "residente dormido". Un juego que solo duerme no
//!   se correlaciona jamás, y el banco concluiría que la detección está rota
//!   cuando el roto es el juego falso.
//! - **Nombre y ruta**: `is_game_like` veta launchers, navegadores, wrappers de
//!   Proton y cualquier exe bajo `/usr`, `/bin`, `/lib` o `/run`. Por eso el
//!   binario se copia con nombre de juego a una carpeta del banco y se ejecuta
//!   desde ahí — el nombre del proceso es el del fichero, no el de argv.
//!
//! El proceso vive lo que se le diga, escribe en la carpeta cada pocos
//! segundos (como un autosave) y sale. Eso dibuja el ciclo completo que el
//! motor tiene que ver: arranque, escrituras atribuidas, cierre, y el backup
//! que dispara `GameStopped`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rand::{RngCore, SeedableRng};

/// Variable que le dice a la copia renombrada que ella es el juego. Sin esto
/// la copia repetiría el `spawn` y tendríamos una bomba de forks.
pub const ROLE_ENV: &str = "HOARD_PRUEBAS_ROLE";

/// Prepara un ejecutable con nombre de juego y lo lanza.
///
/// Devuelve el hijo para que quien lo lanzó decida si lo espera o lo deja
/// jugando mientras hace otra cosa.
pub fn spawn(
    bindir: &Path,
    name: &str,
    save_dir: &Path,
    seconds: u64,
    write_every: u64,
) -> Result<std::process::Child> {
    let exe = prepare_binary(bindir, name)?;
    let child = std::process::Command::new(&exe)
        .arg("jugar")
        .arg("--save")
        .arg(save_dir)
        .arg("--durante")
        .arg(seconds.to_string())
        .arg("--cada")
        .arg(write_every.to_string())
        .env(ROLE_ENV, "juego")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("lanzando el juego falso {}", exe.display()))?;
    Ok(child)
}

/// Copia este binario con el nombre del juego. La copia es lo que ve
/// `sysinfo`: mismo código, nombre distinto.
pub fn prepare_binary(bindir: &Path, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(bindir).with_context(|| format!("creando {}", bindir.display()))?;
    let exe = bindir.join(name);
    let yo = std::env::current_exe().context("no sé cuál es mi propio ejecutable")?;

    // Si ya está y es del mismo tamaño, sirve: copiar 20 MB por juego y por
    // vuelta es tiempo de disco que no mide nada.
    let vale = std::fs::metadata(&exe)
        .ok()
        .zip(std::fs::metadata(&yo).ok())
        .map(|(a, b)| a.len() == b.len())
        .unwrap_or(false);
    if !vale {
        std::fs::copy(&yo, &exe)
            .with_context(|| format!("copiando el binario a {}", exe.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&exe)?.permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&exe, p)?;
        }
    }
    Ok(exe)
}

/// El bucle del juego: escribe, quema un poco de CPU, repite.
pub fn play(save_dir: &Path, seconds: u64, write_every: u64) -> Result<()> {
    std::fs::create_dir_all(save_dir).with_context(|| format!("creando {}", save_dir.display()))?;
    let hasta = Instant::now() + Duration::from_secs(seconds);
    let cada = Duration::from_secs(write_every.max(1));
    let mut siguiente_escritura = Instant::now();
    let mut turno = 0u64;

    while Instant::now() < hasta {
        if Instant::now() >= siguiente_escritura {
            autosave(save_dir, turno)?;
            turno += 1;
            siguiente_escritura = Instant::now() + cada;
        }
        // Un juego consume CPU. Este quema un poco y duerme el resto: ~6 % de
        // un núcleo, orden de magnitud por encima del 0,5 % que exige la
        // correlación y lo bastante bajo para que veinte a la vez no cocinen
        // la máquina (y falseen la medida del propio banco).
        quemar(Duration::from_millis(25));
        std::thread::sleep(Duration::from_millis(375));
    }
    Ok(())
}

/// Escribe un autosave rotando tres huecos, como haría el juego.
fn autosave(save_dir: &Path, turno: u64) -> Result<()> {
    let hueco = turno % 3;
    let path = save_dir.join(format!("autosave{hueco}.dat"));
    let mut rng = rand::rngs::StdRng::seed_from_u64(turno ^ 0xA5A5_1234);
    let mut buf = vec![0u8; 512 * 1024];
    rng.fill_bytes(&mut buf);
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("escribiendo {}", tmp.display()))?;
        f.write_all(&buf)?;
        f.flush()?;
    }
    // Rename atómico: es como escribe un juego serio, y es también lo que el
    // watcher ve como un evento de creación y no como cien de escritura.
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn quemar(cuanto: Duration) {
    let hasta = Instant::now() + cuanto;
    let mut x: u64 = 0x243F_6A88_85A3_08D3;
    while Instant::now() < hasta {
        for _ in 0..10_000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
        }
        std::hint::black_box(x);
    }
}

/// Nombres de juego para un lote. Se evitan a propósito los que
/// `is_game_like` veta (nada con "steam", "wine", "proton", "setup"…) — un
/// juego falso que el motor descarta por nombre no prueba nada.
pub fn nombres(cuantos: usize) -> Vec<String> {
    const BASE: &[&str] = &[
        "factorio",
        "stardew",
        "terraria",
        "hades",
        "celeste",
        "hollowknight",
        "rimworld",
        "noita",
        "dyson",
        "vampiresurv",
    ];
    (0..cuantos)
        .map(|i| {
            if i < BASE.len() {
                BASE[i].to_string()
            } else {
                format!("{}{}", BASE[i % BASE.len()], i / BASE.len() + 1)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn los_nombres_pasan_el_filtro_del_motor() {
        for n in nombres(25) {
            assert!(
                hoard_agent::correlation::is_game_like(&n, None),
                "{n} lo vetaría el motor y el juego falso sería invisible"
            );
        }
    }
}
