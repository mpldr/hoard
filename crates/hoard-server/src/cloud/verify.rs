//! `hoard-server verify-blobs` — la pasada forense sobre los blobs de la nube.
//!
//! Un blob es content-addressed: su nombre **es** el sha256 de su contenido.
//! Nada en el camino normal vuelve a comprobarlo. El commit hace un HEAD (que
//! sólo dice el tamaño) y el barrido de compresión sí verifica lo que él mismo
//! comprime, pero un objeto que se subió mal y nunca se comprimió no lo mira
//! nadie: se descubre al restaurar, que es el peor momento posible.
//!
//! Y se subieron mal. Hasta ago-2026 el cliente hasheaba el fichero y lo subía
//! en dos lecturas distintas; si el juego rotaba el save entre medias —`save` →
//! `save.bak` y uno nuevo en su sitio, el patrón normal de autoguardado— el
//! objeto acababa con bytes que no son los que su nombre promete. El cliente ya
//! no puede producirlos (hashea el propio stream del PUT y aborta antes de
//! confirmar la versión), pero los de entonces siguen ahí.
//!
//! Esto los encuentra: lee cada objeto entero, lo descomprime si hace falta,
//! hashea y compara. Escribe el veredicto en `cloud_blobs.integrity` para no
//! releer lo ya sano en la siguiente vuelta, y nombra los saves afectados —
//! usuario, juego, versión y fichero— porque lo que el operador necesita saber
//! no es "el blob abc123 miente" sino "a esta persona no le va a restaurar bien
//! tal partida".
//!
//! Sólo lee y anota. **No borra nada**: un blob roto sigue siendo la única
//! copia que hay de esa versión, y tirarlo cambiaría "restaura mal" por "no
//! restaura". Qué hacer con ellos es una decisión de producto, no de barrido.

use anyhow::{Context, Result};
use async_compression::tokio::bufread::ZstdDecoder;
use futures::{stream, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::io::{AsyncBufRead, AsyncReadExt};

use crate::config::Config;

/// Qué se le pide a la pasada.
#[derive(Debug, Clone)]
pub struct Options {
    /// Cuántos blobs mirar como mucho. `None` = todos los que queden.
    pub limit: Option<i64>,
    /// Volver a mirar también los que ya tienen veredicto.
    pub recheck: bool,
    /// Lecturas simultáneas.
    pub concurrency: usize,
    /// No escribir veredictos en la base: sólo informar.
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limit: None,
            recheck: false,
            // Cada lectura es una descarga entera desde R2: unas pocas a la vez
            // saturan el enlace sin dejar la máquina sin aire para servir.
            concurrency: 4,
            dry_run: false,
        }
    }
}

/// El veredicto de un objeto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Los bytes hashean a su sha256.
    Ok,
    /// El objeto está, pero su contenido no es el que promete su nombre.
    Mismatch,
    /// No está en el bucket.
    Missing,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Mismatch => "mismatch",
            Verdict::Missing => "missing",
        }
    }
}

/// Un blob que no pasó, con lo que hace falta para avisar a quien le importa.
#[derive(Debug, Clone)]
pub struct Damage {
    pub user_id: uuid::Uuid,
    pub sha256: String,
    pub verdict: Verdict,
    /// Qué se leyó en realidad (vacío si el objeto no estaba).
    pub actual_sha256: String,
    /// `(game_slug, version_num, relative_path)` de cada sitio que lo usa.
    pub used_by: Vec<(String, i64, String)>,
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub checked: usize,
    pub ok: usize,
    pub damaged: Vec<Damage>,
}

/// Hash del contenido **lógico** del objeto: descomprime antes de hashear
/// cuando el blob está guardado en zstd, porque el sha256 que promete el nombre
/// es el de los bytes originales, no el de su empaquetado.
///
/// Streaming y con buffer fijo: hay blobs de cientos de MB y esto corre en una
/// máquina de 512 MB.
pub async fn digest_of<R: AsyncBufRead + Unpin>(reader: R, zstd: bool) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    if zstd {
        let mut dec = ZstdDecoder::new(reader);
        loop {
            let n = dec.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            len += n as u64;
        }
    } else {
        let mut r = reader;
        loop {
            let n = r.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            len += n as u64;
        }
    }
    Ok((hex::encode(hasher.finalize()), len))
}

