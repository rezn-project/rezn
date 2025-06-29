use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use common::types::DesiredMap;
use reqwest::StatusCode;
use std::sync::Arc;

use crate::AppState;

type AppError = (StatusCode, String);

#[utoipa::path(
    get,
    path = "/state",
    responses(
        (status = 200, body = Object)
    ),
    tag = "State",
)]
pub async fn get_state_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<DesiredMap>, AppError> {
    tracing::debug!("Retrieving current state");

    let data = app.store.read("desired").map_err(app_error)?;
    let desired: DesiredMap = serde_json::from_slice(&data).map_err(app_error)?;

    Ok(Json(desired))
}

#[utoipa::path(
    get,
    path = "/state/raw",
    responses(
        (status = 200, body = Object)
    ),
    tag = "State",
)]
pub async fn get_state_raw_handler(State(app): State<Arc<AppState>>) -> Result<Response, AppError> {
    let data = app.store.read("desired").map_err(app_error)?;
    Ok(([("Content-Type", "application/json")], Bytes::from(data)).into_response())
}

fn app_error<E: std::fmt::Display>(e: E) -> AppError {
    tracing::warn!("internal error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
