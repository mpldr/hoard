//! Persistent storage for the desktop client's session.
//!
//! Two pieces are kept on disk:
//!
//! * The bearer token, which is sensitive and should live in the OS keychain
//!   when one is available (Secret Service on Linux, Credential Manager on
//!   Windows, Keychain on macOS — all surfaced by the `keyring` crate).
//! * The server URL and a cached copy of the last-seen user info, which are
//!   not sensitive and live in a TOML file at `<config>/desktop/session.toml`
//!   so we can show the username without hitting the network on startup. These
//!   are also mirrored into the keychain blob, so a lost or unreadable cache
//!   file no longer signs the user out.
//!
//! When the OS keychain is unavailable (e.g. headless Linux without
//! libsecret) the token falls back into the same TOML file, which is created
//! with `0600` permissions on Unix.
//!
//! The desktop app uses a separate file from `hoard-cli`'s `config.toml` so
//! that running the CLI does not stomp the GUI's session and vice versa.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::api::Whoami;
use crate::config::CliConfig;

const KEYRING_SERVICE: &str = "hoard-desktop";
const KEYRING_USER: &str = "default";

/// In-memory view of the desktop client's saved session.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub url: String,
    pub token: String,
    pub user: Option<UserSection>,
}

/// Where the token actually ended up after `save`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStorage {
    /// Stored via the OS secret service (preferred).
    Keyring,
    /// Stored in the TOML file at 0600 because the keyring was unavailable.
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Session {
    #[serde(default)]
    server: ServerSection,
    #[serde(default)]
    user: Option<UserSection>,
    /// Filesystem fallback when the OS keyring is unavailable. In normal
    /// operation this is `None` and the token lives in the keyring.
    #[serde(default)]
    auth: Option<AuthSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ServerSection {
    #[serde(default)]
    url: String,
}

/// Subset of `/v1/auth/whoami` we cache locally so the dashboard can show the
/// username without an extra round-trip on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSection {
    pub user_id: String,
    pub username: String,
    pub is_admin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthSection {
    token: String,
}

/// What we stash in the OS keychain. Historically this was the bare token
/// string; it's now a small TOML document so the keychain alone can restore a
/// session (token + server URL + cached user) even when the on-disk cache is
/// missing or unreadable. Reads tolerate the legacy bare-token form.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyringBlob {
    #[serde(default)]
    token: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    user: Option<UserSection>,
}

impl From<Whoami> for UserSection {
    fn from(w: Whoami) -> Self {
        Self {
            user_id: w.user_id,
            username: w.username.into_inner(),
            is_admin: w.is_admin,
        }
    }
}

/// Resolve the on-disk path of the session metadata file.
pub fn session_path() -> Result<PathBuf> {
    let dirs = CliConfig::project_dirs()?;
    Ok(dirs.config_dir().join("desktop").join("session.toml"))
}

/// Persist credentials. Token goes to the OS keychain when available, with a
/// transparent file fallback otherwise.
pub fn save(creds: &Credentials) -> Result<TokenStorage> {
    let session = Session {
        server: ServerSection {
            url: creds.url.clone(),
        },
        user: creds.user.clone(),
        auth: None,
    };
    write_session(&session)?;

    match try_keyring_set(creds) {
        Ok(()) => {
            // Belt and braces: if the file had a stale token from a previous
            // fallback run, scrub it now that the keyring took over.
            scrub_file_token().ok();
            Ok(TokenStorage::Keyring)
        }
        Err(_) => {
            let mut session = read_session()?.unwrap_or_default();
            session.auth = Some(AuthSection {
                token: creds.token.clone(),
            });
            write_session(&session)?;
            Ok(TokenStorage::File)
        }
    }
}

/// Load credentials if any are stored. Returns `Ok(None)` when no session is
/// present yet (e.g. fresh install) — that is not an error.
pub fn load() -> Result<Option<Credentials>> {
    match read_session() {
        // Normal path: the on-disk cache is readable and has a server URL. The
        // token comes from the keychain, falling back to the file copy.
        Ok(Some(session)) if !session.server.url.is_empty() => {
            let token = match try_keyring_get() {
                Ok(Some(blob)) if !blob.token.is_empty() => Some(blob.token),
                _ => session.auth.as_ref().map(|a| a.token.clone()),
            };
            match token.filter(|t| !t.is_empty()) {
                Some(token) => Ok(Some(Credentials {
                    url: session.server.url,
                    token,
                    user: session.user,
                })),
                None => Ok(None),
            }
        }
        // Cache absent, empty, or unreadable (e.g. an ACL a previous Windows
        // build clamped down and `read_session` couldn't repair). Don't drop
        // the session over a disk hiccup: the keychain now carries the URL too,
        // so it can restore everything on its own.
        read => {
            if let Ok(Some(blob)) = try_keyring_get() {
                if !blob.token.is_empty() && !blob.url.is_empty() {
                    let creds = Credentials {
                        url: blob.url,
                        token: blob.token,
                        user: blob.user,
                    };
                    // Best-effort: rewrite the cache so it's healthy again, with
                    // sane inherited permissions.
                    let _ = write_session(&Session {
                        server: ServerSection {
                            url: creds.url.clone(),
                        },
                        user: creds.user.clone(),
                        auth: None,
                    });
                    return Ok(Some(creds));
                }
            }
            // Nothing recoverable from the keychain. Surface a real read error;
            // treat "absent/empty" as simply not-logged-in.
            match read {
                Err(e) => Err(e),
                _ => Ok(None),
            }
        }
    }
}

