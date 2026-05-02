use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::routes::health::ServerState;

#[derive(Deserialize)]
pub struct SearchQuery {
    search: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    20
}

#[derive(Serialize)]
pub struct GameResponse {
    slug: String,
    display_name: String,
    engine: Option<String>,
    save_paths_json: Option<String>,
}

pub async fn list(
    State(state): State<Arc<ServerState>>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<GameResponse>>, StatusCode> {
    let limit = q.limit.clamp(1, 100);

    let rows = if let Some(search) = q.search {
        let pattern = format!("%{}%", search);
        sqlx::query_as!(
            GameRow,
            "SELECT slug, display_name, engine, save_paths_json FROM games
             WHERE slug LIKE ? OR display_name LIKE ?
             ORDER BY slug LIMIT ?",
            pattern,
            pattern,
            limit
        )
        .fetch_all(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        sqlx::query_as!(
            GameRow,
            "SELECT slug, display_name, engine, save_paths_json FROM games
             ORDER BY slug LIMIT ?",
            limit
        )
        .fetch_all(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    Ok(Json(
        rows.into_iter()
            .map(|r| GameResponse {
                slug: r.slug,
                display_name: r.display_name,
                engine: r.engine,
                save_paths_json: r.save_paths_json,
            })
            .collect(),
    ))
}

pub async fn get_one(
    State(state): State<Arc<ServerState>>,
    Path(slug): Path<String>,
) -> Result<Json<GameResponse>, StatusCode> {
    let row = sqlx::query_as!(
        GameRow,
        "SELECT slug, display_name, engine, save_paths_json FROM games WHERE slug = ?",
        slug
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(GameResponse {
        slug: row.slug,
        display_name: row.display_name,
        engine: row.engine,
        save_paths_json: row.save_paths_json,
    }))
}

struct GameRow {
    slug: String,
    display_name: String,
    engine: Option<String>,
    save_paths_json: Option<String>,
}
