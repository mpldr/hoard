use axum::{extract::Extension, response::Json};
use serde::Serialize;

use crate::auth::AuthUser;

#[derive(Serialize)]
pub struct WhoamiResponse {
    user_id: String,
    username: String,
    is_admin: bool,
}

pub async fn whoami(Extension(user): Extension<AuthUser>) -> Json<WhoamiResponse> {
    Json(WhoamiResponse {
        user_id: user.user_id.to_string(),
        username: user.username,
        is_admin: user.is_admin,
    })
}