/// Wipe stored credentials. Idempotent — clearing twice is fine.
pub fn clear() -> Result<()> {
    // Best-effort: errors here mean the entry didn't exist, which is fine.
    let _ = try_keyring_delete();
    let path = session_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Cheap shape check on a token string — `hoard_v1_` followed by 64 lowercase
/// hex characters. Avoids round-tripping obviously-wrong input through the
/// network.
pub fn is_valid_token(token: &str) -> bool {
    const PREFIX: &str = "hoard_v1_";
    if token.len() != PREFIX.len() + 64 {
        return false;
    }
    if !token.starts_with(PREFIX) {
        return false;
    }
    token[PREFIX.len()..]
        .chars()
        .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

// ---- internals ---------------------------------------------------------

fn read_session() -> Result<Option<Session>> {
    let path = session_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // A session file written by an older build can carry a broken ACL
        // (icacls /inheritance:r granted to a principal that doesn't resolve to
        // this process's identity) → the file exists but reads back "access
        // denied". The owner can always rewrite the DACL, so reset inherited
        // permissions and retry once before giving up.
        #[cfg(windows)]
        Err(_) if reset_acl_windows(&path) => std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} after ACL reset", path.display()))?,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let s: Session =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(s))
}

fn write_session(s: &Session) -> Result<()> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(s).context("serializing session")?;

    // Atomic write: a plain truncate+write leaves the session file half-written
    // if the process dies mid-write, and a truncated TOML fails to parse on next
    // launch → spurious sign-out. Write to a sibling temp file then rename over the
    // target (atomic on the same filesystem), so a reader only ever sees the old or
    // the new file. Solves Windows issues with inherited ACLs on partially-written
    // files and sync-folder interference (OneDrive, Dropbox).
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }

    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;

    Ok(())
}

/// Repair a session file a previous build's ACL-hardening left unreadable.
///
/// Older versions ran `icacls /inheritance:r /grant:r %USERNAME%:F` on the
/// file. When `%USERNAME%` didn't resolve to the process's actual identity
/// (Microsoft accounts, a same-named local account, roaming/redirected
/// profiles) the file ended up owned by the user but granting access to the
/// wrong principal, so a later launch reads it back as "access denied". The
/// owner keeps the implicit right to rewrite the DACL, so `icacls /reset`
/// restores the inherited, per-user permissions and the retry read then
/// succeeds. Best-effort — returns whether the reset ran cleanly so the caller
/// only retries the read when it's worth it.
#[cfg(windows)]
fn reset_acl_windows(path: &std::path::Path) -> bool {
    match std::process::Command::new("icacls")
        .arg(path)
        .arg("/reset")
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(path = %path.display(), "credentials: reset stale ACL on session file");
            true
        }
        Ok(out) => {
            tracing::warn!(
                status = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "credentials: icacls /reset did not repair the session file",
            );
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "credentials: failed to run icacls /reset");
            false
        }
    }
}

fn scrub_file_token() -> Result<()> {
    let Some(mut session) = read_session()? else {
        return Ok(());
    };
    if session.auth.is_some() {
        session.auth = None;
        write_session(&session)?;
    }
    Ok(())
}

fn try_keyring_set(creds: &Credentials) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    // Store the whole session (token + URL + cached user) as TOML so the
    // keychain can restore it without the on-disk cache. See `KeyringBlob`.
    let blob = toml::to_string(&KeyringBlob {
        token: creds.token.clone(),
        url: creds.url.clone(),
        user: creds.user.clone(),
    })
    .context("serializing keychain blob")?;
    entry.set_password(&blob)?;
    Ok(())
}

fn try_keyring_get() -> Result<Option<KeyringBlob>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    match entry.get_password() {
        Ok(raw) => Ok(Some(parse_keyring_blob(&raw))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Parse a keychain payload, tolerating the legacy format where the entry was
/// just the bare token string (no TOML wrapper).
fn parse_keyring_blob(raw: &str) -> KeyringBlob {
    match toml::from_str::<KeyringBlob>(raw) {
        Ok(blob) if !blob.token.is_empty() => blob,
        _ => KeyringBlob {
            token: raw.trim().to_string(),
            url: String::new(),
            user: None,
        },
    }
}

fn try_keyring_delete() -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation_accepts_canonical() {
        let good = format!("hoard_v1_{}", "a".repeat(64));
        assert!(is_valid_token(&good));
    }

    #[test]
    fn token_validation_rejects_wrong_prefix() {
        let bad = format!("hoard_v2_{}", "a".repeat(64));
        assert!(!is_valid_token(&bad));
    }

    #[test]
    fn token_validation_rejects_short() {
        assert!(!is_valid_token("hoard_v1_abcd"));
    }

    #[test]
    fn token_validation_rejects_uppercase_hex() {
        let bad = format!("hoard_v1_{}", "A".repeat(64));
        assert!(!is_valid_token(&bad));
    }

    #[test]
    fn token_validation_rejects_non_hex() {
        let bad = format!("hoard_v1_{}", "z".repeat(64));
        assert!(!is_valid_token(&bad));
    }
}
