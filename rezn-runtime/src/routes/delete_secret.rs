use anyhow::anyhow;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::{
    routes::common::{app_error, AppError},
    AppState,
};

#[derive(Debug, Deserialize)]
pub struct SecretQuery {
    key: String,
}

#[utoipa::path(
    delete,
    path = "/secret",
    params(
        ("key" = String, Query, description = "Full secret key, e.g. rezn/prod/db_url")
    ),
    responses(
        (status = 200, body = bool, description = "Secret deleted"),
        (status = 404, description = "Secret not found")
    ),
    tag = "Secrets",
)]
pub async fn delete_secret_handler(
    State(app): State<Arc<AppState>>,
    Query(query): Query<SecretQuery>,
) -> Result<(StatusCode, Json<bool>), AppError> {
    let removed = app.secret_store.delete(&query.key).map_err(app_error)?; // ↓ returns bool now

    if removed {
        Ok((StatusCode::OK, Json(true)))
    } else {
        Ok((StatusCode::NOT_FOUND, Json(false)))
    }
}
