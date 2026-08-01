//! Hoard-Wrapped share card — avatar local y "sacar la foto".
//!
//! Todo lo que toca esta pieza es **de este equipo y solo de este equipo**: la
//! foto y el nombre de la tarjeta no viajan al servidor ni a la nube, y no van
//! en el export de la cuenta. La foto vive como un único `avatar.png` bajo el
//! app-data dir; el resto de la configuración (nombre, frase, rango) la guarda
//! el frontend en su `store` local. Borrar el fichero es todo el "olvídame"
//! que hace falta.
//!
//! El recorte/escalado del avatar se hace en el webview (canvas) antes de
//! llegar aquí, así que este módulo solo ve PNG ya normalizado: un formato,
//! un tamaño acotado, nada de adivinar MIME al releerlo.
//!
//! `wrapple_save_card` escribe la tarjeta renderizada en la galería del
//! sistema (`Pictures/Hoard/`) y le inyecta metadatos PNG `tEXt` — título,
//! autor del software y `https://hoard.services`. Es la parte "SEO" del
//! encargo: una imagen que se comparte suelta lleva de dónde salió, tanto a la
//! vista (marca de agua) como en sus metadatos, que es lo que leen los
//! buscadores de imágenes y los visores.

use std::path::{Path, PathBuf};

use base64::Engine;
use tauri::ipc::Response;
use tauri::Manager;

/// Extensiones que aceptamos al elegir foto. La lista es la misma que la de
/// carátulas personalizadas: lo que el webview sabe decodificar.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "tif"];

/// Tope al leer el fichero que elige el usuario. Una foto de móvil ronda los
/// 5 MB; 32 nos deja sitio de sobra sin permitir que un TIFF de 400 MB entre
/// entero en la RAM del webview.
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// Tope al guardar. El avatar sale de un canvas de 512×512 (≈300 KB) y la
/// tarjeta de uno de 1200×630 (≈1,5 MB); 16 MB es holgura, no permiso.
const MAX_PNG_BYTES: usize = 16 * 1024 * 1024;

fn wrapple_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("wrapple"))
}

fn avatar_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(wrapple_dir(app)?.join("avatar.png"))
}

fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
}

fn decode_png(data: &str) -> Result<Vec<u8>, String> {
    // El frontend manda base64 pelado, pero aceptamos también el data-URL
    // entero por si alguien pasa el `toDataURL()` tal cual.
    let payload = data.rsplit_once(",").map_or(data, |(_, tail)| tail);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|e| format!("imagen ilegible: {e}"))?;
    if bytes.is_empty() {
        return Err("imagen vacía".into());
    }
    if bytes.len() > MAX_PNG_BYTES {
        return Err("imagen demasiado grande".into());
    }
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return Err("la imagen no es un PNG".into());
    }
    Ok(bytes)
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Lee un fichero de imagen que ha elegido el usuario y devuelve sus bytes al
/// webview, que es quien lo recorta y escala. No copiamos nada todavía: hasta
/// que no confirma el recorte, en disco no queda rastro.
#[tauri::command]
pub async fn wrapple_read_image(source_path: String) -> Result<Response, String> {
    let src = PathBuf::from(&source_path);
    if !has_image_extension(&src) {
        return Err("ese fichero no es una imagen".into());
    }
    let meta = tokio::fs::metadata(&src)
        .await
        .map_err(|e| format!("no se pudo leer la imagen: {e}"))?;
    if !meta.is_file() {
        return Err("la ruta no es un fichero".into());
    }
    if meta.len() > MAX_SOURCE_BYTES {
        return Err("la imagen pesa demasiado (máx. 32 MB)".into());
    }
    tokio::fs::read(&src)
        .await
        .map(Response::new)
        .map_err(|e| format!("no se pudo leer la imagen: {e}"))
}

