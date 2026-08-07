//! Bajar y aplicar un componente: qué fichero de la release le toca a esta
//! máquina, cómo se comprueba que es nuestro, y cómo se instala.
//!
//! Vive en el agente y no en un frontend porque lo usan los dos caminos de
//! actualización —`hoard upgrade` desde la terminal y el botón de la app— y son
//! **la misma operación**. Duplicar aquí significaría que un día la app instala
//! un `.deb` donde la terminal instala un AppImage, y el usuario acaba con dos
//! Hoard distintos en la misma máquina.
//!
//! Nada de esto abre una ventana ni la necesita: elegir asset, verificar la
//! firma y llamar a `dpkg`/`rpm` es lógica de negocio corriente. Lo que se queda
//! en el desktop es *preguntar* al usuario y pintar el progreso.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::Delivery;

/// Repo de las releases. Mismo que resuelven `install.sh` / `install.ps1`.
const REPO: &str = "rleeon/hoard";

/// Clave pública minisign con la que CI firma **todo** lo publicado (ADR 0017;
/// el job de firma está aislado del que compila para que ninguna dependencia de
/// terceros vea la clave privada).
///
/// Un binario sin firma válida no se instala. Es la única defensa real entre
/// "bajo un ejecutable de internet" y "lo ejecuto con privilegios": el TLS de
/// GitHub dice que el fichero llegó entero, no que lo hayamos publicado
/// nosotros.
pub const MINISIGN_PUBKEY: &str = "RWSeOL1nHXZI9oa+WOdrc6yVasLPeBurvGWnERo4tN9F+YIQn7ipx3eO";

/// Un fichero publicado en la release.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    assets: Vec<Asset>,
    #[serde(default)]
    tag_name: String,
}

/// Los ficheros de una release. `None` como versión = la última publicada.
pub async fn release_assets(version: Option<&str>) -> Result<(String, Vec<Asset>)> {
    let url = match version {
        Some(v) => format!(
            "https://api.github.com/repos/{REPO}/releases/tags/v{}",
            v.trim_start_matches('v')
        ),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("hoard/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("asking GitHub for {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "GitHub answered {} for {url} — the release may not be published yet",
            resp.status()
        );
    }
    let rel: Release = resp.json().await.context("parsing the release")?;
    Ok((rel.tag_name.trim_start_matches('v').to_string(), rel.assets))
}

/// El fichero que le toca a esta vía de entrega.
///
/// Se busca **por vía**, no "a ver qué hay": la vía ya la decidió
/// [`super::resolve_delivery`] mirando la máquina (raíz inmutable, gestor
/// disponible, si podemos elevar), y volver a adivinar aquí por sufijos sería
/// tener la política escrita dos veces y en desacuerdo.
pub fn asset_for(delivery: Delivery, assets: &[Asset]) -> Option<&Asset> {
    let suffixes: &[&str] = match delivery {
        Delivery::Deb => &[".deb"],
        Delivery::Rpm => &[".rpm"],
        Delivery::AppImage => &[".AppImage"],
        // NSIS antes que MSI, y el orden importa desde que `hoardd` sobrevive a
        // la ventana: el instalador tiene que sobrescribir un `hoardd.exe` que
        // el servicio tiene abierto, y sólo el bundle NSIS lleva el hook que lo
        // para antes (`installer-hooks.nsh`). Por el MSI ese hook no corre y la
        // actualización muere contra el fichero bloqueado.
        Delivery::Nsis => &["-setup.exe", ".exe", ".msi"],
        Delivery::Dmg => &[".dmg"],
        // Lo mantiene un tercero: no hay fichero nuestro que bajar.
        Delivery::Managed => return None,
    };
    suffixes.iter().find_map(|suffix| {
        let mut matching = assets
            .iter()
            .filter(|a| a.name.ends_with(suffix) && !a.name.ends_with(".minisig"))
            .peekable();
        // Sin candidatos, nada que decidir.
        matching.peek()?;
        let candidates: Vec<&Asset> = matching.collect();
        pick_for_arch(&candidates)
    })
}

/// Tokens con los que los bundles nombran **nuestra** arquitectura.
fn arch_tokens() -> &'static [&'static str] {
    match std::env::consts::ARCH {
        "x86_64" => &["x86_64", "amd64", "x64"],
        "aarch64" => &["aarch64", "arm64"],
        _ => &[],
    }
}

