//! `hoard-pruebas` — banco de pruebas del motor de Hoard.
//!
//! Existe por una pregunta concreta que el uso normal no sabe contestar: el
//! mismo save restaura en 25 s, en 15 s o al instante, y desde fuera las tres
//! veces parecen la misma operación. Sin repetir la medida muchas veces, sobre
//! formas de save distintas y con el desglose por fases delante, "va lento a
//! ratos" no es un dato accionable.
//!
//! Dos mandos hacen el trabajo:
//!
//! - `bench` mide el motor en línea recta (sube, restaura frío, caliente e
//!   incremental) y saca p50/p95 más el reparto entre manifiesto, índice y
//!   transferencia.
//! - `soak` monta juegos inventados que juegan de verdad contra el servicio y
//!   recoge lo que sale por el journal: qué se guardó, qué falló, y qué juego
//!   jugó sin que quedara copia.
//!
//! Nada de esto se empaqueta con la app: es una herramienta de desarrollo.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod bench;
mod fakegame;
mod fixture;
mod report;
mod session;
mod soak;

use fixture::{Mutation, Shape};

#[derive(Parser)]
#[command(
    name = "hoard-pruebas",
    about = "Banco de pruebas: saves y juegos inventados, medidos contra el motor de verdad",
    version
)]
struct Cli {
    /// Carpeta de trabajo (fixtures, destinos, binarios de los juegos falsos)
    #[arg(long, global = true, default_value = "/tmp/hoard-pruebas")]
    workdir: PathBuf,

    /// Servidor contra el que medir. Por defecto, la sesión ya configurada.
    #[arg(long, global = true)]
    server: Option<String>,

    /// Token para `--server` (self-hosted). Con sesión Cloud no hace falta.
    #[arg(long, global = true, env = "HOARD_PRUEBAS_TOKEN")]
    token: Option<String>,

    /// Más detalle por pantalla
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mide backup y restore repetidas veces y saca la estadística
    Bench {
        /// Formas de save a medir (repetible)
        #[arg(long = "forma", value_enum, default_values_t = vec![Shape::Factorio, Shape::Swarm])]
        formas: Vec<Shape>,
        /// Multiplicador de tamaño sobre la forma base
        #[arg(long, default_value_t = 1.0)]
        escala: f64,
        /// Cuántas veces repetir cada medición
        #[arg(long = "vueltas", default_value_t = 3)]
        vueltas: usize,
        /// Semilla: la misma da los mismos bytes
        #[arg(long, default_value_t = 20260727)]
        semilla: u64,
        /// Descargas en paralelo a barrer (repetible: `--concurrencia 1 --concurrencia 8`)
        #[arg(long = "concurrencia", default_values_t = vec![4usize])]
        concurrencia: Vec<usize>,
        /// No borrar del servidor los saves creados
        #[arg(long)]
        keep: bool,
        /// Escribir el informe crudo en JSON
        #[arg(long)]
        json: Option<PathBuf>,
    },

    /// Juegos inventados jugando contra el servicio, recogiendo su journal
    Soak {
        /// Cuántos juegos a la vez
        #[arg(long = "juegos", default_value_t = 5)]
        juegos: usize,
        /// Duración total en segundos
        #[arg(long = "duracion", default_value_t = 600)]
        duracion: u64,
        /// Cuánto dura cada partida, en segundos
        #[arg(long = "sesion", default_value_t = 45)]
        sesion: u64,
        /// Cada cuántos segundos escribe un autosave cada juego
        #[arg(long = "cada", default_value_t = 10)]
        escribe_cada: u64,
        /// Forma de los saves (por defecto pequeños: son muchos y reales)
        #[arg(long = "forma", value_enum, default_value = "tiny")]
        forma: Shape,
        /// Multiplicador de tamaño
        #[arg(long, default_value_t = 1.0)]
        escala: f64,
        /// No limpiar al terminar
        #[arg(long)]
        keep: bool,
    },

    /// Genera un save inventado y para ahí
    Generar {
        #[arg(long, value_enum)]
        forma: Shape,
        #[arg(long, default_value_t = 1.0)]
        escala: f64,
        #[arg(long, default_value_t = 20260727)]
        semilla: u64,
        /// Dónde escribirlo
        #[arg(long)]
        destino: PathBuf,
    },

    /// Cambia un save generado como lo haría el juego
    Mutar {
        /// Carpeta del save
        destino: PathBuf,
        #[arg(long, value_enum, default_value = "rotate")]
        tipo: Mutation,
        /// Porcentaje de ficheros afectados (para `touch` y `bump`)
        #[arg(long, default_value_t = 10)]
        porcentaje: u8,
        #[arg(long, default_value_t = 20260727)]
        semilla: u64,
    },

