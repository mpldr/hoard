//! # Newtypes de identidad con la puerta en `serde` (ADR 0021, C.3 — Slice 3)
//!
//! El veneno entró por **datos persistidos**, no por código construyendo mal:
//! el save de `GSE Saves` quedó rastreado con `game_slug` = nombre de usuario de
//! Windows, y como el username es componente de ruta de todo ejecutable del
//! perfil, cualquier app disparaba "estás jugando" (la correlación fantasma de
//! julio 2026). Un `GameSlug` que valide en `new()` pero derive `Deserialize` a
//! pelo no habría parado nada: la deserialización es una puerta trasera que
//! construye el tipo sin pasar por el validador.
//!
//! Por eso aquí **no se deriva `Deserialize` a pelo en ningún sitio**: cada
//! newtype lleva `#[serde(try_from = "String")]`, así que la *única* vía de
//! construcción —incluida la que usa el JSON de disco y de red— es
//! `Self::parse`. Es imposible saltarse la puerta sin editar este fichero.
//!
//! ## Dos puertas, dos trabajos
//!
//! | | [`parse`](GameSlug::parse) | [`repair`](GameSlug::repair) |
//! |---|---|---|
//! | Quién la usa | `serde` (wire, IPC, datos **nuevos**) | el cargador de estado **ya persistido** |
//! | Valor inválido | error | se re-deriva o se pone en cuarentena |
//!
//! La segunda existe porque el veneno **ya está en disco**: un `try_from`
//! estricto sobre `state.json` / la DB del server brickearía instalaciones
//! existentes (el motor no arrancaría). La ADR es explícita (C.3, "hazard de
//! upgrade"): la puerta estricta protege lo nuevo; lo viejo se limpia al leer —
//! re-derivar, loggear, marcar— y **nunca** se rechaza en duro. La limpieza
//! durable definitiva vive en la migración del Slice 5.
//!
//! ## Por qué newtypes y no `String`
//!
//! Dos capas de protección independientes:
//!
//! 1. **La forma del valor** la garantiza `parse` (un slug vacío o con basura no
//!    existe como `GameSlug`).
//! 2. **La categoría** la garantiza el sistema de tipos: `slug == username` —el
//!    error real que costó la correlación fantasma— es un error de compilación
//!    en cuanto los dos lados son [`GameSlug`] y [`Username`], porque no hay
//!    `PartialEq` entre ellos ni conversión implícita.

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize, Serializer};
use thiserror::Error;

/// Tope de longitud de un slug. Mismo que aplica [`slugify`], que es quien mintó
/// todos los slugs del catálogo.
pub const MAX_SLUG_LEN: usize = 96;

/// Tope de longitud de un nombre de usuario. Generoso a propósito: el server
/// nunca validó usernames (los crea `hoard-admin user create` con lo que teclee
/// el operador), así que el tope sólo frena lo absurdo.
pub const MAX_USERNAME_LEN: usize = 128;

/// Slug sintético para el tiempo atribuido a un día pero no a un juego
/// concreto. Es vocabulario del protocolo de playtime (agente y server lo
/// declaran por separado hoy), así que [`GameSlug::parse`] lo acepta como
/// **reservado**: no es un nombre de juego pero sí un valor legítimo del campo.
pub const OTHER_SLUG: &str = "__other__";

/// Longitud mínima de un token de identidad para que cuente en el match
/// genérico. Por debajo (`gta`, `ori`, `ff`) es demasiado corto y colisiona con
/// carpetas o nombres de proceso cualesquiera.
pub const MIN_IDENTITY_TOKEN_LEN: usize = 4;

/// Tokens de fontanería: componentes de perfil de usuario y de rutas de
/// instalación. Un slug degenerado igual a uno de estos convierte procesos
/// cualesquiera en señal fuerte de "estás jugando" — la correlación fantasma.
///
/// La lista es **estática y pura** a propósito: el kernel no puede leer el
/// entorno. Los componentes del home real (el username incluido, que fue el caso
/// real) los añade el shell — ver `hoard_agent::agent::is_generic_identity_token`,
/// que extiende esta lista con `directories::UserDirs`.
pub const GENERIC_IDENTITY_TOKENS: &[&str] = &[
    "users",
    "home",
    "appdata",
    "roaming",
    "local",
    "locallow",
    "documents",
    "savedgames",
    "mygames",
    "saves",
    "games",
    "programfiles",
    "programfilesx86",
    "steamapps",
    "common",
    "compatdata",
    "drivec",
    "windows",
    "desktop",
    "downloads",
];

