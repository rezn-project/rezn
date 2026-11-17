use crate::{
    routes::common::{app_error, AppError},
    store, AppState,
};
use axum::{extract::State, Json};
use std::collections::BTreeMap;
use std::sync::Arc;

#[utoipa::path(
    get, path = "/state",
    responses((status = 200, description = "Accepted desired intent per deployment, not runtime outcomes: an IR v1 instruction array or an IR v2 program object", body = Object), (status = 500, description = "Persisted state cannot be read or validated", body = String)), tag = "State"
)]
pub async fn get_state_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<BTreeMap<String, serde_json::Value>>, AppError> {
    Ok(Json(
        store::load(&app.db)
            .and_then(|state| state.programs())
            .map_err(app_error)?,
    ))
}

#[utoipa::path(
    get, path = "/state/raw",
    responses((status = 200, description = "Desired intent as JSON", body = Object), (status = 500, description = "Persisted state cannot be read or validated", body = String)), tag = "State"
)]
pub async fn get_state_raw_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<BTreeMap<String, serde_json::Value>>, AppError> {
    get_state_handler(State(app)).await
}
