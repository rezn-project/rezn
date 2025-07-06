use anyhow::anyhow;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
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
        ("key" = String, Query, description = "The full secret key name, e.g. rezn/prod/db_url")
    ),
    responses(
        (status = 200, description = "Secret deleted", body = bool),
        (status = 404, description = "Secret not found")
    ),
    tag = "Secrets",
)]
pub async fn delete_secret_handler(
    State(app): State<Arc<AppState>>,
    Query(query): Query<SecretQuery>,
) -> Result<Json<bool>, AppError> {
    let existed = app
        .secret_store
        .get(&query.key)
        .map_err(app_error)?
        .is_some();

    if !existed {
        return Err(app_error(anyhow!("Secret '{}' not found", query.key)));
    }

    app.secret_store.delete(&query.key).map_err(app_error)?;

    Ok(Json(true))
}