/// Por qué un valor no pasó la puerta. `kind` nombra el newtype para que el
/// mensaje sea diagnosticable sin envolverlo.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("{kind}: valor vacío")]
    Empty { kind: &'static str },
    #[error("{kind}: {len} caracteres, el máximo es {max}")]
    TooLong {
        kind: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{kind}: carácter inválido {ch:?} en {raw:?}")]
    BadChar {
        kind: &'static str,
        ch: char,
        raw: String,
    },
    #[error("{kind}: forma inválida ({expected}), recibido {raw:?}")]
    BadShape {
        kind: &'static str,
        expected: &'static str,
        raw: String,
    },
}

/// Por qué un valor persistido no se pudo reparar y va a cuarentena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineReason {
    /// No quedaba nada tras normalizar (vacío, sólo espacios, sólo símbolos).
    /// Re-derivar aquí **fabricaría** un identificador que nadie escribió.
    Empty,
    /// Sintácticamente válido pero degenerado: coincide con un token de
    /// fontanería ([`GENERIC_IDENTITY_TOKENS`]). Éste es el veneno de la
    /// correlación fantasma; repararlo no aplica porque el valor *ya* tiene la
    /// forma correcta — lo que está mal es que signifique cualquier cosa.
    Degenerate,
    /// La forma es irrecuperable por construcción: no se puede inventar un UUID
    /// ni un SHA-256 a partir de basura.
    Unrecoverable,
}

impl fmt::Display for QuarantineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            QuarantineReason::Empty => "vacío tras normalizar",
            QuarantineReason::Degenerate => "token genérico de fontanería",
            QuarantineReason::Unrecoverable => "forma irrecuperable",
        };
        f.write_str(s)
    }
}

/// Resultado de pasar un valor **ya persistido** por la puerta indulgente.
/// Nunca es un error: cargar estado viejo no puede fallar (ADR 0021 C.3), sólo
/// puede acabar en uno de estos tres sitios.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repair<T> {
    /// Ya era válido; pasa tal cual.
    Clean(T),
    /// Se re-derivó un valor válido del crudo. `raw` se conserva para el log.
    Repaired { value: T, raw: String },
    /// No hay valor de fiar. El llamante decide qué hacer con el crudo (dejarlo
    /// donde está y marcarlo, o descartar la fila); lo que **no** puede hacer es
    /// abortar la carga.
    Quarantined {
        raw: String,
        reason: QuarantineReason,
    },
}

impl<T> Repair<T> {
    /// El valor reparado, o `None` si acabó en cuarentena.
    pub fn value(&self) -> Option<&T> {
        match self {
            Repair::Clean(v) | Repair::Repaired { value: v, .. } => Some(v),
            Repair::Quarantined { .. } => None,
        }
    }

    /// Consume y devuelve el valor reparado, o `None` si acabó en cuarentena.
    pub fn into_value(self) -> Option<T> {
        match self {
            Repair::Clean(v) | Repair::Repaired { value: v, .. } => Some(v),
            Repair::Quarantined { .. } => None,
        }
    }

    /// ¿Pasó sin tocar nada? Útil para no loggear el 99.9% de los casos.
    pub fn is_clean(&self) -> bool {
        matches!(self, Repair::Clean(_))
    }

    /// ¿Acabó en cuarentena?
    pub fn is_quarantined(&self) -> bool {
        matches!(self, Repair::Quarantined { .. })
    }
}