/// Todos los tokens de arquitectura que sabemos reconocer, nuestros o no.
const KNOWN_ARCH_TOKENS: &[&str] = &["x86_64", "amd64", "x64", "aarch64", "arm64"];

/// De varios ficheros del mismo formato, el de esta arquitectura.
///
/// Hoy cada release publica un solo bundle por sistema, así que "coge el
/// primero" acierta por casualidad. Deja de acertar en cuanto se publique un
/// segundo: elegiría por el orden en que GitHub liste los ficheros, y un `.deb`
/// de amd64 en un ARM no es un fallo ruidoso sino un `dpkg` quejándose de algo
/// que no parece tener que ver. Y el tarball del núcleo **ya** se publica para
/// ARM, así que la máquina que puede caer aquí existe hoy.
///
/// Si ningún candidato lleva token de arquitectura, la release no distingue y
/// vale el primero. Si los llevan pero ninguno es el nuestro, se devuelve `None`
/// a propósito: "no hay paquete para tu arquitectura" es una respuesta útil;
/// instalar el de otra, no.
fn pick_for_arch<'a>(candidates: &[&'a Asset]) -> Option<&'a Asset> {
    let ours = arch_tokens();
    if let Some(hit) = candidates
        .iter()
        .find(|a| ours.iter().any(|t| contains_token(&a.name, t)))
    {
        return Some(hit);
    }
    let any_tagged = candidates
        .iter()
        .any(|a| KNOWN_ARCH_TOKENS.iter().any(|t| contains_token(&a.name, t)));
    if any_tagged {
        return None;
    }
    candidates.first().copied()
}