    /// El bucle del juego falso. Lo invoca `soak`; a mano sirve para probar la
    /// detección con un solo juego delante.
    Jugar {
        /// Carpeta donde escribe sus autosaves
        #[arg(long = "save")]
        save: PathBuf,
        /// Segundos de partida
        #[arg(long = "durante", default_value_t = 60)]
        durante: u64,
        /// Segundos entre autosaves
        #[arg(long = "cada", default_value_t = 10)]
        cada: u64,
    },

    /// Quita del estado local (y del servidor) todo lo que el banco dejó
    Purgar {
        /// Enseñar qué se borraría sin borrarlo
        #[arg(long)]
        seco: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.cmd {
        Cmd::Bench {
            formas,
            escala,
            vueltas,
            semilla,
            concurrencia,
            keep,
            json,
        } => {
            bench::run(bench::BenchArgs {
                shapes: formas,
                scale: escala,
                rounds: vueltas,
                seed: semilla,
                workdir: cli.workdir,
                concurrency: concurrencia,
                server: cli.server,
                token: cli.token,
                keep,
                json,
            })
            .await
        }

        Cmd::Soak {
            juegos,
            duracion,
            sesion,
            escribe_cada,
            forma,
            escala,
            keep,
        } => {
            soak::run(soak::SoakArgs {
                juegos,
                duracion,
                sesion,
                escribe_cada,
                shape: forma,
                scale: escala,
                workdir: cli.workdir,
                server: cli.server,
                token: cli.token,
                keep,
            })
            .await
        }

        Cmd::Generar {
            forma,
            escala,
            semilla,
            destino,
        } => {
            let fx = fixture::generate(forma, escala, semilla, &destino)?;
            println!(
                "{} · {} ficheros · {} · {}",
                forma.as_str(),
                fx.files,
                report::fmt_bytes(fx.bytes),
                destino.display()
            );
            Ok(())
        }

        Cmd::Mutar {
            destino,
            tipo,
            porcentaje,
            semilla,
        } => {
            let cambio = fixture::mutate(&destino, tipo, porcentaje, semilla)?;
            println!("{cambio}");
            Ok(())
        }

        Cmd::Jugar {
            save,
            durante,
            cada,
        } => fakegame::play(&save, durante, cada),

        Cmd::Purgar { seco } => purgar(seco).await,
    }
}

/// Red de seguridad: quita del `state.json` del usuario cualquier save que
/// haya dejado el banco. Es imprescindible porque `soak` registra saves de
/// verdad en el estado de verdad — si el proceso muere entre registrar y
/// limpiar, esos saves seguirían sincronizando solos para siempre.
async fn purgar(seco: bool) -> Result<()> {
    let (state, path) = hoard_agent::state::CliState::load_default()?;
    let mios: Vec<String> = state
        .saves
        .iter()
        .filter(|(_, s)| s.game_slug.starts_with("pruebas-") || s.label.starts_with("pruebas-"))
        .map(|(id, _)| id.clone())
        .collect();

    if mios.is_empty() {
        println!("nada que purgar en {}", path.display());
        return Ok(());
    }
    println!("{} saves de pruebas en el estado local:", mios.len());
    for id in &mios {
        if let Some(s) = state.saves.get(id) {
            println!("  {id}  {}  {}", s.game_slug, s.local_path.display());
        }
    }
    if seco {
        println!("(--seco: no se ha tocado nada)");
        return Ok(());
    }

    // Primero el servidor, mientras todavía sabemos los ids. Si falla, el
    // untrack local se hace igual: dejar de vigilarlo es lo urgente.
    let servidor = session::resolve(None, None).await;
    for id in &mios {
        if let Ok(active) = &servidor {
            let _ = hoard_agent::library::delete_completely(&active.client, id).await;
        }
        hoard_agent::library::untrack(id).ok();
    }
    println!("purgados {} saves", mios.len());
    if servidor.is_err() {
        println!("(sin sesión: se quitaron del estado local, pero pueden seguir en el servidor)");
    }
    Ok(())
}

fn init_tracing(verbose: bool) {
    let filtro = if verbose {
        "hoard_pruebas=debug,hoard_agent=debug"
    } else {
        "hoard_pruebas=info,hoard_agent=warn"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filtro)),
        )
        .with_target(false)
        .init();
}
