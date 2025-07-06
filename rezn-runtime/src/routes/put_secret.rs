use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use utoipa::ToSchema;

use anyhow::Result;

use crate::{
    routes::common::{app_error, AppError},
    AppState,
};

#[derive(Debug, Deserialize, ToSchema)]
pub struct PutSecretPayload {
    name: String,
    secret: String,
}

#[utoipa::path(
    post,
    path = "/secrets",
    request_body(
        content = PutSecretPayload,
        description = "Payload to create a secret",
        content_type = "application/json",
    ),
    responses(
        (status = 200, body = bool)
    ),
    tag = "Secrets",
)]
pub async fn put_secret_handler(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<PutSecretPayload>,
) -> Result<Json<bool>, AppError> {
    let name = payload.name;
    let secret = payload.secret;

    app.secret_store
        .put(&name, secret.as_bytes())
        .map_err(app_error)?;

    Ok(Json(true))
}
