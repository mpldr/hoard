//! Qué dice el aviso, y en qué idioma.
//!
//! El daemon no puede usar el i18n del frontend (es JSON de Svelte cargado en el
//! webview), así que las cuatro frases que manda viven aquí, en los mismos ocho
//! idiomas que la app. Son pocas y no crecen: este slice cambia **quién** avisa,
//! no de cuántas cosas. Si algún día son muchas, la respuesta es compartir los
//! `.json` en tiempo de compilación, no dos catálogos que driftan.
//!
//! El idioma sale de la preferencia que el usuario eligió en Ajustes
//! (`prefs.language`, que hasta ahora sólo leía el frontend) y, si no la ha
//! tocado, del entorno (`LC_ALL`/`LC_MESSAGES`/`LANG`). Un servicio de fondo que
//! avisa en un idioma distinto al de la ventana se lee como si fuera otro
//! programa.

use super::Kind;

/// Un aviso ya escrito, listo para el transporte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub title: String,
    pub body: String,
}

/// Los idiomas de la app (`ui/src/lib/i18n/locales`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
    De,
    Fr,
    It,
    Ja,
    Pt,
    Zh,
}

impl Lang {
    /// El idioma del usuario: primero lo que eligió en la app, y si no lo ha
    /// elegido, lo que dice el entorno. Cualquier cosa que no reconozcamos cae
    /// en inglés, que es el idioma fuente.
    pub fn for_user(pref: Option<&str>) -> Self {
        pref.and_then(Self::parse)
            .or_else(Self::from_env)
            .unwrap_or(Lang::En)
    }

