use axum::extract::State;
use axum::Json;

use std::sync::Arc;

use crate::routes::common::AppError;
use crate::{AppState, ContainerID, StatsMap, TimestampedStats};

#[utoipa::path(
    get,
    path = "/stats",
    responses(
        (status = 200, body = BTreeMap<ContainerID, TimestampedStats>)
    ),
    tag = "Stats",
)]
#[axum::debug_handler]
pub async fn get_stats_handler(
    State(app): State<Arc<AppState>>,
) -> Result<Json<StatsMap>, AppError> {
    let stats = app.stats.read().await;

    Ok(Json(stats.clone()))
}
