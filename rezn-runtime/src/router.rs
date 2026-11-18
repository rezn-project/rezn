use std::sync::Arc;

use axum::{
    routing::{delete, get, post},
    Router,
};
use utoipa::OpenApi;

use crate::{
    routes::{
        apply::apply_handler,
        delete_secret::delete_secret_handler,
        get_secrets::{get_secret_handler, get_secrets_handler},
        program_secrets::{delete_program_secret, list_program_secrets, put_program_secret},
        put_secret::put_secret_handler,
        state::{get_state_handler, get_state_raw_handler},
        status::get_status_handler,
    },
    AppState,
};

#[derive(OpenApi)]
#[openapi(
    info(description = "Rezn Api"),
    paths(
        crate::routes::apply::apply_handler,
        crate::routes::state::get_state_handler,
        crate::routes::status::get_status_handler,
        crate::routes::state::get_state_raw_handler,
        crate::routes::get_secrets::get_secret_handler,
        crate::routes::get_secrets::get_secrets_handler,
        crate::routes::put_secret::put_secret_handler,
        crate::routes::delete_secret::delete_secret_handler,
        crate::routes::program_secrets::put_program_secret,
        crate::routes::program_secrets::list_program_secrets,
        crate::routes::program_secrets::delete_program_secret
    )
)]
struct ApiDoc;

pub(crate) fn build_router(app: Arc<AppState>) -> Router {
    Router::new()
        .route("/secrets", get(get_secrets_handler))
        .route("/secret", get(get_secret_handler))
        .route("/secret", delete(delete_secret_handler))
        .route("/secrets", post(put_secret_handler))
        .route("/namespaces/{namespace}/secrets", get(list_program_secrets))
        .route(
            "/namespaces/{namespace}/secrets/{name}",
            axum::routing::put(put_program_secret).delete(delete_program_secret),
        )
        .route("/apply", post(apply_handler))
        .route("/status", get(get_status_handler))
        .route("/state", get(get_state_handler))
        .route("/state/raw", get(get_state_raw_handler))
        .with_state(app)
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/swagger")
                .url("/api/openapi.json", ApiDoc::openapi()),
        )
}
