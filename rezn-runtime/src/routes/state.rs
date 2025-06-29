use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use common::types::DesiredMap;
use reqwest::StatusCode;
use std::collections::BTreeMap;
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

    let desired: DesiredMap = match app.db.get("desired").map_err(app_error)? {
        Some(bytes) => match serde_json::from_slice(&bytes) {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!("Invalid 'desired' state, falling back to empty: {e}");
                BTreeMap::new()
            }
        },
        None => {
            tracing::debug!("No 'desired' state found, returning empty");
            BTreeMap::new()
        }
    };

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
    let data = match app.db.get("desired").map_err(app_error)? {
        Some(ivec) => Bytes::from(ivec.to_vec()),
        None => Bytes::copy_from_slice(b"{}"),
    };
    Ok(([("Content-Type", "application/json")], Bytes::from(data)).into_response())
}

fn app_error<E: std::fmt::Display>(e: E) -> AppError {
    tracing::warn!("internal error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