/// Genera el envoltorio común de un newtype de identidad.
///
/// Lo que **no** genera es `parse`: cada tipo escribe la suya, y el macro la
/// cablea como única entrada (`TryFrom`, `FromStr` y `Deserialize` pasan todas
/// por ahí). El campo interno es privado y este módulo es el único sitio del
/// workspace donde se puede construir uno saltándose `parse`.
macro_rules! newtype_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            /// Nombre del tipo en los mensajes de error.
            pub const KIND: &'static str = $kind;

            /// El valor como `&str`. No hay `From<$name> for String` implícito
            /// aparte de [`Self::into_inner`]: convertir a texto es explícito.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume el newtype y devuelve el `String` de dentro.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                Self::parse(&s)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;
            fn try_from(s: &str) -> Result<Self, Self::Error> {
                Self::parse(s)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        /// Permite `HashMap<$name, _>::get(&str)` sin construir el newtype.
        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        /// A mano y no por `#[serde(transparent)]`/`into = "String"`: serializar
        /// no debe clonar, y así queda claro que el valor viaja como string
        /// pelado (el JSON es idéntico al que emitía el `String` que sustituye).
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// GameSlug
// ---------------------------------------------------------------------------

newtype_id!(
    /// Identificador estable de un juego en el catálogo: minúsculas ASCII,
    /// dígitos y guiones (`stardew-valley`, `2064-read-only-memories`).
    ///
    /// Es la forma que produce [`slugify`], que es quien mintó el catálogo
    /// entero, así que la puerta acepta exactamente eso. Normaliza espacios de
    /// borde y mayúsculas —dos slugs que sólo difieran en caja son el mismo
    /// juego, y tratarlos como distintos duplicaba filas— y rechaza todo lo
    /// demás.
    GameSlug,
    "game_slug"
);

impl GameSlug {
    /// El slug reservado `__other__` (ver [`OTHER_SLUG`]).
    pub fn other() -> Self {
        Self(OTHER_SLUG.to_string())
    }

    /// Marcador para un slug persistido **irrecuperable** (vacío, sólo
    /// símbolos) que aun así hay que emitir por el wire porque la fila existe.
    /// Es deliberadamente visible y no casa con ningún juego real, así que no
    /// puede correlacionar con nada: la alternativa —devolver un 500— dejaría al
    /// usuario sin listado entero por una fila mala.
    pub fn unknown() -> Self {
        Self("unknown-game".to_string())
    }

    /// ¿Es el slug sintético `__other__` en vez de un juego real?
    pub fn is_other(&self) -> bool {
        self.0 == OTHER_SLUG
    }

    /// **La puerta.** `serde`, `TryFrom` y `FromStr` pasan todos por aquí.
    ///
    /// Normaliza (trim + minúsculas ASCII) y luego exige la forma canónica.
    /// Normalizar en vez de rechazar es deliberado: es idempotente
    /// (`parse(parse(x)) == parse(x)`), no puede brickear nada, y elimina la
    /// clase de bug "el mismo juego con dos cajas distintas". Lo que se rechaza
    /// es lo que [`slugify`] no puede haber emitido nunca.
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(IdError::Empty { kind: Self::KIND });
        }
        if trimmed == OTHER_SLUG {
            return Ok(Self(OTHER_SLUG.to_string()));
        }
        let s = trimmed.to_ascii_lowercase();
        if s.chars().count() > MAX_SLUG_LEN {
            return Err(IdError::TooLong {
                kind: Self::KIND,
                len: s.chars().count(),
                max: MAX_SLUG_LEN,
            });
        }
        if let Some(ch) = s.chars().find(|c| !c.is_ascii_alphanumeric() && *c != '-') {
            return Err(IdError::BadChar {
                kind: Self::KIND,
                ch,
                raw: raw.to_string(),
            });
        }
        if !s.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            return Err(IdError::BadShape {
                kind: Self::KIND,
                expected: "empieza por letra o dígito",
                raw: raw.to_string(),
            });
        }
        if s.ends_with('-') {
            return Err(IdError::BadShape {
                kind: Self::KIND,
                expected: "no termina en guion",
                raw: raw.to_string(),
            });
        }
        if s.contains("--") {
            return Err(IdError::BadShape {
                kind: Self::KIND,
                expected: "sin guiones consecutivos",
                raw: raw.to_string(),
            });
        }
        Ok(Self(s))
    }

    /// **La puerta indulgente**, sólo para slugs que ya están en disco.
    ///
    /// - Válido → [`Repair::Clean`].
    /// - Recuperable → se re-deriva con [`slugify`] (el mismo algoritmo que lo
    ///   mintó) → [`Repair::Repaired`].
    /// - Degenerado ([`GENERIC_IDENTITY_TOKENS`]) o vacío → [`Repair::Quarantined`].
    ///
    /// Un slug degenerado **no se re-deriva**: `users` ya es un slug bien
    /// formado, el problema es que casa con todo. Fabricarle otro nombre sería
    /// inventar un juego; lo correcto es marcarlo y dejar que el llamante decida
    /// (hoy: conservarlo tal cual y excluirlo de la correlación, sin tocar la
    /// identidad que el server ya conoce).
    pub fn repair(raw: &str) -> Repair<Self> {
        let degenerate = |s: &str| {
            let tok = canon_token(s);
            tok.len() >= MIN_IDENTITY_TOKEN_LEN && GENERIC_IDENTITY_TOKENS.contains(&tok.as_str())
        };
        if let Ok(v) = Self::parse(raw) {
            if !v.is_other() && degenerate(v.as_str()) {
                return Repair::Quarantined {
                    raw: raw.to_string(),
                    reason: QuarantineReason::Degenerate,
                };
            }
            return Repair::Clean(v);
        }
        // Nada aprovechable: `slugify` devolvería su relleno `game`, que sería
        // inventarse un juego a partir de la nada.
        if !raw.chars().any(|c| c.is_ascii_alphanumeric()) {
            return Repair::Quarantined {
                raw: raw.to_string(),
                reason: QuarantineReason::Empty,
            };
        }
        match Self::parse(&slugify(raw)) {
            Ok(v) if !degenerate(v.as_str()) => Repair::Repaired {
                value: v,
                raw: raw.to_string(),
            },
            Ok(_) => Repair::Quarantined {
                raw: raw.to_string(),
                reason: QuarantineReason::Degenerate,
            },
            Err(_) => Repair::Quarantined {
                raw: raw.to_string(),
                reason: QuarantineReason::Unrecoverable,
            },
        }
    }
}