    /// `"es"`, `"es-ES"`, `"es_ES.UTF-8"` → [`Lang::Es`].
    fn parse(tag: &str) -> Option<Self> {
        let head = tag
            .split(['-', '_', '.'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match head.as_str() {
            "en" => Some(Lang::En),
            "es" => Some(Lang::Es),
            "de" => Some(Lang::De),
            "fr" => Some(Lang::Fr),
            "it" => Some(Lang::It),
            "ja" => Some(Lang::Ja),
            "pt" => Some(Lang::Pt),
            "zh" => Some(Lang::Zh),
            _ => None,
        }
    }

    fn from_env() -> Option<Self> {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|key| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
            .find_map(|value| Self::parse(&value))
    }

    fn strings(self) -> Strings {
        match self {
            Lang::En => EN,
            Lang::Es => ES,
            Lang::De => DE,
            Lang::Fr => FR,
            Lang::It => IT,
            Lang::Ja => JA,
            Lang::Pt => PT,
            Lang::Zh => ZH,
        }
    }
}

/// Las frases de un idioma. Los huecos (`{name}`, `{version}`…) los rellena
/// [`fill`]; un test comprueba que ningún idioma se deja uno por el camino.
#[derive(Debug, Clone, Copy)]
struct Strings {
    saved_title: &'static str,
    /// `{name}`, `{version}`, `{size}`
    saved_body: &'static str,
    failed_title: &'static str,
    failed_retrying_title: &'static str,
    /// `{name}`, `{error}`
    failed_body: &'static str,
    too_large_title: &'static str,
    /// `{name}`, `{size}`, `{limit}`
    too_large_body: &'static str,
    /// Sin cifras: el 413 self-hosted no trae límite ni tamaño. `{name}`
    too_large_body_generic: &'static str,
    stuck_title: &'static str,
    /// `{name}`, `{count}`
    stuck_body: &'static str,
}

const EN: Strings = Strings {
    saved_title: "Backup saved",
    saved_body: "{name} · v{version} ({size})",
    failed_title: "Backup failed",
    failed_retrying_title: "Backup failed (retrying)",
    failed_body: "{name}: {error}",
    too_large_title: "That save is over your plan",
    too_large_body: "{name}: {size} is over your plan's {limit} per-save limit.",
    too_large_body_generic: "{name} is over your plan's per-save limit.",
    stuck_title: "Cloud restore is failing",
    stuck_body: "{name} — failures in a row: {count}. Hoard keeps retrying, less and less often.",
};

const ES: Strings = Strings {
    saved_title: "Copia guardada",
    saved_body: "{name} · v{version} ({size})",
    failed_title: "La copia falló",
    failed_retrying_title: "La copia falló (reintentando)",
    failed_body: "{name}: {error}",
    too_large_title: "La partida supera tu plan",
    too_large_body: "{name}: {size} supera el límite de {limit} por partida de tu plan.",
    too_large_body_generic: "{name} supera el límite por partida de tu plan.",
    stuck_title: "La restauración desde la nube está fallando",
    stuck_body:
        "{name} — fallos seguidos: {count}. Hoard sigue reintentando, cada vez con menos frecuencia.",
};

const DE: Strings = Strings {
    saved_title: "Sicherung gespeichert",
    saved_body: "{name} · v{version} ({size})",
    failed_title: "Sicherung fehlgeschlagen",
    failed_retrying_title: "Sicherung fehlgeschlagen (neuer Versuch)",
    failed_body: "{name}: {error}",
    too_large_title: "Der Spielstand sprengt deinen Tarif",
    too_large_body: "{name}: {size} überschreitet das Limit von {limit} pro Spielstand.",
    too_large_body_generic: "{name} überschreitet das Limit deines Tarifs pro Spielstand.",
    stuck_title: "Die Wiederherstellung aus der Cloud schlägt fehl",
    stuck_body: "{name} — Fehler in Folge: {count}. Hoard versucht es weiter, immer seltener.",
};

const FR: Strings = Strings {
    saved_title: "Sauvegarde enregistrée",
    saved_body: "{name} · v{version} ({size})",
    failed_title: "Échec de la sauvegarde",
    failed_retrying_title: "Échec de la sauvegarde (nouvelle tentative)",
    failed_body: "{name} : {error}",
    too_large_title: "Cette partie dépasse votre offre",
    too_large_body: "{name} : {size} dépasse la limite de {limit} par partie de votre offre.",
    too_large_body_generic: "{name} dépasse la limite par partie de votre offre.",
    stuck_title: "La restauration depuis le cloud échoue",
    stuck_body: "{name} — échecs consécutifs : {count}. Hoard réessaie, de moins en moins souvent.",
};

const IT: Strings = Strings {
    saved_title: "Backup salvato",
    saved_body: "{name} · v{version} ({size})",
    failed_title: "Backup non riuscito",
    failed_retrying_title: "Backup non riuscito (nuovo tentativo)",
    failed_body: "{name}: {error}",
    too_large_title: "Questo salvataggio supera il tuo piano",
    too_large_body: "{name}: {size} supera il limite di {limit} per salvataggio del tuo piano.",
    too_large_body_generic: "{name} supera il limite per salvataggio del tuo piano.",
    stuck_title: "Il ripristino dal cloud sta fallendo",
    stuck_body: "{name} — errori di fila: {count}. Hoard continua a riprovare, sempre più di rado.",
};

const JA: Strings = Strings {
    saved_title: "バックアップを保存しました",
    saved_body: "{name} · v{version}（{size}）",
    failed_title: "バックアップに失敗しました",
    failed_retrying_title: "バックアップに失敗しました（再試行中）",
    failed_body: "{name}: {error}",
    too_large_title: "このセーブはプランの上限を超えています",
    too_large_body: "{name}: {size} はプランのセーブごとの上限 {limit} を超えています。",
    too_large_body_generic: "{name} はプランのセーブごとの上限を超えています。",
    stuck_title: "クラウドからの復元に失敗しています",
    stuck_body: "{name} — 連続失敗: {count} 回。Hoard は間隔を空けながら再試行を続けます。",
};

const PT: Strings = Strings {
    saved_title: "Cópia guardada",
    saved_body: "{name} · v{version} ({size})",
    failed_title: "A cópia falhou",
    failed_retrying_title: "A cópia falhou (a tentar de novo)",
    failed_body: "{name}: {error}",
    too_large_title: "Este save excede o teu plano",
    too_large_body: "{name}: {size} excede o limite de {limit} por save do teu plano.",
    too_large_body_generic: "{name} excede o limite por save do teu plano.",
    stuck_title: "O restauro a partir da nuvem está a falhar",
    stuck_body:
        "{name} — falhas seguidas: {count}. O Hoard continua a tentar, cada vez menos vezes.",
};

const ZH: Strings = Strings {
    saved_title: "备份已保存",
    saved_body: "{name} · v{version}（{size}）",
    failed_title: "备份失败",
    failed_retrying_title: "备份失败（正在重试）",
    failed_body: "{name}：{error}",
    too_large_title: "该存档超出你的套餐",
    too_large_body: "{name}：{size} 超过套餐中每个存档 {limit} 的上限。",
    too_large_body_generic: "{name} 超过套餐中每个存档的上限。",
    stuck_title: "云端恢复持续失败",
    stuck_body: "{name} — 连续失败：{count} 次。Hoard 会继续重试，频率逐渐降低。",
};

/// Escribe el aviso.
pub fn render(kind: &Kind, name: &str, lang: Lang) -> Note {
    let s = lang.strings();
    match kind {
        Kind::BackupSaved { version, bytes } => Note {
            title: s.saved_title.to_string(),
            body: fill(
                s.saved_body,
                &[
                    ("name", name),
                    ("version", &version.to_string()),
                    ("size", &bytes_human(*bytes)),
                ],
            ),
        },
        Kind::BackupFailed { error, retrying } => Note {
            title: if *retrying {
                s.failed_retrying_title.to_string()
            } else {
                s.failed_title.to_string()
            },
            body: fill(s.failed_body, &[("name", name), ("error", error)]),
        },
        Kind::BackupTooLarge {
            limit_bytes,
            actual_bytes,
        } => Note {
            title: s.too_large_title.to_string(),
            // El 413 de un self-hosted no trae cuerpo estructurado, así que sin
            // cifras se dice lo que sabemos en vez de enseñar "0 B".
            body: if *limit_bytes == 0 {
                fill(s.too_large_body_generic, &[("name", name)])
            } else {
                fill(
                    s.too_large_body,
                    &[
                        ("name", name),
                        ("size", &bytes_human(*actual_bytes)),
                        ("limit", &bytes_human(*limit_bytes)),
                    ],
                )
            },
        },
        Kind::RestoreStuck { failures } => Note {
            title: s.stuck_title.to_string(),
            body: fill(
                s.stuck_body,
                &[("name", name), ("count", &failures.to_string())],
            ),
        },
    }
}

/// Sustituye `{clave}` por su valor. Deliberadamente tonto: son plantillas
/// nuestras, no entrada del usuario.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in values {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

/// Tamaño legible, con los mismos cortes que la UI (`formatBytes` en
/// `stores/agent.ts`) para que el aviso y la ventana no digan cifras distintas
/// del mismo archivo.
fn bytes_human(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let n_f = n as f64;
    if n_f < KB {
        format!("{n} B")
    } else if n_f < MB {
        format!("{:.1} KB", n_f / KB)
    } else if n_f < GB {
        format!("{:.0} MB", n_f / MB)
    } else {
        format!("{:.1} GB", n_f / GB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Lang; 8] = [
        Lang::En,
        Lang::Es,
        Lang::De,
        Lang::Fr,
        Lang::It,
        Lang::Ja,
        Lang::Pt,
        Lang::Zh,
    ];

    /// Un hueco mal escrito no da error de compilación: sale literalmente en la
    /// notificación del usuario ("{nombre}: falló"). Este test es la red.
    #[test]
    fn every_language_keeps_its_placeholders() {
        for lang in ALL {
            let s = lang.strings();
            for hole in ["{name}", "{version}", "{size}"] {
                assert!(s.saved_body.contains(hole), "{lang:?} saved_body: {hole}");
            }
            for hole in ["{name}", "{error}"] {
                assert!(s.failed_body.contains(hole), "{lang:?} failed_body: {hole}");
            }
            for hole in ["{name}", "{size}", "{limit}"] {
                assert!(
                    s.too_large_body.contains(hole),
                    "{lang:?} too_large_body: {hole}"
                );
            }
            assert!(s.too_large_body_generic.contains("{name}"));
            for hole in ["{name}", "{count}"] {
                assert!(s.stuck_body.contains(hole), "{lang:?} stuck_body: {hole}");
            }
        }
    }

    /// Nada de frases vacías: una notificación sin título no se ve en GNOME.
    #[test]
    fn nothing_renders_empty_and_nothing_leaks_a_hole() {
        let kinds = [
            Kind::BackupSaved {
                version: 3,
                bytes: 5 * 1024 * 1024,
            },
            Kind::BackupFailed {
                error: "boom".into(),
                retrying: true,
            },
            Kind::BackupFailed {
                error: "boom".into(),
                retrying: false,
            },
            Kind::BackupTooLarge {
                limit_bytes: 1024,
                actual_bytes: 4096,
            },
            Kind::BackupTooLarge {
                limit_bytes: 0,
                actual_bytes: 0,
            },
            Kind::RestoreStuck { failures: 3 },
        ];
        for lang in ALL {
            for kind in &kinds {
                let note = render(kind, "Factorio", lang);
                assert!(!note.title.trim().is_empty(), "{lang:?} {kind:?}");
                assert!(!note.body.trim().is_empty(), "{lang:?} {kind:?}");
                assert!(!note.body.contains('{'), "unfilled hole: {}", note.body);
                assert!(note.body.contains("Factorio"), "{lang:?} {kind:?}");
            }
        }
    }

    #[test]
    fn the_users_choice_beats_the_environment() {
        assert_eq!(Lang::for_user(Some("es-ES")), Lang::Es);
        assert_eq!(Lang::for_user(Some("ja")), Lang::Ja);
        // Un idioma que la app no tiene no puede dejar el aviso en blanco.
        assert_eq!(Lang::for_user(Some("eu")), Lang::for_user(None));
    }

    #[test]
    fn locale_tags_are_parsed_the_way_the_environment_writes_them() {
        assert_eq!(Lang::parse("es_ES.UTF-8"), Some(Lang::Es));
        assert_eq!(Lang::parse("pt-BR"), Some(Lang::Pt));
        assert_eq!(Lang::parse("C"), None);
        assert_eq!(Lang::parse(""), None);
    }

    #[test]
    fn sizes_read_like_the_ui() {
        assert_eq!(bytes_human(512), "512 B");
        assert_eq!(bytes_human(2048), "2.0 KB");
        assert_eq!(bytes_human(5 * 1024 * 1024), "5 MB");
        assert_eq!(bytes_human(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn a_retrying_failure_says_so() {
        let retry = render(
            &Kind::BackupFailed {
                error: "no".into(),
                retrying: true,
            },
            "Factorio",
            Lang::Es,
        );
        let final_ = render(
            &Kind::BackupFailed {
                error: "no".into(),
                retrying: false,
            },
            "Factorio",
            Lang::Es,
        );
        assert_ne!(retry.title, final_.title);
    }
}