/// Compara lo leído con lo prometido. Puro: el veredicto es la parte que
/// merece un test, y no necesita ni bucket ni base.
pub fn judge(declared_sha: &str, declared_len: i64, actual: Option<(&str, u64)>) -> Verdict {
    match actual {
        None => Verdict::Missing,
        Some((sha, len)) => {
            // El tamaño también manda: `size_bytes` es lo que se le cobra al
            // usuario y lo que la restauración espera. Un objeto que hashea
            // bien pero mide otra cosa es igual de inservible.
            if sha == declared_sha && len == declared_len.max(0) as u64 {
                Verdict::Ok
            } else {
                Verdict::Mismatch
            }
        }
    }
}

/// Corre la pasada. Devuelve el informe; imprimirlo es cosa del llamante.
pub async fn run(cfg: &Config, opts: Options) -> Result<Report> {
    let cloud_cfg = cfg
        .cloud
        .as_ref()
        .context("verify-blobs: this server has no [cloud] section")?;
    let pool = super::db::connect(&cfg.database.url, cfg.database.max_connections)
        .await
        .context("verify-blobs: connecting to Postgres")?;
    let r2 = super::r2::R2Store::from_config(&cloud_cfg.r2)
        .await
        .context("verify-blobs: building the R2 client")?;

    let mut sql = String::from(
        "SELECT user_id, sha256, size_bytes, r2_key, encoding, stored_bytes
           FROM cloud_blobs",
    );
    if !opts.recheck {
        sql.push_str(" WHERE verified_at IS NULL");
    }
    sql.push_str(" ORDER BY created_at");
    if let Some(n) = opts.limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    let rows = sqlx::query(&sql).fetch_all(&pool).await?;

    let checks = rows.iter().map(|row| {
        let r2 = &r2;
        async move {
            let user_id: uuid::Uuid = row.get("user_id");
            let sha: String = row.get("sha256");
            let size: i64 = row.get("size_bytes");
            let key: String = row.get("r2_key");
            let encoding: Option<String> = row.get("encoding");
            let stored: Option<i64> = row.get("stored_bytes");
            // Sólo hay que descomprimir cuando el barrido TERMINÓ de escribir
            // la versión comprimida; `encoding='zstd'` con `stored_bytes` a
            // NULL es una reclamación en curso y el objeto sigue crudo.
            let zstd = encoding.as_deref() == Some("zstd") && stored.is_some();

            let actual = match r2.get_reader(&key).await {
                Ok(reader) => Some(digest_of(reader, zstd).await?),
                // Cualquier fallo de lectura se trata como "no está": si el
                // objeto no se puede bajar, restaurar tampoco va a poder.
                Err(e) => {
                    tracing::debug!(error = %e, key = %key, "verify-blobs: unreadable object");
                    None
                }
            };
            let verdict = judge(&sha, size, actual.as_ref().map(|(s, l)| (s.as_str(), *l)));
            Ok::<_, anyhow::Error>((
                user_id,
                sha,
                verdict,
                actual.map(|(s, _)| s).unwrap_or_default(),
            ))
        }
    });

    let results: Vec<(uuid::Uuid, String, Verdict, String)> = stream::iter(checks)
        .buffer_unordered(opts.concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    let mut report = Report {
        checked: results.len(),
        ..Default::default()
    };
    for (user_id, sha, verdict, actual) in results {
        if !opts.dry_run {
            sqlx::query(
                "UPDATE cloud_blobs SET verified_at = now(), integrity = $3
                  WHERE user_id = $1 AND sha256 = $2",
            )
            .bind(user_id)
            .bind(&sha)
            .bind(verdict.as_str())
            .execute(&pool)
            .await?;
        }
        if verdict == Verdict::Ok {
            report.ok += 1;
            continue;
        }
        // Quién lo sufre. Un blob puede estar referenciado por varias versiones
        // (dedup), y el operador necesita la lista entera para avisar.
        let used_by: Vec<(String, i64, String)> = sqlx::query(
            "SELECT s.game_slug, f.version_num, f.relative_path
               FROM save_version_files f
               JOIN saves s ON s.id = f.save_id
              WHERE f.sha256 = $1 AND s.user_id = $2
              ORDER BY s.game_slug, f.version_num",
        )
        .bind(&sha)
        .bind(user_id)
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|r| {
            (
                r.get("game_slug"),
                r.get("version_num"),
                r.get("relative_path"),
            )
        })
        .collect();

        tracing::warn!(
            user_id = %user_id,
            sha = %sha,
            verdict = verdict.as_str(),
            actual = %actual,
            references = used_by.len(),
            "verify-blobs: damaged blob"
        );
        report.damaged.push(Damage {
            user_id,
            sha256: sha,
            verdict,
            actual_sha256: actual,
            used_by,
        });
    }
    Ok(report)
}