/// Slug lower-kebab canónico. **Fuente de verdad única** del algoritmo:
/// `hoard_manifest::ludusavi::slugify` delega aquí, y `data/convert-ludusavi.py`
/// es su gemelo byte-compatible. Divergir rompe en silencio el cruce
/// catálogo ↔ detección ↔ server.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true;
    for ch in name.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("game");
    }
    if out.len() > MAX_SLUG_LEN {
        out.truncate(MAX_SLUG_LEN);
        while out.ends_with('-') {
            out.pop();
        }
    }
    if !out.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        out.insert(0, 'g');
    }
    out
}

/// Token canónico de identidad de un juego/proceso: sólo alfanuméricos ASCII en
/// minúscula, sin separadores ni extensión. Unifica las tres formas en las que
/// el mismo juego aparece — slug (`victoria-3`), nombre visible (`Victoria 3`) y
/// ejecutable (`victoria3.exe` → `victoria3`) — en una sola clave comparable.
pub fn canon_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Username
// ---------------------------------------------------------------------------

newtype_id!(
    /// Nombre de usuario en un server self-hosted.
    ///
    /// La puerta es **deliberadamente permisiva**: el server nunca validó
    /// usernames (`hoard-admin user create` inserta lo que le den) y hay
    /// cuentas vivas con espacios o acentos. Una regla de estilo aquí no
    /// arreglaría ningún bug y sí dejaría a esos usuarios sin poder hacer
    /// `whoami` — o sea, sin poder entrar.
    ///
    /// Lo que sí veta es lo que nunca puede ser un usuario: vacío, sólo
    /// espacios, caracteres de control (rompen logs y cabeceras) y longitudes
    /// absurdas. El valor real del tipo no es la validación sino la
    /// **categoría**: un `Username` no puede acabar en un campo `GameSlug` por
    /// accidente, que es exactamente lo que produjo la correlación fantasma.
    Username,
    "username"
);

impl Username {
    /// Marcador para un username persistido irrecuperable (vacío). Mismo
    /// razonamiento que [`GameSlug::unknown`]: en self-hosted el username es
    /// dato de presentación —la autorización va por token → `user_id`—, así que
    /// degradar el nombre visible es infinitamente mejor que un 500 en
    /// `whoami`, que deja la cuenta sin poder abrir la app.
    pub fn unknown() -> Self {
        Self("unknown".to_string())
    }

