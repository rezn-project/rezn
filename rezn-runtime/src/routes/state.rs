use crate::{
    routes::common::{app_error, AppError},
    store, AppState,
};
use axum::{extract::State, Json};
use common::types::DesiredMap;
use std::sync::Arc;

#[utoipa::path(
    get, path = "/state",
    responses((status = 200, description = "Accepted desired intent, not runtime outcomes", body = Object), (status = 500, description = "Persisted state cannot be read or validated", body = String)), tag = "State"
)]
pub async fn get_state_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<DesiredMap>, AppError> {
    Ok(Json(
        store::load(&app.db)
            .and_then(|state| state.desired())
            .map_err(app_error)?,
    ))
}

#[utoipa::path(
    get, path = "/state/raw",
    responses((status = 200, description = "Desired intent as JSON", body = Object), (status = 500, description = "Persisted state cannot be read or validated", body = String)), tag = "State"
)]
pub async fn get_state_raw_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<DesiredMap>, AppError> {
    get_state_handler(State(app)).await
}