/// ¿Aparece `token` como pieza del nombre y no dentro de otra palabra?
///
/// Se comprueban los bordes en vez de trocear por separadores, y no es un
/// detalle de estilo: el token más importante —`x86_64`— **lleva un `_` dentro**,
/// así que partir por `_` lo destruye antes de poder buscarlo y ningún fichero
/// nombrado a la manera habitual casaría jamás. Un borde es cualquier cosa que
/// no sea alfanumérica, o el principio/fin del nombre.
fn contains_token(name: &str, token: &str) -> bool {
    let hay = name.to_ascii_lowercase();
    let needle = token.to_ascii_lowercase();
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(&needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Baja un asset y su `.minisig`, verifica la firma y deja el fichero en disco.
/// Devuelve dónde quedó.
///
/// Falla cerrado: sin `.minisig` publicado, o con una firma que no case, no se
/// escribe nada aplicable. Un instalador se ejecuta con privilegios; "seguro que
/// está bien" no es una política.
pub async fn download_verified(
    asset: &Asset,
    assets: &[Asset],
    dest_dir: &Path,
) -> Result<PathBuf> {
    let sig_name = format!("{}.minisig", asset.name);
    let sig = assets
        .iter()
        .find(|a| a.name == sig_name)
        .with_context(|| {
            format!(
                "{} has no published signature ({sig_name}). Refusing to install an \
                 unverified installer.",
                asset.name
            )
        })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(concat!("hoard/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let bytes = client
        .get(&asset.url)
        .send()
        .await
        .with_context(|| format!("downloading {}", asset.name))?
        .error_for_status()?
        .bytes()
        .await?;
    let sig_text = client
        .get(&sig.url)
        .send()
        .await
        .with_context(|| format!("downloading {sig_name}"))?
        .error_for_status()?
        .text()
        .await?;

    verify(&bytes, &sig_text).with_context(|| format!("verifying {}", asset.name))?;

    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let path = dest_dir.join(&asset.name);
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Comprueba `bytes` contra la firma minisign `sig_text` con [`MINISIGN_PUBKEY`].
pub fn verify(bytes: &[u8], sig_text: &str) -> Result<()> {
    use minisign_verify::{PublicKey, Signature};

    let pubkey = PublicKey::from_base64(MINISIGN_PUBKEY)
        .map_err(|e| anyhow::anyhow!("the embedded public key is unusable: {e}"))?;
    let signature = Signature::decode(sig_text)
        .map_err(|e| anyhow::anyhow!("the signature file is malformed: {e}"))?;
    pubkey
        .verify(bytes, &signature, false)
        .map_err(|e| anyhow::anyhow!("signature does not match: {e}"))?;
    Ok(())
}

/// Instala el fichero ya verificado según su vía.
///
/// Los paquetes nativos necesitan privilegios; el AppImage no toca nada fuera
/// del home. `noninteractive` corta cualquier vía que pudiera pararse a
/// preguntar — dentro de `curl … | sh` no hay a quién preguntar, y colgarse es
/// peor que no instalar.
pub async fn apply_desktop(
    delivery: Delivery,
    path: &Path,
    noninteractive: bool,
) -> Result<PathBuf> {
    match delivery {
        Delivery::Deb => {
            elevated(&["dpkg", "-i"], path, noninteractive).await?;
            Ok(PathBuf::from("/usr/bin/hoard-desktop"))
        }
        Delivery::Rpm => {
            elevated(&["rpm", "-U", "--force"], path, noninteractive).await?;
            Ok(PathBuf::from("/usr/bin/hoard-desktop"))
        }
        Delivery::AppImage => place_appimage(path),
        Delivery::Nsis => {
            // El instalador se encarga (y lleva el hook que para el servicio
            // antes de tocar `hoardd.exe`). `/S` = silencioso.
            let status = tokio::process::Command::new(path)
                .arg("/S")
                .status()
                .await
                .with_context(|| format!("running {}", path.display()))?;
            if !status.success() {
                bail!("the installer exited with {status}");
            }
            Ok(path.to_path_buf())
        }
        Delivery::Dmg => {
            bail!(
                "a .dmg can't be installed unattended — open {} and drag Hoard to \
                 Applications.",
                path.display()
            )
        }
        Delivery::Managed => {
            bail!("this install is managed by your package manager; nothing to do")
        }
    }
}

/// Corre un gestor de paquetes con privilegios: `pkexec` primero (pinta su propio
/// diálogo, no depende de esta terminal), `sudo -n` si no.
///
/// Ninguna de las dos vías puede quedarse esperando a un humano, que es lo que
/// pasaría dentro de `curl … | sh`, donde el stdin es el propio script: `sudo`
/// lleva `-n` siempre, así que sin credencial en caché falla al instante, y
/// `pkexec` sólo se elige cuando hay sesión gráfica a la que pintar el diálogo
/// (lo comprueba [`super::can_elevate`] antes de que la vía llegue hasta aquí).
///
/// `noninteractive` cierra el hueco que queda: [`super::can_elevate`] da por
/// bueno `pkexec` cuando hay `$DISPLAY`, pero `$DISPLAY` puede estar puesto sin
/// que haya un agente de polkit escuchando —SSH con X11 reenviado es el caso
/// típico— y entonces `pkexec` se queda esperando a un diálogo que nadie va a
/// pintar. Con la bandera puesta sólo valen root y `sudo -n`: fallar con un
/// mensaje es siempre mejor que colgar un instalador.
async fn elevated(cmd: &[&str], path: &Path, noninteractive: bool) -> Result<()> {
    let mut argv: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
    argv.push(path.to_string_lossy().to_string());

    let is_root = {
        #[cfg(unix)]
        {
            // SAFETY: `geteuid` no toma argumentos y no falla.
            unsafe { libc::geteuid() == 0 }
        }
        #[cfg(not(unix))]
        {
            false
        }
    };

    let (program, args): (String, Vec<String>) = if is_root {
        (argv[0].clone(), argv[1..].to_vec())
    } else if which("pkexec") && !noninteractive {
        ("pkexec".into(), argv)
    } else if which("sudo") {
        let mut a = vec!["-n".to_string()];
        a.extend(argv);
        ("sudo".into(), a)
    } else if noninteractive {
        bail!(
            "this package needs privileges and nothing here can grant them without asking \
             (run `hoard install` yourself from a terminal, or use `--headless`)"
        )
    } else {
        bail!("no way to get the privileges this package needs (no pkexec, no sudo)")
    };

    let status = tokio::process::Command::new(&program)
        .args(&args)
        .status()
        .await
        .with_context(|| format!("running `{program}`"))?;
    if !status.success() {
        bail!("`{program} {}` exited with {status}", args.join(" "));
    }
    Ok(())
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

/// Coloca el AppImage en el home y le pone entrada de menú.
///
/// Sin `sudo` y sin gestor de paquetes: es la vía que funciona donde los otros
/// dos no pueden (SteamOS, Bazzite, Arch). El motor **no** va aquí dentro — lo
/// puso el instalador en ruta estable —, que es lo que permite que el sync de
/// esta máquina arranque en boot pese a que la app sea un AppImage.
fn place_appimage(downloaded: &Path) -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("no HOME in the environment")?;
    let bin = home.join(".local").join("bin");
    std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    let dest = bin.join("hoard-desktop");

    // Se escribe a un temporal y se renombra encima, nunca `copy` directo: si la
    // app está abierta —y lo normal es actualizar desde la propia app— el kernel
    // rechaza escribir sobre su ejecutable con `ETXTBSY`. `rename` sobre un
    // binario en marcha sí vale: el proceso vivo conserva su inode y el nombre
    // pasa a apuntar al nuevo. Y de paso es atómico, así que un fallo a medias
    // no deja un AppImage truncado donde había uno que funcionaba.
    let staging = bin.join(".hoard-desktop.new");
    let _ = std::fs::remove_file(&staging);
    std::fs::copy(downloaded, &staging)
        .with_context(|| format!("staging the AppImage at {}", staging.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", staging.display()))?;
    }
    if let Err(e) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_file(&staging);
        return Err(e).with_context(|| format!("installing the AppImage at {}", dest.display()));
    }
    write_desktop_entry(&home, &dest)?;
    Ok(dest)
}

/// La entrada de menú. Sin ella el AppImage existe pero no se puede lanzar desde
/// ningún sitio salvo la terminal — y en modo gaming eso es no existir.
fn write_desktop_entry(home: &Path, exe: &Path) -> Result<()> {
    let dir = home.join(".local").join("share").join("applications");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Hoard\n\
         Comment=Game save sync\n\
         Exec=\"{}\"\n\
         Icon=hoard\n\
         Terminal=false\n\
         Categories=Utility;Game;\n",
        exe.display()
    );
    let path = dir.join("dev.hoard.desktop.desktop");
    std::fs::write(&path, entry).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets() -> Vec<Asset> {
        ["deb", "rpm", "AppImage", "dmg"]
            .iter()
            .flat_map(|ext| {
                let name = format!("Hoard_1.2.0_amd64.{ext}");
                [
                    Asset {
                        url: format!("https://example.invalid/{name}"),
                        name: name.clone(),
                    },
                    Asset {
                        url: format!("https://example.invalid/{name}.minisig"),
                        name: format!("{name}.minisig"),
                    },
                ]
            })
            .chain([Asset {
                name: "Hoard_1.2.0_x64-setup.exe".into(),
                url: "https://example.invalid/setup".into(),
            }])
            .collect()
    }

    #[test]
    fn each_delivery_picks_its_own_file() {
        let a = assets();
        assert!(asset_for(Delivery::Deb, &a).unwrap().name.ends_with(".deb"));
        assert!(asset_for(Delivery::Rpm, &a).unwrap().name.ends_with(".rpm"));
        assert!(asset_for(Delivery::AppImage, &a)
            .unwrap()
            .name
            .ends_with(".AppImage"));
        assert!(asset_for(Delivery::Dmg, &a).unwrap().name.ends_with(".dmg"));
        assert_eq!(
            asset_for(Delivery::Nsis, &a).unwrap().name,
            "Hoard_1.2.0_x64-setup.exe"
        );
    }

    /// La firma nunca puede colarse como el propio artefacto: `.deb.minisig`
    /// también "acaba en .deb" si se mira con poco cuidado, y hacer `dpkg -i`
    /// sobre un fichero de firma es un fallo absurdo de diagnosticar.
    #[test]
    fn a_signature_is_never_mistaken_for_the_artifact() {
        let a = assets();
        for d in [
            Delivery::Deb,
            Delivery::Rpm,
            Delivery::AppImage,
            Delivery::Dmg,
        ] {
            assert!(!asset_for(d, &a).unwrap().name.ends_with(".minisig"));
        }
    }

    /// Una instalación de terceros no tiene fichero nuestro que bajar. Devolver
    /// `Some` aquí acabaría pisando por debajo lo que puso el gestor de paquetes
    /// de la distro.
    #[test]
    fn a_managed_install_has_nothing_to_fetch() {
        assert!(asset_for(Delivery::Managed, &assets()).is_none());
    }

    #[test]
    fn a_release_without_our_format_reports_nothing() {
        let only_windows = vec![Asset {
            name: "Hoard_1.2.0_x64-setup.exe".into(),
            url: "https://example.invalid/setup".into(),
        }];
        assert!(asset_for(Delivery::Deb, &only_windows).is_none());
    }

    /// Una release con dos arquitecturas no puede resolverse por el orden en que
    /// GitHub liste los ficheros. Hoy sólo se publica una por sistema, así que
    /// "el primero" acierta de casualidad; el día que se publique la segunda,
    /// esto es lo que evita un `.deb` de amd64 aterrizando en un ARM.
    #[test]
    fn a_two_arch_release_picks_this_machines_arch() {
        let two = vec![
            Asset {
                name: "Hoard_1.2.0_arm64.deb".into(),
                url: "u".into(),
            },
            Asset {
                name: "Hoard_1.2.0_amd64.deb".into(),
                url: "u".into(),
            },
        ];
        let want = if cfg!(target_arch = "x86_64") {
            "Hoard_1.2.0_amd64.deb"
        } else {
            "Hoard_1.2.0_arm64.deb"
        };
        assert_eq!(
            asset_for(Delivery::Deb, &two).map(|a| a.name.as_str()),
            Some(want)
        );
    }

    /// Y si la release sólo trae otra arquitectura, mejor decir que no hay nada
    /// que instalar el paquete equivocado: el error de `dpkg` sobre una
    /// arquitectura ajena no lleva a ninguna parte.
    #[test]
    fn a_release_for_another_arch_only_reports_nothing() {
        let other = if cfg!(target_arch = "x86_64") {
            "Hoard_1.2.0_arm64.deb"
        } else {
            "Hoard_1.2.0_amd64.deb"
        };
        let only_other = vec![Asset {
            name: other.into(),
            url: "u".into(),
        }];
        assert!(asset_for(Delivery::Deb, &only_other).is_none());
    }

    /// Una release de una sola arquitectura no etiqueta nada, y eso tiene que
    /// seguir valiendo — es el caso de hoy.
    #[test]
    fn an_untagged_release_still_resolves() {
        let untagged = vec![Asset {
            name: "Hoard.deb".into(),
            url: "u".into(),
        }];
        assert_eq!(
            asset_for(Delivery::Deb, &untagged).map(|a| a.name.as_str()),
            Some("Hoard.deb")
        );
    }

    /// El token es una pieza del nombre, no una subcadena: `x64` no puede casar
    /// dentro de `x86_64` ni al revés.
    #[test]
    fn arch_tokens_match_whole_parts_only() {
        // El token con `_` dentro es el que rompía la primera versión de esto.
        assert!(contains_token("hoard-1.2.0-linux-x86_64.tar.gz", "x86_64"));
        assert!(contains_token("Hoard_1.2.0_x64-setup.exe", "x64"));
        assert!(contains_token("Hoard_1.2.0_amd64.deb", "amd64"));
        assert!(contains_token("Hoard_1.2.0_aarch64.dmg", "aarch64"));
        // Y no cuela dentro de otra palabra.
        assert!(!contains_token("Hoard_1.2.0_x86_64.deb", "x64"));
        assert!(!contains_token("Hoard_prearm64x.deb", "arm64"));
    }

    /// Falla cerrado: basura por firma no verifica. Es la aserción que separa
    /// "bajé un fichero" de "bajé *nuestro* fichero".
    #[test]
    fn a_bogus_signature_does_not_verify() {
        assert!(verify(b"payload", "untrusted comment: nope\nnot-a-signature\n").is_err());
    }
}