    /// **La puerta.** Normaliza recortando espacios de borde; rechaza vacío,
    /// caracteres de control y > [`MAX_USERNAME_LEN`].
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(IdError::Empty { kind: Self::KIND });
        }
        if s.chars().count() > MAX_USERNAME_LEN {
            return Err(IdError::TooLong {
                kind: Self::KIND,
                len: s.chars().count(),
                max: MAX_USERNAME_LEN,
            });
        }
        if let Some(ch) = s.chars().find(|c| c.is_control()) {
            return Err(IdError::BadChar {
                kind: Self::KIND,
                ch,
                raw: raw.to_string(),
            });
        }
        Ok(Self(s.to_string()))
    }

    /// Puerta indulgente para usernames ya persistidos: quita los caracteres de
    /// control y trunca; sólo va a cuarentena si no queda nada.
    pub fn repair(raw: &str) -> Repair<Self> {
        if let Ok(v) = Self::parse(raw) {
            return Repair::Clean(v);
        }
        let cleaned: String = raw
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_USERNAME_LEN)
            .collect();
        match Self::parse(&cleaned) {
            Ok(value) => Repair::Repaired {
                value,
                raw: raw.to_string(),
            },
            Err(_) => Repair::Quarantined {
                raw: raw.to_string(),
                reason: QuarantineReason::Empty,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// SaveId
// ---------------------------------------------------------------------------

newtype_id!(
    /// Identificador de un save. Siempre un UUID v4 en forma canónica
    /// (36 caracteres, minúsculas, con guiones): lo mintan el server
    /// (`Uuid::new_v4().to_string()`), Postgres en cloud, o el cliente cuando
    /// crea la fila local antes del primer upload — las tres formas coinciden.
    ///
    /// La puerta exige la forma canónica exacta y **no normaliza**: un id es una
    /// clave de búsqueda contra el server, y "arreglarlo" (bajar la caja, quitar
    /// llaves) produciría un id que apunta a otro sitio del que se escribió.
    SaveId,
    "save_id"
);

impl SaveId {
    /// **La puerta.** UUID canónico en minúsculas con guiones.
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(IdError::Empty { kind: Self::KIND });
        }
        if !is_canonical_uuid(s) {
            return Err(IdError::BadShape {
                kind: Self::KIND,
                expected: "UUID canónico en minúsculas (8-4-4-4-12)",
                raw: raw.to_string(),
            });
        }
        Ok(Self(s.to_string()))
    }

    /// Puerta indulgente: sólo recupera la caja (un UUID en mayúsculas sigue
    /// apuntando al mismo save). Cualquier otra cosa es irrecuperable — no se
    /// puede inventar un identificador que el server nunca minó.
    pub fn repair(raw: &str) -> Repair<Self> {
        if let Ok(v) = Self::parse(raw) {
            return Repair::Clean(v);
        }
        let lowered = raw.trim().to_ascii_lowercase();
        match Self::parse(&lowered) {
            Ok(value) => Repair::Repaired {
                value,
                raw: raw.to_string(),
            },
            Err(_) => Repair::Quarantined {
                raw: raw.to_string(),
                reason: QuarantineReason::Unrecoverable,
            },
        }
    }
}

