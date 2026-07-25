//! `hoard login` / `logout` / `whoami`.
//!
//! Two session kinds, never both effectively at once (Cloud wins over self-host
//! when resolving — see `session::resolve`):
//! - **Cloud** (default, bare `hoard login`): Supabase **without a browser** —
//!   email + password, or an email OTP code if you leave the password blank.
//! - **self-host** (`hoard login --token <token>`): the server's bearer token.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use hoard_agent::api::ApiClient;
use hoard_agent::cloud_auth;
use hoard_agent::config::CliConfig;
use hoard_agent::state;

use crate::commands::link;

pub async fn login(token: Option<String>, server: Option<String>, force_email: bool) -> Result<()> {
    // Explicit flags skip the menu and stay scriptable.
    if let Some(token) = token {
        return login_selfhost(token, server).await;
    }
    if force_email {
        return login_cloud_email(&cloud_auth::cloud_base_url()).await;
    }

    // No flags: ask what to sign in to — but only if there's a terminal to ask
    // on. Piped/CI/service contexts can't answer a prompt, so default to Cloud
    // (its device/email flow doesn't depend on an interactive menu).
    if !io::stdin().is_terminal() {
        return login_cloud(false).await;
    }
    match choose_kind()? {
        Kind::Cloud => login_cloud(false).await,
        Kind::SelfHost => login_selfhost_interactive().await,
    }
}

enum Kind {
    Cloud,
    SelfHost,
}

/// Interactive top-level choice: Hoard Cloud or a self-hosted server.
fn choose_kind() -> Result<Kind> {
    println!("Where do you want to sign in?");
    println!("   1) Hoard Cloud          (managed, no browser)");
    println!("   2) Self-hosted server   (your own Hoard server + access token)");
    println!();
    loop {
        match prompt("Pick [1-2, default 1]: ")?.as_str() {
            "" | "1" => return Ok(Kind::Cloud),
            "2" => return Ok(Kind::SelfHost),
            _ => println!("Type 1 or 2."),
        }
    }
}

/// self-host: validates the bearer against the server and saves it in the config.
/// `server` overrides the configured URL (and is persisted) when given.
async fn login_selfhost(token: String, server: Option<String>) -> Result<()> {
    let path = CliConfig::default_path()?;
    let mut cfg = CliConfig::load(&path)?;
    if let Some(url) = server {
        cfg.server.url = url.trim().to_string();
    }

    let client = ApiClient::new(cfg.server.url.clone(), token.clone())?;
    let me = client.whoami().await?;

    cfg.auth.token = Some(token);
    cfg.save(&path)?;
    println!(
        "connected to self-host ({}) as {} (admin: {}) — saved to {}",
        cfg.server.url,
        me.username,
        me.is_admin,
        path.display()
    );
    Ok(())
}

/// self-host from the interactive menu: asks for the server URL (defaulting to
/// whatever's in the config) and the access token, then validates and saves.
async fn login_selfhost_interactive() -> Result<()> {
    let (cfg, _) = CliConfig::load_default()?;
    let default_url = cfg.server.url.clone();

    let url = prompt(&format!("Server URL [{default_url}]: "))?;
    let url = if url.is_empty() { default_url } else { url };

    let token = prompt("Access token (hoard_v1_…): ")?;
    if token.is_empty() {
        anyhow::bail!("no access token given");
    }
    login_selfhost(token, Some(url)).await
}

/// Cloud without a browser. Main path: **mobile pairing** — the CLI shows a URL
/// and a code, you approve it from your phone (already signed in on the web) and
/// the server mints the session. If the server doesn't have it configured, it falls
/// back to the email+password / OTP-code path. The session is stored where the
/// desktop keeps it (keyring + `cloud.toml`) to share the login.
async fn login_cloud(force_email: bool) -> Result<()> {
    let base = cloud_auth::cloud_base_url();
    println!("Sign in to Hoard Cloud ({base})");

    // `--email`: skip phone pairing (which just re-approves whatever account the
    // phone browser already has) and let the user type the address to sign in as.
    if force_email {
        return login_cloud_email(&base).await;
    }

    match cloud_auth::device_start(local_hostname().as_deref()).await? {
        // Server with the feature: mobile pairing.
        Some(start) => return login_cloud_device(&base, start).await,
        // Server without the feature (older version or Cloud without
        // service_role): fall back to email. A real network failure still
        // propagates (the `?` above).
        None => println!("(mobile pairing unavailable on the server; using email)"),
    }
    login_cloud_email(&base).await
}

/// Mobile pairing: shows a URL + code and polls until it's approved from the
/// phone.
async fn login_cloud_device(base: &str, start: cloud_auth::DeviceStart) -> Result<()> {
    let link = start
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&start.verification_uri);
    println!();
    println!("  Open this on your phone (signed in to Hoard Cloud):");
    println!("    {link}");
    println!("  and confirm the code:  {}", start.user_code);
    println!();
    print!("Waiting for approval");
    io::stdout().flush().ok();

    let interval = Duration::from_secs(start.interval_secs.max(1));
    let ttl = if start.expires_in_secs == 0 {
        600
    } else {
        start.expires_in_secs
    };
    let deadline = Instant::now() + Duration::from_secs(ttl);

    let tokens = loop {
        if Instant::now() >= deadline {
            println!();
            anyhow::bail!("the code expired before it was approved. Retry with `hoard login`.");
        }
        tokio::time::sleep(interval).await;
        match cloud_auth::device_poll(&start.device_code).await? {
            cloud_auth::DeviceStatus::Approved(t) => {
                println!("\nApproved.");
                break t;
            }
            cloud_auth::DeviceStatus::Denied => {
                println!();
                anyhow::bail!("pairing rejected from the phone.");
            }
            cloud_auth::DeviceStatus::Expired => {
                println!();
                anyhow::bail!("the code expired. Retry with `hoard login`.");
            }
            cloud_auth::DeviceStatus::Pending => {
                print!(".");
                io::stdout().flush().ok();
            }
        }
    };

    finish_cloud_login(base, &tokens).await
}