/// Guarda el avatar de la tarjeta (PNG ya recortado por el webview). Local y
/// nada más: nunca se sube.
#[tauri::command]
pub async fn wrapple_set_avatar(app: tauri::AppHandle, png_base64: String) -> Result<(), String> {
    let bytes = decode_png(&png_base64)?;
    let dir = wrapple_dir(&app)?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    // Escritura atómica: un fallo a medias dejaría un PNG truncado que el
    // webview no sabe pintar y que además persiste entre arranques.
    let dest = dir.join("avatar.png");
    let tmp = dir.join("avatar.png.tmp");
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Bytes del avatar guardado. `Err` cuando no hay ninguno — el frontend cae a
/// las iniciales sin más.
#[tauri::command]
pub async fn wrapple_avatar_bytes(app: tauri::AppHandle) -> Result<Response, String> {
    let path = avatar_path(&app)?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| "sin avatar".to_string())?;
    if bytes.is_empty() {
        return Err("sin avatar".into());
    }
    Ok(Response::new(bytes))
}

/// Olvida la foto local.
#[tauri::command]
pub async fn wrapple_clear_avatar(app: tauri::AppHandle) -> Result<(), String> {
    let path = avatar_path(&app)?;
    let _ = tokio::fs::remove_file(&path).await;
    Ok(())
}

/// Escribe la tarjeta renderizada en la galería del sistema y devuelve la ruta
/// final para poder enseñársela al usuario.
///
/// Destino: `<Imágenes>/Hoard/hoard-wrapped-<fecha>.png`. Si el sistema no
/// declara carpeta de imágenes (cuentas de servicio, entornos raros) caemos a
/// Descargas y luego al home, porque fallar el guardado por no encontrar una
/// carpeta canónica sería absurdo.
#[tauri::command]
pub async fn wrapple_save_card(
    app: tauri::AppHandle,
    png_base64: String,
    label: Option<String>,
) -> Result<String, String> {
    let bytes = decode_png(&png_base64)?;
    let bytes = with_seo_metadata(bytes, label.as_deref());

    let paths = app.path();
    let gallery = paths
        .picture_dir()
        .or_else(|_| paths.download_dir())
        .or_else(|_| paths.home_dir())
        .map_err(|e| format!("no se encontró carpeta de imágenes: {e}"))?
        .join("Hoard");
    tokio::fs::create_dir_all(&gallery)
        .await
        .map_err(|e| format!("no se pudo crear {}: {e}", gallery.display()))?;

    let dest = gallery.join(format!("hoard-wrapped-{}.png", timestamp_slug()));
    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(|e| format!("no se pudo guardar la imagen: {e}"))?;
    Ok(dest.to_string_lossy().into_owned())
}

/// `20260801-2137`, hora local. Sin dos puntos ni barras: el nombre tiene que
/// sobrevivir a NTFS igual que a ext4.
fn timestamp_slug() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!(
        "{:04}{:02}{:02}-{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    )
}

/// Inyecta chunks `tEXt` con la procedencia justo detrás del IHDR.
///
/// Un PNG es firma + cadena de chunks `[len:u32][tipo:4][datos][crc:u32]`, y
/// los `tEXt` son legales en cualquier punto entre IHDR e IEND. Metemos los
/// nuestros pegados al IHDR para que cualquier lector los vea sin recorrerse la
/// imagen entera. Si el buffer no tiene la pinta esperada devolvemos el PNG
/// intacto: los metadatos son un extra, nunca un motivo para no guardar.
fn with_seo_metadata(png: Vec<u8>, label: Option<&str>) -> Vec<u8> {
    const IHDR_END: usize = 8 + 4 + 4 + 13 + 4; // firma + len + "IHDR" + datos + crc
    if png.len() < IHDR_END || &png[12..16] != b"IHDR" {
        return png;
    }

    let title = match label.map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) => format!("Hoard Wrapped — {l}"),
        None => "Hoard Wrapped".to_string(),
    };
    let fields: [(&str, &str); 5] = [
        ("Title", title.as_str()),
        ("Software", "Hoard — hoard.services"),
        ("Source", "https://hoard.services"),
        ("Copyright", "Hoard · hoard.services"),
        (
            "Description",
            "Resumen de partidas generado con Hoard, copias de seguridad automáticas \
             para tus partidas guardadas — https://hoard.services",
        ),
    ];

    let mut out = Vec::with_capacity(png.len() + 512);
    out.extend_from_slice(&png[..IHDR_END]);
    for (key, value) in fields {
        out.extend_from_slice(&text_chunk(key, value));
    }
    out.extend_from_slice(&png[IHDR_END..]);
    out
}