/// `8-4-4-4-12` en hex minúscula. Se comprueba a mano en vez de con
/// `uuid::Uuid::parse_str` porque esa función acepta además las formas simple,
/// con llaves y URN, y aceptarlas aquí dejaría entrar dos strings distintos para
/// el mismo save (y por tanto dos claves distintas en los mapas de estado).
fn is_canonical_uuid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for len in GROUPS {
        match parts.next() {
            Some(p) if p.len() == len && p.bytes().all(is_lower_hex) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

fn is_lower_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

// ---------------------------------------------------------------------------
// Sha256
// ---------------------------------------------------------------------------

newtype_id!(
    /// Digest SHA-256 en hex: 64 caracteres, minúsculas. Es lo que emite
    /// `hex::encode` en las dos puntas (cliente y server), así que la forma
    /// canónica es la única que existe en la práctica.
    ///
    /// Ojo: un campo `sha256` **vacío** en el wire no es un hash malformado sino
    /// "no aplica" (las versiones content-addressed no tienen digest de archivo
    /// entero). Eso se modela con `Option<Sha256>` y un deserializador que trata
    /// `""` como `None`, no relajando esta puerta.
    Sha256,
    "sha256"
);

impl Sha256 {
    /// Longitud en hex de un digest SHA-256.
    pub const HEX_LEN: usize = 64;

    /// **La puerta.** 64 hex; normaliza la caja porque un digest en mayúsculas
    /// es el mismo digest (a diferencia de un id, aquí el valor *es* el
    /// contenido, no una clave ajena).
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        parse_hex(raw, Self::HEX_LEN, Self::KIND).map(Self)
    }

    /// Puerta indulgente: normaliza la caja, o cuarentena.
    pub fn repair(raw: &str) -> Repair<Self> {
        repair_hex(raw, Self::parse)
    }
}

// ---------------------------------------------------------------------------
// MachineId
// ---------------------------------------------------------------------------

newtype_id!(
    /// Huella estable de una máquina: SHA-256 hex de `/etc/machine-id` (o el
    /// equivalente por SO) más el hostname. Misma forma que [`Sha256`] pero
    /// **tipo distinto** a propósito: una huella de máquina y el digest de un
    /// fichero no son intercambiables, y el compilador debe decirlo.
    MachineId,
    "machine_id"
);

impl MachineId {
    /// Longitud en hex de una huella de máquina.
    pub const HEX_LEN: usize = 64;

    /// **La puerta.** 64 hex, caja normalizada.
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        parse_hex(raw, Self::HEX_LEN, Self::KIND).map(Self)
    }

    /// Puerta indulgente: normaliza la caja, o cuarentena.
    pub fn repair(raw: &str) -> Repair<Self> {
        repair_hex(raw, Self::parse)
    }
}

fn parse_hex(raw: &str, len: usize, kind: &'static str) -> Result<String, IdError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(IdError::Empty { kind });
    }
    let s = s.to_ascii_lowercase();
    if s.chars().count() != len {
        return Err(IdError::BadShape {
            kind,
            expected: "64 caracteres hex",
            raw: raw.to_string(),
        });
    }
    if let Some(ch) = s.chars().find(|c| !c.is_ascii_hexdigit()) {
        return Err(IdError::BadChar {
            kind,
            ch,
            raw: raw.to_string(),
        });
    }
    Ok(s)
}