/// Browserless fallback: email + password, or an email OTP code if you leave the
/// password blank.
async fn login_cloud_email(base: &str) -> Result<()> {
    let email = prompt("Email: ")?;
    if email.is_empty() {
        anyhow::bail!("empty email");
    }
    let password = prompt_hidden("Password (blank = I'll email you a code): ")?;

    let tokens = if password.trim().is_empty() {
        cloud_auth::otp_start(&email).await?;
        println!(
            "Sent a code to {email}. Open it wherever you read your mail (phone, etc.) \
             and type it here."
        );
        let code = prompt("Code: ")?;
        cloud_auth::otp_verify(&email, &code).await?
    } else {
        cloud_auth::login_password(&email, &password).await?
    };

    finish_cloud_login(base, &tokens).await
}

/// Persists the session and sets the Cloud context. Shared by both paths.
///
/// El login es una de las dos únicas escrituras del par de tokens que no hace el
/// servicio (la otra es el logout, que lo borra): acuñar una sesión no es rotarla.
/// Lo que sí es del servicio es lo que venga después, así que se le avisa: su
/// motor está hablando con la cuenta que acaba de dejar de valer.
async fn finish_cloud_login(base: &str, tokens: &cloud_auth::Tokens) -> Result<()> {
    // Wipe any previous session first. `store_tokens` is read-modify-write and
    // would otherwise keep the old `user` snapshot / server_url, so signing in as
    // a different account would leave the old identity lingering on disk.
    let _ = cloud_auth::clear_session();
    cloud_auth::store_tokens(tokens, base)?;
    let me = cloud_auth::fetch_me(base, &tokens.access).await?;
    state::set_active_context(Some(state::cloud_context(&me.user_id)));
    link::notify_session_changed().await;
    println!(
        "connected to Hoard Cloud as {} · plan {}",
        me.email, me.plan
    );
    Ok(())
}

/// Machine name, best-effort, so the phone shows what it's authorizing.
fn local_hostname() -> Option<String> {
    for key in ["HOSTNAME", "COMPUTERNAME", "HOST"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn logout() -> Result<()> {
    let had_cloud = cloud_auth::load_session()?.is_some();
    if had_cloud {
        cloud_auth::clear_session()?;
    }

    let path = CliConfig::default_path()?;
    let mut cfg = CliConfig::load(&path)?;
    let had_selfhost = cfg.auth.token.is_some();
    if had_selfhost {
        cfg.auth.token = None;
        cfg.save(&path)?;
    }

    if had_cloud || had_selfhost {
        // Las credenciales ya no están: que el servicio tire el motor y resuelva
        // de nuevo, en vez de seguir con un token que acabamos de borrar.
        link::notify_session_changed().await;
    }

    match (had_cloud, had_selfhost) {
        (false, false) => println!("no active session"),
        (true, false) => println!("Cloud session closed"),
        (false, true) => println!("self-host session closed"),
        (true, true) => println!("Cloud and self-host sessions closed"),
    }
    Ok(())
}

pub async fn whoami() -> Result<()> {
    if cloud_auth::load_session()?.is_some() {
        // Token prestado por el servicio, no refrescado aquí: dos procesos
        // rotando el mismo refresh token es la reuse-detection que revoca la
        // sesión entera (ADR 0021, Parte A).
        let active = link::resolve_session().await?;
        let Some(sess) = active.cloud else {
            bail!("the stored Cloud session is unreadable — run `hoard login`");
        };
        let me = cloud_auth::fetch_me(&sess.server_url, &sess.access).await?;
        println!(
            "Hoard Cloud\n  email:   {}\n  user_id: {}\n  plan:    {}\n  usage:   {} / {}",
            me.email,
            me.user_id,
            me.plan,
            fmt_bytes(me.storage_used_bytes),
            fmt_bytes(me.storage_limit_bytes),
        );
        return Ok(());
    }

    let (cfg, _) = CliConfig::load_default()?;
    let token = cfg
        .require_token()
        .context("no session. Sign in with `hoard login` or `hoard login --token <token>`.")?;
    let client = ApiClient::new(cfg.server.url.clone(), token)?;
    let me = client.whoami().await?;
    println!(
        "self-host ({})\n  username: {}\n  user_id:  {}\n  admin:    {}",
        cfg.server.url, me.username, me.user_id, me.is_admin
    );
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn prompt_hidden(label: &str) -> Result<String> {
    // Without a tty (pipe, CI) rpassword reads from stdin without hiding; fine.
    Ok(rpassword::prompt_password(label)?)
}

fn fmt_bytes(b: i64) -> String {
    if b <= 0 {
        return "0".to_string();
    }
    let b = b as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.2}G", b / GB)
    } else if b >= MB {
        format!("{:.2}M", b / MB)
    } else if b >= KB {
        format!("{:.2}K", b / KB)
    } else {
        format!("{}B", b as u64)
    }
}