/// Un chunk `tEXt`: clave Latin-1 (1..=79 bytes), NUL, valor. Los caracteres
/// fuera de Latin-1 se caen del valor en vez de romper el chunk.
fn text_chunk(key: &str, value: &str) -> Vec<u8> {
    let latin1 = |s: &str| -> Vec<u8> {
        s.chars()
            .filter(|c| (*c as u32) < 256 && *c != '\0')
            .map(|c| c as u8)
            .collect()
    };
    let mut data = latin1(key);
    data.truncate(79);
    data.push(0);
    data.extend_from_slice(&latin1(value));

    let mut chunk = Vec::with_capacity(data.len() + 12);
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut typed = b"tEXt".to_vec();
    typed.extend_from_slice(&data);
    chunk.extend_from_slice(&typed);
    chunk.extend_from_slice(&crc32(&typed).to_be_bytes());
    chunk
}

/// CRC-32 (IEEE, el del PNG). Sin dependencia: son doce líneas y se llama
/// cinco veces por imagen guardada.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vector conocido: el CRC-32 de "123456789" es 0xCBF43926.
    #[test]
    fn crc32_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    /// El chunk lleva longitud, tipo y un CRC que cubre tipo+datos.
    #[test]
    fn text_chunk_is_well_formed() {
        let chunk = text_chunk("Source", "https://hoard.services");
        let len = u32::from_be_bytes(chunk[0..4].try_into().unwrap()) as usize;
        assert_eq!(&chunk[4..8], b"tEXt");
        assert_eq!(chunk.len(), len + 12);
        let crc = u32::from_be_bytes(chunk[chunk.len() - 4..].try_into().unwrap());
        assert_eq!(crc, crc32(&chunk[4..chunk.len() - 4]));
        // clave NUL valor
        assert_eq!(&chunk[8..14], b"Source");
        assert_eq!(chunk[14], 0);
    }

    /// Los metadatos entran detrás del IHDR y dejan el resto del fichero igual.
    #[test]
    fn metadata_goes_after_ihdr() {
        let mut png = Vec::new();
        png.extend_from_slice(&PNG_SIGNATURE);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0u8; 13]);
        png.extend_from_slice(&[1, 2, 3, 4]); // crc de mentira, no lo tocamos
        png.extend_from_slice(b"IDATTAIL");

        let out = with_seo_metadata(png.clone(), Some("Rust"));
        assert!(out.len() > png.len());
        assert_eq!(&out[..33], &png[..33]); // firma + IHDR intactos
        assert_eq!(&out[37..41], b"tEXt"); // el 1er chunk inyectado va justo detrás
        assert!(out.ends_with(b"IDATTAIL"));
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("hoard.services"));
        assert!(text.contains("Rust"));
    }

    /// Un buffer que no es PNG sale tal cual, sin panic.
    #[test]
    fn non_png_passes_through() {
        let junk = b"no soy un png".to_vec();
        assert_eq!(with_seo_metadata(junk.clone(), None), junk);
    }

    /// Solo entran PNG: base64 válido pero de otro formato se rechaza.
    #[test]
    fn decode_png_rejects_other_formats() {
        let jpeg = base64::engine::general_purpose::STANDARD.encode([0xff, 0xd8, 0xff, 0xe0]);
        assert!(decode_png(&jpeg).is_err());
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(PNG_SIGNATURE)
        );
        assert!(decode_png(&data_url).is_ok());
    }
}
