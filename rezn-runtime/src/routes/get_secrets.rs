use anyhow::anyhow;
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

use crate::routes::common::{app_error, AppError};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SecretQuery {
    key: String,
}

#[utoipa::path(
    get,
    path = "/secret",
    params(
        ("key" = String, Query, description = "The full secret key name, e.g. rezn/prod/db_url")
    ),
    responses(
        (status = 200, description = "Decrypted secret value", body = String),
        (status = 404, description = "Secret not found")
    ),
    tag = "Secrets",
)]
pub async fn get_secret_handler(
    State(app): State<Arc<AppState>>,
    Query(query): Query<SecretQuery>,
) -> Result<Json<String>, AppError> {
    let plaintext = app
        .secret_store
        .get(&query.key)
        .map_err(app_error)?
        .ok_or_else(|| app_error(anyhow!("Secret '{}' not found", query.key)))?;

    let secret_str = String::from_utf8(plaintext)
        .map_err(|_| app_error(anyhow!("Secret '{}' is not valid UTF-8", query.key)))?;

    Ok(Json(secret_str))
}

#[utoipa::path(
    get,
    path = "/secrets",
    responses(
        (status = 200, body = Object)
    ),
    tag = "Secrets",
)]
pub async fn get_secrets_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<Vec<String>>, AppError> {
    let keys: Vec<String> = app.secret_store.keys().map_err(app_error)?;

    Ok(Json(keys))
}
