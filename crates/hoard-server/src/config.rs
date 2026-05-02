use anyhow::{Context, Result};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub retention: RetentionConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub public_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub max_snapshot_size_mb: u64,
    pub upload_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    pub token_lifetime_days: u64,
    pub allow_registration: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetentionConfig {
    pub trash_retention_days: u64,
    pub tmp_cleanup_hours: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            anyhow::bail!(
                "Config file not found at {}. \
                 Create it from the example: cp deploy/config.toml.example {}",
                path.display(),
                path.display()
            );
        }

        let config: Config = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("HOARD__").split("__"))
            .extract()
            .with_context(|| format!("Failed to parse config from {}", path.display()))?;

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.server.port == 0 {
            anyhow::bail!("server.port must be > 0");
        }

        let valid_levels = ["trace", "debug", "info", "warn", "error"];
        if !valid_levels.contains(&self.logging.level.as_str()) {
            anyhow::bail!(
                "logging.level must be one of {:?}, got {:?}",
                valid_levels,
                self.logging.level
            );
        }

        if !self.storage.data_dir.exists() {
            anyhow::bail!(
                "storage.data_dir {:?} does not exist. Create it with: \
                 mkdir -p {}",
                self.storage.data_dir,
                self.storage.data_dir.display()
            );
        }

        // Check write permission by attempting to create a temp file
        let probe = self.storage.data_dir.join(".hoard_write_probe");
        std::fs::write(&probe, b"").with_context(|| {
            format!(
                "storage.data_dir {:?} is not writable",
                self.storage.data_dir
            )
        })?;
        std::fs::remove_file(&probe).ok();

        Ok(())
    }
}