/// Informe legible para la consola de `fly ssh console -C …`.
pub fn print_report(report: &Report, dry_run: bool) {
    println!(
        "verify-blobs: {} checked, {} ok, {} damaged{}",
        report.checked,
        report.ok,
        report.damaged.len(),
        if dry_run {
            " (dry run, nothing written)"
        } else {
            ""
        }
    );
    for d in &report.damaged {
        println!(
            "  {} {} — got {}",
            d.verdict.as_str(),
            d.sha256,
            if d.actual_sha256.is_empty() {
                "nothing"
            } else {
                &d.actual_sha256
            }
        );
        println!("    user {}", d.user_id);
        for (game, version, path) in &d.used_by {
            println!("    used by {game} v{version} — {path}");
        }
        if d.used_by.is_empty() {
            println!("    unreferenced (orphan; safe to delete)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_compression::tokio::bufread::ZstdEncoder;

    async fn zstd_of(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = ZstdEncoder::new(bytes);
        enc.read_to_end(&mut out).await.unwrap();
        out
    }

    /// El hash es el del contenido lógico: un blob comprimido tiene que dar el
    /// mismo sha que el crudo, o la pasada declararía rotos a todos los
    /// comprimidos sanos.
    #[tokio::test]
    async fn the_digest_is_of_the_content_not_of_its_packaging() {
        let content = b"partida de ayer, con sus cosas dentro".repeat(500);
        let want = hex::encode(Sha256::digest(&content));

        let (raw_sha, raw_len) = digest_of(&content[..], false).await.unwrap();
        assert_eq!(raw_sha, want);
        assert_eq!(raw_len, content.len() as u64);

        let packed = zstd_of(&content).await;
        assert!(
            packed.len() < content.len(),
            "el zstd tiene que comprimir algo"
        );
        let (zstd_sha, zstd_len) = digest_of(&packed[..], true).await.unwrap();
        assert_eq!(zstd_sha, want, "descomprimido, es el mismo contenido");
        assert_eq!(zstd_len, content.len() as u64);
    }

    /// El veredicto, que es lo que decide si se avisa a un usuario.
    #[test]
    fn the_verdict_needs_both_the_hash_and_the_size() {
        let sha = "a".repeat(64);
        assert_eq!(judge(&sha, 10, Some((&sha, 10))), Verdict::Ok);
        assert_eq!(judge(&sha, 10, None), Verdict::Missing);
        // La rotación de ago-2026: otro contenido, mismo tamaño.
        assert_eq!(
            judge(&sha, 10, Some((&"b".repeat(64), 10))),
            Verdict::Mismatch
        );
        // Y un objeto que hashea bien pero mide otra cosa tampoco vale.
        assert_eq!(judge(&sha, 10, Some((&sha, 9))), Verdict::Mismatch);
    }
}
