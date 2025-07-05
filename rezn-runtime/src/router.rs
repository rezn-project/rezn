use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use utoipa::OpenApi;

use crate::{
    routes::{
        apply::apply_handler,
        state::{get_state_handler, get_state_raw_handler},
        stats::get_stats_handler,
        stats_ws::stats_ws_handler,
    },
    AppState,
};

#[derive(OpenApi)]
#[openapi(
    info(description = "Rezn Api"),
    paths(
        crate::routes::apply::apply_handler,
        crate::routes::state::get_state_handler,
        crate::routes::state::get_state_raw_handler,
        crate::routes::stats::get_stats_handler,
        crate::routes::stats_ws::stats_ws_handler,
    )
)]
struct ApiDoc;

pub(crate) fn build_router(app: Arc<AppState>) -> Router {
    Router::new()
        .route("/apply", post(apply_handler))
        .route("/stats", get(get_stats_handler))
        .route("/stats/ws", get(stats_ws_handler))
        .route("/state", get(get_state_handler))
        .route("/state/raw", get(get_state_raw_handler))
        .with_state(app)
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/swagger")
                .url("/api/openapi.json", ApiDoc::openapi()),
        )
}