/// `parse` de los tipos hex normaliza la caja, así que "estaba limpio" es "el
/// crudo ya era idéntico al normalizado".
fn repair_hex<T: AsRef<str>>(raw: &str, parse: fn(&str) -> Result<T, IdError>) -> Repair<T> {
    match parse(raw) {
        Ok(value) if value.as_ref() == raw => Repair::Clean(value),
        Ok(value) => Repair::Repaired {
            value,
            raw: raw.to_string(),
        },
        Err(_) => Repair::Quarantined {
            raw: raw.to_string(),
            reason: QuarantineReason::Unrecoverable,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- La puerta de serde -------------------------------------------

    /// El punto entero del slice: **no se puede construir un newtype
    /// deserializando**. Si alguien cambia `try_from` por un derive a pelo, este
    /// test cae.
    #[test]
    fn deserialize_goes_through_the_gate() {
        assert!(serde_json::from_str::<GameSlug>(r#""stardew-valley""#).is_ok());
        for poison in [
            r#""GSE Saves""#,
            r#""""#,
            r#""   ""#,
            r#""stardew--valley""#,
            r#""-leading""#,
            r#""trailing-""#,
            r#""ünïcode""#,
        ] {
            assert!(
                serde_json::from_str::<GameSlug>(poison).is_err(),
                "{poison} debería rebotar en la puerta"
            );
        }
        assert!(serde_json::from_str::<Username>(r#""""#).is_err());
        assert!(serde_json::from_str::<SaveId>(r#""not-a-uuid""#).is_err());
        assert!(serde_json::from_str::<Sha256>(r#""deadbeef""#).is_err());
        assert!(serde_json::from_str::<MachineId>(r#""zz""#).is_err());
    }

    /// El JSON de un newtype es el mismo string pelado que emitía el `String`
    /// que sustituye: cambiar el tipo no cambió un byte del wire.
    #[test]
    fn serializes_as_a_bare_string() {
        let slug = GameSlug::parse("stardew-valley").unwrap();
        assert_eq!(serde_json::to_string(&slug).unwrap(), r#""stardew-valley""#);
        let id = SaveId::parse("3f2504e0-4f89-41d3-9a0c-0305e82c3301").unwrap();
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            r#""3f2504e0-4f89-41d3-9a0c-0305e82c3301""#
        );
    }

    /// La normalización es idempotente: `parse(parse(x)) == parse(x)`. Sin esto,
    /// un round-trip por disco podría mover el valor.
    #[test]
    fn parse_is_idempotent() {
        for raw in ["  Stardew-Valley ", "STARDEW-VALLEY", "stardew-valley"] {
            let once = GameSlug::parse(raw).unwrap();
            let twice = GameSlug::parse(once.as_str()).unwrap();
            assert_eq!(once, twice);
            assert_eq!(once.as_str(), "stardew-valley");
        }
        let sha = "A".repeat(64);
        let once = Sha256::parse(&sha).unwrap();
        assert_eq!(once, Sha256::parse(once.as_str()).unwrap());
        assert_eq!(once.as_str(), "a".repeat(64));
    }

    // ---- Slugs ---------------------------------------------------------

    #[test]
    fn slug_accepts_the_shapes_slugify_emits() {
        for ok in [
            "stardew-valley",
            "2064-read-only-memories",
            "doom",
            "a",
            OTHER_SLUG,
        ] {
            assert!(GameSlug::parse(ok).is_ok(), "{ok} debería pasar");
        }
        assert!(GameSlug::parse(&"a".repeat(MAX_SLUG_LEN)).is_ok());
        assert!(GameSlug::parse(&"a".repeat(MAX_SLUG_LEN + 1)).is_err());
    }

    /// Todo lo que emite `slugify` pasa la puerta. Es el contrato que hace que
    /// `repair` pueda re-derivar sin quedarse en bucle.
    #[test]
    fn slugify_output_always_parses() {
        for raw in [
            "Stardew Valley",
            "GSE Saves",
            "  ...  ",
            "2064: Read Only Memories",
            "ünïcode gäme",
            "!!!",
            "a",
            &"muy largo ".repeat(30),
        ] {
            let s = slugify(raw);
            assert!(
                GameSlug::parse(&s).is_ok(),
                "slugify({raw:?}) = {s:?} no pasa la puerta"
            );
        }
    }

    // ---- Reparación / cuarentena ---------------------------------------

    #[test]
    fn repair_rederives_a_recoverable_slug() {
        match GameSlug::repair("GSE Saves") {
            Repair::Repaired { value, raw } => {
                assert_eq!(value.as_str(), "gse-saves");
                assert_eq!(raw, "GSE Saves");
            }
            other => panic!("esperaba Repaired, salió {other:?}"),
        }
    }

    /// El veneno de julio 2026: un slug que es un token de fontanería (el caso
    /// real fue el username de Windows, que el shell añade a la lista). No se
    /// re-deriva —ya está bien formado— sino que se marca.
    #[test]
    fn repair_quarantines_a_degenerate_slug() {
        for poison in ["users", "appdata", "steamapps", "savedgames"] {
            match GameSlug::repair(poison) {
                Repair::Quarantined { reason, raw } => {
                    assert_eq!(reason, QuarantineReason::Degenerate);
                    assert_eq!(raw, poison);
                }
                other => panic!("{poison} debería ir a cuarentena, salió {other:?}"),
            }
        }
    }

    /// Sin nada alfanumérico no hay slug que derivar: `slugify` devolvería su
    /// relleno `game` y estaríamos inventando un juego.
    #[test]
    fn repair_quarantines_instead_of_fabricating() {
        for empty in ["", "   ", "---", "!!!"] {
            assert!(
                GameSlug::repair(empty).is_quarantined(),
                "{empty:?} debería ir a cuarentena"
            );
        }
    }

    #[test]
    fn repair_leaves_clean_values_alone() {
        assert!(GameSlug::repair("stardew-valley").is_clean());
        assert!(GameSlug::repair(OTHER_SLUG).is_clean());
        assert!(Username::repair("jacka").is_clean());
        assert!(SaveId::repair("3f2504e0-4f89-41d3-9a0c-0305e82c3301").is_clean());
        assert!(Sha256::repair(&"ab".repeat(32)).is_clean());
    }

    #[test]
    fn repair_recovers_uuid_case_but_not_garbage() {
        match SaveId::repair("3F2504E0-4F89-41D3-9A0C-0305E82C3301") {
            Repair::Repaired { value, .. } => {
                assert_eq!(value.as_str(), "3f2504e0-4f89-41d3-9a0c-0305e82c3301")
            }
            other => panic!("esperaba Repaired, salió {other:?}"),
        }
        assert!(SaveId::repair("save-a").is_quarantined());
    }

    // ---- Categoría -----------------------------------------------------

    /// La otra mitad del valor del slice: `slug == username` no compila. Este
    /// test documenta la intención; el compilador es quien la hace cumplir (una
    /// comparación directa entre los dos tipos sería un error de tipos).
    #[test]
    fn slug_and_username_are_different_categories() {
        let slug = GameSlug::parse("jacka").unwrap();
        let user = Username::parse("jacka").unwrap();
        // Comparar exige bajar explícitamente a `str` — y ese descenso es
        // justo el sitio donde un humano se pregunta "¿por qué comparo un
        // usuario con un juego?".
        assert_eq!(slug.as_str(), user.as_str());
    }

    // ---- Formas de identidad -------------------------------------------

    #[test]
    fn uuid_gate_is_strict_about_shape() {
        assert!(SaveId::parse("3f2504e0-4f89-41d3-9a0c-0305e82c3301").is_ok());
        for bad in [
            "3f2504e04f8941d39a0c0305e82c3301",              // simple
            "{3f2504e0-4f89-41d3-9a0c-0305e82c3301}",        // con llaves
            "3f2504e0-4f89-41d3-9a0c-0305e82c330",           // corto
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301-",         // sobra
            "3f2504e0-4f89-41d3-9a0c-0305e82c330g",          // no hex
            "urn:uuid:3f2504e0-4f89-41d3-9a0c-0305e82c3301", // URN
        ] {
            assert!(SaveId::parse(bad).is_err(), "{bad} debería rebotar");
        }
    }

    #[test]
    fn hex_gates_reject_wrong_length_and_charset() {
        assert!(Sha256::parse(&"a".repeat(63)).is_err());
        assert!(Sha256::parse(&"a".repeat(65)).is_err());
        assert!(Sha256::parse(&"g".repeat(64)).is_err());
        assert!(MachineId::parse(&"0".repeat(64)).is_ok());
    }

    #[test]
    fn username_is_permissive_but_rejects_the_impossible() {
        for ok in ["jacka", "John Doe", "señor-ñ", "a"] {
            assert!(Username::parse(ok).is_ok(), "{ok} debería pasar");
        }
        assert!(Username::parse("").is_err());
        assert!(Username::parse("   ").is_err());
        assert!(Username::parse("na\u{0}me").is_err());
        assert!(Username::parse(&"a".repeat(MAX_USERNAME_LEN + 1)).is_err());
    }

    #[test]
    fn borrow_lets_maps_be_queried_by_str() {
        use std::collections::HashMap;
        let mut m: HashMap<GameSlug, u32> = HashMap::new();
        m.insert(GameSlug::parse("doom").unwrap(), 1);
        assert_eq!(m.get("doom"), Some(&1));
    }

    // ---- slugify (portados de hoard-manifest, que ahora delega aquí) ----

    #[test]
    fn slugify_examples() {
        assert_eq!(slugify("Stardew Valley"), "stardew-valley");
        assert_eq!(slugify("DOOM (2016)"), "doom-2016");
        assert_eq!(slugify("  spaces  "), "spaces");
        assert_eq!(slugify(""), "game");
        assert_eq!(slugify("!!!"), "game");
        assert_eq!(
            slugify("2064: Read Only Memories"),
            "2064-read-only-memories"
        );
    }

    #[test]
    fn canon_token_strips_everything_but_alphanumerics() {
        assert_eq!(canon_token("Victoria 3"), "victoria3");
        assert_eq!(canon_token("victoria-3"), "victoria3");
        assert_eq!(canon_token("victoria3.exe"), "victoria3exe");
    }
}
