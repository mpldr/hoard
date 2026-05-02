use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
    pub is_admin: bool,
    pub storage_used_bytes: i64,
    pub storage_quota_bytes: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub slug: String,
    pub display_name: String,
    pub engine: Option<String>,
    pub save_paths_json: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Save {
    pub id: Uuid,
    pub user_id: Uuid,
    pub game_slug: String,
    pub label: String,
    pub local_path_hint: Option<String>,
    pub client_os: Option<String>,
    pub latest_version_num: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: Uuid,
    pub save_id: Uuid,
    pub version_num: i64,
    pub device_name: Option<String>,
    pub notes: Option<String>,
    pub total_size_bytes: i64,
    pub file_count: i64,
    pub is_pinned: bool,
    pub deleted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub id: Uuid,
    pub snapshot_id: Uuid,
    pub relative_path: String,
    pub size_bytes: i64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: String,
    pub device_name: Option<String>,
    pub last_used_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}
