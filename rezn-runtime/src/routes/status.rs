use crate::{
    reconcile::Status,
    routes::common::{app_error, AppError},
    AppState,
};
use axum::{extract::State, Json};
use std::sync::Arc;

#[utoipa::path(
    get, path = "/status",
    responses((status = 200, description = "Desired and last observed outcomes. Unknown/stale/pending observations never claim convergence. Docker Ports come from listings, not the create response.", body = Status), (status = 500, description = "Persisted intent cannot be read or validated", body = String)), tag = "Status"
)]
pub async fn get_status_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<Status>, AppError> {
    Ok(Json(
        app.controller.status(&app.db).await.map_err(app_error)?,
    ))
}
